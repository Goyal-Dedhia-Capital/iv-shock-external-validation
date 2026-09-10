//! Causal, feedback-aware policy for the funded best-family replay.
//!
//! This crate owns only sequential admission and lifecycle intent generation.
//! The feeder owns event timing and the generic engine owns prices, fills,
//! fees, margin, accounting, and portfolio state.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::OnceLock;

use backtest_contracts::{
    AccountState, CONTRACT_VERSION, EngineFeedback, ExecutionOutcome, IntentAction, IntentLeg,
    ResearchRequest, ResearchResponse, Side, TradeIntent,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

pub const RUNNER_ID: &str = "exp046-r2-low-z-funded-v1";
pub const SCHEMA_VERSION: &str = "gdc.exp046.r2-low-z-funded.v1";
pub const MINUTE_NS: i64 = 60_000_000_000;
pub const FIXED_BOOKS: [&str; 17] = [
    "R2_BASE_1X",
    "R2_IV30_1X",
    "R2_IV30_2X",
    "R2_GE_1X",
    "R2_GE_2X",
    "R2_TIER_IV30",
    "R2_TIER_GE",
    "R2_POS_DTE45_1X",
    "R2_EXPANDED_1X",
    "R2_EXPANDED_IVTIER",
    "R2_CORE_IVTIER",
    "R2_SCORE_GATE_1X",
    "R2_SCORE_TIER_2X",
    "R2_SCORE_TIER_3X",
    "R2_LOWZ_SCORE_TIER_2X",
    "R2_SCORE_Q2_RHO_PRIORITY_1X",
    "R2_SCORE_Q2_RHO_PRIORITY_2X",
];

const IV30_HIGH: f64 = 0.302_094_966_173_172;
const BASKET_GAMMA_HIGH: f64 = -0.000_377_104_908_693_581_8;
const MOMENTUM_EFFICIENCY_HIGH: f64 = 0.182_689_751_733_559_92;
const RHO_PREMIUM_LOW_MAX: f64 = 0.027_711_319_570_318_09;

#[derive(Debug, Deserialize)]
struct ScoreTransform {
    median: Vec<f64>,
    low: Vec<f64>,
    high: Vec<f64>,
    mean: Vec<f64>,
    scale: Vec<f64>,
}

#[derive(Debug, Deserialize)]
struct ScoreModel {
    features: Vec<String>,
    transform: ScoreTransform,
    beta: Vec<f64>,
    score_quintile_edges: Vec<f64>,
}

fn score_model() -> &'static ScoreModel {
    static MODEL: OnceLock<ScoreModel> = OnceLock::new();
    MODEL.get_or_init(|| {
        let model: ScoreModel = serde_json::from_str(include_str!("../MODEL.json"))
            .expect("embedded EXP040 score model must parse");
        let n = model.features.len();
        assert_eq!(model.transform.median.len(), n);
        assert_eq!(model.transform.low.len(), n);
        assert_eq!(model.transform.high.len(), n);
        assert_eq!(model.transform.mean.len(), n);
        assert_eq!(model.transform.scale.len(), n);
        assert_eq!(model.beta.len(), 1 + 2 * n);
        assert_eq!(model.score_quintile_edges.len(), 4);
        model
    })
}

fn is_score_book(book: &str) -> bool {
    matches!(
        book,
        "R2_SCORE_GATE_1X"
            | "R2_SCORE_TIER_2X"
            | "R2_SCORE_TIER_3X"
            | "R2_LOWZ_SCORE_TIER_2X"
            | "R2_SCORE_Q2_RHO_PRIORITY_1X"
            | "R2_SCORE_Q2_RHO_PRIORITY_2X"
    )
}

fn tail_score(candidate: &Candidate) -> f64 {
    let model = score_model();
    let n = model.features.len();
    let mut score = model.beta[0];
    for (index, name) in model.features.iter().enumerate() {
        let value = feature(candidate, name);
        let filled = value.unwrap_or(model.transform.median[index]);
        let clipped = filled.clamp(model.transform.low[index], model.transform.high[index]);
        let standardized = (clipped - model.transform.mean[index]) / model.transform.scale[index];
        score += model.beta[1 + index] * standardized;
        score += model.beta[1 + n + index] * if value.is_none() { 1.0 } else { 0.0 };
    }
    assert!(score.is_finite(), "EXP040 score must remain finite");
    score
}

#[allow(clippy::cast_possible_truncation)]
fn tail_score_micro(candidate: &Candidate) -> i64 {
    let scaled = (tail_score(candidate) * 1_000_000.0).round();
    assert!(scaled.abs() < 9_000_000_000_000_000_000.0);
    scaled as i64
}

fn tail_score_bin(candidate: &Candidate) -> u8 {
    let score = tail_score(candidate);
    let passed = score_model()
        .score_quintile_edges
        .iter()
        .take_while(|edge| score > **edge)
        .count();
    1 + u8::try_from(passed).expect("four score edges fit in u8")
}

#[allow(clippy::cast_possible_truncation, clippy::cast_precision_loss)]
fn promoted_q2_priority(score_micro: i64) -> i64 {
    let edges = &score_model().score_quintile_edges;
    let q2_fraction =
        ((score_micro as f64 / 1_000_000.0 - edges[0]) / (edges[1] - edges[0])).clamp(0.0, 1.0);
    ((edges[2] + q2_fraction * (edges[3] - edges[2])) * 1_000_000.0).round() as i64
}

#[allow(clippy::cast_possible_truncation, clippy::cast_precision_loss)]
fn candidate_priority(candidate: &Candidate) -> i64 {
    if is_score_book(&candidate.book) {
        let score = tail_score_micro(candidate);
        // The low-rho promotion is a priority tie-breaker only. It does not
        // alter the frozen score, Q1 gate, or causal population.
        if matches!(
            candidate.book.as_str(),
            "R2_SCORE_Q2_RHO_PRIORITY_1X" | "R2_SCORE_Q2_RHO_PRIORITY_2X"
        ) && tail_score_bin(candidate) == 2
            && is_low_rho(candidate)
        {
            // Map the Q2 score interval monotonically onto Q4. This keeps
            // Q4/Q5 ahead while every promoted Q2 outranks ordinary Q2/Q3.
            promoted_q2_priority(score)
        } else {
            score
        }
    } else {
        candidate.rank_score_micro
    }
}

fn book_rank(book: &str) -> Option<usize> {
    FIXED_BOOKS.iter().position(|fixed| *fixed == book)
}

fn default_books() -> Vec<String> {
    FIXED_BOOKS.iter().map(|book| (*book).to_owned()).collect()
}

fn default_scenarios() -> Vec<String> {
    vec!["baseline".to_owned()]
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AdmissionMode {
    #[default]
    All,
    PositiveScore,
    FamilySpecific,
    /// Score-only challenger: remove the inherited spot/volume/DTE gate,
    /// retain the frozen score-Q1 rejection, and trade one lot.
    ScoreOnlyGate1x,
    /// Score-only challenger: same admission as `ScoreOnlyGate1x`, with the
    /// frozen Q4/Q5 two-lot tier retained.
    ScoreOnlyTier2x,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default = "default_books")]
    pub books: Vec<String>,
    #[serde(default = "default_scenarios")]
    pub scenarios: Vec<String>,
    #[serde(default)]
    pub admission_mode: AdmissionMode,
}

