//! Immutable exchange-calendar validation shared by every strategy process.
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::Path;

const IST_OFFSET_MINUTES: i64 = 330;

#[derive(Clone, Debug, Deserialize)]
struct SessionSpec {
    open: String,
    close: String,
    eligible_last_minute: String,
    #[serde(default)]
    intraday_breaks: Vec<BreakSpec>,
}

#[derive(Clone, Debug, Deserialize)]
struct BreakSpec {
    start: String,
    end: String,
}

#[derive(Clone, Debug, Deserialize)]
struct SpecialSession {
    date: String,
    open: String,
    close: String,
    eligible_last_minute: String,
    #[serde(default)]
    intraday_breaks: Vec<BreakSpec>,
}

#[derive(Clone, Debug, Deserialize)]
struct CalendarFile {
    calendar_version: u32,
    status: String,
    identity: String,
    timezone: String,
    authority: String,
    regular_session: SessionSpec,
    holding_clock: String,
    holidays: Vec<String>,
    special_sessions: Vec<SpecialSession>,
    missing_session_policy: String,
    observed_quotes_may_define_session_bounds: bool,
}

#[derive(Clone, Debug)]
struct Window {
    open: i64,
    close: i64,
    last: i64,
    breaks: Vec<(i64, i64)>,
}

#[derive(Clone, Debug)]
pub struct CalendarGuard {
    regular: Window,
    holidays: BTreeSet<String>,
    specials: BTreeMap<String, Window>,
}

impl CalendarGuard {
    /// Load, hash-check, and normalize the immutable calendar.
    ///
    /// # Errors
    ///
    /// Returns an error for a hash mismatch, placeholder, unsupported policy,
    /// duplicate date, or malformed window.
    pub fn load(path: &Path, expected_sha256: &str) -> Result<Self, String> {
        let bytes = fs::read(path).map_err(|error| format!("calendar read failed: {error}"))?;
        let actual = format!("{:x}", Sha256::digest(&bytes));
        if actual != expected_sha256 {
            return Err("calendar SHA-256 mismatch".into());
        }
        let file: CalendarFile = serde_json::from_slice(&bytes)
            .map_err(|error| format!("calendar JSON invalid: {error}"))?;
        if file.calendar_version != 1
            || file.status != "READY"
            || file.identity.is_empty()
            || file.identity == "PENDING"
            || file.authority.is_empty()
            || file.authority == "PENDING"
            || file.timezone != "Asia/Kolkata"
            || file.holding_clock != "elapsed_calendar_minutes"
            || file.missing_session_policy != "fail_closed"
            || file.observed_quotes_may_define_session_bounds
        {
            return Err("calendar identity or policy is not executable".into());
        }
        let regular = window(&file.regular_session)?;
        let mut holidays = BTreeSet::new();
        for date in file.holidays {
            parse_date(&date)?;
            if !holidays.insert(date) {
                return Err("duplicate holiday".into());
            }
        }
        let mut specials = BTreeMap::new();
        for special in file.special_sessions {
            parse_date(&special.date)?;
            let spec = SessionSpec {
                open: special.open,
                close: special.close,
                eligible_last_minute: special.eligible_last_minute,
                intraday_breaks: special.intraday_breaks,
            };
            if specials.insert(special.date, window(&spec)?).is_some() {
                return Err("duplicate special session".into());
            }
        }
        if specials.keys().any(|date| holidays.contains(date)) {
            return Err("date cannot be both holiday and special session".into());
        }
        Ok(Self {
            regular,
            holidays,
            specials,
        })
    }

    /// Validate a declared exchange session date.
    ///
    /// # Errors
    ///
    /// Returns an error for a holiday, weekend, or malformed date.
    pub fn validate_session_date(&self, date: &str) -> Result<(), String> {
        self.session(date).map(|_| ())
    }

