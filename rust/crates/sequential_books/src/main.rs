//! Canonical sequential research policy service. No prices or accounting.
use backtest_contracts::{CONTRACT_VERSION, ResearchRequest, ResearchResponse, TradeIntent};
use iv_shock_calendar_guard::CalendarGuard;
use iv_shock_sequential_books::{
    Admission, Books, Candidate, Recipient, Selector, SourceUnit, schedule_sources,
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::{BTreeMap, BTreeSet};
use std::io::{self, BufRead, Write};
use std::path::Path;

#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum Packet {
    Schedule {
        rows: Vec<Candidate>,
        horizons: Vec<u32>,
    },
    Minute {
        sources: Vec<SourceUnit>,
        recipients: Vec<Recipient>,
        selectors: Vec<Selector>,
        #[serde(default)]
        compact: bool,
        #[serde(default)]
        source_groups: BTreeMap<String, String>,
    },
    Summary,
}

fn validate_calendar(guard: &CalendarGuard, request: &ResearchRequest) -> Result<(), String> {
    let packet: Packet = serde_json::from_value(request.input.research_payload.clone())
        .map_err(|error| error.to_string())?;
    match packet {
        Packet::Schedule { rows, .. } => {
            for row in rows {
                guard.validate_minute(&row.session_date, row.event_minute)?;
            }
        }
        Packet::Minute { sources, .. } => {
            for source in sources {
                guard.validate_minute(&source.session_date, source.event_minute)?;
                if source.session_end_minute
                    != guard.eligible_last_epoch_minute(&source.session_date)?
                {
                    return Err("source session end differs from bound exchange calendar".into());
                }
            }
        }
        Packet::Summary => {}
    }
    Ok(())
}
#[derive(Clone, Default)]
struct Runner {
    sequence: u64,
    initialized: bool,
    books: Books,
    daily_rejection_counts: BTreeMap<(String, String, String), u64>,
    daily_attempts: u64,
    daily_admitted: u64,
    daily_rejected: u64,
    bundle_hash: String,
}

#[derive(Deserialize, Serialize)]
struct PersistedState {
    bundle_hash: String,
    sequence: u64,
    books: Books,
    rejection_counts: Vec<(String, String, String, u64)>,
    attempts: u64,
    admitted: u64,
    rejected: u64,
}

fn validate_compact_source_groups(
    sources: &[SourceUnit],
    source_groups: &BTreeMap<String, String>,
) -> Result<(), String> {
    let source_ids: BTreeSet<String> = sources.iter().map(|source| source.id.clone()).collect();
    if source_ids.len() != sources.len() {
        return Err("compact source_groups require unique source unit IDs".into());
    }
    if source_groups.len() != source_ids.len() {
        return Err("compact source_groups must be an exact source ID map".into());
    }
    if source_groups.keys().any(|id| !source_ids.contains(id)) {
        return Err("compact source_groups contain an unknown source unit ID".into());
    }
    if source_ids.iter().any(|id| !source_groups.contains_key(id)) {
        return Err("compact source_groups are missing a source unit ID".into());
    }
    if source_groups.values().any(String::is_empty) {
        return Err("compact source_groups require non-empty descriptors".into());
    }
    Ok(())
}
impl Runner {
    fn persisted_state(&self) -> PersistedState {
        PersistedState {
            bundle_hash: self.bundle_hash.clone(),
            sequence: self.sequence,
            books: self.books.clone(),
            rejection_counts: self
                .daily_rejection_counts
                .iter()
                .map(|((book, group, reason), count)| {
                    (book.clone(), group.clone(), reason.clone(), *count)
                })
                .collect(),
            attempts: self.daily_attempts,
            admitted: self.daily_admitted,
            rejected: self.daily_rejected,
        }
    }