impl Config {
    #[must_use]
    pub fn baseline() -> Self {
        Self {
            books: default_books(),
            scenarios: default_scenarios(),
            admission_mode: AdmissionMode::All,
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
        let mut scenarios = BTreeSet::new();
        for scenario in &self.scenarios {
            if scenario.is_empty() || !scenarios.insert(scenario) {
                return Err("scenario identifiers must be nonempty and unique".to_owned());
            }
        }
        Ok(())
    }
}

fn score_rejection(mode: AdmissionMode, candidate: &Candidate) -> Option<&'static str> {
    if matches!(
        mode,
        AdmissionMode::ScoreOnlyGate1x | AdmissionMode::ScoreOnlyTier2x
    ) && tail_score_bin(candidate) == 1
    {
        return Some("tail_score_q1");
    }
    if candidate.rank_score_micro > 0 {
        return None;
    }
    match mode {
        AdmissionMode::PositiveScore => Some("score_nonpositive"),
        AdmissionMode::FamilySpecific
            if matches!(
                candidate.book.as_str(),
                "R2_GE_1X" | "R2_GE_2X" | "R2_TIER_IV30" | "R2_TIER_GE"
            ) =>
        {
            Some("score_nonpositive_family_gate")
        }
        AdmissionMode::All
        | AdmissionMode::FamilySpecific
        | AdmissionMode::ScoreOnlyGate1x
        | AdmissionMode::ScoreOnlyTier2x => None,
    }
}

fn feature(candidate: &Candidate, name: &str) -> Option<f64> {
    candidate
        .features
        .get(name)
        .copied()
        .flatten()
        .filter(|value| value.is_finite())
}

/// The feeder supplies causal values only. The explicit sentinel keeps the
/// inherited policy fixtures outside EXP037 semantics and makes accidental
/// use of an older feeder fail closed at candidate validation time.
fn feature_rejection_for(mode: AdmissionMode, candidate: &Candidate) -> Option<&'static str> {
    if feature(candidate, "exp040_gate") != Some(1.0) {
        return Some("exp040_gate_missing");
    }
    if matches!(
        mode,
        AdmissionMode::ScoreOnlyGate1x | AdmissionMode::ScoreOnlyTier2x
    ) {
        return None;
    }
    let spot = feature(candidate, "spot_return_30m_pct");
    let volume = feature(candidate, "structure_event_volume_min");
    let iv30 = feature(candidate, "shock_iv_change_30m_pp");
    let gamma = feature(candidate, "basket_event_gamma");
    let efficiency = feature(candidate, "momentum_efficiency30");
    let dte = feature(candidate, "shock_calendar_dte");
    let strict = spot.is_some_and(|value| value <= 0.0) && volume.is_some_and(|value| value <= 0.0);
    let positive_volume_dte45 = spot.is_some_and(|value| value <= 0.0)
        && volume.is_some_and(|value| value > 0.0)
        && dte.is_some_and(|value| value > 45.0);
    let iv30_high = iv30.is_some_and(|value| value > IV30_HIGH);
    let gamma_efficiency_high = gamma.is_some_and(|value| value > BASKET_GAMMA_HIGH)
        && efficiency.is_some_and(|value| value > MOMENTUM_EFFICIENCY_HIGH);
    let expanded = strict || positive_volume_dte45;
    if is_score_book(&candidate.book) {
        if !expanded {
            return Some("causal_feature_gate");
        }
        return (tail_score_bin(candidate) == 1).then_some("tail_score_q1");
    }
    match candidate.book.as_str() {
        "R2_BASE_1X" | "R2_TIER_IV30" | "R2_TIER_GE" | "R2_CORE_IVTIER" if strict => None,
        "R2_IV30_1X" | "R2_IV30_2X" if strict && iv30_high => None,
        "R2_GE_1X" | "R2_GE_2X" if strict && gamma_efficiency_high => None,
        "R2_POS_DTE45_1X" if positive_volume_dte45 => None,
        "R2_EXPANDED_1X" | "R2_EXPANDED_IVTIER" if strict || positive_volume_dte45 => None,
        _ => Some("causal_feature_gate"),
    }
}

#[cfg(test)]
fn feature_rejection(candidate: &Candidate) -> Option<&'static str> {
    feature_rejection_for(AdmissionMode::All, candidate)
}

fn quantity_multiplier_for(mode: AdmissionMode, candidate: &Candidate) -> u64 {
    if matches!(mode, AdmissionMode::ScoreOnlyGate1x) {
        return 1;
    }
    if matches!(mode, AdmissionMode::ScoreOnlyTier2x) {
        return u64::from(tail_score_bin(candidate) >= 4) + 1;
    }
    let iv30_high =
        feature(candidate, "shock_iv_change_30m_pp").is_some_and(|value| value > IV30_HIGH);
    let gamma_efficiency_high = feature(candidate, "basket_event_gamma")
        .is_some_and(|value| value > BASKET_GAMMA_HIGH)
        && feature(candidate, "momentum_efficiency30")
            .is_some_and(|value| value > MOMENTUM_EFFICIENCY_HIGH);
    match candidate.book.as_str() {
        "R2_IV30_2X" | "R2_GE_2X" => 2,
        "R2_TIER_IV30" | "R2_EXPANDED_IVTIER" | "R2_CORE_IVTIER" if iv30_high => 2,
        "R2_TIER_GE" if gamma_efficiency_high => 2,
        "R2_SCORE_TIER_2X"
        | "R2_LOWZ_SCORE_TIER_2X"
        | "R2_SCORE_Q2_RHO_PRIORITY_1X"
        | "R2_SCORE_Q2_RHO_PRIORITY_2X"
            if tail_score_bin(candidate) >= 4 =>
        {
            2
        }
        "R2_SCORE_Q2_RHO_PRIORITY_2X"
            if tail_score_bin(candidate) == 2 && is_low_rho(candidate) =>
        {
            2
        }
        "R2_SCORE_TIER_3X" if tail_score_bin(candidate) == 5 => 3,
        "R2_SCORE_TIER_3X" if tail_score_bin(candidate) == 4 => 2,
        _ => 1,
    }
}

#[cfg(test)]
fn quantity_multiplier(candidate: &Candidate) -> u64 {
    quantity_multiplier_for(AdmissionMode::All, candidate)
}

fn is_low_rho(candidate: &Candidate) -> bool {
    feature(candidate, "rho_per_premium")
        .is_some_and(|value| value.is_finite() && value <= RHO_PREMIUM_LOW_MAX)
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
    pub option_type: String,
    #[serde(default)]
    pub strike: i64,
    #[serde(default)]
    pub expiry: String,
    #[serde(default)]
    pub expiry_minute: i64,
    #[serde(default)]
    pub represented: bool,
}

