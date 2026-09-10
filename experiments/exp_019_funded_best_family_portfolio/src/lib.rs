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
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

pub const RUNNER_ID: &str = "exp019-funded-best-family-portfolio-v1";
pub const SCHEMA_VERSION: &str = "gdc.exp019.funded-best-family-portfolio.v1";
pub const MINUTE_NS: i64 = 60_000_000_000;
pub const FIXED_BOOKS: [&str; 5] = ["H3_F1", "H3_F2", "H5_F2", "H5_F3", "H5_F4"];

fn book_rank(book: &str) -> Option<usize> {
    FIXED_BOOKS.iter().position(|fixed| *fixed == book)
}

fn default_books() -> Vec<String> {
    FIXED_BOOKS.iter().map(|book| (*book).to_owned()).collect()
}

fn default_scenarios() -> Vec<String> {
    vec!["baseline".to_owned()]
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default = "default_books")]
    pub books: Vec<String>,
    #[serde(default = "default_scenarios")]
    pub scenarios: Vec<String>,
}

impl Config {
    #[must_use]
    pub fn baseline() -> Self {
        Self {
            books: default_books(),
            scenarios: default_scenarios(),
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
    pub date: String,
    pub event_minute: i64,
    pub entry_minute: i64,
    pub scheduled_exit_minute: i64,
    pub contract_id: String,
    pub quantity: u64,
    pub entry_eligible: bool,
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
        #[serde(default)]
        session_date: Option<String>,
        #[serde(default)]
        bar_minute: Option<i64>,
        #[serde(default)]
        candidates: Vec<Candidate>,
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
        Ok(())
    }

    fn position(candidate: &Candidate) -> Position {
        let strategy_position_id = format!(
            "exp019|{}|{}|{}",
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
            quantity: candidate.quantity,
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
                ("experiment".to_owned(), "exp019".to_owned()),
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
                ("experiment".to_owned(), "exp019".to_owned()),
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
        minute: i64,
        mut candidates: Vec<Candidate>,
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
            let occupied = {
                let state = self
                    .books
                    .get(&candidate.book)
                    .ok_or_else(|| "missing book".to_owned())?;
                state.halted || state.active.is_some() || state.pending.is_some()
            };
            let reason = if !candidate.entry_eligible {
                Some("entry_ineligible")
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
        let result = self
            .apply_feedback(feedback)
            .and_then(|()| self.process_minute_inner(minute, candidates));
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
            if let Some(position) = &state.active
                && (position.book != *book
                    || position.strategy_position_id.is_empty()
                    || position.entry_minute >= position.scheduled_exit_minute
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
            if state.halted && !persisted.halted {
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
        };
        if minute != request.input.decision_at_ns.div_euclid(MINUTE_NS) {
            return Err("packet minute differs from decision clock".to_owned());
        }
        let snapshot = self.clone();
        let result = self
            .restore_state(&request.state, request.sequence)
            .and_then(|()| {
                let Packet::Minute {
                    minute, candidates, ..
                } = packet;
                if request.sequence > 0 {
                    self.apply_feedback(&request.feedback)?;
                    self.last_feedback_sequence = Some(request.sequence - 1);
                }
                let (detail, actions) = self.process_minute_inner(minute, candidates)?;
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

    fn candidate(id: &str, book: &str, minute: i64, legs: Vec<Leg>) -> Candidate {
        Candidate {
            candidate_id: id.to_owned(),
            book: book.to_owned(),
            scenario: "baseline".to_owned(),
            structure_id: "single_leg".to_owned(),
            source_event_id: format!("source-{id}"),
            date: "2026-01-02".to_owned(),
            event_minute: minute - 1,
            entry_minute: minute,
            scheduled_exit_minute: minute + 10,
            contract_id: legs[0].contract_id.clone(),
            quantity: legs[0].quantity,
            entry_eligible: true,
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
                    session_date: None,
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
    fn multi_leg_open_and_reverse_close_are_atomic() {
        let mut runner = Runner::default();
        let (_, open) = runner
            .process_minute(
                100,
                vec![candidate(
                    "a",
                    "H3_F1",
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
                vec![candidate("a", "H3_F1", 100, vec![leg("A", Side::Sell, 1)])],
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
                vec![candidate("a", "H3_F1", 100, vec![leg("A", Side::Sell, 1)])],
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
                vec![candidate("b", "H3_F1", 111, vec![leg("B", Side::Sell, 1)])],
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
    fn five_books_have_independent_occupancy() {
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
            .expect("five books");
        assert_eq!(actions.len(), 5);
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
}
