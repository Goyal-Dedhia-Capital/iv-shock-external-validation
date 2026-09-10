//! Causal, feedback-aware policy for the funded best-family replay.
//!
//! This crate owns only sequential admission and lifecycle intent generation.
//! The feeder owns event timing and the generic engine owns prices, fills,
//! fees, margin, accounting, and portfolio state.

use std::collections::{BTreeMap, BTreeSet};

use backtest_contracts::{
    AccountState, CONTRACT_VERSION, EngineFeedback, ExecutionOutcome, IntentAction, IntentLeg,
    ResearchRequest, ResearchResponse, Side, TradeIntent,
};
use iv_shock_decision::{Detector, EventDiagnostic};
use iv_shock_sequential_books::{Candidate as SourceCandidate, matching_families};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::fs;
use std::path::Path;

pub const RUNNER_ID: &str = "iv-shock-h3-h5-strategy-policy-v1";
pub const SCHEMA_VERSION: &str = "gdc.iv-shock.h3-h5-strategy-policy.v1";
pub const MINUTE_NS: i64 = 60_000_000_000;
pub const REFERENCE_MODEL_SHA256: &str =
    "158d5bc92d2da250174649bfd1f1a4c7961d96bda14e9467f4adcb844ffb0b20";
pub const FIXED_BOOKS: [&str; 10] = [
    "H3_F1", "H3_F2", "H3_F3", "H3_F4", "H3_F5", "H5_F1", "H5_F2", "H5_F3", "H5_F4", "H5_F5",
];

pub const FINAL_FEATURES: [&str; 19] = [
    "spot_return_30m_pct",
    "observed_risk_reversal_moneyness_pp",
    "structure_iv_change_5m_pp",
    "structure_event_volume_min",
    "structure_trailing15_volume_min",
    "structure_price_age_minutes",
    "recipient_abs_delta",
    "calendar_dte",
    "target_abs_atm_distance_points",
    "iv_pp",
    "momentum_fast_slow_logpct",
    "moneyness",
    "basket_event_delta",
    "basket_event_gamma",
    "basket_event_vega",
    "basket_event_theta",
    "source_calendar_dte",
    "source_expiry_count",
    "source_unit_count",
];

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelFit {
    pub beta: Vec<f64>,
    pub book: String,
    pub clip_high: Vec<f64>,
    pub clip_low: Vec<f64>,
    pub features: Vec<String>,
    pub fit_month: String,
    pub mean: Vec<f64>,
    pub median: Vec<f64>,
    pub missing_indicators: Vec<String>,
    pub policy: String,
    pub scale: Vec<f64>,
    pub score_threshold_rupees: f64,
    pub train_end_date: String,
    pub train_n: usize,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelBundle {
    pub schema: String,
    pub start_date: String,
    pub end_date: String,
    pub warmup_end: String,
    pub minimum_training_rows: usize,
    pub ridge_penalty: f64,
    pub target_winsor_quantiles: Vec<f64>,
    pub score_gate: String,
    pub fits: Vec<ModelFit>,
}

fn final_policy(book: &str) -> Option<&'static str> {
    match book {
        "H5_F1" => Some("spot_down"),
        "H5_F2" | "H5_F5" => Some("spot_down_skew_pos_low_volume"),
        "H3_F4" | "H5_F3" => Some("spot_down_low_volume"),
        _ => None,
    }
}

impl ModelBundle {
    /// Load and bind an exact immutable monthly model bundle.
    ///
    /// # Errors
    ///
    /// Returns an error when the file, hash, schema, or frozen policy differs.
    pub fn load(path: &Path, expected_sha256: &str) -> Result<Self, String> {
        let bytes = fs::read(path).map_err(|error| format!("model bundle read failed: {error}"))?;
        let digest = format!("{:x}", Sha256::digest(&bytes));
        if digest != expected_sha256 || expected_sha256 != REFERENCE_MODEL_SHA256 {
            return Err("model bundle SHA-256 mismatch".to_owned());
        }
        let bundle: Self = serde_json::from_slice(&bytes)
            .map_err(|error| format!("model bundle JSON is invalid: {error}"))?;
        if bundle.schema != "gdc.exp032.monthly-ridge-bundle.v1"
            || bundle.minimum_training_rows != 60
            || bundle.ridge_penalty.to_bits() != 10.0_f64.to_bits()
            || bundle.target_winsor_quantiles != [0.025, 0.975]
            || bundle.start_date != "2024-07-01"
            || bundle.end_date != "2025-12-31"
            || bundle.warmup_end != "2024-06-30"
            || bundle.score_gate
                != "predicted_net_rupees > prior-training 67th-percentile fitted score"
            || bundle.fits.len() != 90
        {
            return Err("model bundle policy differs from frozen EXP032".to_owned());
        }
        let months = [
            "2024-07", "2024-08", "2024-09", "2024-10", "2024-11", "2024-12", "2025-01", "2025-02",
            "2025-03", "2025-04", "2025-05", "2025-06", "2025-07", "2025-08", "2025-09", "2025-10",
            "2025-11", "2025-12",
        ];
        let expected: BTreeSet<(String, String, String)> =
            ["H3_F4", "H5_F1", "H5_F2", "H5_F3", "H5_F5"]
                .into_iter()
                .flat_map(|book| {
                    months.into_iter().map(move |month| {
                        (
                            book.to_owned(),
                            final_policy(book).unwrap_or("").to_owned(),
                            month.to_owned(),
                        )
                    })
                })
                .collect();
        let actual: BTreeSet<_> = bundle
            .fits
            .iter()
            .map(|fit| (fit.book.clone(), fit.policy.clone(), fit.fit_month.clone()))
            .collect();
        let expected_features: Vec<String> =
            FINAL_FEATURES.iter().map(|v| (*v).to_owned()).collect();
        let expected_missing: Vec<String> = FINAL_FEATURES
            .iter()
            .map(|v| format!("{v}__missing"))
            .collect();
        if actual != expected
            || bundle.fits.iter().any(|fit| {
                fit.features != expected_features
                    || fit.missing_indicators != expected_missing
                    || fit.beta.len() != 39
                    || [
                        fit.median.len(),
                        fit.clip_low.len(),
                        fit.clip_high.len(),
                        fit.mean.len(),
                        fit.scale.len(),
                    ] != [19; 5]
                    || fit.train_n < 60
            })
        {
            return Err("model bundle fit inventory differs from frozen EXP032".to_owned());
        }
        Ok(bundle)
    }

    /// Score one entry-known final candidate against its exact prior-month fit.
    ///
    /// # Errors
    ///
    /// Returns an error for missing fits, dates outside the bundle, or invalid
    /// transforms and features.
    pub fn score(&self, candidate: &Candidate) -> Result<i64, String> {
        let month = candidate
            .date
            .get(..7)
            .ok_or_else(|| "candidate date has no YYYY-MM month".to_owned())?;
        if candidate.date < self.start_date || candidate.date > self.end_date {
            return Err("candidate date lies outside the frozen model bundle".to_owned());
        }
        let policy = final_policy(&candidate.book)
            .ok_or_else(|| "candidate book has no final policy".to_owned())?;
        let fit = self
            .fits
            .iter()
            .find(|fit| {
                fit.book == candidate.book && fit.policy == policy && fit.fit_month == month
            })
            .ok_or_else(|| "candidate has no exact family-month model fit".to_owned())?;
        let expected_features: Vec<String> = FINAL_FEATURES
            .iter()
            .map(|value| (*value).to_owned())
            .collect();
        if fit.features != expected_features
            || fit.beta.len() != 39
            || [
                fit.median.len(),
                fit.clip_low.len(),
                fit.clip_high.len(),
                fit.mean.len(),
                fit.scale.len(),
            ] != [19; 5]
            || fit.train_n < self.minimum_training_rows
            || fit.train_end_date.as_str() >= format!("{month}-01").as_str()
        {
            return Err("family-month fit violates the frozen causal schema".to_owned());
        }
        let mut prediction = fit.beta[0];
        for (index, name) in FINAL_FEATURES.iter().enumerate() {
            let raw = feature(candidate, name);
            let filled = raw.unwrap_or(fit.median[index]);
            let clipped = filled.clamp(fit.clip_low[index], fit.clip_high[index]);
            let scale = fit.scale[index];
            if !clipped.is_finite() || !scale.is_finite() || scale <= 0.0 {
                return Err("family-month fit contains an invalid transform".to_owned());
            }
            prediction += ((clipped - fit.mean[index]) / scale) * fit.beta[index + 1];
            prediction += f64::from(raw.is_none()) * fit.beta[index + 20];
        }
        let rank = ((prediction - fit.score_threshold_rupees) * 1_000_000.0).round();
        if !rank.is_finite() {
            return Err("model score is outside the supported range".to_owned());
        }
        format!("{rank:.0}")
            .parse()
            .map_err(|_| "model score is outside the supported range".to_owned())
    }
}

fn book_rank(book: &str) -> Option<usize> {
    FIXED_BOOKS.iter().position(|fixed| *fixed == book)
}

fn recipient_money_matches(bin: &str, value: f64) -> bool {
    value.is_finite()
        && match bin {
            "<-10%" => value < -0.10,
            "-10:-5%" => (-0.10..-0.05).contains(&value),
            "-5:-3%" => (-0.05..-0.03).contains(&value),
            "-3:-2%" => (-0.03..-0.02).contains(&value),
            "-2:-1%" => (-0.02..-0.01).contains(&value),
            "-1:1%" => (-0.01..=0.01).contains(&value),
            "1:2%" => value > 0.01 && value <= 0.02,
            "2:3%" => value > 0.02 && value <= 0.03,
            "3:5%" => value > 0.03 && value <= 0.05,
            "5:10%" => value > 0.05 && value <= 0.10,
            ">10%" => value > 0.10,
            _ => false,
        }
}

fn recipient_dte_matches(bin: &str, value: i32) -> bool {
    match bin {
        "0-7d" => (0..=7).contains(&value),
        "8-14d" => (8..=14).contains(&value),
        "15-30d" => (15..=30).contains(&value),
        "31-60d" => (31..=60).contains(&value),
        "61-90d" => (61..=90).contains(&value),
        "91-180d" => (91..=180).contains(&value),
        ">180d" => value > 180,
        _ => false,
    }
}

