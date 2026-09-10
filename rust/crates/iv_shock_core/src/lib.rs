//! Causal, persistent six-detector IV-shock decision core for H5.
//!
//! The feeder owns the historical data and sends one ordered JSON request at a
//! time. This crate owns signal scoring and diagnostics; executable intents
//! are emitted only by the family/lifecycle policy crate.
//! it does not price fills, charge costs, reserve margin, or maintain an
//! account.  The default path emits diagnostics and no actions.

#![allow(
    clippy::cast_precision_loss,
    clippy::suboptimal_flops,
    clippy::unreadable_literal,
    clippy::too_long_first_doc_paragraph
)]

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fs;
use std::path::Path;

use backtest_contracts::{
    CONTRACT_VERSION, IntentAction, IntentLeg, Money, ResearchRequest, ResearchResponse, Side,
    TradeIntent,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;

pub const SCHEMA_VERSION: &str = "gdc.h5.iv-shocks.v1";
pub const RUNNER_ID: &str = "h5-iv-shocks-v1";
/// The feeder should provide a pinned bundle identity through
/// `IV_SHOCK_RESEARCH_BUNDLE_HASH` or `RunnerConfig`. The default is intentionally
/// explicit that no source/configuration pin has been bound yet.
pub const BUNDLE_HASH: &str = "UNBOUND";
pub const MINIMUM_SUPPORT: usize = 200;
pub const CALIBRATION_SESSIONS: usize = 60;
pub const QUIET_GAP_MINUTES: i64 = 15;
pub const S3_HALF_LIFE_MINUTES: f64 = 60.0;
pub const NEIGHBOR_RADIUS: f64 = 0.02;
pub const MINIMUM_NEIGHBORS: usize = 3;

const fn default_true() -> bool {
    true
}

const fn default_scoring() -> bool {
    true
}

/// The six detector identities used by the validation grid.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum Detector {
    S0,
    S1,
    S2,
    S3,
    S4,
    S5,
}

impl Detector {
    pub const ALL: [Self; 6] = [Self::S0, Self::S1, Self::S2, Self::S3, Self::S4, Self::S5];

    #[must_use]
    pub const fn id(self) -> &'static str {
        match self {
            Self::S0 => "S0",
            Self::S1 => "S1",
            Self::S2 => "S2",
            Self::S3 => "S3",
            Self::S4 => "S4",
            Self::S5 => "S5",
        }
    }

    #[must_use]
    pub const fn metric(self) -> Option<Metric> {
        match self {
            Self::S0 | Self::S1 | Self::S2 | Self::S3 => Some(Metric::Raw),
            Self::S4 => Some(Metric::Log),
            Self::S5 => Some(Metric::Residual),
        }
    }
}

/// The calibration series supplied by the feeder.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Metric {
    Raw,
    Log,
    Residual,
}

/// The explicit fallback keys used by the inherited calibration convention.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct GroupKeys {
    #[serde(default, alias = "L0")]
    pub l0: String,
    #[serde(default, alias = "L1")]
    pub l1: String,
    #[serde(default, alias = "L2")]
    pub l2: String,
    #[serde(default, alias = "L3")]
    pub l3: String,
}

impl GroupKeys {
    #[must_use]
    pub fn at(&self, level: usize) -> &str {
        match level {
            0 => &self.l0,
            1 => &self.l1,
            2 => &self.l2,
            _ => &self.l3,
        }
    }
}

/// Signal-time metadata from the inherited classifier.  Unknown metadata is
/// retained for auditability but is never used by the detector formulas.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SignalMetadata {
    #[serde(default)]
    pub raw_iv_sign: Option<i8>,
    #[serde(default)]
    pub inherited_sign: Option<i8>,
    #[serde(default)]
    pub original_score_sign: Option<i8>,
    #[serde(default, flatten)]
    pub extra: BTreeMap<String, Value>,
}

/// An eligible one-minute source event.  The feeder supplies all raw values;
/// the strategy never opens the canonical dataset itself.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RawEligibleEvent {
    pub contract_id: String,
    #[serde(alias = "delta_iv", alias = "raw_delta_iv")]
    pub raw_delta: f64,
    #[serde(default, alias = "log_delta_iv", alias = "log_calendar_iv_change")]
    pub log_delta: Option<f64>,
    #[serde(
        default,
        alias = "idiosyncratic_residual",
        alias = "leave_one_out_residual"
    )]
    pub residual: Option<f64>,
    #[serde(default)]
    pub calendar_iv: Option<f64>,
    #[serde(default)]
    pub previous_calendar_iv: Option<f64>,
    #[serde(default = "default_true")]
    pub calibration_sample: bool,
    #[serde(default)]
    pub group_keys: GroupKeys,
    #[serde(default, alias = "signalmetadata")]
    pub signal_metadata: SignalMetadata,
    #[serde(default)]
    pub neighbor_count: Option<usize>,
    #[serde(default)]
    pub intent: Option<IntentSpec>,
}

/// Optional decision description.  It is inert unless the minute packet sets
/// `emit_actions=true` and the requested detector passes its quiet gap.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IntentSpec {
    pub detector: Detector,
    pub action: IntentAction,
    pub side: Side,
    pub quantity: u64,
    pub instrument_id: Option<String>,
    pub limit_price: Option<Money>,
    pub basket_key: Option<String>,
    pub strategy_position_id: Option<String>,
    pub intent_id: Option<String>,
    pub decision_id: Option<String>,
    #[serde(default = "default_atomic")]
    pub atomic: bool,
}

const fn default_atomic() -> bool {
    true
}

fn default_bundle_hash() -> String {
    std::env::var("IV_SHOCK_RESEARCH_BUNDLE_HASH").unwrap_or_else(|_| BUNDLE_HASH.to_owned())
}