    /// Validate an epoch minute against its declared local session.
    ///
    /// # Errors
    ///
    /// Returns an error when date, session window, or break membership fails.
    pub fn validate_minute(&self, date: &str, epoch_minute: i64) -> Result<(), String> {
        let day = parse_date(date)?;
        let local = epoch_minute
            .checked_add(IST_OFFSET_MINUTES)
            .ok_or_else(|| "calendar minute overflow".to_owned())?;
        if local.div_euclid(1440) != day {
            return Err("declared session date differs from decision clock".into());
        }
        let minute = local.rem_euclid(1440);
        let session = self.session(date)?;
        if minute < session.open || minute >= session.close || minute > session.last {
            return Err("minute is outside the eligible session window".into());
        }
        if session
            .breaks
            .iter()
            .any(|(start, end)| (*start..*end).contains(&minute))
        {
            return Err("minute falls inside an intraday break".into());
        }
        Ok(())
    }

    /// Return the last executable epoch minute for a declared session.
    ///
    /// # Errors
    ///
    /// Returns an error for an invalid or unavailable exchange session.
    pub fn eligible_last_epoch_minute(&self, date: &str) -> Result<i64, String> {
        let day = parse_date(date)?;
        let session = self.session(date)?;
        day.checked_mul(1440)
            .and_then(|value| value.checked_add(session.last))
            .and_then(|value| value.checked_sub(IST_OFFSET_MINUTES))
            .ok_or_else(|| "calendar minute overflow".to_owned())
    }

    fn session(&self, date: &str) -> Result<&Window, String> {
        let day = parse_date(date)?;
        if let Some(window) = self.specials.get(date) {
            return Ok(window);
        }
        if self.holidays.contains(date) || (day + 3).rem_euclid(7) >= 5 {
            return Err("date is not an exchange session".into());
        }
        Ok(&self.regular)
    }
}

fn window(spec: &SessionSpec) -> Result<Window, String> {
    let open = parse_time(&spec.open)?;
    let close = parse_time(&spec.close)?;
    let last = parse_time(&spec.eligible_last_minute)?;
    if open >= close || last < open || last >= close {
        return Err("calendar session window is invalid".into());
    }
    let mut breaks = Vec::new();
    for item in &spec.intraday_breaks {
        let start = parse_time(&item.start)?;
        let end = parse_time(&item.end)?;
        if start < open || start >= end || end > close {
            return Err("calendar break is outside the session".into());
        }
        breaks.push((start, end));
    }
    breaks.sort_unstable();
    if breaks.windows(2).any(|pair| pair[0].1 > pair[1].0) {
        return Err("calendar breaks overlap".into());
    }
    Ok(Window {
        open,
        close,
        last,
        breaks,
    })
}

fn parse_time(value: &str) -> Result<i64, String> {
    let fields: Vec<_> = value.split(':').collect();
    if fields.len() != 3 {
        return Err("calendar time must be HH:MM:SS".into());
    }
    let hour: i64 = fields[0].parse().map_err(|_| "invalid calendar hour")?;
    let minute: i64 = fields[1].parse().map_err(|_| "invalid calendar minute")?;
    let second: i64 = fields[2].parse().map_err(|_| "invalid calendar second")?;
    if !(0..24).contains(&hour) || !(0..60).contains(&minute) || second != 0 {
        return Err("calendar time is not minute-aligned".into());
    }
    Ok(hour * 60 + minute)
}