fn validate_family_and_clock(candidate: &Candidate, minute: i64) -> Result<(), String> {
    let source = SourceCandidate {
        detector: candidate.detector.clone(),
        session_date: candidate.date.clone(),
        contract_id: candidate.source_contract_id.clone(),
        event_minute: candidate.event_minute,
        expiry: String::new(),
        side: candidate.source_side.clone(),
        raw_sign: candidate.source_raw_sign,
        surface: candidate.source_surface.clone(),
        source_rank: candidate.source_expiry_rank,
        source_dte: candidate.source_dte,
        source_log_moneyness: candidate.source_log_moneyness,
        formation_lane: candidate.formation_lane.clone(),
        recipient_relative_bin: candidate.recipient_relative_bin.clone(),
    };
    let family = matching_families(&source)
        .into_iter()
        .find(|family| family.id == candidate.book)
        .ok_or_else(|| "candidate source fields do not match the frozen family".to_owned())?;
    if candidate.detector != "S0" {
        return Err("only frozen S0 may enter the executable policy".to_owned());
    }
    if !candidate.detector_score.is_finite()
        || candidate.detector_threshold.to_bits() != 2.0_f64.to_bits()
        || candidate.detector_support < 200
        || candidate.detector_score.abs() < candidate.detector_threshold
        || !candidate.detector_qualified_after_quiet
    {
        return Err("candidate lacks a qualifying frozen S0 diagnostic".to_owned());
    }
    if candidate.recipient_side != family.recipient_side {
        return Err("candidate recipient side differs from the frozen family".to_owned());
    }
    let expected_entry = candidate
        .event_minute
        .checked_add(1)
        .ok_or_else(|| "candidate entry minute overflow".to_owned())?;
    let expected_exit = expected_entry
        .checked_add(i64::from(family.hold_minutes))
        .ok_or_else(|| "candidate exit minute overflow".to_owned())?;
    if candidate.entry_minute != expected_entry || candidate.entry_minute != minute {
        return Err("candidate entry must be exactly source event t+1".to_owned());
    }
    if candidate.scheduled_exit_minute != expected_exit {
        return Err("scheduled exit differs from the frozen family horizon".to_owned());
    }
    if !candidate.recipient_represented
        || candidate.recipient_expiry_minute < candidate.scheduled_exit_minute
        || !recipient_money_matches(
            &candidate.recipient_money_bin,
            candidate.recipient_log_moneyness,
        )
        || !recipient_dte_matches(&candidate.recipient_dte_bin, candidate.recipient_dte)
    {
        return Err("recipient is outside the frozen event-time chain scope".to_owned());
    }
    Ok(())
}

fn validate_detector_evidence(
    candidates: &[Candidate],
    diagnostics: &[EventDiagnostic],
) -> Result<(), String> {
    for candidate in candidates {
        let expected_event_id = format!(
            "iv-shock-source|{}|{}|{}",
            candidate.date, candidate.event_minute, candidate.source_contract_id
        );
        if candidate.source_event_id != expected_event_id {
            return Err("candidate source event ID differs from its causal identity".to_owned());
        }
        let diagnostic = diagnostics
            .iter()
            .find(|diagnostic| {
                diagnostic.contract_id == candidate.source_contract_id
                    && diagnostic.ts_minute == candidate.event_minute
                    && diagnostic.raw_iv_sign == candidate.source_raw_sign
            })
            .ok_or_else(|| {
                format!(
                    "candidate {} has no matching detector diagnostic",
                    candidate.candidate_id
                )
            })?;
        let detector = diagnostic
            .detectors
            .iter()
            .find(|detector| detector.detector == Detector::S0)
            .ok_or_else(|| "matching diagnostic has no S0 result".to_owned())?;
        if candidate.detector != "S0"
            || detector.score != Some(candidate.detector_score)
            || detector.threshold != Some(candidate.detector_threshold)
            || detector.support != candidate.detector_support
            || detector.qualified_after_quiet != candidate.detector_qualified_after_quiet
        {
            return Err("candidate detector fields differ from typed S0 evidence".to_owned());
        }
    }
    Ok(())
}

fn default_books() -> Vec<String> {
    FIXED_BOOKS.iter().map(|book| (*book).to_owned()).collect()
}

fn default_scenarios() -> Vec<String> {
    vec!["baseline".to_owned()]
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PolicyMode {
    #[default]
    Baseline,
    FinalCandidate,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default)]
    pub mode: PolicyMode,
    #[serde(default = "default_books")]
    pub books: Vec<String>,
    #[serde(default = "default_scenarios")]
    pub scenarios: Vec<String>,
}

impl Config {
    #[must_use]
    pub fn baseline() -> Self {
        Self {
            mode: PolicyMode::Baseline,
            books: default_books(),
            scenarios: default_scenarios(),
        }
    }

    #[must_use]
    pub fn final_candidate() -> Self {
        Self {
            mode: PolicyMode::FinalCandidate,
            books: ["H3_F4", "H5_F1", "H5_F2", "H5_F3", "H5_F5"]
                .into_iter()
                .map(str::to_owned)
                .collect(),
            scenarios: vec!["final_candidate".to_owned()],
        }
    }

    /// # Errors
    ///
    /// Returns an error when JSON is malformed or contains unsupported,
    /// duplicate, or empty identifiers.
    pub fn from_json_str(value: &str) -> Result<Self, String> {
        let config: Self = serde_json::from_str(value)
            .map_err(|error| format!("configuration is invalid JSON: {error}"))?;
        config.validate()?;
        Ok(config)
    }

    fn validate(&self) -> Result<(), String> {
        if self.books.is_empty() || self.scenarios.is_empty() {
            return Err("configuration needs at least one book and scenario".to_owned());
        }
        let mut seen = BTreeSet::new();
        for book in &self.books {
            if book_rank(book).is_none() {
                return Err(format!("unsupported fixed book {book}"));
            }
            if !seen.insert(book) {
                return Err(format!("duplicate book {book}"));
            }
        }
        if self.mode == PolicyMode::FinalCandidate
            && self.books.iter().any(|book| {
                !matches!(
                    book.as_str(),
                    "H3_F4" | "H5_F1" | "H5_F2" | "H5_F3" | "H5_F5"
                )
            })
        {
            return Err("final-candidate mode permits only its five frozen families".to_owned());
        }
        let mut scenarios = BTreeSet::new();
        for scenario in &self.scenarios {
            if scenario.is_empty() || !scenarios.insert(scenario) {
                return Err("scenario identifiers must be nonempty and unique".to_owned());
            }
        }
        Ok(())
    }
}