/// One compact packet in the persistent JSONL protocol.  `kind` also accepts
/// the legacy alias `type` so feeder adapters can migrate without changing
/// signal semantics.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct InputPacket {
    #[serde(default, alias = "type")]
    pub kind: String,
    #[serde(default)]
    pub session_date: Option<String>,
    #[serde(default, alias = "event_minute")]
    pub ts_minute: Option<i64>,
    #[serde(default = "default_scoring")]
    pub scoring: bool,
    #[serde(default)]
    pub events: Vec<RawEligibleEvent>,
    #[serde(default)]
    pub emit_actions: bool,
    #[serde(default)]
    pub emit_all_diagnostics: bool,
    /// A checkpoint/restore path for strategy-owned state only.  It is used
    /// only by the explicit `checkpoint` and `restore` packet kinds; ordinary
    /// minute packets never read or write files.
    #[serde(default, alias = "checkpoint_path", alias = "restore_path")]
    pub path: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct JsonlRequest {
    pub input: InputPacket,
    #[serde(default)]
    pub state: Value,
    #[serde(default)]
    pub feedback: Value,
    pub sequence: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct JsonlResponse {
    pub schema_version: String,
    pub artifact_consumed: bool,
    pub matching_sequence: u64,
    pub runner_id: String,
    pub bundle_hash: String,
    pub state: Value,
    pub actions: Vec<TradeIntent>,
    pub diagnostics: Vec<EventDiagnostic>,
    pub summary: DiagnosticSummary,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct DiagnosticSummary {
    pub detectors: Vec<DetectorSummary>,
    pub inventory: Vec<InventorySummary>,
}

#[derive(Debug, Clone, Serialize)]
pub struct DetectorSummary {
    pub detector: Detector,
    pub supported: usize,
    pub qualified_before_quiet: usize,
    pub qualified_after_quiet: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct InventorySummary {
    pub detector: Detector,
    pub raw_iv_sign: i8,
    pub report_group: String,
    pub reason: String,
    pub eligible: usize,
    pub supported: usize,
    pub threshold_supported: usize,
    pub qualified_before_quiet: usize,
    pub qualified_after_quiet: usize,
    pub sampled: usize,
}
type InventoryKey = (Detector, i8, String, String);
fn count_inventory(
    event: &RawEligibleEvent,
    diagnostic: &EventDiagnostic,
    rows: &mut BTreeMap<InventoryKey, InventorySummary>,
) {
    let group = event
        .signal_metadata
        .extra
        .get("report_group")
        .and_then(Value::as_str)
        .unwrap_or("UNSPECIFIED");
    for d in &diagnostic.detectors {
        let reason = d.reason.as_deref().unwrap_or("supported");
        let row = rows
            .entry((
                d.detector,
                diagnostic.raw_iv_sign,
                group.to_owned(),
                reason.to_owned(),
            ))
            .or_insert_with(|| InventorySummary {
                detector: d.detector,
                raw_iv_sign: diagnostic.raw_iv_sign,
                report_group: group.into(),
                reason: reason.into(),
                eligible: 0,
                supported: 0,
                threshold_supported: 0,
                qualified_before_quiet: 0,
                qualified_after_quiet: 0,
                sampled: 0,
            });
        row.eligible += 1;
        row.supported += usize::from(d.score.is_some());
        row.threshold_supported += usize::from(d.score.is_some() && d.threshold.is_some());
        row.qualified_before_quiet += usize::from(d.qualified_before_quiet);
        row.qualified_after_quiet += usize::from(d.qualified_after_quiet);
        row.sampled += usize::from(event.calibration_sample);
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct EventDiagnostic {
    pub contract_id: String,
    pub ts_minute: i64,
    pub raw_iv_sign: i8,
    pub inherited_sign: Option<i8>,
    pub detectors: Vec<DetectorDiagnostic>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct DetectorDiagnostic {
    pub detector: Detector,
    pub score: Option<f64>,
    pub threshold: Option<f64>,
    pub threshold_support: usize,
    pub threshold_target: usize,
    pub threshold_selected: usize,
    pub threshold_ties: usize,
    pub prior_absolute_tail_probability: Option<f64>,
    pub prior_tail_support: usize,
    pub empirical_two_sided_tail_probability: Option<f64>,
    pub support: usize,
    pub calibration_level: Option<String>,
    pub qualified_before_quiet: bool,
    pub qualified_after_quiet: bool,
    pub reason: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunnerConfig {
    pub minimum_support: usize,
    pub quiet_gap_minutes: i64,
    pub threshold: f64,
    pub s3_alpha: f64,
    pub neighbor_minimum: usize,
    pub history_sessions: usize,
    pub runner_id: String,
    pub bundle_hash: String,
    #[serde(default)]
    pub emit_all_diagnostics: bool,
}

impl Default for RunnerConfig {
    fn default() -> Self {
        Self {
            minimum_support: MINIMUM_SUPPORT,
            quiet_gap_minutes: QUIET_GAP_MINUTES,
            threshold: 2.0,
            s3_alpha: 1.0 - (-1.0 / S3_HALF_LIFE_MINUTES).exp2(),
            neighbor_minimum: MINIMUM_NEIGHBORS,
            history_sessions: CALIBRATION_SESSIONS,
            runner_id: RUNNER_ID.to_owned(),
            bundle_hash: default_bundle_hash(),
            emit_all_diagnostics: false,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunnerState {
    pub next_sequence: u64,
    pub config: RunnerConfig,
    /// Bundle hash that originally produced the most recently restored
    /// checkpoint, when it differs from this binary's active bundle.  This
    /// is provenance only; `config.bundle_hash` always identifies the active
    /// binary after a guarded restore.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checkpoint_source_bundle_hash: Option<String>,
    #[serde(default)]
    pub history: VecDeque<SessionRecord>,
    #[serde(default)]
    pub current_session: Option<SessionRecord>,
    #[serde(default)]
    pub s3_states: BTreeMap<String, EwmaState>,
    #[serde(default)]
    pub s3_attempted: BTreeSet<String>,
    #[serde(default)]
    pub s3_support: BTreeMap<String, usize>,
    #[serde(default)]
    pub last_qualified: BTreeMap<String, i64>,
    #[serde(default)]
    pub previous_prequential_sampled_scores: VecDeque<PrequentialScore>,
}

impl RunnerState {
    const fn new(config: RunnerConfig) -> Self {
        Self {
            next_sequence: 0,
            config,
            checkpoint_source_bundle_hash: None,
            history: VecDeque::new(),
            current_session: None,
            s3_states: BTreeMap::new(),
            s3_attempted: BTreeSet::new(),
            s3_support: BTreeMap::new(),
            last_qualified: BTreeMap::new(),
            previous_prequential_sampled_scores: VecDeque::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionRecord {
    #[serde(default)]
    pub last_minute: Option<i64>,
    pub session_date: String,
    #[serde(default)]
    pub scoring: bool,
    #[serde(default)]
    pub observations: Vec<CalibrationObservation>,
    /// All finite S3 eligible observations for this session, grouped by
    /// contract and kept in strictly increasing minute order.  Keeping the
    /// session date once at the record level avoids repeating it for every
    /// raw observation in the durable checkpoint.
    #[serde(default)]
    pub raw_by_contract: BTreeMap<String, Vec<(i64, f64)>>,
    /// Legacy checkpoint field.  It is accepted only to migrate old
    /// checkpoints into [`raw_by_contract`], then cleared before the state is
    /// used or written again.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub eligible_raw_history: Vec<RawHistoryObservation>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CalibrationObservation {
    pub session_date: String,
    pub ts_minute: i64,
    pub contract_id: String,
    pub raw_delta: f64,
    pub log_delta: Option<f64>,
    pub residual: Option<f64>,
    pub group_keys: GroupKeys,
}

/// All finite eligible raw innovations for S3.  This intentionally remains
/// separate from the deterministic 1/128 calibration sample used by S0/S1/
/// S2/S4/S5: S3's per-contract initialization uses every eligible prior raw
/// innovation in the preceding 60 sessions.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RawHistoryObservation {
    pub session_date: String,
    pub ts_minute: i64,
    pub contract_id: String,
    pub raw_delta: f64,
}

/// Transient append returned by event scoring.  Unlike the legacy checkpoint
/// representation this does not duplicate the session date.
#[derive(Debug, Clone)]
struct RawHistoryAppend {
    ts_minute: i64,
    contract_id: String,
    raw_delta: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EwmaState {
    pub mean: f64,
    pub variance: f64,
    pub updates: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PrequentialScore {
    pub session_date: String,
    pub event_id: String,
    pub calibration_key: String,
    pub detector: Detector,
    pub absolute_score: f64,
    pub raw_iv_sign: i8,
}

/// Lightweight response state.  Full calibration history stays in the
/// persistent runner and can be durably saved only by an explicit checkpoint
/// packet, so every minute response remains bounded.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct StateMetadata {
    state_kind: String,
    next_sequence: u64,
    config: RunnerConfig,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    checkpoint_source_bundle_hash: Option<String>,
    current_session_date: Option<String>,
    current_session_scoring: Option<bool>,
    #[serde(default)]
    last_completed_session: Option<String>,
    #[serde(default)]
    current_session_last_minute: Option<i64>,
    current_session_observations: usize,
    history_sessions: usize,
    history_observations: usize,
    s3_states: BTreeMap<String, EwmaState>,
    s3_attempted: BTreeSet<String>,
    s3_support: BTreeMap<String, usize>,
    last_qualified: BTreeMap<String, i64>,
    previous_prequential_score_count: usize,
}

impl StateMetadata {
    fn into_state(self) -> RunnerState {
        let current_session = self.current_session_date.map(|session_date| SessionRecord {
            last_minute: self.current_session_last_minute,
            session_date,
            scoring: self.current_session_scoring.unwrap_or(true),
            observations: Vec::new(),
            raw_by_contract: BTreeMap::new(),
            eligible_raw_history: Vec::new(),
        });
        RunnerState {
            next_sequence: self.next_sequence,
            config: self.config,
            checkpoint_source_bundle_hash: self.checkpoint_source_bundle_hash,
            history: VecDeque::new(),
            current_session,
            s3_states: self.s3_states,
            s3_attempted: self.s3_attempted,
            s3_support: self.s3_support,
            last_qualified: self.last_qualified,
            previous_prequential_sampled_scores: VecDeque::new(),
        }
    }
}

#[derive(Debug, Error)]
pub enum ProtocolError {
    #[error("invalid JSON request: {0}")]
    Json(#[from] serde_json::Error),
    #[error("sequence mismatch: expected {expected}, received {received}")]
    Sequence { expected: u64, received: u64 },
    #[error("missing or invalid packet field: {0}")]
    InvalidPacket(String),
    #[error("state decode failed: {0}")]
    StateDecode(String),
    #[error("response state encode failed: {0}")]
    StateEncode(#[source] serde_json::Error),
    #[error("checkpoint failed: {0}")]
    Checkpoint(#[source] std::io::Error),
    #[error("checkpoint state encode failed: {0}")]
    CheckpointEncode(#[source] serde_json::Error),
    #[error("checkpoint state decode failed: {0}")]
    CheckpointDecode(#[source] serde_json::Error),
}

/// Migrate the pre-compaction raw history field and validate the ordering
/// invariants needed by the compact S3 representation.  This function is
/// deliberately idempotent so it can run on every full-state restore and
/// cache rebuild.
fn migrate_legacy_raw_history(state: &mut RunnerState) -> Result<(), ProtocolError> {
    let mut previous_date: Option<&str> = None;
    for session in &state.history {
        if previous_date.is_some_and(|previous| previous >= session.session_date.as_str()) {
            return Err(ProtocolError::StateDecode(
                "history session dates must be strictly increasing".to_owned(),
            ));
        }
        previous_date = Some(session.session_date.as_str());
    }
    if let Some(current) = state.current_session.as_ref()
        && state
            .history
            .back()
            .is_some_and(|previous| previous.session_date >= current.session_date)
    {
        return Err(ProtocolError::StateDecode(
            "current session date must follow history".to_owned(),
        ));
    }

    for session in &mut state.history {
        migrate_session_raw_history(session)?;
    }
    if let Some(current) = state.current_session.as_mut() {
        migrate_session_raw_history(current)?;
    }
    Ok(())
}

fn migrate_session_raw_history(session: &mut SessionRecord) -> Result<(), ProtocolError> {
    let mut legacy_by_contract: BTreeMap<String, Vec<(i64, f64)>> = BTreeMap::new();
    for observation in &session.eligible_raw_history {
        if observation.session_date != session.session_date {
            return Err(ProtocolError::StateDecode(format!(
                "raw history date mismatch for contract {}",
                observation.contract_id
            )));
        }
        // The old cache filtered non-finite values.  Preserve that behavior
        // while compacting instead of allowing an invalid raw value into S3.
        if observation.raw_delta.is_finite() {
            legacy_by_contract
                .entry(observation.contract_id.clone())
                .or_default()
                .push((observation.ts_minute, observation.raw_delta));
        }
    }
    normalize_raw_history_map(&mut legacy_by_contract, &session.session_date)?;

    normalize_raw_history_map(&mut session.raw_by_contract, &session.session_date)?;
    if !legacy_by_contract.is_empty() {
        if session.raw_by_contract.is_empty() {
            session.raw_by_contract = legacy_by_contract;
        } else {
            // A checkpoint written by the compact representation has no
            // legacy rows.  Refuse an ambiguous mixed representation instead
            // of silently double-counting or dropping one side.
            return Err(ProtocolError::StateDecode(
                "checkpoint contains both compact and legacy raw history".to_owned(),
            ));
        }
    }
    session.eligible_raw_history.clear();
    Ok(())
}

fn normalize_raw_history_map(
    raw_by_contract: &mut BTreeMap<String, Vec<(i64, f64)>>,
    session_date: &str,
) -> Result<(), ProtocolError> {
    for (contract_id, rows) in raw_by_contract.iter_mut() {
        rows.retain(|(_, value)| value.is_finite());
        rows.sort_unstable_by_key(|(ts_minute, _)| *ts_minute);
        if rows.windows(2).any(|window| window[0].0 == window[1].0) {
            return Err(ProtocolError::StateDecode(format!(
                "duplicate raw history timestamp for contract {contract_id} on {session_date}"
            )));
        }
    }
    Ok(())
}

fn expected_checkpoint_source_bundle_hash() -> Option<String> {
    std::env::var("IV_SHOCK_CHECKPOINT_SOURCE_BUNDLE_HASH")
        .ok()
        .filter(|value| !value.is_empty())
}

/// Keep the active binary's bundle identity when restoring a checkpoint from
/// another binary.  A cross-binary restore is accepted only when the caller
/// explicitly allowlists the checkpoint's source hash.
fn guard_restored_bundle(
    active_bundle_hash: &str,
    mut restored: RunnerState,
    expected_source_bundle_hash: Option<&str>,
) -> Result<RunnerState, ProtocolError> {
    let source_bundle_hash = restored.config.bundle_hash.clone();
    if source_bundle_hash != active_bundle_hash {
        if expected_source_bundle_hash != Some(source_bundle_hash.as_str()) {
            return Err(ProtocolError::StateDecode(format!(
                "checkpoint bundle hash {source_bundle_hash:?} differs from active bundle {active_bundle_hash:?}; set IV_SHOCK_CHECKPOINT_SOURCE_BUNDLE_HASH to the exact checkpoint source hash"
            )));
        }
        restored.checkpoint_source_bundle_hash = Some(source_bundle_hash);
        active_bundle_hash.clone_into(&mut restored.config.bundle_hash);
    }
    Ok(restored)
}

#[derive(Debug, Clone)]
struct SeriesStats {
    sorted: Vec<f64>,
    median: f64,
    mad: f64,
    mean: f64,
    sample_sd: f64,
}

#[derive(Debug, Default, Clone)]
struct CalibrationBook {
    raw: BTreeMap<String, SeriesStats>,
    log: BTreeMap<String, SeriesStats>,
    residual: BTreeMap<String, SeriesStats>,
}

#[derive(Debug, Clone)]
struct ThresholdSpec {
    value: Option<f64>,
    support: usize,
    target: usize,
    selected: usize,
    ties: usize,
    reason: Option<String>,
}

type EventDetectorScores = BTreeMap<String, [Option<f64>; 6]>;
type ScoreEventOutput = Result<
    (
        EventDiagnostic,
        Option<TradeIntent>,
        Option<CalibrationObservation>,
        Option<RawHistoryAppend>,
    ),
    ProtocolError,
>;

impl CalibrationBook {
    fn build(history: &VecDeque<SessionRecord>) -> Self {
        let mut raw: BTreeMap<String, Vec<f64>> = BTreeMap::new();
        let mut log: BTreeMap<String, Vec<f64>> = BTreeMap::new();
        let mut residual: BTreeMap<String, Vec<f64>> = BTreeMap::new();
        for session in history {
            for observation in &session.observations {
                for level in 0..4 {
                    let key = group_key(level, observation.group_keys.at(level));
                    if observation.raw_delta.is_finite() {
                        raw.entry(key.clone())
                            .or_default()
                            .push(observation.raw_delta);
                    }
                    if let Some(value) = observation.log_delta.filter(|value| value.is_finite()) {
                        log.entry(key.clone()).or_default().push(value);
                    }
                    if let Some(value) = observation.residual.filter(|value| value.is_finite()) {
                        residual.entry(key).or_default().push(value);
                    }
                }
            }
        }
        Self {
            raw: finish_series(raw),
            log: finish_series(log),
            residual: finish_series(residual),
        }
    }

    fn stats(&self, metric: Metric, level: usize, value: &str) -> Option<&SeriesStats> {
        let key = group_key(level, value);
        match metric {
            Metric::Raw => self.raw.get(&key),
            Metric::Log => self.log.get(&key),
            Metric::Residual => self.residual.get(&key),
        }
    }
}

fn group_key(level: usize, value: &str) -> String {
    format!("L{level}|{value}")
}

fn finish_series(input: BTreeMap<String, Vec<f64>>) -> BTreeMap<String, SeriesStats> {
    input
        .into_iter()
        .map(|(key, mut values)| {
            values.sort_by(f64::total_cmp);
            let median = median_sorted(&values);
            let mut deviations: Vec<f64> =
                values.iter().map(|value| (value - median).abs()).collect();
            deviations.sort_by(f64::total_cmp);
            let mad = median_sorted(&deviations);
            let mean = values.iter().sum::<f64>() / values.len() as f64;
            let variance = if values.len() > 1 {
                values
                    .iter()
                    .map(|value| (value - mean).powi(2))
                    .sum::<f64>()
                    / (values.len() - 1) as f64
            } else {
                f64::NAN
            };
            (
                key,
                SeriesStats {
                    sorted: values,
                    median,
                    mad,
                    mean,
                    sample_sd: variance.sqrt(),
                },
            )
        })
        .collect()
}

fn median_sorted(values: &[f64]) -> f64 {
    if values.is_empty() {
        return f64::NAN;
    }
    let middle = values.len() / 2;
    if values.len().is_multiple_of(2) {
        values[middle - 1].midpoint(values[middle])
    } else {
        values[middle]
    }
}

/// Exact robust score used by S0, including the inherited 1.4826 scale factor.
#[must_use]
pub fn robust_mad_score(value: f64, samples: &[f64]) -> Option<f64> {
    let mut sorted: Vec<f64> = samples.iter().copied().filter(|x| x.is_finite()).collect();
    sorted.sort_by(f64::total_cmp);
    let median = median_sorted(&sorted);
    let mut deviations: Vec<f64> = sorted
        .iter()
        .map(|sample| (sample - median).abs())
        .collect();
    deviations.sort_by(f64::total_cmp);
    let mad = median_sorted(&deviations);
    if !value.is_finite() || !median.is_finite() || !mad.is_finite() || mad <= 0.0 {
        return None;
    }
    Some((value - median) / (1.4826 * mad))
}

/// Conventional sample-standard-deviation score used by S1.
#[must_use]
pub fn sample_sd_score(value: f64, samples: &[f64]) -> Option<f64> {
    let finite: Vec<f64> = samples.iter().copied().filter(|x| x.is_finite()).collect();
    if finite.len() < 2 || !value.is_finite() {
        return None;
    }
    let mean = finite.iter().sum::<f64>() / finite.len() as f64;
    let variance =
        finite.iter().map(|x| (x - mean).powi(2)).sum::<f64>() / (finite.len() - 1) as f64;
    let scale = variance.sqrt();
    if !mean.is_finite() || !scale.is_finite() || scale <= 0.0 {
        None
    } else {
        Some((value - mean) / scale)
    }
}

/// S2's distribution-free score.  Ties use the midrank `(below + equal/2)/n`
/// and the centered historical sample is represented by its median rank.
#[must_use]
pub fn midrank_tail_score(value: f64, samples: &[f64]) -> Option<f64> {
    if !value.is_finite() {
        return None;
    }
    let mut sorted: Vec<f64> = samples.iter().copied().filter(|x| x.is_finite()).collect();
    sorted.sort_by(f64::total_cmp);
    if sorted.is_empty() {
        return None;
    }
    let below = sorted.partition_point(|x| *x < value);
    let above = sorted.partition_point(|x| *x <= value);
    let midrank = (below as f64 + (above - below) as f64 / 2.0) / sorted.len() as f64;
    let epsilon = 0.5 / sorted.len() as f64;
    Some(normal_inverse_cdf(midrank.clamp(epsilon, 1.0 - epsilon)))
}

/// Initialize S3 from the first 200 strictly prior observations.
#[must_use]
pub fn ewma_initialize(samples: &[f64], alpha: f64, minimum_support: usize) -> Option<EwmaState> {
    let mut finite: Vec<f64> = samples.iter().copied().filter(|x| x.is_finite()).collect();
    if finite.len() < minimum_support
        || finite.len() < 2
        || !alpha.is_finite()
        || !(0.0..1.0).contains(&alpha)
    {
        return None;
    }
    finite.truncate(minimum_support);
    let mean = finite.iter().sum::<f64>() / finite.len() as f64;
    let variance =
        finite.iter().map(|x| (x - mean).powi(2)).sum::<f64>() / (finite.len() - 1) as f64;
    if !mean.is_finite() || !variance.is_finite() {
        return None;
    }
    let mut state = EwmaState {
        mean,
        variance,
        updates: finite.len() as u64,
    };
    for value in samples
        .iter()
        .copied()
        .filter(|x| x.is_finite())
        .skip(minimum_support)
    {
        ewma_update(&mut state, value, alpha);
    }
    Some(state)
}

/// Update S3 after scoring the current value, preserving score-before-update.
pub fn ewma_update(state: &mut EwmaState, value: f64, alpha: f64) {
    if !value.is_finite()
        || !state.mean.is_finite()
        || !state.variance.is_finite()
        || !alpha.is_finite()
    {
        return;
    }
    let delta = value - state.mean;
    state.mean += alpha * delta;
    state.variance = (1.0 - alpha) * (state.variance + alpha * delta.powi(2));
    state.updates = state.updates.saturating_add(1);
}

fn normal_inverse_cdf(probability: f64) -> f64 {
    // Peter J. Acklam's rational approximation.  Input is clamped by the
    // caller, so the branches below only handle the approximation regions.
    const A: [f64; 6] = [
        -3.969683028665376e1,
        2.209460984245205e2,
        -2.759285104469687e2,
        1.38357751867269e2,
        -3.066479806614716e1,
        2.506628277459239,
    ];
    const B: [f64; 5] = [
        -5.447609879822406e1,
        1.615858368580409e2,
        -1.556989798598866e2,
        6.680131188771972e1,
        -1.328068155288572e1,
    ];
    const C: [f64; 6] = [
        -7.784894002430293e-3,
        -3.223964580411365e-1,
        -2.400758277161838,
        -2.549732539343734,
        4.374664141464968,
        2.938163982698783,
    ];
    const D: [f64; 4] = [
        7.784695709041462e-3,
        3.224671290700398e-1,
        2.445134137142996,
        3.754408661907416,
    ];
    let low = 0.02425;
    let high = 1.0 - low;
    if probability < low {
        let q = (-2.0 * probability.ln()).sqrt();
        let numerator = ((((C[0] * q + C[1]) * q + C[2]) * q + C[3]) * q + C[4]) * q + C[5];
        let denominator = (((D[0] * q + D[1]) * q + D[2]) * q + D[3]) * q + 1.0;
        numerator / denominator
    } else if probability <= high {
        let q = probability - 0.5;
        let r = q * q;
        let numerator = (((((A[0] * r + A[1]) * r + A[2]) * r + A[3]) * r + A[4]) * r + A[5]) * q;
        let denominator = ((((B[0] * r + B[1]) * r + B[2]) * r + B[3]) * r + B[4]) * r + 1.0;
        numerator / denominator
    } else {
        let q = (-2.0 * (1.0 - probability).ln()).sqrt();
        let numerator = ((((C[0] * q + C[1]) * q + C[2]) * q + C[3]) * q + C[4]) * q + C[5];
        let denominator = (((D[0] * q + D[1]) * q + D[2]) * q + D[3]) * q + 1.0;
        -(numerator / denominator)
    }
}

/// Immutable per-contract S3 initialization prepared from the completed
/// sessions visible at the current session start.  The raw observations used
/// to build this seed are intentionally dropped immediately; current-session
/// updates live in `RunnerState::s3_states` after the first event is scored.
#[derive(Debug, Clone)]
struct S3PriorSeed {
    support: usize,
    state: Option<EwmaState>,
}

/// Stateful persistent strategy runner.
#[derive(Clone)]
pub struct Runner {
    state: RunnerState,
    initialized: bool,
    books: Option<CalibrationBook>,
    threshold_cache: BTreeMap<(Detector, i8, String), ThresholdSpec>,
    severity_cache: BTreeMap<(Detector, i8, String), Vec<f64>>,
    /// Per-contract prior support and EWMA seed.  The raw history used to
    /// derive each entry is collected one contract at a time and is not
    /// retained in the runner.
    s3_prior_cache: BTreeMap<String, S3PriorSeed>,
}

fn reject_direct_intent_bypass(packet: &InputPacket) -> Result<(), ProtocolError> {
    if packet.emit_actions || packet.events.iter().any(|event| event.intent.is_some()) {
        return Err(ProtocolError::InvalidPacket(
            "detector is diagnostics-only; route qualified S0 through strategy policy".to_owned(),
        ));
    }
    Ok(())
}

impl Default for Runner {
    fn default() -> Self {
        Self::new(RunnerConfig::default())
    }
}

impl Runner {
    #[must_use]
    pub const fn new(config: RunnerConfig) -> Self {
        Self {
            state: RunnerState::new(config),
            initialized: false,
            books: None,
            threshold_cache: BTreeMap::new(),
            severity_cache: BTreeMap::new(),
            s3_prior_cache: BTreeMap::new(),
        }
    }

    #[must_use]
    pub const fn state(&self) -> &RunnerState {
        &self.state
    }

    /// Process one request and return the protocol response.
    ///
    /// # Errors
    ///
    /// Returns a protocol error when the JSON state, sequence, packet, or
    /// explicit checkpoint operation is invalid.
    #[allow(clippy::needless_pass_by_value)]
    pub fn process_request(
        &mut self,
        request: JsonlRequest,
    ) -> Result<JsonlResponse, ProtocolError> {
        let snapshot = self.clone();
        match self.process_request_inner(&request) {
            Ok(response) => Ok(response),
            Err(error) => {
                *self = snapshot;
                Err(error)
            }
        }
    }

    fn process_request_inner(
        &mut self,
        request: &JsonlRequest,
    ) -> Result<JsonlResponse, ProtocolError> {
        if self.initialized && request.input.kind.eq_ignore_ascii_case("restore") {
            return Err(ProtocolError::InvalidPacket(
                "restore requires a fresh process".into(),
            ));
        }
        if request.input.kind.eq_ignore_ascii_case("checkpoint")
            && request.input.path.as_ref().is_none_or(String::is_empty)
        {
            return Err(ProtocolError::InvalidPacket("checkpoint.path".into()));
        }
        if !self.initialized && request.input.kind.eq_ignore_ascii_case("restore") {
            let path = request
                .input
                .path
                .as_deref()
                .ok_or_else(|| ProtocolError::InvalidPacket("restore.path".to_owned()))?;
            self.restore_checkpoint(path)?;
            self.initialized = true;
        } else {
            self.restore_state_if_needed(&request.state)?;
        }
        if request.sequence != self.state.next_sequence {
            return Err(ProtocolError::Sequence {
                expected: self.state.next_sequence,
                received: request.sequence,
            });
        }
        let (diagnostics, actions, summary) = self.process_packet(&request.input)?;
        self.state.next_sequence = self
            .state
            .next_sequence
            .checked_add(1)
            .ok_or_else(|| ProtocolError::InvalidPacket("sequence overflow".to_owned()))?;
        if request.input.kind.eq_ignore_ascii_case("checkpoint") {
            let path = request
                .input
                .path
                .as_deref()
                .ok_or_else(|| ProtocolError::InvalidPacket("checkpoint.path".to_owned()))?;
            self.write_checkpoint(path)?;
        }
        let state = self.response_state()?;
        Ok(JsonlResponse {
            schema_version: SCHEMA_VERSION.to_owned(),
            artifact_consumed: true,
            matching_sequence: request.sequence,
            runner_id: self.state.config.runner_id.clone(),
            bundle_hash: self.state.config.bundle_hash.clone(),
            state,
            actions,
            diagnostics,
            summary,
        })
    }

    /// Process one canonical JSONL line. Malformed or legacy private envelopes
    /// fail closed so the executable has only one external protocol.
    ///
    /// # Errors
    ///
    /// Returns a protocol error when the line cannot be decoded or processed.
    pub fn process_json(&mut self, line: &str) -> Result<String, ProtocolError> {
        let envelope: Value = serde_json::from_str(line)?;
        if envelope
            .get("input")
            .and_then(|input| input.get("research_payload"))
            .is_none()
        {
            return Err(ProtocolError::InvalidPacket(
                "canonical ResearchRequest with input.research_payload required".to_owned(),
            ));
        }
        let request: ResearchRequest = serde_json::from_value(envelope)?;
        let response = self.process_canonical_request(request)?;
        serde_json::to_string(&response).map_err(ProtocolError::StateEncode)
    }

    /// Adapt the same runner to the generic engine's canonical
    /// `ResearchRequest`/`ResearchResponse` boundary.  The feeder places an
    /// `InputPacket` in `input.research_payload`; diagnostics are carried in a
    /// compact research-owned response state because the canonical response
    /// intentionally has no extra diagnostics field.
    ///
    /// # Errors
    ///
    /// Returns a protocol error when the canonical payload, state, sequence,
    /// or strategy packet is invalid.
    pub fn process_canonical_request(
        &mut self,
        request: ResearchRequest,
    ) -> Result<ResearchResponse, ProtocolError> {
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
            return Err(ProtocolError::InvalidPacket(
                "canonical identity, sequence or causal clock mismatch".to_owned(),
            ));
        }
        let payload = if request.input.research_payload.get("input").is_some() {
            request
                .input
                .research_payload
                .get("input")
                .cloned()
                .ok_or_else(|| ProtocolError::InvalidPacket("research_payload.input".to_owned()))?
        } else {
            request.input.research_payload.clone()
        };
        let state = request
            .state
            .get("runner_state")
            .cloned()
            .unwrap_or(request.state);
        let input: InputPacket = serde_json::from_value(payload)?;
        if input.kind.eq_ignore_ascii_case("minute_packet")
            && input
                .ts_minute
                .and_then(|minute| minute.checked_mul(60_000_000_000))
                != Some(request.input.decision_at_ns)
        {
            return Err(ProtocolError::InvalidPacket(
                "source minute differs from canonical decision clock".to_owned(),
            ));
        }
        let feedback = serde_json::to_value(request.feedback)?;
        let response = self.process_request(JsonlRequest {
            input,
            state,
            feedback,
            sequence: request.sequence,
        })?;
        let state = serde_json::json!({
            "runner_state": response.state,
            "diagnostics": response.diagnostics,
            "summary": response.summary,
        });
        Ok(ResearchResponse {
            schema_version: CONTRACT_VERSION.to_owned(),
            artifact_consumed: response.artifact_consumed,
            runner_id: response.runner_id,
            bundle_hash: response.bundle_hash,
            state,
            actions: response.actions,
        })
    }

    fn restore_state_if_needed(&mut self, value: &Value) -> Result<(), ProtocolError> {
        if self.initialized {
            return Ok(());
        }
        let is_empty_object = value.as_object().is_some_and(serde_json::Map::is_empty);
        if !value.is_null() && !is_empty_object {
            let mut restored: RunnerState =
                if value.get("state_kind").and_then(Value::as_str) == Some("metadata") {
                    let metadata: StateMetadata = serde_json::from_value(value.clone())
                        .map_err(|error| ProtocolError::StateDecode(error.to_string()))?;
                    if metadata.next_sequence != 0
                        || metadata.last_completed_session.is_some()
                        || metadata.history_observations > 0
                        || !metadata.s3_states.is_empty()
                        || !metadata.last_qualified.is_empty()
                        || metadata.history_sessions > 0
                        || metadata.current_session_date.is_some()
                        || metadata.previous_prequential_score_count > 0
                    {
                        return Err(ProtocolError::StateDecode(
                            "metadata is not a restart checkpoint".into(),
                        ));
                    }
                    metadata.into_state()
                } else {
                    serde_json::from_value(value.clone())
                        .map_err(|error| ProtocolError::StateDecode(error.to_string()))?
                };
            let active_bundle_hash = self.state.config.bundle_hash.clone();
            let expected_source_bundle_hash = expected_checkpoint_source_bundle_hash();
            restored = guard_restored_bundle(
                &active_bundle_hash,
                restored,
                expected_source_bundle_hash.as_deref(),
            )?;
            migrate_legacy_raw_history(&mut restored)?;
            self.state = restored;
            self.books = self
                .state
                .current_session
                .as_ref()
                .map(|_| CalibrationBook::build(&self.state.history));
            self.rebuild_s3_prior_cache()?;
            if self.state.current_session.is_some() {
                self.build_threshold_cache();
            }
        }
        self.initialized = true;
        Ok(())
    }

    fn response_state(&self) -> Result<Value, ProtocolError> {
        serde_json::to_value(self.state_metadata()).map_err(ProtocolError::StateEncode)
    }

    fn state_metadata(&self) -> StateMetadata {
        StateMetadata {
            state_kind: "metadata".to_owned(),
            next_sequence: self.state.next_sequence,
            config: self.state.config.clone(),
            checkpoint_source_bundle_hash: self.state.checkpoint_source_bundle_hash.clone(),
            last_completed_session: self.state.history.back().map(|s| s.session_date.clone()),
            current_session_date: self
                .state
                .current_session
                .as_ref()
                .map(|session| session.session_date.clone()),
            current_session_scoring: self
                .state
                .current_session
                .as_ref()
                .map(|session| session.scoring),
            current_session_last_minute: self
                .state
                .current_session
                .as_ref()
                .and_then(|s| s.last_minute),
            current_session_observations: self
                .state
                .current_session
                .as_ref()
                .map_or(0, |session| session.observations.len()),
            history_sessions: self.state.history.len(),
            history_observations: self
                .state
                .history
                .iter()
                .map(|session| session.observations.len())
                .sum(),
            s3_states: self.state.s3_states.clone(),
            s3_attempted: self.state.s3_attempted.clone(),
            s3_support: self.state.s3_support.clone(),
            last_qualified: self.state.last_qualified.clone(),
            previous_prequential_score_count: self.state.previous_prequential_sampled_scores.len(),
        }
    }

    /// Write the full strategy-owned checkpoint to a caller-selected path.
    /// Ordinary responses use only [`StateMetadata`].
    ///
    /// # Errors
    ///
    /// Returns a checkpoint error when strategy state cannot be serialized or
    /// atomically written to the requested path.
    pub fn write_checkpoint(&self, path: &str) -> Result<(), ProtocolError> {
        let destination = Path::new(path);
        if destination.as_os_str().is_empty() {
            return Err(ProtocolError::InvalidPacket(
                "empty checkpoint path".to_owned(),
            ));
        }
        let bytes = serde_json::to_vec(&self.state).map_err(ProtocolError::CheckpointEncode)?;
        let temporary = destination.with_extension("tmp");
        fs::write(&temporary, bytes).map_err(ProtocolError::Checkpoint)?;
        fs::rename(&temporary, destination).map_err(ProtocolError::Checkpoint)
    }

    /// Restore a previously written strategy-owned checkpoint.  The caller
    /// must use the checkpoint's `next_sequence` in the restore request.
    ///
    /// # Errors
    ///
    /// Returns a checkpoint error when the path cannot be read or its state is
    /// not a valid strategy checkpoint.
    pub fn restore_checkpoint(&mut self, path: &str) -> Result<(), ProtocolError> {
        let expected_source_bundle_hash = expected_checkpoint_source_bundle_hash();
        self.restore_checkpoint_with_expected_source_bundle_hash(
            path,
            expected_source_bundle_hash.as_deref(),
        )
    }

    /// Restore a checkpoint while explicitly allowlisting a different source
    /// bundle.  This narrow API is deterministic for callers that do not want
    /// to rely on the process environment; `restore_checkpoint` uses
    /// `IV_SHOCK_CHECKPOINT_SOURCE_BUNDLE_HASH` for the production JSONL path.
    ///
    /// # Errors
    ///
    /// Returns an error when the checkpoint is unreadable, malformed, or was
    /// produced by a different bundle without an exact allowlist match.
    pub fn restore_checkpoint_with_expected_source_bundle_hash(
        &mut self,
        path: &str,
        expected_source_bundle_hash: Option<&str>,
    ) -> Result<(), ProtocolError> {
        if self.initialized {
            return Err(ProtocolError::InvalidPacket(
                "restore requires fresh process".into(),
            ));
        }
        let bytes = fs::read(Path::new(path)).map_err(ProtocolError::Checkpoint)?;
        let restored: RunnerState =
            serde_json::from_slice(&bytes).map_err(ProtocolError::CheckpointDecode)?;
        let active_bundle_hash = self.state.config.bundle_hash.clone();
        let mut restored =
            guard_restored_bundle(&active_bundle_hash, restored, expected_source_bundle_hash)?;
        migrate_legacy_raw_history(&mut restored)?;
        self.state = restored;
        self.books = self
            .state
            .current_session
            .as_ref()
            .map(|_| CalibrationBook::build(&self.state.history));
        self.rebuild_s3_prior_cache()?;
        if self.state.current_session.is_some() {
            self.build_threshold_cache();
        }
        self.initialized = true;
        Ok(())
    }

    fn process_packet(
        &mut self,
        packet: &InputPacket,
    ) -> Result<(Vec<EventDiagnostic>, Vec<TradeIntent>, DiagnosticSummary), ProtocolError> {
        match packet.kind.to_ascii_lowercase().as_str() {
            "session_start" => {
                self.start_session(packet)?;
                Ok((Vec::new(), Vec::new(), DiagnosticSummary::default()))
            }
            "minute_packet" => self.process_minute(packet),
            "session_end" => {
                self.end_session(packet)?;
                Ok((Vec::new(), Vec::new(), DiagnosticSummary::default()))
            }
            "checkpoint" | "restore" => Ok((Vec::new(), Vec::new(), DiagnosticSummary::default())),
            kind => Err(ProtocolError::InvalidPacket(format!(
                "unknown packet kind {kind:?}"
            ))),
        }
    }

    fn start_session(&mut self, packet: &InputPacket) -> Result<(), ProtocolError> {
        let session_date = packet
            .session_date
            .clone()
            .ok_or_else(|| ProtocolError::InvalidPacket("session_start.session_date".to_owned()))?;
        if self.state.current_session.is_some() {
            return Err(ProtocolError::InvalidPacket(
                "session already open".to_owned(),
            ));
        }
        if self
            .state
            .history
            .back()
            .is_some_and(|record| record.session_date >= session_date)
        {
            return Err(ProtocolError::InvalidPacket(
                "session dates must be strictly increasing".to_owned(),
            ));
        }
        self.state.current_session = Some(SessionRecord {
            last_minute: None,
            session_date,
            scoring: packet.scoring,
            observations: Vec::new(),
            raw_by_contract: BTreeMap::new(),
            eligible_raw_history: Vec::new(),
        });
        self.state.s3_states.clear();
        self.state.last_qualified.clear();
        self.state.s3_attempted.clear();
        self.state.s3_support.clear();
        self.books = Some(CalibrationBook::build(&self.state.history));
        self.rebuild_s3_prior_cache()?;
        self.build_threshold_cache();
        Ok(())
    }

    /// Build compact S3 initialization seeds from the completed prior
    /// sessions.  A single contract's values are materialized at a time;
    /// after [`ewma_initialize`] returns, those raw values are dropped.
    fn rebuild_s3_prior_cache(&mut self) -> Result<(), ProtocolError> {
        migrate_legacy_raw_history(&mut self.state)?;

        let mut contract_ids = BTreeSet::new();
        for session in &self.state.history {
            contract_ids.extend(session.raw_by_contract.keys().cloned());
        }

        let minimum_support = self.state.config.minimum_support;
        let alpha = self.state.config.s3_alpha;
        let history_sessions = self.state.config.history_sessions;
        let history_start = self.state.history.len().saturating_sub(history_sessions);
        let mut prior_cache = BTreeMap::new();
        for contract_id in contract_ids {
            let mut samples = Vec::new();
            for session in self.state.history.iter().skip(history_start) {
                if let Some(rows) = session.raw_by_contract.get(&contract_id) {
                    samples.extend(rows.iter().map(|(_, value)| *value));
                }
            }
            let support = samples.len();
            let state = (support >= minimum_support)
                .then(|| ewma_initialize(&samples, alpha, minimum_support))
                .flatten();
            prior_cache.insert(contract_id, S3PriorSeed { support, state });
        }
        self.s3_prior_cache = prior_cache;
        Ok(())
    }

    fn build_threshold_cache(&mut self) {
        self.threshold_cache.clear();
        self.severity_cache.clear();
        let mut by_population: BTreeMap<(i8, String), EventDetectorScores> = BTreeMap::new();
        let current_session_date = self
            .state
            .current_session
            .as_ref()
            .map(|session| session.session_date.as_str());
        for record in &self.state.previous_prequential_sampled_scores {
            if current_session_date.is_some_and(|date| record.session_date.as_str() >= date) {
                continue;
            }
            if record.absolute_score.is_finite() {
                self.severity_cache
                    .entry((
                        record.detector,
                        record.raw_iv_sign,
                        record.calibration_key.clone(),
                    ))
                    .or_default()
                    .push(record.absolute_score);
            }
            if record.raw_iv_sign == 0 {
                continue;
            }
            let scores = by_population
                .entry((record.raw_iv_sign, record.calibration_key.clone()))
                .or_default()
                .entry(record.event_id.clone())
                .or_insert([None; 6]);
            scores[detector_index(record.detector)] = Some(record.absolute_score);
        }
        for values in self.severity_cache.values_mut() {
            values.sort_by(f64::total_cmp);
        }
        for ((raw_sign, calibration_key), event_scores) in by_population {
            for detector in Detector::ALL {
                if detector == Detector::S0 {
                    continue;
                }
                let mut matched: Vec<(bool, f64)> = event_scores
                    .values()
                    .filter_map(|scores| {
                        Some((
                            scores[detector_index(Detector::S0)]? >= self.state.config.threshold,
                            scores[detector_index(detector)]?,
                        ))
                    })
                    .filter(|(_, score)| score.is_finite())
                    .collect();
                let support = matched.len();
                let target = matched.iter().filter(|(s0, _)| *s0).count();
                let spec = if support < self.state.config.minimum_support {
                    ThresholdSpec {
                        value: None,
                        support,
                        target,
                        selected: 0,
                        ties: 0,
                        reason: Some("insufficient_threshold_support".to_owned()),
                    }
                } else if target == 0 {
                    ThresholdSpec {
                        value: None,
                        support,
                        target,
                        selected: 0,
                        ties: 0,
                        reason: Some("zero_s0_target".to_owned()),
                    }
                } else {
                    matched.sort_by(|left, right| left.1.total_cmp(&right.1));
                    let rank = support.saturating_sub(target).min(support - 1);
                    let value = matched[rank].1;
                    let selected = matched.iter().filter(|(_, score)| *score >= value).count();
                    let ties = matched
                        .iter()
                        .filter(|(_, score)| score.total_cmp(&value).is_eq())
                        .count();
                    ThresholdSpec {
                        value: Some(value),
                        support,
                        target,
                        selected,
                        ties,
                        reason: None,
                    }
                };
                self.threshold_cache
                    .insert((detector, raw_sign, calibration_key.clone()), spec);
            }
        }
    }

    fn process_minute(
        &mut self,
        packet: &InputPacket,
    ) -> Result<(Vec<EventDiagnostic>, Vec<TradeIntent>, DiagnosticSummary), ProtocolError> {
        let session_date = packet
            .session_date
            .as_deref()
            .ok_or_else(|| ProtocolError::InvalidPacket("minute_packet.session_date".to_owned()))?;
        let ts_minute = packet
            .ts_minute
            .ok_or_else(|| ProtocolError::InvalidPacket("minute_packet.ts_minute".to_owned()))?;
        let session = self.state.current_session.as_ref().ok_or_else(|| {
            ProtocolError::InvalidPacket("minute packet without session_start".to_owned())
        })?;
        if session.session_date != session_date {
            return Err(ProtocolError::InvalidPacket(
                "minute packet date mismatch".to_owned(),
            ));
        }
        if session
            .last_minute
            .is_some_and(|previous| ts_minute <= previous)
        {
            return Err(ProtocolError::InvalidPacket(
                "minute timestamps must be strictly increasing".to_owned(),
            ));
        }
        let mut batch_contracts = BTreeSet::new();
        if packet
            .events
            .iter()
            .any(|event| !batch_contracts.insert(&event.contract_id))
        {
            return Err(ProtocolError::InvalidPacket(
                "duplicate contract in minute batch".to_owned(),
            ));
        }
        reject_direct_intent_bypass(packet)?;
        let scoring = session.scoring;
        let mut diagnostics = Vec::with_capacity(packet.events.len());
        let mut actions = Vec::new();
        let mut counts = [(0_usize, 0_usize, 0_usize); 6];
        let mut inventory = BTreeMap::new();
        for event in &packet.events {
            let (diagnostic, maybe_action, observation, raw_history) =
                self.score_event(event, ts_minute, scoring, packet.emit_actions)?;
            if scoring {
                count_inventory(event, &diagnostic, &mut inventory);
            }
            for (index, detector) in diagnostic.detectors.iter().enumerate() {
                if detector.score.is_some() {
                    counts[index].0 = counts[index].0.saturating_add(1);
                }
                if detector.qualified_before_quiet {
                    counts[index].1 = counts[index].1.saturating_add(1);
                }
                if detector.qualified_after_quiet {
                    counts[index].2 = counts[index].2.saturating_add(1);
                }
            }
            let keep_diagnostic = packet.emit_all_diagnostics
                || self.state.config.emit_all_diagnostics
                || diagnostic
                    .detectors
                    .iter()
                    .any(|detector| detector.qualified_after_quiet);
            if keep_diagnostic {
                diagnostics.push(diagnostic);
            }
            if let Some(action) = maybe_action {
                actions.push(action);
            }
            let session = self
                .state
                .current_session
                .as_mut()
                .expect("session validated above");
            if let Some(observation) = observation {
                session.observations.push(observation);
            }
            if let Some(observation) = raw_history {
                append_raw_history(session, observation);
            }
        }
        self.state
            .current_session
            .as_mut()
            .expect("validated session")
            .last_minute = Some(ts_minute);
        let summary = DiagnosticSummary {
            inventory: inventory.into_values().collect(),
            detectors: Detector::ALL
                .into_iter()
                .zip(counts)
                .map(|(detector, (supported, before, after))| DetectorSummary {
                    detector,
                    supported,
                    qualified_before_quiet: before,
                    qualified_after_quiet: after,
                })
                .collect(),
        };
        Ok((diagnostics, actions, summary))
    }

    fn end_session(&mut self, packet: &InputPacket) -> Result<(), ProtocolError> {
        let expected = packet
            .session_date
            .as_deref()
            .ok_or_else(|| ProtocolError::InvalidPacket("session_end.session_date".to_owned()))?;
        let current = self.state.current_session.take().ok_or_else(|| {
            ProtocolError::InvalidPacket("session_end without session_start".to_owned())
        })?;
        if current.session_date != expected {
            self.state.current_session = Some(current);
            return Err(ProtocolError::InvalidPacket(
                "session_end date mismatch".to_owned(),
            ));
        }
        self.state.history.push_back(current);
        while self.state.history.len() > self.state.config.history_sessions {
            self.state.history.pop_front();
        }
        if let Some(oldest) = self
            .state
            .history
            .front()
            .map(|session| session.session_date.clone())
        {
            self.state
                .previous_prequential_sampled_scores
                .retain(|score| score.session_date >= oldest);
        }
        self.books = None;
        self.state.s3_states.clear();
        self.state.last_qualified.clear();
        self.state.s3_attempted.clear();
        self.state.s3_support.clear();
        self.s3_prior_cache.clear();
        Ok(())
    }

    #[allow(clippy::too_many_lines)]
    fn score_event(
        &mut self,
        event: &RawEligibleEvent,
        ts_minute: i64,
        scoring: bool,
        emit_actions: bool,
    ) -> ScoreEventOutput {
        let raw_sign = sign_of(event.raw_delta);
        let mut detector_diagnostics = Vec::with_capacity(Detector::ALL.len());
        let mut requested_detector_passed = false;
        let event_id = self
            .state
            .current_session
            .as_ref()
            .map(|session| format!("{}|{ts_minute}|{}", session.session_date, event.contract_id))
            .ok_or_else(|| {
                ProtocolError::InvalidPacket("event without active session".to_owned())
            })?;
        let mut pending_scores = Vec::new();
        for detector in Detector::ALL {
            let result = self.detector_score(detector, event);
            let threshold =
                self.threshold_for(detector, raw_sign, result.calibration_key.as_deref());
            let before = result
                .score
                .zip(threshold.value)
                .is_some_and(|(score, threshold)| score.abs() >= threshold);
            let key = format!("{}|{}", detector.id(), event.contract_id);
            let after = before
                && scoring
                && self.state.last_qualified.get(&key).is_none_or(|previous| {
                    ts_minute - *previous >= self.state.config.quiet_gap_minutes
                });
            if before {
                self.state.last_qualified.insert(key, ts_minute);
            }
            if detector == Detector::S0
                && after
                && event
                    .intent
                    .as_ref()
                    .is_some_and(|intent| intent.detector == detector)
            {
                requested_detector_passed = true;
            }
            if event.calibration_sample
                && let Some(score) = result.score.filter(|score| score.is_finite())
            {
                pending_scores.push(PrequentialScore {
                    session_date: self
                        .state
                        .current_session
                        .as_ref()
                        .map_or_else(String::new, |session| session.session_date.clone()),
                    event_id: event_id.clone(),
                    calibration_key: result
                        .calibration_key
                        .clone()
                        .unwrap_or_else(|| "UNSUPPORTED".to_owned()),
                    detector,
                    absolute_score: score.abs(),
                    raw_iv_sign: raw_sign,
                });
            }
            let prior = result
                .calibration_key
                .as_ref()
                .and_then(|key| self.severity_cache.get(&(detector, raw_sign, key.clone())));
            let prior_tail_support = prior.map_or(0, Vec::len);
            let prior_absolute_tail_probability = result.score.and_then(|score| {
                prior
                    .filter(|values| values.len() >= self.state.config.minimum_support)
                    .map(|values| 1.0 - empirical_midrank(score.abs(), values))
            });
            let empirical_two_sided_tail_probability = if detector == Detector::S2 {
                result
                    .level
                    .as_ref()
                    .and_then(|level| level.strip_prefix('L'))
                    .and_then(|level| level.parse::<usize>().ok())
                    .and_then(|level| {
                        self.books
                            .as_ref()?
                            .stats(Metric::Raw, level, event.group_keys.at(level))
                    })
                    .map(|stats| {
                        let cdf = empirical_midrank(event.raw_delta, &stats.sorted);
                        2.0 * cdf.min(1.0 - cdf)
                    })
            } else {
                None
            };
            detector_diagnostics.push(DetectorDiagnostic {
                detector,
                score: result.score,
                threshold: threshold.value,
                threshold_support: threshold.support,
                threshold_target: threshold.target,
                threshold_selected: threshold.selected,
                threshold_ties: threshold.ties,
                prior_absolute_tail_probability,
                prior_tail_support,
                empirical_two_sided_tail_probability,
                support: result.support,
                calibration_level: result.level,
                qualified_before_quiet: before,
                qualified_after_quiet: after,
                reason: result.reason.or(threshold.reason),
            });
        }
        for score in pending_scores {
            self.state
                .previous_prequential_sampled_scores
                .push_back(score);
        }
        let maybe_action = if emit_actions && requested_detector_passed {
            event
                .intent
                .as_ref()
                .map(|spec| build_trade_intent(event, ts_minute, spec, spec.detector))
        } else {
            None
        };
        let current_date = self
            .state
            .current_session
            .as_ref()
            .map(|record| record.session_date.clone())
            .ok_or_else(|| {
                ProtocolError::InvalidPacket("event without active session".to_owned())
            })?;
        let observation = event.calibration_sample.then(|| CalibrationObservation {
            session_date: current_date.clone(),
            ts_minute,
            contract_id: event.contract_id.clone(),
            raw_delta: event.raw_delta,
            log_delta: derived_log_delta(event),
            residual: event
                .residual
                .filter(|value| value.is_finite())
                .filter(|_| {
                    event.neighbor_count.unwrap_or(0) >= self.state.config.neighbor_minimum
                }),
            group_keys: event.group_keys.clone(),
        });
        let raw_history = event.raw_delta.is_finite().then(|| RawHistoryAppend {
            ts_minute,
            contract_id: event.contract_id.clone(),
            raw_delta: event.raw_delta,
        });
        Ok((
            EventDiagnostic {
                contract_id: event.contract_id.clone(),
                ts_minute,
                raw_iv_sign: raw_sign,
                inherited_sign: event.signal_metadata.inherited_sign,
                detectors: detector_diagnostics,
            },
            maybe_action,
            observation,
            raw_history,
        ))
    }

    /// Match each alternative detector's historical prequential event rate to
    /// S0 on the same raw-sign and detector-common event support.  Only scores
    /// from completed prior sessions are visible here; current-session scores
    /// are appended after all six detectors have been evaluated.
    fn threshold_for(
        &self,
        detector: Detector,
        raw_sign: i8,
        calibration_key: Option<&str>,
    ) -> ThresholdSpec {
        if detector == Detector::S0 {
            return ThresholdSpec {
                value: Some(self.state.config.threshold),
                support: 0,
                target: 0,
                selected: 0,
                ties: 0,
                reason: None,
            };
        }
        if raw_sign == 0 {
            return ThresholdSpec {
                value: None,
                support: 0,
                target: 0,
                selected: 0,
                ties: 0,
                reason: Some("zero_raw_sign".to_owned()),
            };
        }
        let Some(calibration_key) = calibration_key else {
            return ThresholdSpec {
                value: None,
                support: 0,
                target: 0,
                selected: 0,
                ties: 0,
                reason: Some("threshold_population_unselected".to_owned()),
            };
        };
        self.threshold_cache
            .get(&(detector, raw_sign, calibration_key.to_owned()))
            .cloned()
            .unwrap_or_else(|| ThresholdSpec {
                value: None,
                support: 0,
                target: 0,
                selected: 0,
                ties: 0,
                reason: Some("insufficient_threshold_support".to_owned()),
            })
    }

    fn detector_score(&mut self, detector: Detector, event: &RawEligibleEvent) -> ScoreResult {
        if detector == Detector::S3 {
            return self.score_s3(event);
        }
        let metric = detector.metric().expect("non-S3 detector has a metric");
        let value = match metric {
            Metric::Raw => event.raw_delta,
            Metric::Log => {
                let Some(value) = derived_log_delta(event) else {
                    return ScoreResult::missing("log_delta_missing");
                };
                value
            }
            Metric::Residual => {
                if event.neighbor_count.is_none() {
                    return ScoreResult::missing("neighbor_count_missing");
                }
                if event.neighbor_count < Some(self.state.config.neighbor_minimum) {
                    return ScoreResult::missing("insufficient_neighbors");
                }
                let Some(value) = event.residual else {
                    return ScoreResult::missing("residual_missing");
                };
                value
            }
        };
        if !value.is_finite() {
            return ScoreResult::missing("value_non_finite");
        }
        let book = self
            .books
            .get_or_insert_with(|| CalibrationBook::build(&self.state.history));
        let mut chosen = None;
        let mut largest_support = 0;
        for level in 0..4 {
            let key = event.group_keys.at(level);
            if key.is_empty() {
                continue;
            }
            if let Some(stats) = book.stats(metric, level, key) {
                largest_support = largest_support.max(stats.sorted.len());
                if stats.sorted.len() >= self.state.config.minimum_support
                    && detector_stats_usable(detector, stats)
                {
                    chosen = Some((level, stats));
                    break;
                }
            }
        }
        let Some((level, stats)) = chosen else {
            return ScoreResult {
                score: None,
                support: largest_support,
                level: None,
                calibration_key: None,
                reason: Some(if largest_support < self.state.config.minimum_support {
                    "insufficient_support".to_owned()
                } else {
                    "invalid_scale".to_owned()
                }),
            };
        };
        let support = stats.sorted.len();
        let calibration_key = group_key(level, event.group_keys.at(level));
        let score = match detector {
            Detector::S0 | Detector::S4 | Detector::S5 => robust_from_stats(value, stats),
            Detector::S1 => conventional_from_stats(value, stats),
            Detector::S2 => Some(midrank_from_stats(value, stats)),
            Detector::S3 => unreachable!("S3 handled above"),
        };
        score.map_or_else(
            || ScoreResult {
                score: None,
                support,
                level: Some(format!("L{level}")),
                calibration_key: Some(calibration_key.clone()),
                reason: Some("invalid_scale".to_owned()),
            },
            |score| ScoreResult {
                score: Some(score),
                support,
                level: Some(format!("L{level}")),
                calibration_key: Some(calibration_key.clone()),
                reason: None,
            },
        )
    }

    fn score_s3(&mut self, event: &RawEligibleEvent) -> ScoreResult {
        let contract_id = event.contract_id.clone();
        if !self.state.s3_attempted.contains(&contract_id) {
            let (support, initial_state) = self
                .s3_prior_cache
                .get(&contract_id)
                .map_or((0, None), |seed| (seed.support, seed.state.clone()));
            self.state.s3_support.insert(contract_id.clone(), support);
            if let Some(state) = initial_state {
                self.state.s3_states.insert(contract_id.clone(), state);
            }
            self.state.s3_attempted.insert(contract_id.clone());
        }
        let support = *self.state.s3_support.get(&contract_id).unwrap_or(&0);
        let threshold_key = self
            .calibration_key_for_raw(event)
            .unwrap_or_else(|| format!("PER_CONTRACT|{contract_id}"));
        let Some(state) = self.state.s3_states.get_mut(&contract_id) else {
            return ScoreResult {
                score: None,
                support,
                level: Some("PER_CONTRACT".to_owned()),
                calibration_key: Some(threshold_key),
                reason: (support < self.state.config.minimum_support)
                    .then_some("insufficient_support".to_owned())
                    .or_else(|| Some("invalid_scale".to_owned())),
            };
        };
        let score =
            if event.raw_delta.is_finite() && state.variance.is_finite() && state.variance > 0.0 {
                Some((event.raw_delta - state.mean) / state.variance.sqrt())
            } else {
                None
            };
        let reason = score.is_none().then_some("invalid_scale".to_owned());
        if event.raw_delta.is_finite() {
            ewma_update(state, event.raw_delta, self.state.config.s3_alpha);
        }
        ScoreResult {
            score,
            support,
            level: Some("PER_CONTRACT".to_owned()),
            calibration_key: Some(threshold_key),
            reason,
        }
    }

    fn calibration_key_for_raw(&self, event: &RawEligibleEvent) -> Option<String> {
        let book = self.books.as_ref()?;
        for level in 0..4 {
            let value = event.group_keys.at(level);
            if value.is_empty() {
                continue;
            }
            if let Some(stats) = book.stats(Metric::Raw, level, value)
                && stats.sorted.len() >= self.state.config.minimum_support
                && detector_stats_usable(Detector::S0, stats)
            {
                return Some(group_key(level, value));
            }
        }
        None
    }
}

fn append_raw_history(session: &mut SessionRecord, observation: RawHistoryAppend) {
    session
        .raw_by_contract
        .entry(observation.contract_id)
        .or_default()
        .push((observation.ts_minute, observation.raw_delta));
}

#[derive(Debug)]
struct ScoreResult {
    score: Option<f64>,
    support: usize,
    level: Option<String>,
    calibration_key: Option<String>,
    reason: Option<String>,
}

impl ScoreResult {
    fn missing(reason: &str) -> Self {
        Self {
            score: None,
            support: 0,
            level: None,
            calibration_key: None,
            reason: Some(reason.to_owned()),
        }
    }
}

fn robust_from_stats(value: f64, stats: &SeriesStats) -> Option<f64> {
    (stats.mad.is_finite() && stats.mad > 0.0)
        .then_some((value - stats.median) / (1.4826 * stats.mad))
        .filter(|score| score.is_finite())
}

fn detector_stats_usable(detector: Detector, stats: &SeriesStats) -> bool {
    match detector {
        Detector::S0 | Detector::S4 | Detector::S5 => stats.mad.is_finite() && stats.mad > 0.0,
        Detector::S1 => stats.sample_sd.is_finite() && stats.sample_sd > 0.0,
        Detector::S2 => true,
        Detector::S3 => false,
    }
}

fn conventional_from_stats(value: f64, stats: &SeriesStats) -> Option<f64> {
    (stats.sample_sd.is_finite() && stats.sample_sd > 0.0)
        .then_some((value - stats.mean) / stats.sample_sd)
        .filter(|score| score.is_finite())
}

fn empirical_midrank(value: f64, sorted: &[f64]) -> f64 {
    let below = sorted.partition_point(|x| *x < value);
    let above = sorted.partition_point(|x| *x <= value);
    let probability = (below as f64 + (above - below) as f64 / 2.0) / sorted.len() as f64;
    let epsilon = 0.5 / sorted.len() as f64;
    probability.clamp(epsilon, 1.0 - epsilon)
}
fn midrank_from_stats(value: f64, stats: &SeriesStats) -> f64 {
    normal_inverse_cdf(empirical_midrank(value, &stats.sorted))
}

fn derived_log_delta(event: &RawEligibleEvent) -> Option<f64> {
    let current = event.calendar_iv.filter(|v| v.is_finite() && *v > 0.0)?;
    let previous = event
        .previous_calendar_iv
        .filter(|v| v.is_finite() && *v > 0.0)?;
    let derived = (current / previous).ln();
    if !derived.is_finite() {
        return None;
    }
    match event.log_delta {
        Some(value)
            if value.is_finite() && (value - derived).abs() <= 1e-10 * (1.0 + derived.abs()) =>
        {
            Some(value)
        }
        Some(_) => None,
        None => Some(derived),
    }
}

fn sign_of(value: f64) -> i8 {
    if value > 0.0 {
        1
    } else if value < 0.0 {
        -1
    } else {
        0
    }
}

const fn detector_index(detector: Detector) -> usize {
    match detector {
        Detector::S0 => 0,
        Detector::S1 => 1,
        Detector::S2 => 2,
        Detector::S3 => 3,
        Detector::S4 => 4,
        Detector::S5 => 5,
    }
}

fn build_trade_intent(
    event: &RawEligibleEvent,
    ts_minute: i64,
    spec: &IntentSpec,
    detector: Detector,
) -> TradeIntent {
    let fallback = format!("{}-{detector:?}-{ts_minute}", event.contract_id);
    TradeIntent {
        schema_version: CONTRACT_VERSION.to_owned(),
        intent_id: spec
            .intent_id
            .clone()
            .unwrap_or_else(|| format!("h5-{fallback}")),
        decision_id: spec
            .decision_id
            .clone()
            .unwrap_or_else(|| format!("h5-decision-{fallback}")),
        strategy_position_id: spec
            .strategy_position_id
            .clone()
            .unwrap_or_else(|| format!("h5-position-{fallback}")),
        basket_key: spec
            .basket_key
            .clone()
            .unwrap_or_else(|| "h5-iv-shocks".to_owned()),
        action: spec.action,
        atomic: spec.atomic,
        legs: vec![IntentLeg {
            instrument_id: spec
                .instrument_id
                .clone()
                .unwrap_or_else(|| event.contract_id.clone()),
            side: spec.side,
            quantity: spec.quantity,
            limit_price: spec.limit_price,
        }],
        lineage: BTreeMap::from([
            ("detector".to_owned(), detector.id().to_owned()),
            ("source_contract_id".to_owned(), event.contract_id.clone()),
            (
                "signal_raw_iv_sign".to_owned(),
                sign_of(event.raw_delta).to_string(),
            ),
        ]),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_detectors_are_stable() {
        assert_eq!(
            Detector::ALL.map(Detector::id),
            ["S0", "S1", "S2", "S3", "S4", "S5"]
        );
    }

    #[test]
    fn formulas_match_known_values() {
        let samples: Vec<f64> = (0..200).map(f64::from).collect();
        let robust = robust_mad_score(205.0, &samples).expect("nonzero MAD");
        assert!((robust - (205.0 - 99.5) / (1.4826 * 50.0)).abs() < 1e-12);
        assert!(robust_mad_score(250.0, &samples).unwrap().abs() >= 2.0);
        assert!(robust_mad_score(247.0, &samples).unwrap().abs() < 2.0);
        let classical = sample_sd_score(205.0, &samples).expect("nonzero SD");
        assert!(classical > 1.8 && classical < 1.9);
        let rank = midrank_tail_score(199.0, &samples).expect("rank");
        assert!(rank > 2.5);
        let state = ewma_initialize(&samples, 1.0 - (-1.0_f64 / 60.0).exp2(), 200).expect("EWMA");
        assert_eq!(state.updates, 200);
    }

    #[test]
    fn invalid_scales_and_ties_fail_closed() {
        assert!(robust_mad_score(1.0, &[1.0; 200]).is_none());
        assert!(sample_sd_score(1.0, &[1.0; 200]).is_none());
        assert_eq!(midrank_tail_score(1.0, &[1.0; 200]), Some(0.0));
    }

    #[test]
    fn s3_uses_all_sorted_prior_history_and_scores_before_update() {
        let mut runner = Runner::new(RunnerConfig {
            minimum_support: 2,
            ..RunnerConfig::default()
        });
        runner.state.history.push_back(SessionRecord {
            last_minute: None,
            session_date: "2024-01-01".to_owned(),
            scoring: true,
            observations: Vec::new(),
            raw_by_contract: BTreeMap::new(),
            eligible_raw_history: vec![
                RawHistoryObservation {
                    session_date: "2024-01-01".to_owned(),
                    ts_minute: 601,
                    contract_id: "c".to_owned(),
                    raw_delta: 100.0,
                },
                RawHistoryObservation {
                    session_date: "2024-01-01".to_owned(),
                    ts_minute: 602,
                    contract_id: "c".to_owned(),
                    raw_delta: 200.0,
                },
                RawHistoryObservation {
                    session_date: "2024-01-01".to_owned(),
                    ts_minute: 600,
                    contract_id: "c".to_owned(),
                    raw_delta: 0.0,
                },
            ],
        });
        runner.state.current_session = Some(SessionRecord {
            last_minute: None,
            session_date: "2024-01-02".to_owned(),
            scoring: true,
            observations: Vec::new(),
            raw_by_contract: BTreeMap::new(),
            eligible_raw_history: Vec::new(),
        });
        runner.rebuild_s3_prior_cache().unwrap();

        let event = RawEligibleEvent {
            contract_id: "c".to_owned(),
            raw_delta: 300.0,
            log_delta: None,
            residual: None,
            calendar_iv: None,
            previous_calendar_iv: None,
            calibration_sample: false,
            group_keys: GroupKeys::default(),
            signal_metadata: SignalMetadata::default(),
            neighbor_count: None,
            intent: None,
        };
        let result = runner.score_s3(&event);
        let alpha = runner.state.config.s3_alpha;
        let mut expected = EwmaState {
            mean: 50.0,
            variance: 5_000.0,
            updates: 2,
        };
        ewma_update(&mut expected, 200.0, alpha);
        let expected_score = (300.0 - expected.mean) / expected.variance.sqrt();
        ewma_update(&mut expected, 300.0, alpha);

        assert_eq!(result.support, 3);
        assert!((result.score.expect("S3 score") - expected_score).abs() < 1e-12);
        let actual = runner.state.s3_states.get("c").expect("S3 state");
        assert_eq!(actual.updates, expected.updates);
        assert!((actual.mean - expected.mean).abs() < 1e-12);
    }

    fn s3_fixture_runner(
        raw_by_contract: BTreeMap<String, Vec<(i64, f64)>>,
        legacy: Vec<RawHistoryObservation>,
    ) -> Runner {
        let mut runner = Runner::new(RunnerConfig {
            minimum_support: 2,
            ..RunnerConfig::default()
        });
        runner.state.history.push_back(SessionRecord {
            last_minute: None,
            session_date: "2024-01-01".to_owned(),
            scoring: true,
            observations: Vec::new(),
            raw_by_contract,
            eligible_raw_history: legacy,
        });
        runner.state.current_session = Some(SessionRecord {
            last_minute: None,
            session_date: "2024-01-02".to_owned(),
            scoring: true,
            observations: Vec::new(),
            raw_by_contract: BTreeMap::new(),
            eligible_raw_history: Vec::new(),
        });
        runner.rebuild_s3_prior_cache().unwrap();
        runner
    }

    fn s3_fixture_event(raw_delta: f64) -> RawEligibleEvent {
        RawEligibleEvent {
            contract_id: "c".to_owned(),
            raw_delta,
            log_delta: None,
            residual: None,
            calendar_iv: None,
            previous_calendar_iv: None,
            calibration_sample: false,
            group_keys: GroupKeys::default(),
            signal_metadata: SignalMetadata::default(),
            neighbor_count: None,
            intent: None,
        }
    }

    #[test]
    fn compact_and_legacy_s3_history_have_identical_scores() {
        let compact = s3_fixture_runner(
            BTreeMap::from([("c".to_owned(), vec![(600, 0.0), (601, 100.0), (602, 200.0)])]),
            Vec::new(),
        );
        let legacy = s3_fixture_runner(
            BTreeMap::new(),
            vec![
                RawHistoryObservation {
                    session_date: "2024-01-01".to_owned(),
                    ts_minute: 602,
                    contract_id: "c".to_owned(),
                    raw_delta: 200.0,
                },
                RawHistoryObservation {
                    session_date: "2024-01-01".to_owned(),
                    ts_minute: 600,
                    contract_id: "c".to_owned(),
                    raw_delta: 0.0,
                },
                RawHistoryObservation {
                    session_date: "2024-01-01".to_owned(),
                    ts_minute: 601,
                    contract_id: "c".to_owned(),
                    raw_delta: 100.0,
                },
            ],
        );
        let mut compact = compact;
        let mut legacy = legacy;
        let event = s3_fixture_event(300.0);
        let compact_result = compact.score_s3(&event);
        let legacy_result = legacy.score_s3(&event);
        assert_eq!(compact_result.support, legacy_result.support);
        assert_eq!(compact_result.score, legacy_result.score);
        let compact_state = &compact.state.s3_states["c"];
        let legacy_state = &legacy.state.s3_states["c"];
        assert_eq!(compact_state.updates, legacy_state.updates);
        assert!((compact_state.mean - legacy_state.mean).abs() < f64::EPSILON);
        assert!((compact_state.variance - legacy_state.variance).abs() < f64::EPSILON);
        assert!(legacy.state.history[0].eligible_raw_history.is_empty());
        assert_eq!(
            legacy.state.history[0].raw_by_contract["c"],
            vec![(600, 0.0), (601, 100.0), (602, 200.0)]
        );
    }

    #[test]
    fn compact_s3_zero_scale_fails_closed() {
        let mut runner = s3_fixture_runner(
            BTreeMap::from([("c".to_owned(), vec![(600, 1.0), (601, 1.0)])]),
            Vec::new(),
        );
        let result = runner.score_s3(&s3_fixture_event(1.0));
        assert_eq!(result.support, 2);
        assert!(result.score.is_none());
        assert_eq!(result.reason.as_deref(), Some("invalid_scale"));
        assert!(runner.state.s3_states["c"].variance.abs() < f64::EPSILON);
    }

    #[test]
    fn old_checkpoint_migrates_legacy_raw_history_and_clears_it() {
        let mut old_state = RunnerState::new(RunnerConfig {
            minimum_support: 2,
            ..RunnerConfig::default()
        });
        old_state.history.push_back(SessionRecord {
            last_minute: None,
            session_date: "2024-01-01".to_owned(),
            scoring: true,
            observations: Vec::new(),
            raw_by_contract: BTreeMap::new(),
            eligible_raw_history: vec![
                RawHistoryObservation {
                    session_date: "2024-01-01".to_owned(),
                    ts_minute: 602,
                    contract_id: "c".to_owned(),
                    raw_delta: 200.0,
                },
                RawHistoryObservation {
                    session_date: "2024-01-01".to_owned(),
                    ts_minute: 600,
                    contract_id: "c".to_owned(),
                    raw_delta: 0.0,
                },
                RawHistoryObservation {
                    session_date: "2024-01-01".to_owned(),
                    ts_minute: 601,
                    contract_id: "c".to_owned(),
                    raw_delta: 100.0,
                },
            ],
        });
        old_state.current_session = Some(SessionRecord {
            last_minute: Some(600),
            session_date: "2024-01-02".to_owned(),
            scoring: true,
            observations: Vec::new(),
            raw_by_contract: BTreeMap::new(),
            eligible_raw_history: Vec::new(),
        });
        let mut encoded = serde_json::to_value(old_state).unwrap();
        for key in ["history", "current_session"] {
            if let Some(value) = encoded.get_mut(key) {
                if let Some(records) = value.as_array_mut() {
                    for record in records {
                        record
                            .as_object_mut()
                            .expect("session object")
                            .remove("raw_by_contract");
                    }
                } else if let Some(record) = value.as_object_mut() {
                    record.remove("raw_by_contract");
                }
            }
        }
        let path = std::env::temp_dir().join(format!(
            "h5-compact-legacy-{}-{}.json",
            std::process::id(),
            std::thread::current().name().unwrap_or("test")
        ));
        std::fs::write(&path, serde_json::to_vec(&encoded).unwrap()).unwrap();
        let mut restored = Runner::default();
        restored.restore_checkpoint(path.to_str().unwrap()).unwrap();
        std::fs::remove_file(path).unwrap();
        let session = &restored.state.history[0];
        assert!(session.eligible_raw_history.is_empty());
        assert_eq!(
            session.raw_by_contract["c"],
            vec![(600, 0.0), (601, 100.0), (602, 200.0)]
        );
        let checkpoint = serde_json::to_value(restored.state()).unwrap();
        assert!(
            checkpoint["history"][0]
                .get("eligible_raw_history")
                .is_none()
        );
        let result = restored.score_s3(&s3_fixture_event(300.0));
        assert_eq!(result.support, 3);
        assert!(result.score.is_some());
    }

    #[test]
    fn compact_history_rejects_duplicate_contract_timestamp() {
        let mut state = RunnerState::new(RunnerConfig::default());
        state.history.push_back(SessionRecord {
            last_minute: None,
            session_date: "2024-01-01".to_owned(),
            scoring: true,
            observations: Vec::new(),
            raw_by_contract: BTreeMap::from([("c".to_owned(), vec![(600, 1.0), (600, 2.0)])]),
            eligible_raw_history: Vec::new(),
        });
        let error = migrate_legacy_raw_history(&mut state).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("duplicate raw history timestamp")
        );
    }

    fn write_bundle_checkpoint(bundle_hash: &str, tag: &str) -> std::path::PathBuf {
        let state = RunnerState::new(RunnerConfig {
            bundle_hash: bundle_hash.to_owned(),
            ..RunnerConfig::default()
        });
        let path =
            std::env::temp_dir().join(format!("h5-bundle-{}-{tag}.json", std::process::id()));
        std::fs::write(&path, serde_json::to_vec(&state).unwrap()).unwrap();
        path
    }

    #[test]
    fn cross_bundle_restore_requires_exact_source_allowlist() {
        let path = write_bundle_checkpoint("old-bundle", "reject");
        let mut runner = Runner::new(RunnerConfig {
            bundle_hash: "new-bundle".to_owned(),
            ..RunnerConfig::default()
        });
        let error = runner
            .restore_checkpoint_with_expected_source_bundle_hash(path.to_str().unwrap(), None)
            .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("IV_SHOCK_CHECKPOINT_SOURCE_BUNDLE_HASH")
        );
        assert_eq!(runner.state.config.bundle_hash, "new-bundle");
        assert!(runner.state.checkpoint_source_bundle_hash.is_none());
        let mut wrong = Runner::new(RunnerConfig {
            bundle_hash: "new-bundle".to_owned(),
            ..RunnerConfig::default()
        });
        assert!(
            wrong
                .restore_checkpoint_with_expected_source_bundle_hash(
                    path.to_str().unwrap(),
                    Some("wrong-bundle"),
                )
                .is_err()
        );
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn cross_bundle_restore_keeps_active_hash_and_reports_source_hash() {
        let path = write_bundle_checkpoint("old-bundle", "accept");
        let mut runner = Runner::new(RunnerConfig {
            bundle_hash: "new-bundle".to_owned(),
            ..RunnerConfig::default()
        });
        runner
            .restore_checkpoint_with_expected_source_bundle_hash(
                path.to_str().unwrap(),
                Some("old-bundle"),
            )
            .unwrap();
        std::fs::remove_file(path).unwrap();
        assert_eq!(runner.state.config.bundle_hash, "new-bundle");
        assert_eq!(
            runner.state.checkpoint_source_bundle_hash.as_deref(),
            Some("old-bundle")
        );
        let metadata = runner.response_state().unwrap();
        assert_eq!(metadata["config"]["bundle_hash"], "new-bundle");
        assert_eq!(metadata["checkpoint_source_bundle_hash"], "old-bundle");
    }

    #[test]
    fn same_bundle_restore_needs_no_source_allowlist() {
        let path = write_bundle_checkpoint("same-bundle", "same");
        let mut runner = Runner::new(RunnerConfig {
            bundle_hash: "same-bundle".to_owned(),
            ..RunnerConfig::default()
        });
        runner
            .restore_checkpoint_with_expected_source_bundle_hash(path.to_str().unwrap(), None)
            .unwrap();
        std::fs::remove_file(path).unwrap();
        assert_eq!(runner.state.config.bundle_hash, "same-bundle");
        assert!(runner.state.checkpoint_source_bundle_hash.is_none());
        assert!(runner.response_state().unwrap()["checkpoint_source_bundle_hash"].is_null());
    }

    #[test]
    fn fallback_skips_unsupported_finer_scale() {
        let mut runner = Runner::new(RunnerConfig {
            minimum_support: 2,
            ..RunnerConfig::default()
        });
        let group_keys = GroupKeys {
            l0: "invalid-a".to_owned(),
            l1: "usable".to_owned(),
            l2: String::new(),
            l3: String::new(),
        };
        runner.state.history.push_back(SessionRecord {
            last_minute: None,
            session_date: "2024-01-01".to_owned(),
            scoring: true,
            observations: vec![
                CalibrationObservation {
                    session_date: "2024-01-01".to_owned(),
                    ts_minute: 600,
                    contract_id: "a".to_owned(),
                    raw_delta: 1.0,
                    log_delta: Some(1.0),
                    residual: Some(1.0),
                    group_keys,
                },
                CalibrationObservation {
                    session_date: "2024-01-01".to_owned(),
                    ts_minute: 601,
                    contract_id: "b".to_owned(),
                    raw_delta: 2.0,
                    log_delta: Some(2.0),
                    residual: Some(2.0),
                    group_keys: GroupKeys {
                        l0: "invalid-b".to_owned(),
                        l1: "usable".to_owned(),
                        l2: String::new(),
                        l3: String::new(),
                    },
                },
            ],
            raw_by_contract: BTreeMap::new(),
            eligible_raw_history: Vec::new(),
        });
        runner.books = Some(CalibrationBook::build(&runner.state.history));
        let event = RawEligibleEvent {
            contract_id: "c".to_owned(),
            raw_delta: 3.0,
            log_delta: Some(3.0),
            residual: Some(3.0),
            calendar_iv: None,
            previous_calendar_iv: None,
            calibration_sample: false,
            group_keys: GroupKeys {
                l0: "invalid-a".to_owned(),
                l1: "usable".to_owned(),
                l2: String::new(),
                l3: String::new(),
            },
            signal_metadata: SignalMetadata::default(),
            neighbor_count: Some(3),
            intent: None,
        };
        let result = runner.detector_score(Detector::S1, &event);
        assert_eq!(result.level.as_deref(), Some("L1"));
        assert_eq!(result.support, 2);
        assert!(result.score.is_some());
    }

    #[test]
    fn threshold_cache_is_prior_only_and_reports_ties() {
        let mut runner = Runner::new(RunnerConfig {
            minimum_support: 2,
            ..RunnerConfig::default()
        });
        runner.state.current_session = Some(SessionRecord {
            last_minute: None,
            session_date: "2024-01-03".to_owned(),
            scoring: true,
            observations: Vec::new(),
            raw_by_contract: BTreeMap::new(),
            eligible_raw_history: Vec::new(),
        });
        let key = "L0|population".to_owned();
        runner.state.previous_prequential_sampled_scores.extend([
            PrequentialScore {
                session_date: "2024-01-01".to_owned(),
                event_id: "e1".to_owned(),
                calibration_key: key.clone(),
                detector: Detector::S0,
                absolute_score: 3.0,
                raw_iv_sign: 1,
            },
            PrequentialScore {
                session_date: "2024-01-01".to_owned(),
                event_id: "e1".to_owned(),
                calibration_key: key.clone(),
                detector: Detector::S1,
                absolute_score: 4.0,
                raw_iv_sign: 1,
            },
            PrequentialScore {
                session_date: "2024-01-02".to_owned(),
                event_id: "e2".to_owned(),
                calibration_key: key.clone(),
                detector: Detector::S0,
                absolute_score: 1.0,
                raw_iv_sign: 1,
            },
            PrequentialScore {
                session_date: "2024-01-02".to_owned(),
                event_id: "e2".to_owned(),
                calibration_key: key.clone(),
                detector: Detector::S1,
                absolute_score: 2.0,
                raw_iv_sign: 1,
            },
            PrequentialScore {
                session_date: "2024-01-03".to_owned(),
                event_id: "current".to_owned(),
                calibration_key: key.clone(),
                detector: Detector::S0,
                absolute_score: 3.0,
                raw_iv_sign: 1,
            },
            PrequentialScore {
                session_date: "2024-01-03".to_owned(),
                event_id: "current".to_owned(),
                calibration_key: key.clone(),
                detector: Detector::S1,
                absolute_score: 100.0,
                raw_iv_sign: 1,
            },
        ]);
        runner.build_threshold_cache();
        let threshold = runner.threshold_for(Detector::S1, 1, Some(&key));
        assert_eq!(threshold.support, 2);
        assert_eq!(threshold.target, 1);
        assert_eq!(threshold.value, Some(4.0));
        assert_eq!(threshold.selected, 1);
        assert_eq!(threshold.ties, 1);
        let prior = &runner.severity_cache[&(Detector::S1, 1, key)];
        assert_eq!(prior, &vec![2.0, 4.0], "current-session severity excluded");
        assert!((1.0 - empirical_midrank(4.0, prior) - 0.25).abs() < 1e-14);
        assert!((empirical_midrank(2.0, &[1.0, 2.0, 2.0, 4.0]) - 0.5).abs() < 1e-14);
        assert!((1.0 - empirical_midrank(9.0, &[1.0, 2.0, 2.0, 4.0]) - 0.125).abs() < 1e-14);
    }

    #[test]
    fn residual_detector_requires_neighbor_support() {
        let mut runner = Runner::default();
        let event = RawEligibleEvent {
            contract_id: "c".to_owned(),
            raw_delta: 1.0,
            log_delta: None,
            residual: Some(1.0),
            calendar_iv: None,
            previous_calendar_iv: None,
            calibration_sample: false,
            group_keys: GroupKeys::default(),
            signal_metadata: SignalMetadata::default(),
            neighbor_count: Some(MINIMUM_NEIGHBORS - 1),
            intent: None,
        };
        let result = runner.detector_score(Detector::S5, &event);
        assert_eq!(result.reason.as_deref(), Some("insufficient_neighbors"));
    }
    #[test]
    fn log_metric_requires_positive_consistent_endpoints() {
        let mut event: RawEligibleEvent = serde_json::from_value(
            serde_json::json!({"contract_id":"c","raw_delta":1.0,"log_delta":0.5}),
        )
        .unwrap();
        assert!(derived_log_delta(&event).is_none());
        event.calendar_iv = Some(2.0);
        event.previous_calendar_iv = Some(1.0);
        assert!(
            derived_log_delta(&event).is_none(),
            "inconsistent supplied log rejected"
        );
        event.log_delta = Some(2.0_f64.ln());
        assert_eq!(derived_log_delta(&event), event.log_delta);
        event.previous_calendar_iv = Some(0.0);
        assert!(derived_log_delta(&event).is_none());
        let mut runner = Runner::default();
        runner.state.current_session = Some(SessionRecord {
            last_minute: None,
            session_date: "2024-01-01".into(),
            scoring: false,
            observations: vec![],
            raw_by_contract: BTreeMap::new(),
            eligible_raw_history: vec![],
        });
        event.calibration_sample = true;
        let (_, _, sample, _) = runner.score_event(&event, 600, false, false).unwrap();
        assert!(sample.unwrap().log_delta.is_none());
    }
}