fn parse_date(value: &str) -> Result<i64, String> {
    let fields: Vec<_> = value.split('-').collect();
    if fields.len() != 3 {
        return Err("calendar date must be YYYY-MM-DD".into());
    }
    let year: i64 = fields[0].parse().map_err(|_| "invalid calendar year")?;
    let month: i64 = fields[1].parse().map_err(|_| "invalid calendar month")?;
    let day: i64 = fields[2].parse().map_err(|_| "invalid calendar day")?;
    let leap = year % 4 == 0 && (year % 100 != 0 || year % 400 == 0);
    let month_days = [
        31,
        if leap { 29 } else { 28 },
        31,
        30,
        31,
        30,
        31,
        31,
        30,
        31,
        30,
        31,
    ];
    if !(1..=12).contains(&month) {
        return Err("calendar date is invalid".into());
    }
    let month_index = usize::try_from(month - 1).map_err(|_| "calendar date is invalid")?;
    if day < 1 || day > month_days[month_index] {
        return Err("calendar date is invalid".into());
    }
    let adjusted_year = year - i64::from(month <= 2);
    let era = adjusted_year.div_euclid(400);
    let year_of_era = adjusted_year - era * 400;
    let adjusted_month = month + if month > 2 { -3 } else { 9 };
    let day_of_year = (153 * adjusted_month + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    Ok(era * 146_097 + day_of_era - 719_468)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    static FIXTURE_SEQUENCE: AtomicU64 = AtomicU64::new(0);

    fn fixture() -> (std::path::PathBuf, String) {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let sequence = FIXTURE_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let process = std::process::id();
        let path = std::env::temp_dir().join(format!(
            "iv-shock-calendar-{process}-{nonce}-{sequence}.json"
        ));
        let value = serde_json::json!({
            "calendar_version": 1,
            "status": "READY",
            "identity": "fixture-v1",
            "timezone": "Asia/Kolkata",
            "authority": "NSE fixture",
            "regular_session": {"open":"09:15:00","close":"15:30:00","eligible_last_minute":"15:29:00","intraday_breaks":[]},
            "holding_clock": "elapsed_calendar_minutes",
            "holidays": ["2024-01-26"],
            "special_sessions": [{"date":"2024-01-20","open":"18:00:00","close":"19:00:00","eligible_last_minute":"18:59:00","intraday_breaks":[]}],
            "missing_session_policy": "fail_closed",
            "observed_quotes_may_define_session_bounds": false
        });
        let bytes = serde_json::to_vec(&value).unwrap();
        fs::write(&path, &bytes).unwrap();
        let digest = format!("{:x}", Sha256::digest(&bytes));
        (path, digest)
    }

    fn epoch_minute(date: &str, local_minute: i64) -> i64 {
        parse_date(date).unwrap() * 1440 + local_minute - IST_OFFSET_MINUTES
    }

    #[test]
    fn normal_holiday_weekend_and_special_windows_are_enforced() {
        let (path, digest) = fixture();
        let guard = CalendarGuard::load(&path, &digest).unwrap();
        assert!(
            guard
                .validate_minute("2024-01-01", epoch_minute("2024-01-01", 9 * 60 + 15))
                .is_ok()
        );
        assert!(
            guard
                .validate_minute("2024-01-01", epoch_minute("2024-01-01", 9 * 60 + 14))
                .is_err()
        );
        assert!(
            guard
                .validate_minute("2024-01-01", epoch_minute("2024-01-01", 15 * 60 + 29))
                .is_ok()
        );
        assert!(
            guard
                .validate_minute("2024-01-01", epoch_minute("2024-01-01", 15 * 60 + 30))
                .is_err()
        );
        assert!(guard.validate_session_date("2024-01-26").is_err());
        assert!(guard.validate_session_date("2024-01-27").is_err());
        assert!(
            guard
                .validate_minute("2024-01-20", epoch_minute("2024-01-20", 18 * 60 + 59))
                .is_ok()
        );
        assert_eq!(
            guard.eligible_last_epoch_minute("2024-01-01").unwrap(),
            epoch_minute("2024-01-01", 15 * 60 + 29)
        );
        assert_eq!(
            guard.eligible_last_epoch_minute("2024-01-20").unwrap(),
            epoch_minute("2024-01-20", 18 * 60 + 59)
        );
        assert!(
            guard
                .validate_minute("2024-01-20", epoch_minute("2024-01-20", 15 * 60))
                .is_err()
        );
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn wrong_hash_and_clock_date_fail_closed() {
        let (path, digest) = fixture();
        assert!(CalendarGuard::load(&path, &format!("x{digest}")).is_err());
        let guard = CalendarGuard::load(&path, &digest).unwrap();
        assert!(
            guard
                .validate_minute("2024-01-02", epoch_minute("2024-01-01", 10 * 60))
                .is_err()
        );
        fs::remove_file(path).unwrap();
    }
}