impl Default for Config {
    fn default() -> Self {
        Self::baseline()
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Leg {
    pub contract_id: String,
    pub side: Side,
    pub quantity: u64,
    #[serde(default)]
    pub option_type: Option<String>,
    #[serde(default)]
    pub strike: Option<i64>,
    #[serde(default)]
    pub expiry: Option<String>,
    #[serde(default)]
    pub expiry_minute: Option<i64>,
    #[serde(default)]
    pub represented: Option<bool>,
}

fn final_structure_width(book: &str) -> Option<i64> {
    match book {
        "H5_F1" | "H5_F5" => Some(500),
        "H5_F2" | "H5_F3" => Some(300),
        _ => None,
    }
}

fn validate_strike_inventory(strikes: &[i64]) -> Result<(), String> {
    if strikes.iter().copied().collect::<BTreeSet<_>>().len() != strikes.len() {
        return Err("represented same-expiry strike inventory contains duplicates".to_owned());
    }
    Ok(())
}

fn validate_final_structure(candidate: &Candidate) -> Result<(), String> {
    if candidate.scenario != "final_candidate" || candidate.rank_score_micro.is_none() {
        return Err("final candidate requires its scenario and walk-forward score".to_owned());
    }
    validate_strike_inventory(&candidate.represented_same_expiry_strikes)?;
    let lot = candidate
        .official_lot_size
        .ok_or("missing official lot size")?;
    if lot == 0
        || candidate.lot_authority.as_deref().is_none_or(|authority| {
            authority.is_empty() || authority.eq_ignore_ascii_case("PENDING")
        })
        || candidate.quantity != lot
        || candidate.legs.iter().any(|leg| leg.quantity != lot)
    {
        return Err("final candidate must use one authoritative official lot".to_owned());
    }
    if candidate.book == "H3_F4" {
        if candidate.structure_id != "outward_1_ce" || candidate.legs.len() != 1 {
            return Err("H3_F4 requires one outward-1 CE leg".to_owned());
        }
        let leg = &candidate.legs[0];
        let anchor = candidate.anchor_strike.ok_or("missing H3 anchor strike")?;
        let atm = candidate.atm_strike.ok_or("missing H3 ATM strike")?;
        let target = leg.strike.ok_or("missing H3 target strike")?;
        if !candidate.represented_same_expiry_strikes.contains(&anchor) {
            return Err("H3 anchor is absent from the represented event-time chain".to_owned());
        }
        let mut outward: Vec<i64> = candidate
            .represented_same_expiry_strikes
            .iter()
            .copied()
            .filter(|strike| {
                if anchor < atm {
                    *strike < anchor
                } else {
                    *strike > anchor
                }
            })
            .collect();
        outward.sort_by_key(|strike| ((strike - anchor).abs(), *strike));
        if outward.first().copied() != Some(target)
            || leg.side != Side::Sell
            || leg.option_type.as_deref() != Some("CE")
            || leg.expiry.as_deref().is_none_or(str::is_empty)
            || leg.expiry.as_deref() != Some(candidate.recipient_expiry.as_str())
            || leg.expiry_minute != Some(candidate.recipient_expiry_minute)
            || leg.represented != Some(true)
            || candidate.contract_id != leg.contract_id
        {
            return Err("H3_F4 leg is not the deterministic outward-1 CE short".to_owned());
        }
    } else {
        let width = final_structure_width(&candidate.book)
            .ok_or_else(|| "family is outside the final portfolio".to_owned())?;
        if candidate.structure_id != format!("pe_vertical_down_{width}")
            || candidate.legs.len() != 2
        {
            return Err("H5 final family requires its frozen two-leg PE vertical".to_owned());
        }
        let buy = &candidate.legs[0];
        let sell = &candidate.legs[1];
        if buy.side != Side::Buy
            || sell.side != Side::Sell
            || buy.option_type.as_deref() != Some("PE")
            || sell.option_type.as_deref() != Some("PE")
            || buy.expiry.is_none()
            || buy.expiry != sell.expiry
            || buy.expiry.as_deref() != Some(candidate.recipient_expiry.as_str())
            || buy.expiry_minute != Some(candidate.recipient_expiry_minute)
            || sell.expiry_minute != Some(candidate.recipient_expiry_minute)
            || buy.represented != Some(true)
            || sell.represented != Some(true)
            || buy
                .strike
                .is_none_or(|strike| !candidate.represented_same_expiry_strikes.contains(&strike))
            || sell
                .strike
                .is_none_or(|strike| !candidate.represented_same_expiry_strikes.contains(&strike))
            || buy.quantity != sell.quantity
            || buy
                .strike
                .zip(sell.strike)
                .is_none_or(|(high, low)| high - low != width)
            || candidate.contract_id != buy.contract_id
            || candidate.quantity != buy.quantity
        {
            return Err(
                "H5 vertical leg identity, expiry, width, side, or quantity differs".to_owned(),
            );
        }
    }
    Ok(())
}

fn feature(candidate: &Candidate, name: &str) -> Option<f64> {
    candidate
        .features
        .get(name)
        .copied()
        .flatten()
        .filter(|value| value.is_finite())
}

fn final_rejection_reason(candidate: &Candidate) -> Option<&'static str> {
    if candidate.rank_score_micro.is_none_or(|score| score <= 0) {
        return Some("walk_forward_score_not_positive");
    }
    if feature(candidate, "spot_return_30m_pct").is_none_or(|value| value > 0.0) {
        return Some("spot_filter_failed_or_missing");
    }
    if matches!(
        candidate.book.as_str(),
        "H3_F4" | "H5_F2" | "H5_F3" | "H5_F5"
    ) && feature(candidate, "structure_event_volume_min").is_none_or(|value| value > 0.0)
    {
        return Some("low_volume_filter_failed_or_missing");
    }
    if matches!(candidate.book.as_str(), "H5_F2" | "H5_F5")
        && feature(candidate, "observed_risk_reversal_moneyness_pp")
            .is_none_or(|value| value <= 0.0)
    {
        return Some("risk_reversal_filter_failed_or_missing");
    }
    None
}

/// Candidate fields are entry-known only.  In particular, no exit quote or
/// exit-availability field is accepted here.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Candidate {
    pub candidate_id: String,
    pub detector: String,
    pub detector_score: f64,
    pub detector_threshold: f64,
    pub detector_support: usize,
    pub detector_qualified_after_quiet: bool,
    pub book: String,
    pub scenario: String,
    pub structure_id: String,
    pub source_event_id: String,
    pub source_contract_id: String,
    pub source_side: String,
    pub source_raw_sign: i8,
    pub source_surface: String,
    pub source_dte: i32,
    pub source_expiry_rank: u32,
    pub source_log_moneyness: f64,
    #[serde(default)]
    pub formation_lane: String,
    #[serde(default)]
    pub recipient_relative_bin: String,
    pub recipient_side: String,
    pub recipient_money_bin: String,
    pub recipient_log_moneyness: f64,
    pub recipient_dte_bin: String,
    pub recipient_dte: i32,
    #[serde(default)]
    pub recipient_expiry: String,
    pub recipient_expiry_minute: i64,
    pub recipient_represented: bool,
    pub session_end_minute: i64,
    pub date: String,
    pub event_minute: i64,
    pub entry_minute: i64,
    pub scheduled_exit_minute: i64,
    pub contract_id: String,
    pub quantity: u64,
    #[serde(default)]
    pub official_lot_size: Option<u64>,
    #[serde(default)]
    pub lot_authority: Option<String>,
    pub entry_eligible: bool,
    #[serde(default)]
    pub rank_score_micro: Option<i64>,
    #[serde(default)]
    pub anchor_strike: Option<i64>,
    #[serde(default)]
    pub atm_strike: Option<i64>,
    #[serde(default)]
    pub represented_same_expiry_strikes: Vec<i64>,
    #[serde(default)]
    pub features: BTreeMap<String, Option<f64>>,
    pub legs: Vec<Leg>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Packet {
    Minute {
        #[serde(alias = "event_minute")]
        minute: i64,
        session_date: String,
        #[serde(default)]
        bar_minute: Option<i64>,
        #[serde(default)]
        candidates: Vec<Candidate>,
        #[serde(default)]
        detector_diagnostics: Vec<EventDiagnostic>,
    },
    SessionEnd {
        session_date: String,
        session_end_minute: i64,
    },
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct Position {
    pub scenario: String,
    pub book: String,
    pub structure_id: String,
    pub candidate_id: String,
    pub source_event_id: String,
    pub date: String,
    pub contract_id: String,
    pub strategy_position_id: String,
    pub entry_minute: i64,
    pub scheduled_exit_minute: i64,
    pub session_end_minute: i64,
    pub quantity: u64,
    pub official_lot_size: Option<u64>,
    pub lot_authority: Option<String>,
    pub legs: Vec<Leg>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum PendingAction {
    Open {
        intent_id: String,
        position: Position,
    },
    Close {
        intent_id: String,
        position: Position,
    },
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
pub struct Counts {
    pub attempts: u64,
    pub admitted: u64,
    pub rejected: u64,
    pub close_intents: u64,
    pub close_retries: u64,
    pub filled_opens: u64,
    pub filled_closes: u64,
    pub margin_rejected_opens: u64,
    pub partial_halts: u64,
    pub unresolved_session_end: u64,
    pub reason_counts: BTreeMap<String, u64>,
}

impl Counts {
    fn inc_reason(&mut self, reason: &str) {
        let count = self.reason_counts.entry(reason.to_owned()).or_default();
        *count = count.saturating_add(1);
    }
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
pub struct BookState {
    pub active: Option<Position>,
    pub pending: Option<PendingAction>,
    pub last_minute: Option<i64>,
    pub halted: bool,
    pub unresolved: bool,
    pub counts: Counts,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct PersistedState {
    pub schema_version: String,
    pub bundle_hash: String,
    pub config: Config,
    pub next_sequence: u64,
    pub books: BTreeMap<String, BookState>,
    pub totals: Counts,
    pub halted: bool,
    /// Sequence of the most recently applied engine feedback. The initial
    /// sequence-0 feedback is intentionally empty and is not recorded here;
    /// sequence-0 execution feedback arrives on request 1.
    pub last_feedback_sequence: Option<u64>,
    pub current_session_date: Option<String>,
}

#[derive(Clone, Debug)]
pub struct Runner {
    config: Config,
    sequence: u64,
    initialized: bool,
    books: BTreeMap<String, BookState>,
    totals: Counts,
    halted: bool,
    last_feedback_sequence: Option<u64>,
    bundle_hash: Option<String>,
    current_session_date: Option<String>,
}

impl Default for Runner {
    fn default() -> Self {
        Self::new(Config::baseline())
    }
}

impl Runner {
    #[must_use]
    /// # Panics
    ///
    /// Panics when the supplied configuration fails validation. Process
    /// adapters should use [`Self::try_new`] for untrusted configuration.
    pub fn new(config: Config) -> Self {
        Self::try_new(config).expect("Runner::new requires valid configuration")
    }

    /// # Errors
    ///
    /// Returns an error when the configuration contains unsupported or
    /// duplicate books or scenarios.
    pub fn try_new(config: Config) -> Result<Self, String> {
        config.validate()?;
        let books = config
            .books
            .iter()
            .map(|book| (book.clone(), BookState::default()))
            .collect();
        Ok(Self {
            config,
            sequence: 0,
            initialized: false,
            books,
            totals: Counts::default(),
            halted: false,
            last_feedback_sequence: None,
            bundle_hash: None,
            current_session_date: None,
        })
    }

    #[must_use]
    pub const fn sequence(&self) -> u64 {
        self.sequence
    }

    #[must_use]
    pub fn state_value(&self) -> Value {
        json!({"runner_state": self.persisted_state()})
    }

    fn persisted_state(&self) -> PersistedState {
        PersistedState {
            schema_version: SCHEMA_VERSION.to_owned(),
            bundle_hash: self.bundle_hash.clone().unwrap_or_default(),
            config: self.config.clone(),
            next_sequence: self.sequence,
            books: self.books.clone(),
            totals: self.totals.clone(),
            halted: self.halted,
            last_feedback_sequence: self.last_feedback_sequence,
            current_session_date: self.current_session_date.clone(),
        }
    }

    fn validate_candidate(&self, candidate: &Candidate, minute: i64) -> Result<(), String> {
        if !self.books.contains_key(&candidate.book) || book_rank(&candidate.book).is_none() {
            return Err(format!("unsupported configured book {}", candidate.book));
        }
        if !self
            .config
            .scenarios
            .iter()
            .any(|s| s == &candidate.scenario)
        {
            return Err(format!("unsupported scenario {}", candidate.scenario));
        }
        if candidate.candidate_id.is_empty()
            || candidate.source_event_id.is_empty()
            || candidate.date.is_empty()
            || candidate.contract_id.is_empty()
            || candidate.structure_id.is_empty()
        {
            return Err("candidate identity fields must be nonempty".to_owned());
        }
        validate_family_and_clock(candidate, minute)?;
        if candidate.legs.is_empty() {
            return Err("candidate must contain at least one leg".to_owned());
        }
        if candidate.entry_eligible && candidate.quantity == 0 {
            return Err("eligible candidate quantity must be positive".to_owned());
        }
        match self.config.mode {
            PolicyMode::Baseline => {
                if candidate.structure_id != "single_leg" || candidate.legs.len() != 1 {
                    return Err("baseline mode requires exactly one leg".to_owned());
                }
            }
            PolicyMode::FinalCandidate => validate_final_structure(candidate)?,
        }
        let mut contracts = BTreeSet::new();
        for leg in &candidate.legs {
            if leg.contract_id.is_empty()
                || leg.quantity == 0
                || !contracts.insert(&leg.contract_id)
            {
                return Err("legs need unique contracts and positive quantities".to_owned());
            }
            if self.config.mode == PolicyMode::Baseline {
                if leg.contract_id != candidate.contract_id || leg.quantity != candidate.quantity {
                    return Err("single-leg identity or quantity differs from candidate".to_owned());
                }
                let expected_side = if candidate.book.starts_with("H3_") {
                    Side::Sell
                } else {
                    Side::Buy
                };
                if leg.side != expected_side {
                    return Err("leg direction differs from H3 short/H5 long policy".to_owned());
                }
            }
        }
        Ok(())
    }

    fn position(candidate: &Candidate) -> Position {
        let strategy_position_id = format!(
            "iv-shock|{}|{}|{}",
            candidate.scenario, candidate.book, candidate.candidate_id
        );
        Position {
            scenario: candidate.scenario.clone(),
            book: candidate.book.clone(),
            structure_id: candidate.structure_id.clone(),
            candidate_id: candidate.candidate_id.clone(),
            source_event_id: candidate.source_event_id.clone(),
            date: candidate.date.clone(),
            contract_id: candidate.contract_id.clone(),
            strategy_position_id,
            entry_minute: candidate.entry_minute,
            scheduled_exit_minute: candidate.scheduled_exit_minute,
            session_end_minute: candidate.session_end_minute,
            quantity: candidate.quantity,
            official_lot_size: candidate.official_lot_size,
            lot_authority: candidate.lot_authority.clone(),
            legs: candidate.legs.clone(),
        }
    }

    fn open_intent(position: &Position) -> TradeIntent {
        let intent_id = format!("{}|open", position.strategy_position_id);
        TradeIntent {
            schema_version: CONTRACT_VERSION.to_owned(),
            intent_id,
            decision_id: position.source_event_id.clone(),
            strategy_position_id: position.strategy_position_id.clone(),
            basket_key: position.strategy_position_id.clone(),
            action: IntentAction::Open,
            atomic: true,
            legs: position
                .legs
                .iter()
                .map(|leg| IntentLeg {
                    instrument_id: leg.contract_id.clone(),
                    side: leg.side,
                    quantity: leg.quantity,
                    limit_price: None,
                })
                .collect(),
            lineage: BTreeMap::from([
                ("strategy".to_owned(), "iv_shock_h3_h5".to_owned()),
                ("policy".to_owned(), "one_active_per_family_book".to_owned()),
                ("scenario".to_owned(), position.scenario.clone()),
                ("book".to_owned(), position.book.clone()),
                ("structure_id".to_owned(), position.structure_id.clone()),
                ("candidate_id".to_owned(), position.candidate_id.clone()),
                (
                    "source_event_id".to_owned(),
                    position.source_event_id.clone(),
                ),
                (
                    "scheduled_exit_minute".to_owned(),
                    position.scheduled_exit_minute.to_string(),
                ),
                (
                    "official_lot_size".to_owned(),
                    position
                        .official_lot_size
                        .map_or_else(String::new, |v| v.to_string()),
                ),
                (
                    "lot_authority".to_owned(),
                    position.lot_authority.clone().unwrap_or_default(),
                ),
            ]),
        }
    }

    fn close_intent(position: &Position, minute: i64) -> TradeIntent {
        let intent_id = format!("{}|close|{}", position.strategy_position_id, minute);
        TradeIntent {
            schema_version: CONTRACT_VERSION.to_owned(),
            intent_id,
            decision_id: format!("{}|close|{}", position.source_event_id, minute),
            strategy_position_id: position.strategy_position_id.clone(),
            basket_key: position.strategy_position_id.clone(),
            action: IntentAction::Close,
            atomic: true,
            legs: position
                .legs
                .iter()
                .map(|leg| IntentLeg {
                    instrument_id: leg.contract_id.clone(),
                    side: match leg.side {
                        Side::Buy => Side::Sell,
                        Side::Sell => Side::Buy,
                    },
                    quantity: leg.quantity,
                    limit_price: None,
                })
                .collect(),
            lineage: BTreeMap::from([
                ("strategy".to_owned(), "iv_shock_h3_h5".to_owned()),
                (
                    "policy".to_owned(),
                    "reverse_legs_after_filled_open".to_owned(),
                ),
                ("book".to_owned(), position.book.clone()),
                ("scenario".to_owned(), position.scenario.clone()),
                ("candidate_id".to_owned(), position.candidate_id.clone()),
                (
                    "scheduled_exit_minute".to_owned(),
                    position.scheduled_exit_minute.to_string(),
                ),
                ("close_attempt_minute".to_owned(), minute.to_string()),
            ]),
        }
    }

    fn count_rejection(&mut self, book: &str, reason: &str) -> Result<(), String> {
        let book_state = self
            .books
            .get_mut(book)
            .ok_or_else(|| format!("missing book {book}"))?;
        book_state.counts.rejected = book_state.counts.rejected.saturating_add(1);
        book_state.counts.inc_reason(reason);
        self.totals.rejected = self.totals.rejected.saturating_add(1);
        self.totals.inc_reason(reason);
        Ok(())
    }

    fn apply_outcome(&mut self, outcome: &ExecutionOutcome) -> Result<(), String> {
        let target_book = self
            .books
            .iter()
            .find_map(|(book, state)| match &state.pending {
                Some(PendingAction::Open { intent_id, .. }) if intent_id == &outcome.intent_id => {
                    Some(book.clone())
                }
                Some(PendingAction::Close { intent_id, .. }) if intent_id == &outcome.intent_id => {
                    Some(book.clone())
                }
                _ => None,
            })
            .ok_or_else(|| {
                format!(
                    "feedback outcome {} has no pending intent",
                    outcome.intent_id
                )
            })?;
        let state = self
            .books
            .get_mut(&target_book)
            .ok_or_else(|| format!("missing book {target_book}"))?;
        let expected_position_id = match state.pending.as_ref() {
            Some(PendingAction::Open { position, .. } | PendingAction::Close { position, .. }) => {
                &position.strategy_position_id
            }
            None => return Err(format!("pending intent {} disappeared", outcome.intent_id)),
        };
        if &outcome.strategy_position_id != expected_position_id {
            return Err("feedback strategy position differs from pending intent".to_owned());
        }
        let pending = state
            .pending
            .take()
            .ok_or_else(|| format!("pending intent {} disappeared", outcome.intent_id))?;
        match (&pending, outcome.status) {
            (PendingAction::Open { position, .. }, backtest_contracts::OutcomeStatus::Filled) => {
                state.active = Some(position.clone());
                state.counts.filled_opens = state.counts.filled_opens.saturating_add(1);
            }
            (PendingAction::Open { .. }, backtest_contracts::OutcomeStatus::Rejected) => {
                state.counts.margin_rejected_opens =
                    state.counts.margin_rejected_opens.saturating_add(1);
                state.counts.inc_reason("engine_rejected_open");
            }
            (PendingAction::Open { .. }, backtest_contracts::OutcomeStatus::Deferred) => {
                state.counts.inc_reason("engine_deferred_open");
            }
            (PendingAction::Close { .. }, backtest_contracts::OutcomeStatus::Filled) => {
                state.active = None;
                state.counts.filled_closes = state.counts.filled_closes.saturating_add(1);
            }
            (PendingAction::Close { .. }, backtest_contracts::OutcomeStatus::Rejected)
            | (PendingAction::Close { .. }, backtest_contracts::OutcomeStatus::Deferred) => {
                state.active = Some(match pending {
                    PendingAction::Close { position, .. } => position,
                    PendingAction::Open { .. } => unreachable!("matched close status"),
                });
                state.counts.close_retries = state.counts.close_retries.saturating_add(1);
                state.counts.inc_reason("engine_rejected_close_retry");
            }
            (_, backtest_contracts::OutcomeStatus::PartiallyFilled) => {
                state.halted = true;
                self.halted = true;
                state.counts.partial_halts = state.counts.partial_halts.saturating_add(1);
                return Ok(());
            }
        }
        Ok(())
    }

    fn apply_feedback(&mut self, feedback: &EngineFeedback) -> Result<(), String> {
        for outcome in &feedback.outcomes {
            self.apply_outcome(outcome)?;
        }
        Ok(())
    }

    #[allow(clippy::too_many_lines)]
    fn process_minute_inner(
        &mut self,
        session_date: &str,
        minute: i64,
        mut candidates: Vec<Candidate>,
    ) -> Result<(Value, Vec<TradeIntent>), String> {
        // One immutable event stream can drive isolated books and the shared
        // portfolio. Candidates for fixed books outside this runner's config
        // are intentionally outside its universe, not malformed inputs.
        candidates.retain(|candidate| self.books.contains_key(&candidate.book));
        for candidate in &candidates {
            self.validate_candidate(candidate, minute)?;
            if candidate.date != session_date {
                return Err("candidate date differs from minute packet date".to_owned());
            }
        }
        let mut candidate_ids = BTreeSet::new();
        for candidate in &candidates {
            if !candidate_ids.insert(&candidate.candidate_id) {
                return Err("duplicate candidate_id in minute packet".to_owned());
            }
        }
        if let Some(previous) = self.current_session_date.as_deref() {
            if session_date < previous {
                return Err("session dates must be chronological".to_owned());
            }
            if session_date != previous {
                for state in self.books.values_mut() {
                    if state.active.is_some() && !state.unresolved {
                        state.halted = true;
                        state.unresolved = true;
                        state.counts.unresolved_session_end =
                            state.counts.unresolved_session_end.saturating_add(1);
                        state.counts.inc_reason("unresolved_at_session_end");
                        self.totals.unresolved_session_end =
                            self.totals.unresolved_session_end.saturating_add(1);
                        self.totals.inc_reason("unresolved_at_session_end");
                    }
                }
            }
        }
        self.current_session_date = Some(session_date.to_owned());
        if self
            .books
            .values()
            .any(|state| state.last_minute.is_some_and(|last| minute <= last))
        {
            return Err("minute packets must be strictly chronological".to_owned());
        }
        let mut actions = Vec::new();
        // Close scheduling runs before candidate admission.  An unresolved or
        // rejected close leaves active intact; the next minute retries it.
        for book in &self.config.books {
            let state = self
                .books
                .get_mut(book)
                .ok_or_else(|| format!("missing book {book}"))?;
            if state.halted || state.pending.is_some() {
                continue;
            }
            let Some(position) = state.active.clone() else {
                continue;
            };
            if minute >= position.scheduled_exit_minute {
                let intent = Self::close_intent(&position, minute);
                let intent_id = intent.intent_id.clone();
                state.pending = Some(PendingAction::Close {
                    intent_id,
                    position,
                });
                state.counts.close_intents = state.counts.close_intents.saturating_add(1);
                actions.push(intent);
            }
        }
        candidates.sort_by(|left, right| {
            (
                book_rank(&left.book),
                &left.contract_id,
                &left.candidate_id,
                &left.scenario,
            )
                .cmp(&(
                    book_rank(&right.book),
                    &right.contract_id,
                    &right.candidate_id,
                    &right.scenario,
                ))
        });
        for candidate in candidates {
            let final_reason = if self.config.mode == PolicyMode::FinalCandidate {
                final_rejection_reason(&candidate)
            } else {
                None
            };
            let occupied = {
                let state = self
                    .books
                    .get(&candidate.book)
                    .ok_or_else(|| "missing book".to_owned())?;
                state.halted || state.active.is_some() || state.pending.is_some()
            };
            let reason = if candidate.scheduled_exit_minute > candidate.session_end_minute {
                Some("planned_session_end_ineligible")
            } else if !candidate.entry_eligible {
                Some("entry_ineligible")
            } else if let Some(reason) = final_reason {
                Some(reason)
            } else if occupied {
                Some("book_occupied")
            } else if self.halted {
                Some("portfolio_halted")
            } else {
                None
            };
            let state = self
                .books
                .get_mut(&candidate.book)
                .ok_or_else(|| "missing book".to_owned())?;
            state.counts.attempts = state.counts.attempts.saturating_add(1);
            self.totals.attempts = self.totals.attempts.saturating_add(1);
            if let Some(reason) = reason {
                self.count_rejection(&candidate.book, reason)?;
                continue;
            }
            let position = Self::position(&candidate);
            let intent = Self::open_intent(&position);
            let intent_id = intent.intent_id.clone();
            state.pending = Some(PendingAction::Open {
                intent_id,
                position,
            });
            state.counts.admitted = state.counts.admitted.saturating_add(1);
            self.totals.admitted = self.totals.admitted.saturating_add(1);
            actions.push(intent);
        }
        for state in self.books.values_mut() {
            state.last_minute = Some(minute);
        }
        Ok((
            json!({
                "minute": minute,
                "actions": actions.len(),
                "attempts": self.totals.attempts,
                "admitted": self.totals.admitted,
                "rejected": self.totals.rejected,
                "halted": self.halted,
            }),
            actions,
        ))
    }

    fn process_session_end(
        &mut self,
        session_date: &str,
        session_end_minute: i64,
    ) -> Result<(Value, Vec<TradeIntent>), String> {
        if self.current_session_date.as_deref() != Some(session_date) {
            return Err("session-end packet differs from active session".to_owned());
        }
        if self.books.values().any(|state| {
            state
                .last_minute
                .is_some_and(|last| session_end_minute <= last)
        }) {
            return Err("session-end boundary must follow the final processed minute".to_owned());
        }
        for state in self.books.values() {
            let position = state.active.as_ref().or(match state.pending.as_ref() {
                Some(
                    PendingAction::Open { position, .. } | PendingAction::Close { position, .. },
                ) => Some(position),
                None => None,
            });
            if let Some(position) = position
                && position.session_end_minute.checked_add(1) != Some(session_end_minute)
            {
                return Err("session-end boundary differs from active position calendar".to_owned());
            }
        }
        let mut unresolved = 0_u64;
        for state in self.books.values_mut() {
            if (state.active.is_some() || state.pending.is_some()) && !state.unresolved {
                state.halted = true;
                state.unresolved = true;
                state.counts.unresolved_session_end =
                    state.counts.unresolved_session_end.saturating_add(1);
                state.counts.inc_reason("unresolved_at_session_end");
                self.totals.unresolved_session_end =
                    self.totals.unresolved_session_end.saturating_add(1);
                self.totals.inc_reason("unresolved_at_session_end");
                unresolved = unresolved.saturating_add(1);
            }
            state.last_minute = Some(session_end_minute);
        }
        Ok((
            json!({
                "session_date": session_date,
                "session_end_minute": session_end_minute,
                "unresolved_books": unresolved,
            }),
            Vec::new(),
        ))
    }

    /// # Errors
    ///
    /// Returns an error when a candidate violates the causal packet contract,
    /// feedback names no pending intent, or the minute stream is not ordered.
    pub fn process_minute(
        &mut self,
        minute: i64,
        candidates: Vec<Candidate>,
        feedback: &EngineFeedback,
    ) -> Result<(Value, Vec<TradeIntent>), String> {
        let session_date = candidates
            .first()
            .map(|candidate| candidate.date.clone())
            .or_else(|| self.current_session_date.clone())
            .unwrap_or_else(|| "1970-01-01".to_owned());
        let snapshot = self.clone();
        let result = self
            .apply_feedback(feedback)
            .and_then(|()| self.process_minute_inner(&session_date, minute, candidates));
        if result.is_err() {
            *self = snapshot;
        }
        result
    }

    /// Apply the final engine feedback after the last market event.
    ///
    /// The generic engine normally delivers this feedback on a trailing
    /// sequence request. This helper is equivalent for callers that stop at
    /// the final event and need to persist the final lifecycle state without
    /// inventing another market minute. It emits no intent.
    ///
    /// # Errors
    ///
    /// Returns an error when feedback is not for the immediately preceding
    /// event, or has already been applied.
    pub fn settle_feedback(&mut self, feedback: &EngineFeedback) -> Result<(), String> {
        let expected = self
            .sequence
            .checked_sub(1)
            .ok_or_else(|| "cannot settle before the first event".to_owned())?;
        if feedback.sequence != expected || self.last_feedback_sequence == Some(expected) {
            return Err("final feedback is stale, out of order, or already applied".to_owned());
        }
        let snapshot = self.clone();
        if let Err(error) = self.apply_feedback(feedback) {
            *self = snapshot;
            return Err(error);
        }
        self.last_feedback_sequence = Some(expected);
        Ok(())
    }

    fn parse_state(value: &Value) -> Result<Option<PersistedState>, String> {
        if value.is_null() || value == &json!({}) {
            return Ok(None);
        }
        let core = value.get("runner_state").unwrap_or(value);
        serde_json::from_value(core.clone())
            .map(Some)
            .map_err(|error| format!("runner state is invalid: {error}"))
    }

    fn restore_state(
        &mut self,
        state: &Value,
        sequence: u64,
        bundle_hash: &str,
    ) -> Result<(), String> {
        let persisted = Self::parse_state(state)?;
        if self.initialized {
            let Some(persisted) = persisted else {
                return Err("state required after first request".to_owned());
            };
            self.validate_persisted(&persisted, bundle_hash)?;
            if persisted.next_sequence != self.sequence || sequence != self.sequence {
                return Err("state/request sequence mismatch".to_owned());
            }
            if persisted.books != self.books
                || persisted.totals != self.totals
                || persisted.halted != self.halted
                || persisted.last_feedback_sequence != self.last_feedback_sequence
                || persisted.current_session_date != self.current_session_date
            {
                return Err("persisted state differs from live runner state".to_owned());
            }
        } else if let Some(persisted) = persisted {
            self.validate_persisted(&persisted, bundle_hash)?;
            if persisted.next_sequence != sequence {
                return Err("restored sequence mismatch".to_owned());
            }
            self.sequence = persisted.next_sequence;
            self.books = persisted.books;
            self.totals = persisted.totals;
            self.halted = persisted.halted;
            self.last_feedback_sequence = persisted.last_feedback_sequence;
            self.current_session_date = persisted.current_session_date;
            self.bundle_hash = Some(persisted.bundle_hash);
            self.initialized = true;
        } else {
            if sequence != 0 {
                return Err("fresh runner requires sequence zero".to_owned());
            }
            self.initialized = true;
            self.bundle_hash = Some(bundle_hash.to_owned());
        }
        Ok(())
    }

    fn validate_persisted(
        &self,
        persisted: &PersistedState,
        bundle_hash: &str,
    ) -> Result<(), String> {
        if persisted.schema_version != SCHEMA_VERSION
            || persisted.config != self.config
            || persisted.bundle_hash != bundle_hash
            || (self.initialized && self.bundle_hash.as_deref() != Some(bundle_hash))
        {
            return Err("persisted schema/config mismatch".to_owned());
        }
        if persisted.books.keys().collect::<BTreeSet<_>>()
            != self.books.keys().collect::<BTreeSet<_>>()
        {
            return Err("persisted book set mismatch".to_owned());
        }
        if persisted.halted != self.halted && self.initialized {
            return Err("persisted halt state mismatch".to_owned());
        }
        if persisted
            .last_feedback_sequence
            .is_some_and(|sequence| sequence >= persisted.next_sequence)
        {
            return Err("persisted feedback sequence is ahead of request state".to_owned());
        }
        for (book, state) in &persisted.books {
            if let Some(position) = &state.active
                && (position.book != *book
                    || position.strategy_position_id.is_empty()
                    || position.entry_minute >= position.scheduled_exit_minute
                    || position.scheduled_exit_minute > position.session_end_minute
                    || position.legs.is_empty())
            {
                return Err(format!("invalid active position in {book}"));
            }
            if let Some(
                PendingAction::Open { position, .. } | PendingAction::Close { position, .. },
            ) = &state.pending
                && (position.book != *book || position.legs.is_empty())
            {
                return Err(format!("invalid pending position in {book}"));
            }
            if state.halted && !state.unresolved && !persisted.halted {
                return Err(format!("halted book {book} missing global halt"));
            }
        }
        Ok(())
    }

    /// # Errors
    ///
    /// Returns an error when the canonical request, causal clocks, feedback,
    /// or persisted state is invalid.
    #[allow(clippy::needless_pass_by_value)]
    pub fn process_request(
        &mut self,
        request: ResearchRequest,
        bundle_hash: &str,
    ) -> Result<ResearchResponse, String> {
        if request.schema_version != CONTRACT_VERSION
            || request.input.schema_version != CONTRACT_VERSION
            || request.sequence != request.input.sequence
            || request.input.available_at_ns > request.input.sealed_at_ns
            || request.input.sealed_at_ns > request.input.decision_at_ns
            || request.feedback_context_hash.is_empty()
            || request.feedback_feature_hash.is_empty()
            || bundle_hash.is_empty()
        {
            return Err("canonical request/schema/clock mismatch".to_owned());
        }
        if request.sequence == 0 {
            if request.feedback.sequence != 0 || !request.feedback.outcomes.is_empty() {
                return Err("sequence-zero feedback must be the empty initial feedback".to_owned());
            }
        } else if request.feedback.sequence != request.sequence - 1
            || self.last_feedback_sequence == Some(request.sequence - 1)
        {
            return Err("feedback must be the previous event and applied once".to_owned());
        }
        let packet: Packet = serde_json::from_value(request.input.research_payload.clone())
            .map_err(|error| format!("minute packet is invalid: {error}"))?;
        let minute = match &packet {
            Packet::Minute { minute, .. } => *minute,
            Packet::SessionEnd {
                session_end_minute, ..
            } => *session_end_minute,
        };
        if minute != request.input.decision_at_ns.div_euclid(MINUTE_NS) {
            return Err("packet minute differs from decision clock".to_owned());
        }
        let snapshot = self.clone();
        let result = self
            .restore_state(&request.state, request.sequence, bundle_hash)
            .and_then(|()| {
                if request.sequence > 0 {
                    self.apply_feedback(&request.feedback)?;
                    self.last_feedback_sequence = Some(request.sequence - 1);
                }
                let (detail, actions) = match packet {
                    Packet::Minute {
                        minute,
                        session_date,
                        candidates,
                        detector_diagnostics,
                        ..
                    } => {
                        validate_detector_evidence(&candidates, &detector_diagnostics)?;
                        self.process_minute_inner(&session_date, minute, candidates)?
                    }
                    Packet::SessionEnd {
                        session_date,
                        session_end_minute,
                    } => self.process_session_end(&session_date, session_end_minute)?,
                };
                self.sequence = self
                    .sequence
                    .checked_add(1)
                    .ok_or_else(|| "sequence overflow".to_owned())?;
                Ok(ResearchResponse {
                    schema_version: CONTRACT_VERSION.to_owned(),
                    artifact_consumed: true,
                    runner_id: RUNNER_ID.to_owned(),
                    bundle_hash: bundle_hash.to_owned(),
                    state: json!({"runner_state": self.persisted_state(), "detail": detail}),
                    actions,
                })
            });
        if result.is_err() {
            *self = snapshot;
        }
        result
    }
}

/// Keep the protocol dependency visible to downstream adapters without moving
/// account, pricing, or margin ownership into this crate.
#[must_use]
pub const fn empty_feedback(sequence: u64) -> EngineFeedback {
    EngineFeedback {
        sequence,
        outcomes: Vec::new(),
        account: AccountState {
            cash: backtest_contracts::Money::ZERO,
            reserved_margin: backtest_contracts::Money::ZERO,
            realized_pnl: backtest_contracts::Money::ZERO,
            unrealized_pnl: backtest_contracts::Money::ZERO,
            fees_paid: backtest_contracts::Money::ZERO,
            equity: backtest_contracts::Money::ZERO,
            positions: Vec::new(),
        },
        blockers: Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use backtest_contracts::{ExecutionOutcome, Fill, OutcomeStatus, SealedEvent};

    fn candidate(id: &str, book: &str, minute: i64, mut legs: Vec<Leg>) -> Candidate {
        let (source_side, source_raw_sign, source_surface, source_dte, formation, relative) =
            match book {
                "H3_F1" => ("CE", 1, "coherent", 30, "observed", "observed|positive"),
                "H3_F2" => ("CE", 1, "coherent", 31, "observed", "observed|positive"),
                "H3_F3" => ("CE", 1, "coherent", 31, "pchip", "pchip|positive"),
                "H3_F4" => ("CE", 1, "coherent", 30, "pchip", "pchip|positive"),
                "H3_F5" => ("CE", -1, "coherent", 30, "pchip", "pchip|negative"),
                "H5_F1" => ("CE", 1, "coherent", 31, "unsupported", "unsupported"),
                "H5_F2" => ("CE", 1, "coherent", 30, "unsupported", "unsupported"),
                "H5_F3" => ("PE", -1, "coherent", 30, "unsupported", "unsupported"),
                "H5_F4" => ("PE", -1, "coherent", 31, "unsupported", "unsupported"),
                "H5_F5" => ("PE", 1, "idiosyncratic", 31, "unsupported", "unsupported"),
                _ => ("UNKNOWN", 0, "unknown", -1, "unsupported", "unsupported"),
            };
        let expected_side = if book.starts_with("H3_") {
            Side::Sell
        } else {
            Side::Buy
        };
        for leg in &mut legs {
            leg.side = expected_side;
        }
        Candidate {
            candidate_id: id.to_owned(),
            detector: "S0".to_owned(),
            detector_score: 2.5,
            detector_threshold: 2.0,
            detector_support: 200,
            detector_qualified_after_quiet: true,
            book: book.to_owned(),
            scenario: "baseline".to_owned(),
            structure_id: "single_leg".to_owned(),
            source_event_id: format!(
                "iv-shock-source|2026-01-02|{}|source-contract-{id}",
                minute - 1
            ),
            source_contract_id: format!("source-contract-{id}"),
            source_side: source_side.to_owned(),
            source_raw_sign,
            source_surface: source_surface.to_owned(),
            source_dte,
            source_expiry_rank: 3,
            source_log_moneyness: 0.0,
            formation_lane: formation.to_owned(),
            recipient_relative_bin: relative.to_owned(),
            recipient_side: if book.starts_with("H3_") { "CE" } else { "PE" }.to_owned(),
            recipient_money_bin: "-1:1%".to_owned(),
            recipient_log_moneyness: 0.0,
            recipient_dte_bin: "31-60d".to_owned(),
            recipient_dte: 31,
            recipient_expiry: String::new(),
            recipient_expiry_minute: minute + 10_000,
            recipient_represented: true,
            session_end_minute: minute + 1_000,
            date: "2026-01-02".to_owned(),
            event_minute: minute - 1,
            entry_minute: minute,
            scheduled_exit_minute: minute
                + if matches!(book, "H5_F2" | "H5_F3" | "H5_F5") {
                    60
                } else {
                    120
                },
            contract_id: legs[0].contract_id.clone(),
            quantity: legs[0].quantity,
            official_lot_size: None,
            lot_authority: None,
            entry_eligible: true,
            rank_score_micro: None,
            anchor_strike: None,
            atm_strike: None,
            represented_same_expiry_strikes: Vec::new(),
            features: BTreeMap::new(),
            legs,
        }
    }

    fn outcome(intent: &TradeIntent, status: OutcomeStatus) -> ExecutionOutcome {
        ExecutionOutcome {
            intent_id: intent.intent_id.clone(),
            strategy_position_id: intent.strategy_position_id.clone(),
            status,
            fills: Vec::<Fill>::new(),
            reason: None,
        }
    }

    fn feedback(sequence: u64, outcomes: Vec<ExecutionOutcome>) -> EngineFeedback {
        let mut value = empty_feedback(sequence);
        value.outcomes = outcomes;
        value
    }

    fn request(
        sequence: u64,
        minute: i64,
        state: Value,
        feedback: EngineFeedback,
        candidates: Vec<Candidate>,
    ) -> ResearchRequest {
        let detector_diagnostics = candidates
            .iter()
            .map(|candidate| EventDiagnostic {
                contract_id: candidate.source_contract_id.clone(),
                ts_minute: candidate.event_minute,
                raw_iv_sign: candidate.source_raw_sign,
                inherited_sign: None,
                detectors: vec![iv_shock_decision::DetectorDiagnostic {
                    detector: Detector::S0,
                    score: Some(candidate.detector_score),
                    threshold: Some(candidate.detector_threshold),
                    threshold_support: candidate.detector_support,
                    threshold_target: candidate.detector_support,
                    threshold_selected: candidate.detector_support,
                    threshold_ties: 0,
                    prior_absolute_tail_probability: None,
                    prior_tail_support: candidate.detector_support,
                    empirical_two_sided_tail_probability: None,
                    support: candidate.detector_support,
                    calibration_level: Some("test".to_owned()),
                    qualified_before_quiet: true,
                    qualified_after_quiet: candidate.detector_qualified_after_quiet,
                    reason: None,
                }],
            })
            .collect();
        ResearchRequest {
            schema_version: CONTRACT_VERSION.to_owned(),
            input: SealedEvent {
                schema_version: CONTRACT_VERSION.to_owned(),
                event_id: format!("event-{sequence}"),
                sequence,
                decision_at_ns: minute * MINUTE_NS,
                available_at_ns: minute * MINUTE_NS,
                sealed_at_ns: minute * MINUTE_NS,
                quotes: BTreeMap::new(),
                margin_facts: BTreeMap::new(),
                research_payload: serde_json::to_value(Packet::Minute {
                    minute,
                    session_date: candidates.first().map_or_else(
                        || "2026-01-02".to_owned(),
                        |candidate| candidate.date.clone(),
                    ),
                    bar_minute: None,
                    candidates,
                    detector_diagnostics,
                })
                .expect("packet serializes"),
            },
            state,
            feedback,
            sequence,
            feedback_context_hash: "feedback".to_owned(),
            feedback_feature_hash: "features".to_owned(),
        }
    }

    fn session_end_request(
        sequence: u64,
        session_date: &str,
        session_end_minute: i64,
        state: Value,
        feedback: EngineFeedback,
    ) -> ResearchRequest {
        ResearchRequest {
            schema_version: CONTRACT_VERSION.to_owned(),
            input: SealedEvent {
                schema_version: CONTRACT_VERSION.to_owned(),
                event_id: format!("session-end-{sequence}"),
                sequence,
                decision_at_ns: session_end_minute * MINUTE_NS,
                available_at_ns: session_end_minute * MINUTE_NS,
                sealed_at_ns: session_end_minute * MINUTE_NS,
                quotes: BTreeMap::new(),
                margin_facts: BTreeMap::new(),
                research_payload: serde_json::to_value(Packet::SessionEnd {
                    session_date: session_date.to_owned(),
                    session_end_minute,
                })
                .expect("packet serializes"),
            },
            state,
            feedback,
            sequence,
            feedback_context_hash: "feedback".to_owned(),
            feedback_feature_hash: "features".to_owned(),
        }
    }

    fn leg(contract: &str, side: Side, quantity: u64) -> Leg {
        Leg {
            contract_id: contract.to_owned(),
            side,
            quantity,
            option_type: None,
            strike: None,
            expiry: None,
            expiry_minute: None,
            represented: None,
        }
    }

    fn option_leg(
        contract: &str,
        side: Side,
        quantity: u64,
        option_type: &str,
        strike: i64,
        expiry: &str,
    ) -> Leg {
        Leg {
            contract_id: contract.to_owned(),
            side,
            quantity,
            option_type: Some(option_type.to_owned()),
            strike: Some(strike),
            expiry: Some(expiry.to_owned()),
            expiry_minute: Some(10_100),
            represented: Some(true),
        }
    }

    #[test]
    fn margin_rejected_open_releases_book_for_next_candidate() {
        let mut runner = Runner::default();
        let (_, first) = runner
            .process_minute(
                100,
                vec![candidate("a", "H3_F1", 100, vec![leg("A", Side::Sell, 1)])],
                &empty_feedback(0),
            )
            .expect("open");
        let reject = feedback(1, vec![outcome(&first[0], OutcomeStatus::Rejected)]);
        let (_, second) = runner
            .process_minute(
                101,
                vec![candidate("b", "H3_F1", 101, vec![leg("B", Side::Sell, 1)])],
                &reject,
            )
            .expect("retry");
        assert_eq!(second.len(), 1);
        assert!(second[0].intent_id.contains('b'));
    }

    #[test]
    fn single_leg_open_and_reverse_close_are_atomic() {
        let mut runner = Runner::default();
        let (_, open) = runner
            .process_minute(
                100,
                vec![candidate(
                    "a",
                    "H3_F1",
                    100,
                    vec![leg("NEAR", Side::Sell, 2)],
                )],
                &empty_feedback(0),
            )
            .expect("open");
        assert!(open[0].atomic);
        let filled = feedback(1, vec![outcome(&open[0], OutcomeStatus::Filled)]);
        let (_, close) = runner
            .process_minute(220, Vec::new(), &filled)
            .expect("close");
        assert_eq!(close.len(), 1);
        assert_eq!(close[0].action, IntentAction::Close);
        assert_eq!(close[0].legs[0].side, Side::Buy);
    }

    #[test]
    fn rejected_close_retries_next_minute_without_new_open() {
        let mut runner = Runner::default();
        let (_, open) = runner
            .process_minute(
                100,
                vec![candidate("a", "H3_F1", 100, vec![leg("A", Side::Sell, 1)])],
                &empty_feedback(0),
            )
            .expect("open");
        let filled = feedback(1, vec![outcome(&open[0], OutcomeStatus::Filled)]);
        let (_, close) = runner
            .process_minute(220, Vec::new(), &filled)
            .expect("close");
        let rejected = feedback(2, vec![outcome(&close[0], OutcomeStatus::Rejected)]);
        let (_, retry) = runner
            .process_minute(221, Vec::new(), &rejected)
            .expect("retry");
        assert_eq!(retry.len(), 1);
        assert_eq!(retry[0].action, IntentAction::Close);
    }

    #[test]
    fn close_and_reentry_have_explicit_one_minute_feedback_delay() {
        let mut runner = Runner::default();
        let (_, open) = runner
            .process_minute(
                100,
                vec![candidate("a", "H3_F1", 100, vec![leg("A", Side::Sell, 1)])],
                &empty_feedback(0),
            )
            .expect("open");
        let filled = feedback(1, vec![outcome(&open[0], OutcomeStatus::Filled)]);
        let (_, close) = runner
            .process_minute(220, Vec::new(), &filled)
            .expect("close");
        assert_eq!(close.len(), 1);
        let close_filled = feedback(2, vec![outcome(&close[0], OutcomeStatus::Filled)]);
        let (_, reentry) = runner
            .process_minute(
                221,
                vec![candidate("b", "H3_F1", 221, vec![leg("B", Side::Sell, 1)])],
                &close_filled,
            )
            .expect("reentry");
        assert_eq!(reentry.len(), 1);
        assert!(reentry[0].intent_id.contains('b'));
    }

    #[test]
    fn simultaneous_candidates_use_deterministic_book_contract_order() {
        let mut runner = Runner::default();
        let (_, actions) = runner
            .process_minute(
                100,
                vec![
                    candidate("z", "H3_F1", 100, vec![leg("Z", Side::Sell, 1)]),
                    candidate("a", "H3_F1", 100, vec![leg("A", Side::Sell, 1)]),
                    candidate("h5", "H5_F2", 100, vec![leg("H", Side::Buy, 1)]),
                ],
                &empty_feedback(0),
            )
            .expect("ordered");
        assert_eq!(actions.len(), 2);
        assert!(actions[0].intent_id.contains('a'));
        assert!(actions[1].intent_id.contains("h5"));
    }

    #[test]
    fn ten_books_have_independent_occupancy() {
        let mut runner = Runner::default();
        let candidates = FIXED_BOOKS
            .iter()
            .enumerate()
            .map(|(index, book)| {
                candidate(
                    &format!("{book}-{index}"),
                    book,
                    100,
                    vec![leg(&format!("C{index}"), Side::Sell, 1)],
                )
            })
            .collect();
        let (_, actions) = runner
            .process_minute(100, candidates, &empty_feedback(0))
            .expect("ten books");
        assert_eq!(actions.len(), 10);
    }

    #[test]
    fn partial_fill_halts_all_future_admission() {
        let mut runner = Runner::default();
        let (_, open) = runner
            .process_minute(
                100,
                vec![candidate("a", "H3_F1", 100, vec![leg("A", Side::Sell, 1)])],
                &empty_feedback(0),
            )
            .expect("open");
        let partial = feedback(1, vec![outcome(&open[0], OutcomeStatus::PartiallyFilled)]);
        let (_, actions) = runner
            .process_minute(
                101,
                vec![candidate("b", "H3_F2", 101, vec![leg("B", Side::Sell, 1)])],
                &partial,
            )
            .expect("halt");
        assert!(actions.is_empty());
    }

    #[test]
    fn canonical_feedback_arrives_one_sequence_late() {
        let mut runner = Runner::default();
        let first_candidate = candidate("a", "H3_F1", 100, vec![leg("A", Side::Sell, 1)]);
        let first = runner
            .process_request(
                request(
                    0,
                    100,
                    Value::Null,
                    empty_feedback(0),
                    vec![first_candidate],
                ),
                "bundle",
            )
            .expect("sequence zero");
        let open = first.actions.first().expect("open intent");
        let filled = feedback(0, vec![outcome(open, OutcomeStatus::Filled)]);
        let second = runner
            .process_request(request(1, 101, first.state, filled, Vec::new()), "bundle")
            .expect("sequence one");
        assert!(second.actions.is_empty());
        let state = second.state["runner_state"].clone();
        assert_eq!(state["next_sequence"], 2);
        assert_eq!(state["last_feedback_sequence"], 0);
    }

    #[test]
    fn family_metadata_and_direction_tampering_fail_without_state_change() {
        let mut runner = Runner::default();
        let before = runner.state_value();
        let mut wrong_source = candidate("bad-source", "H5_F5", 100, vec![leg("P", Side::Buy, 1)]);
        wrong_source.source_surface = "coherent".to_owned();
        assert!(
            runner
                .process_minute(100, vec![wrong_source], &empty_feedback(0))
                .is_err()
        );
        assert_eq!(runner.state_value(), before);

        let mut wrong_horizon =
            candidate("bad-horizon", "H5_F2", 100, vec![leg("P", Side::Buy, 1)]);
        wrong_horizon.scheduled_exit_minute += 1;
        assert!(
            runner
                .process_minute(100, vec![wrong_horizon], &empty_feedback(0))
                .is_err()
        );
        assert_eq!(runner.state_value(), before);

        let mut wrong_recipient =
            candidate("bad-recipient", "H5_F2", 100, vec![leg("P", Side::Buy, 1)]);
        wrong_recipient.recipient_log_moneyness = 0.02;
        assert!(
            runner
                .process_minute(100, vec![wrong_recipient], &empty_feedback(0))
                .is_err()
        );
        assert_eq!(runner.state_value(), before);

        let mut blocked_detector =
            candidate("bad-detector", "H5_F2", 100, vec![leg("P", Side::Buy, 1)]);
        blocked_detector.detector = "S1".to_owned();
        assert!(
            runner
                .process_minute(100, vec![blocked_detector], &empty_feedback(0))
                .is_err()
        );
        assert_eq!(runner.state_value(), before);

        let mut unqualified = candidate("bad-score", "H5_F2", 100, vec![leg("P", Side::Buy, 1)]);
        unqualified.detector_score = 1.99;
        unqualified.detector_qualified_after_quiet = false;
        assert!(
            runner
                .process_minute(100, vec![unqualified], &empty_feedback(0))
                .is_err()
        );
        assert_eq!(runner.state_value(), before);

        let mut wrong_side = candidate("bad-side", "H5_F5", 100, vec![leg("P", Side::Buy, 1)]);
        wrong_side.legs[0].side = Side::Sell;
        assert!(
            runner
                .process_minute(100, vec![wrong_side], &empty_feedback(0))
                .is_err()
        );
        assert_eq!(runner.state_value(), before);
    }

    #[test]
    fn fresh_process_restart_matches_uninterrupted_response() {
        let mut uninterrupted = Runner::default();
        let first = uninterrupted
            .process_request(
                request(
                    0,
                    100,
                    Value::Null,
                    empty_feedback(0),
                    vec![candidate("a", "H5_F2", 100, vec![leg("P", Side::Buy, 1)])],
                ),
                "bundle",
            )
            .expect("initial request");
        let open = first.actions[0].clone();
        let next_feedback = feedback(0, vec![outcome(&open, OutcomeStatus::Filled)]);
        let next_request = request(1, 101, first.state, next_feedback, Vec::new());
        let expected = uninterrupted
            .process_request(next_request.clone(), "bundle")
            .expect("uninterrupted");
        let mut restored = Runner::default();
        let actual = restored
            .process_request(next_request, "bundle")
            .expect("restored");
        assert_eq!(actual, expected);
        assert_eq!(restored.state_value(), uninterrupted.state_value());
    }

    #[test]
    fn canonical_candidate_requires_matching_typed_detector_evidence() {
        let mut runner = Runner::default();
        let candidate = candidate("a", "H5_F2", 100, vec![leg("P", Side::Buy, 1)]);
        let mut input = request(0, 100, Value::Null, empty_feedback(0), vec![candidate]);
        input.input.research_payload["detector_diagnostics"] = json!([]);
        let before = runner.state_value();
        assert!(runner.process_request(input, "bundle").is_err());
        assert_eq!(runner.state_value(), before);
    }

    #[test]
    fn late_planned_exit_is_a_counted_coverage_rejection() {
        let mut runner = Runner::default();
        let mut late = candidate("late", "H5_F2", 100, vec![leg("P", Side::Buy, 1)]);
        late.session_end_minute = 150;
        let response = runner
            .process_request(
                request(0, 100, Value::Null, empty_feedback(0), vec![late]),
                "bundle",
            )
            .expect("coverage rejection is not a packet error");
        assert!(response.actions.is_empty());
        assert_eq!(
            response.state["runner_state"]["books"]["H5_F2"]["counts"]["reason_counts"]["planned_session_end_ineligible"],
            1
        );
    }

    #[test]
    fn rejected_close_is_quarantined_at_session_end_without_global_halt() {
        let mut runner = Runner::default();
        let mut first_candidate = candidate("a", "H3_F1", 100, vec![leg("A", Side::Sell, 1)]);
        first_candidate.session_end_minute = 221;
        let first = runner
            .process_request(
                request(
                    0,
                    100,
                    Value::Null,
                    empty_feedback(0),
                    vec![first_candidate],
                ),
                "bundle",
            )
            .expect("open");
        let open_filled = feedback(0, vec![outcome(&first.actions[0], OutcomeStatus::Filled)]);
        let second = runner
            .process_request(
                request(1, 220, first.state, open_filled, Vec::new()),
                "bundle",
            )
            .expect("close intent");
        let close_rejected = feedback(
            1,
            vec![outcome(&second.actions[0], OutcomeStatus::Rejected)],
        );
        let ended = runner
            .process_request(
                session_end_request(2, "2026-01-02", 222, second.state, close_rejected),
                "bundle",
            )
            .expect("session end");
        assert!(ended.actions.is_empty());
        assert_eq!(ended.state["runner_state"]["halted"], false);
        assert_eq!(
            ended.state["runner_state"]["books"]["H3_F1"]["unresolved"],
            true
        );
        assert_eq!(
            ended.state["runner_state"]["books"]["H3_F1"]["counts"]["unresolved_session_end"],
            1
        );
        let mut next_candidate = candidate("b", "H3_F2", 1_600, vec![leg("B", Side::Sell, 1)]);
        next_candidate.date = "2026-01-05".to_owned();
        next_candidate.source_event_id = format!(
            "iv-shock-source|{}|{}|{}",
            next_candidate.date, next_candidate.event_minute, next_candidate.source_contract_id
        );
        let next = runner
            .process_request(
                request(
                    3,
                    1_600,
                    ended.state,
                    empty_feedback(2),
                    vec![next_candidate],
                ),
                "bundle",
            )
            .expect("other book continues next session");
        assert_eq!(next.actions.len(), 1);
        assert!(next.actions[0].intent_id.contains("H3_F2"));
        assert_eq!(
            next.state["runner_state"]["books"]["H3_F1"]["counts"]["unresolved_session_end"],
            1
        );
    }

    #[test]
    fn missing_close_feedback_quarantines_only_its_family_book() {
        let mut runner = Runner::default();
        let mut first_candidate = candidate("a", "H3_F1", 100, vec![leg("A", Side::Sell, 1)]);
        first_candidate.session_end_minute = 221;
        let first = runner
            .process_request(
                request(
                    0,
                    100,
                    Value::Null,
                    empty_feedback(0),
                    vec![first_candidate],
                ),
                "bundle",
            )
            .expect("open");
        let open_filled = feedback(0, vec![outcome(&first.actions[0], OutcomeStatus::Filled)]);
        let second = runner
            .process_request(
                request(1, 220, first.state, open_filled, Vec::new()),
                "bundle",
            )
            .expect("close intent");
        let ended = runner
            .process_request(
                session_end_request(2, "2026-01-02", 222, second.state, empty_feedback(1)),
                "bundle",
            )
            .expect("quarantine pending close");
        let mut next_candidate = candidate("b", "H3_F2", 1_600, vec![leg("B", Side::Sell, 1)]);
        next_candidate.date = "2026-01-05".to_owned();
        next_candidate.source_event_id = format!(
            "iv-shock-source|{}|{}|{}",
            next_candidate.date, next_candidate.event_minute, next_candidate.source_contract_id
        );
        let next = runner
            .process_request(
                request(
                    3,
                    1_600,
                    ended.state,
                    empty_feedback(2),
                    vec![next_candidate],
                ),
                "bundle",
            )
            .expect("independent family continues");
        assert_eq!(next.actions.len(), 1);
        assert_eq!(
            next.state["runner_state"]["books"]["H3_F1"]["unresolved"],
            true
        );
        assert!(!next.state["runner_state"]["books"]["H3_F1"]["pending"].is_null());
    }

    #[test]
    fn restart_rejects_a_different_research_bundle() {
        let mut runner = Runner::default();
        let first = runner
            .process_request(
                request(0, 100, Value::Null, empty_feedback(0), Vec::new()),
                "bundle-a",
            )
            .expect("initial request");
        let mut restored = Runner::default();
        assert!(
            restored
                .process_request(
                    request(1, 101, first.state, empty_feedback(0), Vec::new()),
                    "bundle-b",
                )
                .is_err()
        );
    }

    #[test]
    fn final_h5_vertical_filters_and_atomic_close_are_executable() {
        let mut runner = Runner::new(Config::final_candidate());
        let mut candidate = candidate(
            "vertical",
            "H5_F2",
            100,
            vec![
                option_leg("PE-22000", Side::Buy, 25, "PE", 22_000, "2026-01-29"),
                option_leg("PE-21700", Side::Sell, 25, "PE", 21_700, "2026-01-29"),
            ],
        );
        candidate.scenario = "final_candidate".to_owned();
        candidate.structure_id = "pe_vertical_down_300".to_owned();
        candidate.contract_id = "PE-22000".to_owned();
        candidate.quantity = 25;
        candidate.recipient_expiry = "2026-01-29".to_owned();
        candidate.represented_same_expiry_strikes = vec![21_700, 22_000];
        candidate.official_lot_size = Some(25);
        candidate.lot_authority = Some("friend-contract-master@sha256:test".to_owned());
        candidate.legs[1].side = Side::Sell;
        candidate.rank_score_micro = Some(1);
        candidate.features = BTreeMap::from([
            ("spot_return_30m_pct".to_owned(), Some(-0.1)),
            ("observed_risk_reversal_moneyness_pp".to_owned(), Some(0.1)),
            ("structure_event_volume_min".to_owned(), Some(0.0)),
        ]);
        let first = runner
            .process_request(
                request(0, 100, Value::Null, empty_feedback(0), vec![candidate]),
                "bundle",
            )
            .expect("final vertical opens");
        assert_eq!(first.actions.len(), 1);
        assert_eq!(first.actions[0].legs.len(), 2);
        assert!(first.actions[0].atomic);
        let filled = feedback(0, vec![outcome(&first.actions[0], OutcomeStatus::Filled)]);
        let closed = runner
            .process_request(request(1, 160, first.state, filled, Vec::new()), "bundle")
            .expect("final vertical closes");
        assert_eq!(closed.actions[0].legs[0].side, Side::Sell);
        assert_eq!(closed.actions[0].legs[1].side, Side::Buy);
    }

    #[test]
    fn final_h5_500_point_vertical_is_executable() {
        let mut runner = Runner::new(Config::final_candidate());
        let mut candidate = candidate(
            "vertical-500",
            "H5_F1",
            100,
            vec![
                option_leg("PE-22000", Side::Buy, 25, "PE", 22_000, "2026-01-29"),
                option_leg("PE-21500", Side::Sell, 25, "PE", 21_500, "2026-01-29"),
            ],
        );
        candidate.scenario = "final_candidate".to_owned();
        candidate.structure_id = "pe_vertical_down_500".to_owned();
        candidate.contract_id = "PE-22000".to_owned();
        candidate.quantity = 25;
        candidate.recipient_expiry = "2026-01-29".to_owned();
        candidate.represented_same_expiry_strikes = vec![21_500, 22_000];
        candidate.official_lot_size = Some(25);
        candidate.lot_authority = Some("friend-contract-master@sha256:test".to_owned());
        candidate.legs[1].side = Side::Sell;
        candidate.rank_score_micro = Some(1);
        candidate.features = BTreeMap::from([("spot_return_30m_pct".to_owned(), Some(-0.1))]);
        let response = runner
            .process_request(
                request(0, 100, Value::Null, empty_feedback(0), vec![candidate]),
                "bundle",
            )
            .expect("500-point vertical opens");
        assert_eq!(response.actions.len(), 1);
        assert_eq!(response.actions[0].legs.len(), 2);
        assert!(response.actions[0].atomic);
        assert_eq!(response.actions[0].lineage["official_lot_size"], "25");
        assert_eq!(
            response.actions[0].lineage["lot_authority"],
            "friend-contract-master@sha256:test"
        );
    }

    #[test]
    fn final_structure_rejects_unrepresented_leg_and_wrong_lot() {
        let mut candidate = candidate(
            "bad-vertical",
            "H5_F1",
            100,
            vec![
                option_leg("PE-22000", Side::Buy, 50, "PE", 22_000, "2026-01-29"),
                option_leg("PE-21500", Side::Sell, 50, "PE", 21_500, "2026-01-29"),
            ],
        );
        candidate.scenario = "final_candidate".to_owned();
        candidate.structure_id = "pe_vertical_down_500".to_owned();
        candidate.contract_id = "PE-22000".to_owned();
        candidate.quantity = 50;
        candidate.recipient_expiry = "2026-01-29".to_owned();
        candidate.represented_same_expiry_strikes = vec![22_000];
        candidate.official_lot_size = Some(25);
        candidate.lot_authority = Some("friend-contract-master@sha256:test".to_owned());
        candidate.legs[1].side = Side::Sell;
        candidate.rank_score_micro = Some(1);
        candidate.features = BTreeMap::from([("spot_return_30m_pct".to_owned(), Some(-0.1))]);
        let error = Runner::new(Config::final_candidate())
            .process_request(
                request(0, 100, Value::Null, empty_feedback(0), vec![candidate]),
                "bundle",
            )
            .expect_err("wrong lot and incomplete chain must fail closed");
        assert!(error.contains("official lot"));
    }

    #[test]
    fn final_h3_outward_leg_is_selected_from_declared_inventory() {
        let mut runner = Runner::new(Config::final_candidate());
        let mut candidate = candidate(
            "outward",
            "H3_F4",
            100,
            vec![option_leg(
                "CE-20500",
                Side::Sell,
                25,
                "CE",
                20_500,
                "2026-02-26",
            )],
        );
        candidate.scenario = "final_candidate".to_owned();
        candidate.structure_id = "outward_1_ce".to_owned();
        candidate.contract_id = "CE-20500".to_owned();
        candidate.quantity = 25;
        candidate.recipient_expiry = "2026-02-26".to_owned();
        candidate.official_lot_size = Some(25);
        candidate.lot_authority = Some("friend-contract-master@sha256:test".to_owned());
        candidate.anchor_strike = Some(20_600);
        candidate.atm_strike = Some(21_000);
        candidate.represented_same_expiry_strikes = vec![20_400, 20_500, 20_600, 20_700];
        candidate.rank_score_micro = Some(1);
        candidate.features = BTreeMap::from([
            ("spot_return_30m_pct".to_owned(), Some(-0.1)),
            ("structure_event_volume_min".to_owned(), Some(0.0)),
        ]);
        let response = runner
            .process_request(
                request(0, 100, Value::Null, empty_feedback(0), vec![candidate]),
                "bundle",
            )
            .expect("outward leg opens");
        assert_eq!(response.actions.len(), 1);
        assert_eq!(response.actions[0].legs[0].instrument_id, "CE-20500");
    }

    #[test]
    fn frozen_reference_bundle_recomputes_known_monthly_score() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../../models/exp032_reference_model_bundle.json");
        let bundle = ModelBundle::load(
            &path,
            "158d5bc92d2da250174649bfd1f1a4c7961d96bda14e9467f4adcb844ffb0b20",
        )
        .expect("reference model loads");
        let mut candidate = candidate("score", "H5_F2", 100, vec![leg("P", Side::Buy, 1)]);
        candidate.date = "2024-07-01".to_owned();
        candidate.features = FINAL_FEATURES
            .iter()
            .map(|name| ((*name).to_owned(), Some(0.0)))
            .collect();
        assert_eq!(bundle.score(&candidate).expect("score"), 198_495_883);
    }
}