/// Candidate fields are entry-known only.  In particular, no exit quote or
/// exit-availability field is accepted here.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Candidate {
    pub candidate_id: String,
    pub book: String,
    pub scenario: String,
    pub structure_id: String,
    pub source_event_id: String,
    #[serde(default)]
    pub source_side: String,
    #[serde(default)]
    pub source_raw_sign: i8,
    #[serde(default)]
    pub source_surface: String,
    #[serde(default)]
    pub inherited_abs_z: f64,
    pub date: String,
    pub event_minute: i64,
    pub entry_minute: i64,
    pub scheduled_exit_minute: i64,
    pub contract_id: String,
    pub quantity: u64,
    #[serde(default)]
    pub official_lot_size: u64,
    #[serde(default)]
    pub lot_authority: String,
    #[serde(default)]
    pub event_atm_strike: i64,
    #[serde(default)]
    pub recipient_expiry: String,
    #[serde(default)]
    pub recipient_expiry_minute: i64,
    #[serde(default)]
    pub recipient_dte: i32,
    #[serde(default)]
    pub represented_same_expiry_strikes: Vec<i64>,
    pub entry_eligible: bool,
    /// Integer-micro-rupee confidence margin fitted using labels strictly
    /// before this candidate's month. Higher values receive capital first.
    pub rank_score_micro: i64,
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
    pub quantity: u64,
    pub tail_score_micro: i64,
    pub tail_score_bin: u8,
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
    #[serde(default)]
    pub forced_derisk_close_intents: u64,
    #[serde(default)]
    pub forced_derisk_minutes: u64,
    pub partial_halts: u64,
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
    /// Concurrent positions are keyed by immutable strategy identity. The
    /// family book itself is not an occupancy lock in EXP028.
    pub active: BTreeMap<String, Position>,
    pub pending: BTreeMap<String, PendingAction>,
    pub last_minute: Option<i64>,
    pub halted: bool,
    pub counts: Counts,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct PersistedState {
    pub schema_version: String,
    pub config: Config,
    pub next_sequence: u64,
    pub books: BTreeMap<String, BookState>,
    pub totals: Counts,
    pub halted: bool,
    /// Sequence of the most recently applied engine feedback. The initial
    /// sequence-0 feedback is intentionally empty and is not recorded here;
    /// sequence-0 execution feedback arrives on request 1.
    pub last_feedback_sequence: Option<u64>,
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
            config: self.config.clone(),
            next_sequence: self.sequence,
            books: self.books.clone(),
            totals: self.totals.clone(),
            halted: self.halted,
            last_feedback_sequence: self.last_feedback_sequence,
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
        if candidate.event_minute >= candidate.entry_minute || candidate.entry_minute != minute {
            return Err("candidate event/entry clocks are not causal".to_owned());
        }
        if candidate.scheduled_exit_minute <= candidate.entry_minute {
            return Err("scheduled exit must follow entry".to_owned());
        }
        if candidate.legs.is_empty() {
            return Err("candidate must contain at least one leg".to_owned());
        }
        if candidate.entry_eligible && candidate.quantity == 0 {
            return Err("eligible candidate quantity must be positive".to_owned());
        }
        let mut contracts = BTreeSet::new();
        for leg in &candidate.legs {
            if leg.contract_id.is_empty()
                || leg.quantity == 0
                || !contracts.insert(&leg.contract_id)
            {
                return Err("legs need unique contracts and positive quantities".to_owned());
            }
        }
        if candidate.book == "R2_SCORE_TIER_2X" {
            let leg = candidate
                .legs
                .first()
                .ok_or_else(|| "R2 core requires one CE leg".to_owned())?;
            let distance = leg
                .strike
                .checked_sub(candidate.event_atm_strike)
                .ok_or_else(|| "R2 strike distance overflow".to_owned())?;
            if candidate.structure_id != "single_leg"
                || candidate.legs.len() != 1
                || candidate.contract_id != leg.contract_id
                || leg.side != Side::Sell
                || leg.option_type != "CE"
                || candidate.source_side != "CE"
                || candidate.source_raw_sign != 1
                || candidate.source_surface != "coherent"
                || !candidate.inherited_abs_z.is_finite()
                || candidate.inherited_abs_z < 2.0
                || !(31..=60).contains(&candidate.recipient_dte)
                || !(250..=500).contains(&distance)
                || candidate.scheduled_exit_minute - candidate.entry_minute != 30
                || candidate.recipient_expiry.is_empty()
                || leg.expiry != candidate.recipient_expiry
                || leg.expiry_minute != candidate.recipient_expiry_minute
                || candidate.recipient_expiry_minute < candidate.scheduled_exit_minute
                || !leg.represented
                || !candidate
                    .represented_same_expiry_strikes
                    .contains(&leg.strike)
                || candidate.official_lot_size == 0
                || candidate.quantity != candidate.official_lot_size
                || leg.quantity != candidate.official_lot_size
                || candidate.lot_authority.is_empty()
                || candidate.lot_authority.eq_ignore_ascii_case("PENDING")
            {
                return Err("candidate differs from promoted R2 core geometry".to_owned());
            }
        }
        Ok(())
    }

    #[cfg(test)]
    fn position(candidate: &Candidate) -> Result<Position, String> {
        Self::position_for(candidate, AdmissionMode::All)
    }

    fn position_for(candidate: &Candidate, mode: AdmissionMode) -> Result<Position, String> {
        let multiplier = quantity_multiplier_for(mode, candidate);
        let mut legs = candidate.legs.clone();
        for leg in &mut legs {
            leg.quantity = leg
                .quantity
                .checked_mul(multiplier)
                .ok_or_else(|| "leg quantity overflow".to_owned())?;
        }
        let strategy_position_id = format!(
            "exp046|{}|{}|{}",
            candidate.scenario, candidate.book, candidate.candidate_id
        );
        Ok(Position {
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
            quantity: candidate
                .quantity
                .checked_mul(multiplier)
                .ok_or_else(|| "candidate quantity overflow".to_owned())?,
            tail_score_micro: tail_score_micro(candidate),
            tail_score_bin: tail_score_bin(candidate),
            legs,
        })
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
                ("experiment".to_owned(), "exp046".to_owned()),
                (
                    "policy".to_owned(),
                    "overlapping_positions_per_family".to_owned(),
                ),
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
                    "tail_score_micro".to_owned(),
                    position.tail_score_micro.to_string(),
                ),
                (
                    "tail_score_bin".to_owned(),
                    position.tail_score_bin.to_string(),
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
                ("experiment".to_owned(), "exp046".to_owned()),
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
        let (target_book, pending_key) = self
            .books
            .iter()
            .find_map(|(book, state)| {
                state
                    .pending
                    .iter()
                    .find_map(|(key, pending)| match pending {
                        PendingAction::Open { intent_id, .. }
                        | PendingAction::Close { intent_id, .. }
                            if intent_id == &outcome.intent_id =>
                        {
                            Some((book.clone(), key.clone()))
                        }
                        _ => None,
                    })
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
        let pending = state
            .pending
            .remove(&pending_key)
            .ok_or_else(|| format!("pending intent {} disappeared", outcome.intent_id))?;
        match (&pending, outcome.status) {
            (PendingAction::Open { position, .. }, backtest_contracts::OutcomeStatus::Filled) => {
                state
                    .active
                    .insert(position.strategy_position_id.clone(), position.clone());
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
            (PendingAction::Close { position, .. }, backtest_contracts::OutcomeStatus::Filled) => {
                state.active.remove(&position.strategy_position_id);
                state.counts.filled_closes = state.counts.filled_closes.saturating_add(1);
            }
            (PendingAction::Close { .. }, backtest_contracts::OutcomeStatus::Rejected)
            | (PendingAction::Close { .. }, backtest_contracts::OutcomeStatus::Deferred) => {
                if let PendingAction::Close { position, .. } = &pending {
                    state
                        .active
                        .insert(position.strategy_position_id.clone(), position.clone());
                }
                state.counts.close_retries = state.counts.close_retries.saturating_add(1);
                state.counts.inc_reason("engine_rejected_close_retry");
            }
            (_, backtest_contracts::OutcomeStatus::PartiallyFilled) => {
                if let PendingAction::Close { position, .. } = &pending {
                    state
                        .active
                        .insert(position.strategy_position_id.clone(), position.clone());
                }
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

    fn finalize_session(&mut self, session_date: &str, minute: i64) -> Result<Value, String> {
        for state in self.books.values_mut() {
            if state.last_minute.is_some_and(|last| minute <= last) {
                return Err("session end must follow the final minute".to_owned());
            }
            state.last_minute = Some(minute);
            let active_date_mismatch = state
                .active
                .values()
                .any(|position| position.date != session_date);
            let pending_date_mismatch = state.pending.values().any(|pending| match pending {
                PendingAction::Open { position, .. } | PendingAction::Close { position, .. } => {
                    position.date != session_date
                }
            });
            if active_date_mismatch || pending_date_mismatch {
                return Err("session-end date differs from an owned position".to_owned());
            }
            let unresolved = !state.active.is_empty() || !state.pending.is_empty();
            if unresolved && !state.halted {
                state.halted = true;
                state.counts.partial_halts = state.counts.partial_halts.saturating_add(1);
                state.counts.inc_reason("unresolved_at_session_end");
            }
        }
        Ok(json!({"session_date": session_date, "session_end_minute": minute, "finalized": true}))
    }

    #[allow(clippy::too_many_lines)]
    fn process_minute_inner(
        &mut self,
        minute: i64,
        mut candidates: Vec<Candidate>,
        force_derisk: bool,
        session_date: Option<&str>,
    ) -> Result<(Value, Vec<TradeIntent>), String> {
        // One immutable event stream can drive isolated books and the shared
        // portfolio. Candidates for fixed books outside this runner's config
        // are intentionally outside its universe, not malformed inputs.
        candidates.retain(|candidate| self.books.contains_key(&candidate.book));
        for candidate in &candidates {
            self.validate_candidate(candidate, minute)?;
        }
        let mut candidate_ids = BTreeSet::new();
        for candidate in &candidates {
            if !candidate_ids.insert(&candidate.candidate_id) {
                return Err("duplicate candidate_id in minute packet".to_owned());
            }
        }
        if self
            .books
            .values()
            .any(|state| state.last_minute.is_some_and(|last| minute <= last))
        {
            return Err("minute packets must be strictly chronological".to_owned());
        }
        let mut actions = Vec::new();
        // The authorized exit policy is next valid native close during the
        // same session only. If a filled position survives into a later date,
        // retain it as unresolved and halt that family rather than using a
        // future-session quote or silently releasing occupancy.
        if let Some(current_date) = session_date {
            for state in self.books.values_mut() {
                let crossed_session = state
                    .active
                    .values()
                    .any(|position| position.date != current_date);
                if crossed_session && !state.halted {
                    state.halted = true;
                    state.counts.partial_halts = state.counts.partial_halts.saturating_add(1);
                    state.counts.inc_reason("unresolved_same_session_exit");
                }
            }
        }
        // Close scheduling runs before candidate admission. An unresolved or
        // rejected close leaves that position active; the next minute retries
        // it. Other positions in the same family remain independently live.
        for book in &self.config.books {
            let state = self
                .books
                .get_mut(book)
                .ok_or_else(|| format!("missing book {book}"))?;
            if state.halted {
                continue;
            }
            if force_derisk {
                state.counts.forced_derisk_minutes =
                    state.counts.forced_derisk_minutes.saturating_add(1);
            }
            let due_positions: Vec<Position> = state
                .active
                .values()
                .filter(|position| {
                    (force_derisk || minute >= position.scheduled_exit_minute)
                        && !state.pending.contains_key(&position.strategy_position_id)
                })
                .cloned()
                .collect();
            for position in due_positions {
                let intent = Self::close_intent(&position, minute);
                let intent_id = intent.intent_id.clone();
                state.pending.insert(
                    position.strategy_position_id.clone(),
                    PendingAction::Close {
                        intent_id,
                        position,
                    },
                );
                state.counts.close_intents = state.counts.close_intents.saturating_add(1);
                if force_derisk {
                    state.counts.forced_derisk_close_intents =
                        state.counts.forced_derisk_close_intents.saturating_add(1);
                }
                actions.push(intent);
            }
        }
        candidates.sort_by(|left, right| {
            (
                std::cmp::Reverse(candidate_priority(left)),
                book_rank(&left.book),
                &left.contract_id,
                &left.candidate_id,
                &left.scenario,
            )
                .cmp(&(
                    std::cmp::Reverse(candidate_priority(right)),
                    book_rank(&right.book),
                    &right.contract_id,
                    &right.candidate_id,
                    &right.scenario,
                ))
        });
        for candidate in candidates {
            let book_halted = {
                let state = self
                    .books
                    .get(&candidate.book)
                    .ok_or_else(|| "missing book".to_owned())?;
                state.halted
            };
            let reason = if force_derisk {
                Some("portfolio_derisk")
            } else if !candidate.entry_eligible {
                Some("entry_ineligible")
            } else if let Some(reason) =
                feature_rejection_for(self.config.admission_mode, &candidate)
            {
                Some(reason)
            } else if let Some(reason) = score_rejection(self.config.admission_mode, &candidate) {
                Some(reason)
            } else if book_halted {
                Some("book_halted")
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
            let position = Self::position_for(&candidate, self.config.admission_mode)?;
            let intent = Self::open_intent(&position);
            let intent_id = intent.intent_id.clone();
            state.pending.insert(
                position.strategy_position_id.clone(),
                PendingAction::Open {
                    intent_id,
                    position,
                },
            );
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
        let snapshot = self.clone();
        let force_derisk = feedback.blockers.iter().any(|blocker| {
            blocker.contains("risk limit: held margin utilization exceeds")
                || blocker.contains("risk limit: held total capital requirement exceeds")
        });
        let result = self
            .apply_feedback(feedback)
            .and_then(|()| self.process_minute_inner(minute, candidates, force_derisk, None));
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

    fn restore_state(&mut self, state: &Value, sequence: u64) -> Result<(), String> {
        let persisted = Self::parse_state(state)?;
        if self.initialized {
            let Some(persisted) = persisted else {
                return Err("state required after first request".to_owned());
            };
            self.validate_persisted(&persisted)?;
            if persisted.next_sequence != self.sequence || sequence != self.sequence {
                return Err("state/request sequence mismatch".to_owned());
            }
            if persisted.books != self.books
                || persisted.totals != self.totals
                || persisted.halted != self.halted
                || persisted.last_feedback_sequence != self.last_feedback_sequence
            {
                return Err("persisted state differs from live runner state".to_owned());
            }
        } else if let Some(persisted) = persisted {
            self.validate_persisted(&persisted)?;
            if persisted.next_sequence != sequence {
                return Err("restored sequence mismatch".to_owned());
            }
            self.sequence = persisted.next_sequence;
            self.books = persisted.books;
            self.totals = persisted.totals;
            self.halted = persisted.halted;
            self.last_feedback_sequence = persisted.last_feedback_sequence;
            self.initialized = true;
        } else {
            if sequence != 0 {
                return Err("fresh runner requires sequence zero".to_owned());
            }
            self.initialized = true;
        }
        Ok(())
    }

    fn validate_persisted(&self, persisted: &PersistedState) -> Result<(), String> {
        if persisted.schema_version != SCHEMA_VERSION || persisted.config != self.config {
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
            for (position_id, position) in &state.active {
                if position_id != &position.strategy_position_id
                    || position.book != *book
                    || position.strategy_position_id.is_empty()
                    || position.entry_minute >= position.scheduled_exit_minute
                    || position.legs.is_empty()
                {
                    return Err(format!("invalid active position in {book}"));
                }
            }
            for (position_id, pending) in &state.pending {
                let position = match pending {
                    PendingAction::Open { position, .. }
                    | PendingAction::Close { position, .. } => position,
                };
                if position_id != &position.strategy_position_id
                    || position.book != *book
                    || position.legs.is_empty()
                {
                    return Err(format!("invalid pending position in {book}"));
                }
            }
            // A same-session exit gap halts only the affected family. Atomic
            // partial fills still set both the book and global halt flags in
            // apply_outcome; unresolved close coverage is deliberately local.
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
            .restore_state(&request.state, request.sequence)
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
                        ..
                    } => {
                        let force_derisk = request.feedback.blockers.iter().any(|blocker| {
                            blocker.contains("risk limit: held margin utilization exceeds")
                                || blocker
                                    .contains("risk limit: held total capital requirement exceeds")
                        });
                        self.process_minute_inner(
                            minute,
                            candidates,
                            force_derisk,
                            Some(&session_date),
                        )?
                    }
                    Packet::SessionEnd {
                        session_date,
                        session_end_minute,
                    } => (
                        self.finalize_session(&session_date, session_end_minute)?,
                        Vec::new(),
                    ),
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
        margin: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use backtest_contracts::{ExecutionOutcome, Fill, OutcomeStatus, SealedEvent};

    fn candidate(id: &str, book: &str, minute: i64, mut legs: Vec<Leg>) -> Candidate {
        let book = match book {
            "H3_F4" => "R2_BASE_1X",
            "H5_F1" => "R2_IV30_1X",
            "H5_F2" => "R2_GE_1X",
            "H5_F3" => "R2_TIER_IV30",
            "H5_F5" => "R2_TIER_GE",
            value => value,
        };
        let mut features = BTreeMap::from([
            ("exp040_gate".to_owned(), Some(1.0)),
            ("spot_return_30m_pct".to_owned(), Some(-1.0)),
            (
                "structure_event_volume_min".to_owned(),
                Some(if book == "R2_POS_DTE45_1X" { 1.0 } else { 0.0 }),
            ),
            ("shock_iv_change_30m_pp".to_owned(), Some(1.0)),
            ("basket_event_gamma".to_owned(), Some(0.0)),
            ("momentum_efficiency30".to_owned(), Some(1.0)),
            ("shock_calendar_dte".to_owned(), Some(60.0)),
        ]);
        for (name, median) in score_model()
            .features
            .iter()
            .zip(&score_model().transform.median)
        {
            features.entry(name.clone()).or_insert(Some(*median));
        }
        for leg in &mut legs {
            leg.expiry_minute = minute + 10_000;
        }
        Candidate {
            candidate_id: id.to_owned(),
            book: book.to_owned(),
            scenario: "baseline".to_owned(),
            structure_id: "single_leg".to_owned(),
            source_event_id: format!("source-{id}"),
            source_side: "CE".to_owned(),
            source_raw_sign: 1,
            source_surface: "coherent".to_owned(),
            inherited_abs_z: 2.0,
            date: "2026-01-02".to_owned(),
            event_minute: minute - 1,
            entry_minute: minute,
            scheduled_exit_minute: minute + if book == "R2_SCORE_TIER_2X" { 30 } else { 10 },
            contract_id: legs[0].contract_id.clone(),
            quantity: legs[0].quantity,
            official_lot_size: legs[0].quantity,
            lot_authority: "fixture-contract-master@sha256:test".to_owned(),
            event_atm_strike: 20_000,
            recipient_expiry: "2026-02-26".to_owned(),
            recipient_expiry_minute: minute + 10_000,
            recipient_dte: 31,
            represented_same_expiry_strikes: vec![20_250],
            entry_eligible: true,
            rank_score_micro: 0,
            features,
            legs,
        }
    }

    #[test]
    fn causal_feature_gates_are_family_specific_and_fail_closed() {
        let legs = vec![leg("c", Side::Sell, 1)];
        let mut base = candidate("base", "H3_F4", 10, legs.clone());
        assert_eq!(feature_rejection(&base), None);
        base.features
            .insert("spot_return_30m_pct".to_owned(), Some(0.1));
        assert_eq!(feature_rejection(&base), Some("causal_feature_gate"));

        let mut iv30 = candidate("iv30", "H5_F1", 10, legs.clone());
        iv30.features
            .insert("shock_iv_change_30m_pp".to_owned(), Some(0.0));
        assert_eq!(feature_rejection(&iv30), Some("causal_feature_gate"));

        let mut ge = candidate("ge", "H5_F2", 10, legs);
        ge.features
            .insert("momentum_efficiency30".to_owned(), Some(0.0));
        assert_eq!(feature_rejection(&ge), Some("causal_feature_gate"));
    }

    #[test]
    fn promoted_r2_core_geometry_is_enforced() {
        let config = Config::from_json_str(
            r#"{"books":["R2_SCORE_TIER_2X"],"scenarios":["baseline"],"admission_mode":"family_specific"}"#,
        )
        .unwrap();
        let runner = Runner::new(config);
        let valid = candidate(
            "core",
            "R2_SCORE_TIER_2X",
            10,
            vec![leg("NIFTY-20260226-20250-CE", Side::Sell, 25)],
        );
        runner
            .validate_candidate(&valid, 10)
            .expect("promoted R2 geometry passes");
        let mut invalid = valid;
        invalid.legs[0].strike = 20_600;
        assert!(runner.validate_candidate(&invalid, 10).is_err());
    }

    #[test]
    fn missing_session_date_fails_before_state_changes() {
        let mut request = request(0, 10, Value::Null, empty_feedback(0), Vec::new());
        request.input.research_payload["session_date"] = Value::Null;
        let mut runner = Runner::default();
        assert!(runner.process_request(request, "bundle").is_err());
        assert_eq!(runner.sequence(), 0);
    }

    #[test]
    fn session_end_quarantines_an_unresolved_position() {
        let mut runner = Runner::default();
        let original = candidate("open", "H3_F4", 10, vec![leg("CE", Side::Sell, 25)]);
        let position = Runner::position(&original).unwrap();
        runner
            .books
            .get_mut("R2_BASE_1X")
            .unwrap()
            .active
            .insert(position.strategy_position_id.clone(), position);
        runner.finalize_session("2026-01-02", 1_000).unwrap();
        let state = &runner.books["R2_BASE_1X"];
        assert!(state.halted);
        assert_eq!(state.counts.reason_counts["unresolved_at_session_end"], 1);
    }

    #[test]
    fn session_end_rejects_a_date_different_from_owned_positions() {
        let mut runner = Runner::default();
        let original = candidate("open", "H3_F4", 10, vec![leg("CE", Side::Sell, 25)]);
        let position = Runner::position(&original).unwrap();
        runner
            .books
            .get_mut("R2_BASE_1X")
            .unwrap()
            .active
            .insert(position.strategy_position_id.clone(), position);
        assert!(runner.finalize_session("2026-01-03", 1_000).is_err());
    }

    #[test]
    fn serialized_session_end_is_processed_by_the_runner() {
        let mut request = request(0, 1_000, Value::Null, empty_feedback(0), Vec::new());
        request.input.research_payload = serde_json::to_value(Packet::SessionEnd {
            session_date: "2026-01-02".to_owned(),
            session_end_minute: 1_000,
        })
        .unwrap();
        let response = Runner::default()
            .process_request(request, "bundle")
            .expect("session end packet succeeds");
        assert_eq!(response.state["detail"]["finalized"], true);
        assert!(response.actions.is_empty());
    }

    #[test]
    fn sizing_is_owned_by_rust_and_uses_only_event_known_features() {
        let legs = vec![leg("c", Side::Sell, 50)];
        let fixed = candidate("fixed", "R2_IV30_2X", 10, legs.clone());
        assert_eq!(Runner::position(&fixed).unwrap().legs[0].quantity, 100);

        let mut tiered = candidate("tiered", "R2_TIER_GE", 10, legs);
        assert_eq!(Runner::position(&tiered).unwrap().legs[0].quantity, 100);
        tiered
            .features
            .insert("momentum_efficiency30".to_owned(), Some(0.0));
        assert_eq!(Runner::position(&tiered).unwrap().legs[0].quantity, 50);
    }

    #[test]
    fn expanded_gate_adds_only_positive_volume_far_dte_events() {
        let legs = vec![leg("c", Side::Sell, 50)];
        let mut expanded = candidate("expanded", "R2_EXPANDED_1X", 10, legs.clone());
        expanded
            .features
            .insert("structure_event_volume_min".to_owned(), Some(1.0));
        assert_eq!(feature_rejection(&expanded), None);
        expanded
            .features
            .insert("shock_calendar_dte".to_owned(), Some(45.0));
        assert_eq!(feature_rejection(&expanded), Some("causal_feature_gate"));

        let tiered = candidate("tiered", "R2_EXPANDED_IVTIER", 10, legs);
        assert_eq!(Runner::position(&tiered).unwrap().legs[0].quantity, 100);
    }

    #[test]
    fn embedded_score_matches_python_parity_fixtures_and_tiers() {
        #[derive(Deserialize)]
        struct Fixture {
            features: BTreeMap<String, Option<f64>>,
            expected_score: f64,
            expected_bin: u8,
        }
        #[derive(Deserialize)]
        struct FixtureFile {
            parity_fixtures: Vec<Fixture>,
        }

        let fixtures: FixtureFile = serde_json::from_str(include_str!("../MODEL.json")).unwrap();
        assert_eq!(fixtures.parity_fixtures.len(), 5);
        for fixture in fixtures.parity_fixtures {
            let mut value = candidate(
                "score-parity",
                "R2_SCORE_GATE_1X",
                10,
                vec![leg("c", Side::Sell, 50)],
            );
            value.features = fixture.features;
            value.features.insert("exp040_gate".to_owned(), Some(1.0));
            assert!((tail_score(&value) - fixture.expected_score).abs() < 1e-9);
            assert_eq!(tail_score_bin(&value), fixture.expected_bin);

            value.book = "R2_SCORE_TIER_2X".to_owned();
            let expected_2x = if fixture.expected_bin >= 4 { 2 } else { 1 };
            assert_eq!(quantity_multiplier(&value), expected_2x);
            value.book = "R2_SCORE_TIER_3X".to_owned();
            let expected_3x = match fixture.expected_bin {
                5 => 3,
                4 => 2,
                _ => 1,
            };
            assert_eq!(quantity_multiplier(&value), expected_3x);
        }
    }

    #[test]
    fn rho_priority_keeps_q1_rejected_and_promotes_only_low_rho_q2() {
        #[derive(Deserialize)]
        struct Fixture {
            features: BTreeMap<String, Option<f64>>,
            expected_bin: u8,
        }
        #[derive(Deserialize)]
        struct FixtureFile {
            parity_fixtures: Vec<Fixture>,
        }
        let fixtures: FixtureFile = serde_json::from_str(include_str!("../MODEL.json")).unwrap();
        let q1_fixture = fixtures
            .parity_fixtures
            .into_iter()
            .find(|value| value.expected_bin == 1)
            .unwrap();
        let mut value = candidate(
            "rho-q1",
            "R2_SCORE_Q2_RHO_PRIORITY_1X",
            10,
            vec![leg("c", Side::Sell, 50)],
        );
        value.features = q1_fixture.features;
        value.features.insert("exp040_gate".to_owned(), Some(1.0));
        value
            .features
            .insert("rho_per_premium".to_owned(), Some(RHO_PREMIUM_LOW_MAX));
        assert_eq!(feature_rejection(&value), Some("tail_score_q1"));
        assert!(is_low_rho(&value));
        value.features.insert("rho_per_premium".to_owned(), None);
        assert!(!is_low_rho(&value));
    }

    #[test]
    fn rho_priority_variant_b_doubles_only_low_rho_q2() {
        let fixture: Value = serde_json::from_str(include_str!("../MODEL.json")).unwrap();
        let features = fixture["parity_fixtures"]
            .as_array()
            .unwrap()
            .iter()
            .find(|row| row["expected_bin"] == 2)
            .unwrap()["features"]
            .clone();
        let mut value = candidate(
            "rho-q2-2x",
            "R2_SCORE_Q2_RHO_PRIORITY_2X",
            10,
            vec![leg("c", Side::Sell, 50)],
        );
        value.features = serde_json::from_value(features).unwrap();
        value.features.insert("exp040_gate".to_owned(), Some(1.0));
        value
            .features
            .insert("rho_per_premium".to_owned(), Some(RHO_PREMIUM_LOW_MAX));
        assert_eq!(tail_score_bin(&value), 2);
        assert_eq!(quantity_multiplier(&value), 2);

        value.features.insert(
            "rho_per_premium".to_owned(),
            Some(RHO_PREMIUM_LOW_MAX + 1e-12),
        );
        assert!(!is_low_rho(&value));
        assert_eq!(quantity_multiplier(&value), 1);
        value.features.insert("rho_per_premium".to_owned(), None);
        assert!(!is_low_rho(&value));
        assert_eq!(quantity_multiplier(&value), 1);
    }

    #[test]
    fn score_only_admits_non_q1_even_when_inherited_gate_would_reject() {
        let fixture: Value = serde_json::from_str(include_str!("../MODEL.json")).unwrap();
        let features = fixture["parity_fixtures"]
            .as_array()
            .unwrap()
            .iter()
            .find(|row| row["expected_bin"] == 2)
            .unwrap()["features"]
            .clone();
        let mut value = candidate(
            "score-only-q2",
            "R2_SCORE_TIER_2X",
            10,
            vec![leg("c", Side::Sell, 50)],
        );
        value.features = serde_json::from_value(features).unwrap();
        value.features.insert("exp040_gate".to_owned(), Some(1.0));
        value
            .features
            .insert("spot_return_30m_pct".to_owned(), Some(1e-6));
        assert!(tail_score_bin(&value) >= 2);
        assert_eq!(feature_rejection(&value), Some("causal_feature_gate"));
        assert_eq!(
            feature_rejection_for(AdmissionMode::ScoreOnlyGate1x, &value),
            None
        );
        assert_eq!(
            score_rejection(AdmissionMode::ScoreOnlyGate1x, &value),
            None
        );
        assert_eq!(
            quantity_multiplier_for(AdmissionMode::ScoreOnlyGate1x, &value),
            1
        );
        assert_eq!(
            quantity_multiplier_for(AdmissionMode::ScoreOnlyTier2x, &value),
            1
        );
    }

    #[test]
    fn score_only_rejects_q1_and_handles_missing_features_with_frozen_model() {
        let fixture: Value = serde_json::from_str(include_str!("../MODEL.json")).unwrap();
        let features = fixture["parity_fixtures"]
            .as_array()
            .unwrap()
            .iter()
            .find(|row| row["expected_bin"] == 1)
            .unwrap()["features"]
            .clone();
        let mut q1 = candidate(
            "score-only-q1",
            "R2_SCORE_TIER_2X",
            10,
            vec![leg("c", Side::Sell, 50)],
        );
        q1.features = serde_json::from_value(features).unwrap();
        q1.features.insert("exp040_gate".to_owned(), Some(1.0));
        assert_eq!(tail_score_bin(&q1), 1);
        assert_eq!(
            feature_rejection_for(AdmissionMode::ScoreOnlyGate1x, &q1),
            None
        );
        assert_eq!(
            score_rejection(AdmissionMode::ScoreOnlyGate1x, &q1),
            Some("tail_score_q1")
        );
        for name in score_model().features.iter().take(2) {
            q1.features.insert(name.clone(), None);
        }
        assert!(tail_score(&q1).is_finite());
        assert!(tail_score_bin(&q1) >= 1);
    }

    #[test]
    fn score_only_tier_doubles_only_frozen_q4_q5() {
        let fixture: Value = serde_json::from_str(include_str!("../MODEL.json")).unwrap();
        for expected_bin in [2, 3, 4, 5] {
            let features = fixture["parity_fixtures"]
                .as_array()
                .unwrap()
                .iter()
                .find(|row| row["expected_bin"] == expected_bin)
                .unwrap()["features"]
                .clone();
            let mut value = candidate(
                &format!("score-only-{expected_bin}"),
                "R2_SCORE_TIER_2X",
                10,
                vec![leg("c", Side::Sell, 50)],
            );
            value.features = serde_json::from_value(features).unwrap();
            value.features.insert("exp040_gate".to_owned(), Some(1.0));
            assert_eq!(
                quantity_multiplier_for(AdmissionMode::ScoreOnlyTier2x, &value),
                if expected_bin >= 4 { 2 } else { 1 }
            );
        }
    }

    #[test]
    fn low_z_lane_uses_the_same_q1_gate_and_q4_q5_sizing_as_core() {
        let fixture: Value = serde_json::from_str(include_str!("../MODEL.json")).unwrap();
        for (expected_bin, expected_rejection, expected_multiplier) in
            [(1, Some("tail_score_q1"), 1), (4, None, 2)]
        {
            let features = fixture["parity_fixtures"]
                .as_array()
                .unwrap()
                .iter()
                .find(|row| row["expected_bin"] == expected_bin)
                .unwrap()["features"]
                .clone();
            let mut value = candidate(
                &format!("low-z-{expected_bin}"),
                "R2_LOWZ_SCORE_TIER_2X",
                10,
                vec![leg("c", Side::Sell, 50)],
            );
            value.features = serde_json::from_value(features).unwrap();
            value.features.insert("exp040_gate".to_owned(), Some(1.0));
            assert_eq!(feature_rejection(&value), expected_rejection);
            assert_eq!(quantity_multiplier(&value), expected_multiplier);
        }
    }

    #[allow(clippy::cast_possible_truncation, clippy::cast_precision_loss)]
    #[test]
    fn promoted_q2_priority_spans_q4_without_crossing_q3_or_q5() {
        let edges = &score_model().score_quintile_edges;
        let q2_low = (edges[0] * 1_000_000.0).ceil() as i64 + 1;
        let q2_high = (edges[1] * 1_000_000.0).floor() as i64;
        let q3_high = (edges[2] * 1_000_000.0).floor() as i64;
        let q5_low = (edges[3] * 1_000_000.0).ceil() as i64 + 1;
        assert!(promoted_q2_priority(q2_low) > q3_high);
        assert!(promoted_q2_priority(q2_high) < q5_low);
        assert!(promoted_q2_priority(q2_low) < promoted_q2_priority(q2_high));
    }

    #[test]
    fn low_rho_q2_is_deterministically_ahead_of_ordinary_q2() {
        let fixture: Value = serde_json::from_str(include_str!("../MODEL.json")).unwrap();
        let rows = fixture["parity_fixtures"].as_array().unwrap();
        let q2 = rows.iter().find(|row| row["expected_bin"] == 2).unwrap();
        let q3 = rows.iter().find(|row| row["expected_bin"] == 3).unwrap();
        let mut low = candidate(
            "q2-low",
            "R2_SCORE_Q2_RHO_PRIORITY_1X",
            10,
            vec![leg("z", Side::Sell, 1)],
        );
        low.features = serde_json::from_value(q2["features"].clone()).unwrap();
        low.features.insert("exp040_gate".to_owned(), Some(1.0));
        low.features
            .insert("rho_per_premium".to_owned(), Some(RHO_PREMIUM_LOW_MAX));
        let mut ordinary = candidate(
            "q2-ordinary",
            "R2_SCORE_Q2_RHO_PRIORITY_1X",
            10,
            vec![leg("a", Side::Sell, 1)],
        );
        ordinary.features = serde_json::from_value(q3["features"].clone()).unwrap();
        ordinary
            .features
            .insert("exp040_gate".to_owned(), Some(1.0));
        assert_eq!(tail_score_bin(&low), 2);
        assert_eq!(tail_score_bin(&ordinary), 3);
        assert!(candidate_priority(&low) > candidate_priority(&ordinary));
    }

    #[test]
    fn unresolved_position_halts_its_book_across_session_boundary() {
        let mut runner = Runner::default();
        let original = candidate("old", "H3_F4", 100, vec![leg("A", Side::Sell, 1)]);
        let position = Runner::position(&original).unwrap();
        runner
            .books
            .get_mut("R2_BASE_1X")
            .expect("book")
            .active
            .insert(position.strategy_position_id.clone(), position);
        let next = candidate("next", "H3_F4", 200, vec![leg("B", Side::Sell, 1)]);
        let (_, actions) = runner
            .process_minute_inner(200, vec![next], false, Some("2026-01-03"))
            .expect("cross-session handling");
        let state = runner.books.get("R2_BASE_1X").expect("book");
        assert!(state.halted);
        assert!(actions.is_empty());
        assert_eq!(
            state
                .counts
                .reason_counts
                .get("unresolved_same_session_exit"),
            Some(&1)
        );
        runner
            .validate_persisted(&runner.persisted_state())
            .expect("book-local unresolved halt persists safely");
    }

    #[test]
    fn universal_gate_rejects_zero_and_negative_scores_only() {
        let legs = vec![leg("c", Side::Buy, 1)];
        let mut zero = candidate("zero", "H5_F1", 10, legs);
        assert_eq!(
            score_rejection(AdmissionMode::PositiveScore, &zero),
            Some("score_nonpositive")
        );
        zero.rank_score_micro = -1;
        assert_eq!(
            score_rejection(AdmissionMode::PositiveScore, &zero),
            Some("score_nonpositive")
        );
        zero.rank_score_micro = 1;
        assert_eq!(score_rejection(AdmissionMode::PositiveScore, &zero), None);
    }

    #[test]
    fn family_gate_is_hard_for_marginal_h5_and_soft_for_h3_f4_h5_f1() {
        let legs = vec![leg("c", Side::Buy, 1)];
        for book in ["H5_F2", "H5_F3", "H5_F5"] {
            let low = candidate(book, book, 10, legs.clone());
            assert_eq!(
                score_rejection(AdmissionMode::FamilySpecific, &low),
                Some("score_nonpositive_family_gate")
            );
        }
        for book in ["H3_F4", "H5_F1"] {
            let low = candidate(book, book, 10, legs.clone());
            assert_eq!(score_rejection(AdmissionMode::FamilySpecific, &low), None);
        }
    }

    #[test]
    fn configured_score_gate_changes_actions_and_records_the_rejection() {
        let mut config = Config::baseline();
        config.admission_mode = AdmissionMode::PositiveScore;
        let mut runner = Runner::new(config);
        let low = candidate("low", "H5_F3", 10, vec![leg("c", Side::Buy, 1)]);
        let (_, actions) = runner
            .process_minute(10, vec![low], &empty_feedback(0))
            .expect("score rejection");
        assert!(actions.is_empty());
        assert_eq!(runner.totals.attempts, 1);
        assert_eq!(runner.totals.rejected, 1);
        assert_eq!(
            runner.totals.reason_counts.get("score_nonpositive"),
            Some(&1)
        );
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
            option_type: "CE".to_owned(),
            strike: 20_250,
            expiry: "2026-02-26".to_owned(),
            expiry_minute: 10_010,
            represented: true,
        }
    }

    #[test]
    fn margin_rejected_open_releases_book_for_next_candidate() {
        let mut runner = Runner::default();
        let (_, first) = runner
            .process_minute(
                100,
                vec![candidate("a", "H3_F4", 100, vec![leg("A", Side::Sell, 1)])],
                &empty_feedback(0),
            )
            .expect("open");
        let reject = feedback(1, vec![outcome(&first[0], OutcomeStatus::Rejected)]);
        let (_, second) = runner
            .process_minute(
                101,
                vec![candidate("b", "H3_F4", 101, vec![leg("B", Side::Sell, 1)])],
                &reject,
            )
            .expect("retry");
        assert_eq!(second.len(), 1);
        assert!(second[0].intent_id.contains('b'));
    }

    #[test]
    fn multi_leg_open_and_reverse_close_are_atomic() {
        let mut runner = Runner::default();
        let (_, open) = runner
            .process_minute(
                100,
                vec![candidate(
                    "a",
                    "H3_F4",
                    100,
                    vec![leg("NEAR", Side::Buy, 2), leg("FAR", Side::Sell, 1)],
                )],
                &empty_feedback(0),
            )
            .expect("open");
        assert!(open[0].atomic);
        let filled = feedback(1, vec![outcome(&open[0], OutcomeStatus::Filled)]);
        let (_, close) = runner
            .process_minute(110, Vec::new(), &filled)
            .expect("close");
        assert_eq!(close.len(), 1);
        assert_eq!(close[0].action, IntentAction::Close);
        assert_eq!(close[0].legs[0].side, Side::Sell);
        assert_eq!(close[0].legs[1].side, Side::Buy);
    }

    #[test]
    fn rejected_close_retries_next_minute_without_new_open() {
        let mut runner = Runner::default();
        let (_, open) = runner
            .process_minute(
                100,
                vec![candidate("a", "H3_F4", 100, vec![leg("A", Side::Sell, 1)])],
                &empty_feedback(0),
            )
            .expect("open");
        let filled = feedback(1, vec![outcome(&open[0], OutcomeStatus::Filled)]);
        let (_, close) = runner
            .process_minute(110, Vec::new(), &filled)
            .expect("close");
        let rejected = feedback(2, vec![outcome(&close[0], OutcomeStatus::Rejected)]);
        let (_, retry) = runner
            .process_minute(111, Vec::new(), &rejected)
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
                vec![candidate("a", "H3_F4", 100, vec![leg("A", Side::Sell, 1)])],
                &empty_feedback(0),
            )
            .expect("open");
        let filled = feedback(1, vec![outcome(&open[0], OutcomeStatus::Filled)]);
        let (_, close) = runner
            .process_minute(110, Vec::new(), &filled)
            .expect("close");
        assert_eq!(close.len(), 1);
        let close_filled = feedback(2, vec![outcome(&close[0], OutcomeStatus::Filled)]);
        let (_, reentry) = runner
            .process_minute(
                111,
                vec![candidate("b", "H3_F4", 111, vec![leg("B", Side::Sell, 1)])],
                &close_filled,
            )
            .expect("reentry");
        assert_eq!(reentry.len(), 1);
        assert!(reentry[0].intent_id.contains('b'));
    }

    #[test]
    fn simultaneous_candidates_use_causal_score_then_deterministic_order() {
        let mut runner = Runner::default();
        let mut highest = candidate("h5", "H5_F2", 100, vec![leg("H", Side::Buy, 1)]);
        highest.rank_score_micro = 10;
        let (_, actions) = runner
            .process_minute(
                100,
                vec![
                    candidate("z", "H3_F4", 100, vec![leg("Z", Side::Sell, 1)]),
                    candidate("a", "H3_F4", 100, vec![leg("A", Side::Sell, 1)]),
                    highest,
                ],
                &empty_feedback(0),
            )
            .expect("ordered");
        assert_eq!(actions.len(), 3);
        assert!(actions[0].intent_id.contains("h5"));
        assert!(actions[1].intent_id.contains('a'));
        assert!(actions[2].intent_id.contains('z'));
    }

    #[test]
    fn overlapping_same_family_positions_have_independent_feedback() {
        let mut runner = Runner::default();
        let (_, opens) = runner
            .process_minute(
                100,
                vec![
                    candidate("a", "H3_F4", 100, vec![leg("A", Side::Sell, 1)]),
                    candidate("b", "H3_F4", 100, vec![leg("B", Side::Sell, 1)]),
                ],
                &empty_feedback(0),
            )
            .expect("overlapping opens");
        assert_eq!(opens.len(), 2);
        assert_ne!(opens[0].strategy_position_id, opens[1].strategy_position_id);
        let filled = feedback(
            1,
            opens
                .iter()
                .map(|intent| outcome(intent, OutcomeStatus::Filled))
                .collect(),
        );
        let (_, closes) = runner
            .process_minute(110, Vec::new(), &filled)
            .expect("independent closes");
        assert_eq!(closes.len(), 2);
        let rejected = feedback(2, vec![outcome(&closes[0], OutcomeStatus::Rejected)]);
        let (_, retry) = runner
            .process_minute(111, Vec::new(), &rejected)
            .expect("first close retry");
        assert_eq!(retry.len(), 1);
        assert_eq!(
            retry[0].strategy_position_id,
            closes[0].strategy_position_id
        );
        let filled_both = feedback(
            2,
            vec![
                outcome(&retry[0], OutcomeStatus::Filled),
                outcome(&closes[1], OutcomeStatus::Filled),
            ],
        );
        // Both close outcomes bind independently, even though one sibling
        // close was retried.
        let (_, no_action) = runner
            .process_minute(112, Vec::new(), &filled_both)
            .expect("second close feedback");
        assert!(no_action.is_empty());
    }

    #[test]
    fn same_minute_close_is_emitted_before_new_open() {
        let mut runner = Runner::default();
        let (_, open) = runner
            .process_minute(
                100,
                vec![candidate("a", "H3_F4", 100, vec![leg("A", Side::Sell, 1)])],
                &empty_feedback(0),
            )
            .expect("open");
        let filled = feedback(1, vec![outcome(&open[0], OutcomeStatus::Filled)]);
        let (_, actions) = runner
            .process_minute(
                110,
                vec![candidate("b", "H3_F4", 110, vec![leg("B", Side::Sell, 1)])],
                &filled,
            )
            .expect("close then open");
        assert_eq!(actions.len(), 2);
        assert_eq!(actions[0].action, IntentAction::Close);
        assert_eq!(actions[1].action, IntentAction::Open);
        assert_eq!(
            actions[0].strategy_position_id,
            open[0].strategy_position_id
        );
        assert!(actions[1].strategy_position_id.contains("|b"));
    }

    #[test]
    fn held_margin_breach_flattens_early_and_suppresses_new_entries() {
        let mut runner = Runner::default();
        let (_, open) = runner
            .process_minute(
                100,
                vec![candidate("a", "H3_F4", 100, vec![leg("A", Side::Sell, 1)])],
                &empty_feedback(0),
            )
            .expect("open");
        let mut breached = feedback(1, vec![outcome(&open[0], OutcomeStatus::Filled)]);
        breached
            .blockers
            .push("risk limit: held margin utilization exceeds 9000 bps".to_owned());
        let (_, actions) = runner
            .process_minute(
                101,
                vec![candidate("b", "H3_F4", 101, vec![leg("B", Side::Sell, 1)])],
                &breached,
            )
            .expect("forced derisk");
        assert_eq!(actions.len(), 1);
        assert_eq!(actions[0].action, IntentAction::Close);
        assert_eq!(
            actions[0].strategy_position_id,
            open[0].strategy_position_id
        );
        let state = runner.books.get("R2_BASE_1X").expect("book");
        assert_eq!(state.counts.forced_derisk_close_intents, 1);
        assert_eq!(state.counts.reason_counts.get("portfolio_derisk"), Some(&1));
    }

    #[test]
    fn held_total_capital_breach_flattens_early_and_suppresses_new_entries() {
        let mut runner = Runner::default();
        let (_, open) = runner
            .process_minute(
                100,
                vec![candidate("a", "H3_F4", 100, vec![leg("A", Side::Sell, 1)])],
                &empty_feedback(0),
            )
            .expect("open");
        let mut breached = feedback(1, vec![outcome(&open[0], OutcomeStatus::Filled)]);
        breached
            .blockers
            .push("risk limit: held total capital requirement exceeds capital base".to_owned());
        let (_, actions) = runner
            .process_minute(
                101,
                vec![candidate("b", "H3_F4", 101, vec![leg("B", Side::Sell, 1)])],
                &breached,
            )
            .expect("forced derisk");
        assert_eq!(actions.len(), 1);
        assert_eq!(actions[0].action, IntentAction::Close);
        assert_eq!(
            actions[0].strategy_position_id,
            open[0].strategy_position_id
        );
    }

    #[test]
    fn configured_books_have_independent_occupancy() {
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
            .expect("configured books");
        assert_eq!(actions.len(), FIXED_BOOKS.len());
    }

    #[test]
    fn partial_fill_halts_all_future_admission() {
        let mut runner = Runner::default();
        let (_, open) = runner
            .process_minute(
                100,
                vec![candidate("a", "H3_F4", 100, vec![leg("A", Side::Sell, 1)])],
                &empty_feedback(0),
            )
            .expect("open");
        let partial = feedback(1, vec![outcome(&open[0], OutcomeStatus::PartiallyFilled)]);
        let (_, actions) = runner
            .process_minute(
                101,
                vec![candidate("b", "H5_F1", 101, vec![leg("B", Side::Sell, 1)])],
                &partial,
            )
            .expect("halt");
        assert!(actions.is_empty());
    }

    #[test]
    fn canonical_feedback_arrives_one_sequence_late() {
        let mut runner = Runner::default();
        let first_candidate = candidate("a", "H3_F4", 100, vec![leg("A", Side::Sell, 1)]);
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
}