    fn restore_or_validate(
        &mut self,
        state: &serde_json::Value,
        sequence: u64,
    ) -> Result<(), String> {
        let core = state.get("runner_state").unwrap_or(state);
        if !self.initialized {
            if core.is_null() || core == &json!({}) {
                if sequence != 0 {
                    return Err("fresh books runner requires state for nonzero sequence".into());
                }
            } else {
                let restored: PersistedState =
                    serde_json::from_value(core.clone()).map_err(|error| error.to_string())?;
                if restored.sequence != sequence {
                    return Err("restored books sequence mismatch".into());
                }
                if restored.bundle_hash != self.bundle_hash {
                    return Err("restored books bundle hash mismatch".into());
                }
                self.sequence = restored.sequence;
                self.books = restored.books;
                self.daily_rejection_counts = restored
                    .rejection_counts
                    .into_iter()
                    .map(|(book, group, reason, count)| ((book, group, reason), count))
                    .collect();
                self.daily_attempts = restored.attempts;
                self.daily_admitted = restored.admitted;
                self.daily_rejected = restored.rejected;
            }
            self.initialized = true;
            return Ok(());
        }
        let expected = serde_json::to_value(self.persisted_state()).map_err(|e| e.to_string())?;
        if core != &expected || sequence != self.sequence {
            return Err("books state/request differs from live runner".into());
        }
        Ok(())
    }

    fn record_minute_counts(
        &mut self,
        admissions: &[Admission],
        source_groups: &BTreeMap<String, String>,
    ) -> (u64, u64, u64) {
        let attempts = admissions.len() as u64;
        let admitted = admissions
            .iter()
            .filter(|admission| admission.admitted)
            .count() as u64;
        let rejected = attempts - admitted;
        self.daily_attempts += attempts;
        self.daily_admitted += admitted;
        self.daily_rejected += rejected;
        for admission in admissions.iter().filter(|admission| !admission.admitted) {
            let group = source_groups
                .get(&admission.source_unit_id)
                .cloned()
                .unwrap_or_else(|| "unattributed".into());
            let key = (admission.book_id.clone(), group, admission.reason.clone());
            *self.daily_rejection_counts.entry(key).or_default() += 1;
        }
        (attempts, admitted, rejected)
    }

    fn process_minute(
        &mut self,
        sources: Vec<SourceUnit>,
        recipients: &[Recipient],
        selectors: &[Selector],
        compact: bool,
        source_groups: &BTreeMap<String, String>,
    ) -> Result<(serde_json::Value, Vec<TradeIntent>), String> {
        // This check must precede Books::on_minute: that method advances its
        // chronological state before constructing the reservation ledger.
        if compact {
            validate_compact_source_groups(&sources, source_groups)?;
        }
        let admissions = self.books.on_minute(sources, recipients, selectors)?;
        let (attempts, admitted, rejected) = self.record_minute_counts(&admissions, source_groups);
        let visible_admissions: Vec<Admission> = if compact {
            admissions
                .iter()
                .filter(|admission| admission.admitted)
                .cloned()
                .collect()
        } else {
            admissions.clone()
        };
        let actions = admissions
            .iter()
            .filter_map(|admission| admission.intent.clone())
            .collect();
        let detail = json!({
            "admissions": visible_admissions,
            "attempts": attempts,
            "admitted": admitted,
            "rejected": rejected,
        });
        Ok((detail, actions))
    }

    fn summary_detail(&mut self) -> serde_json::Value {
        let daily_rejection_counts: Vec<serde_json::Value> = self
            .daily_rejection_counts
            .iter()
            .map(|((book_id, group, reason), attempts)| {
                json!({
                    "book_id": book_id,
                    "group": group,
                    "reason": reason,
                    "attempts": attempts,
                })
            })
            .collect();
        let detail = json!({
            "daily_rejection_counts": daily_rejection_counts,
            "attempts": self.daily_attempts,
            "admitted": self.daily_admitted,
            "rejected": self.daily_rejected,
        });
        self.daily_rejection_counts.clear();
        self.daily_attempts = 0;
        self.daily_admitted = 0;
        self.daily_rejected = 0;
        detail
    }

    #[allow(clippy::needless_pass_by_value)]
    fn process(&mut self, request: ResearchRequest) -> Result<ResearchResponse, String> {
        let snapshot = self.clone();
        match self.process_inner(request) {
            Ok(response) => Ok(response),
            Err(error) => {
                *self = snapshot;
                Err(error)
            }
        }
    }

    #[allow(clippy::needless_pass_by_value)]
    fn process_inner(&mut self, request: ResearchRequest) -> Result<ResearchResponse, String> {
        if request.schema_version != CONTRACT_VERSION
            || request.input.schema_version != CONTRACT_VERSION
            || request.input.sequence != request.sequence
            || request.feedback.sequence != request.sequence.saturating_sub(1)
            || (request.sequence == 0 && !request.feedback.outcomes.is_empty())
            || request.input.available_at_ns > request.input.sealed_at_ns
            || request.input.sealed_at_ns > request.input.decision_at_ns
            || request.feedback_context_hash.is_empty()
            || request.feedback_feature_hash.is_empty()
        {
            return Err("canonical sequence/schema/clock mismatch".into());
        }
        self.restore_or_validate(&request.state, request.sequence)?;
        let packet: Packet =
            serde_json::from_value(request.input.research_payload).map_err(|e| e.to_string())?;
        let (detail, actions) = match packet {
            Packet::Schedule { rows, horizons } => (
                json!({"scheduled":schedule_sources(rows,&horizons)?}),
                vec![],
            ),
            Packet::Minute {
                sources,
                recipients,
                selectors,
                compact,
                source_groups,
            } => {
                if sources.iter().any(|s| {
                    s.event_minute.checked_mul(60_000_000_000) != Some(request.input.decision_at_ns)
                }) {
                    return Err("source clock differs from sealed clock".into());
                }
                self.process_minute(sources, &recipients, &selectors, compact, &source_groups)?
            }
            Packet::Summary => (self.summary_detail(), vec![]),
        };
        self.sequence = self.sequence.checked_add(1).ok_or("sequence overflow")?;
        Ok(ResearchResponse {
            schema_version: CONTRACT_VERSION.into(),
            artifact_consumed: true,
            runner_id: "h5-book-policy-v1".into(),
            bundle_hash: self.bundle_hash.clone(),
            state: json!({"runner_state":self.persisted_state(),"detail":detail}),
            actions,
        })
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let calendar_path = std::env::var("IV_SHOCK_CALENDAR_PATH")?;
    let calendar_hash = std::env::var("IV_SHOCK_CALENDAR_SHA256")?;
    let calendar = CalendarGuard::load(Path::new(&calendar_path), &calendar_hash)?;
    std::env::var("IV_SHOCK_SOURCE_CONTRACT_SHA256")?;
    let bundle_hash = std::env::var("IV_SHOCK_RESEARCH_BUNDLE_HASH")?;
    let mut runner = Runner {
        bundle_hash,
        ..Runner::default()
    };
    let mut output = io::BufWriter::new(io::stdout().lock());
    for line in io::stdin().lock().lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let request: ResearchRequest = serde_json::from_str(&line)?;
        validate_calendar(&calendar, &request)?;
        let response = runner.process(request).map_err(io::Error::other)?;
        serde_json::to_writer(&mut output, &response)?;
        writeln!(output)?;
        output.flush()?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use backtest_contracts::{AccountState, EngineFeedback, Money, SealedEvent};

    fn source(id: &str) -> SourceUnit {
        SourceUnit {
            id: id.into(),
            detector: "S0".into(),
            session_date: "2024-01-01".into(),
            event_minute: 100,
            expiry: "2024-02-01".into(),
            side: "CE".into(),
            horizon: 5,
            session_end_minute: 599,
        }
    }

    fn recipient() -> Recipient {
        Recipient {
            contract_id: "r".into(),
            expiry: "2024-02-01".into(),
            expiry_minute: 100_000,
            side: "CE".into(),
            money_bin: "atm".into(),
            dte_bin: "31-60".into(),
            logm: 0.0,
            lot_size: Some(25),
            lot_status: "resolved".into(),
        }
    }

    fn selector() -> Selector {
        Selector {
            id: "CE@atm@31-60".into(),
            side: "CE".into(),
            money_bin: "atm".into(),
            dte_bin: "31-60".into(),
            target_logm: 0.0,
        }
    }

    fn groups() -> BTreeMap<String, String> {
        BTreeMap::from([
            (
                "u1".into(),
                "{\"source_policy\":\"frozen_source\",\"source_side\":\"CE\"}".into(),
            ),
            (
                "u2".into(),
                "{\"source_policy\":\"frozen_source\",\"source_side\":\"PE\"}".into(),
            ),
        ])
    }

    fn request(sequence: u64, minute: i64, state: serde_json::Value) -> ResearchRequest {
        let mut minute_source = source("u1");
        minute_source.event_minute = minute;
        ResearchRequest {
            schema_version: CONTRACT_VERSION.to_owned(),
            input: SealedEvent {
                schema_version: CONTRACT_VERSION.to_owned(),
                event_id: format!("event-{sequence}"),
                sequence,
                decision_at_ns: minute * 60_000_000_000,
                available_at_ns: minute * 60_000_000_000,
                sealed_at_ns: minute * 60_000_000_000,
                quotes: BTreeMap::new(),
                margin_facts: BTreeMap::new(),
                research_payload: json!({"kind":"minute","sources":[minute_source],"recipients":[recipient()],"selectors":[selector()]}),
            },
            state,
            feedback: EngineFeedback {
                sequence: sequence.saturating_sub(1),
                outcomes: Vec::new(),
                account: AccountState {
                    cash: Money::ZERO,
                    reserved_margin: Money::ZERO,
                    realized_pnl: Money::ZERO,
                    unrealized_pnl: Money::ZERO,
                    fees_paid: Money::ZERO,
                    equity: Money::ZERO,
                    positions: Vec::new(),
                },
                blockers: Vec::new(),
            },
            sequence,
            feedback_context_hash: "feedback".to_owned(),
            feedback_feature_hash: "features".to_owned(),
        }
    }

    #[test]
    fn full_and_compact_have_identical_admitted_actions_and_counts() {
        let sources = vec![source("u1"), source("u2")];
        let recipients = vec![recipient()];
        let selectors = vec![selector()];
        let source_groups = groups();
        let mut full_runner = Runner::default();
        let mut compact_runner = Runner::default();
        let (full_detail, full_actions) = full_runner
            .process_minute(
                sources.clone(),
                &recipients,
                &selectors,
                false,
                &source_groups,
            )
            .unwrap();
        let (compact_detail, compact_actions) = compact_runner
            .process_minute(sources, &recipients, &selectors, true, &source_groups)
            .unwrap();
        assert_eq!(full_actions, compact_actions);
        for field in ["attempts", "admitted", "rejected"] {
            assert_eq!(full_detail[field], compact_detail[field]);
        }
        let full_admitted: Vec<serde_json::Value> = full_detail["admissions"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|admission| admission["admitted"].as_bool() == Some(true))
            .cloned()
            .collect();
        let compact_admitted: Vec<serde_json::Value> =
            compact_detail["admissions"].as_array().unwrap().clone();
        assert_eq!(full_admitted, compact_admitted);
        assert_eq!(
            full_runner.summary_detail(),
            compact_runner.summary_detail()
        );
    }

    #[test]
    fn invalid_compact_group_map_does_not_mutate_books_or_counts() {
        let sources = vec![source("u1"), source("u2")];
        let recipients = vec![recipient()];
        let selectors = vec![selector()];
        let mut invalid_groups = groups();
        invalid_groups.remove("u2");
        let mut runner = Runner::default();
        assert!(
            runner
                .process_minute(
                    sources.clone(),
                    &recipients,
                    &selectors,
                    true,
                    &invalid_groups,
                )
                .is_err()
        );
        let mut fresh = Runner::default();
        let actual = runner
            .process_minute(sources.clone(), &recipients, &selectors, true, &groups())
            .unwrap();
        let expected = fresh
            .process_minute(sources, &recipients, &selectors, true, &groups())
            .unwrap();
        assert_eq!(actual, expected);
        assert_eq!(runner.summary_detail(), fresh.summary_detail());
    }

    #[test]
    fn restored_books_state_matches_uninterrupted_process() {
        let mut uninterrupted = Runner::default();
        let first = uninterrupted.process(request(0, 100, json!(null))).unwrap();
        let second_request = request(1, 200, first.state);
        let expected = uninterrupted.process(second_request.clone()).unwrap();
        let mut restored = Runner::default();
        let actual = restored.process(second_request).unwrap();
        assert_eq!(
            serde_json::to_value(actual).unwrap(),
            serde_json::to_value(expected).unwrap()
        );
    }
}
