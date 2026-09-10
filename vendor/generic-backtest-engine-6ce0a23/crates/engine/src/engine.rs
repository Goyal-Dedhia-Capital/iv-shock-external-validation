use std::collections::{BTreeMap, BTreeSet};
use std::io::{BufWriter, Write};

use backtest_contracts::{
    AccountState, CONTRACT_VERSION, EngineFeedback, ExecutionOutcome, IntentAction,
    LifecycleEvidence, Money, OutcomeStatus, ResearchRequest, ResearchResponse, SealedEvent, Side,
    TradeIntent,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use thiserror::Error;

use crate::accounting::checked_product;
use crate::policies::{FeeContext, PortfolioMargin, apply_costs_with_context};
use crate::{AccountLedger, CostModel, ExecutionModel, MarginProvider, PricingPolicy};

pub trait DecisionRunner {
    /// Processes exactly one ordered decision request.
    ///
    /// # Errors
    ///
    /// Returns a diagnostic when the research process cannot produce a response.
    fn decide(&mut self, request: &ResearchRequest) -> Result<ResearchResponse, String>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct EngineConfig {
    pub initial_cash: Money,
}

/// Controls whether the engine asks its margin provider for a complete
/// portfolio total. Legacy engines keep the original per-position margin
/// behavior by default.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum PortfolioMarginMode {
    #[default]
    Disabled,
    Required,
}

/// Optional admission limits for a funded replay.
///
/// Utilization is projected marked margin (current reserved margin plus the
/// intent's incremental requirement) divided by current marked equity after
/// estimated fees, represented in basis points. Equity is the ledger's marked
/// equity, not cash; premium cash is therefore not treated as additional
/// equity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct RiskLimits {
    pub max_margin_utilization_bps: u32,
}

impl RiskLimits {
    #[must_use]
    pub const fn new(max_margin_utilization_bps: u32) -> Self {
        Self {
            max_margin_utilization_bps,
        }
    }
}

/// Non-strategy engine extensions. The default preserves the original engine
/// behavior and constructor compatibility.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct EngineOptions {
    #[serde(default)]
    pub portfolio_margin: PortfolioMarginMode,
    #[serde(default)]
    pub risk_limits: Option<RiskLimits>,
    /// Optional freshness bound for held accounting marks during new-risk
    /// admission. `None` retains the legacy availability-only behavior.
    #[serde(default)]
    pub max_accounting_mark_age_ns: Option<u64>,
}

impl Default for EngineOptions {
    fn default() -> Self {
        Self {
            portfolio_margin: PortfolioMarginMode::Disabled,
            risk_limits: None,
            max_accounting_mark_age_ns: None,
        }
    }
}

#[derive(Debug, Error)]
pub enum EngineError {
    #[error("arithmetic overflow")]
    ArithmeticOverflow,
    #[error("invalid event: {0}")]
    InvalidEvent(String),
    #[error("invalid research response: {0}")]
    InvalidResearchResponse(String),
    #[error("research process failed: {0}")]
    Research(String),
    #[error("invalid intent: {0}")]
    InvalidIntent(String),
    #[error("invalid margin amount: {0:?}")]
    InvalidMargin(Money),
    #[error("invalid cost amount: {0:?}")]
    InvalidCost(Money),
    #[error("invalid engine configuration: {0}")]
    InvalidConfig(String),
    #[error("serialization failed: {0}")]
    Serialization(#[from] serde_json::Error),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EngineSnapshot {
    pub schema_version: String,
    pub config: EngineConfig,
    pub last_sequence: Option<u64>,
    pub research_state: Value,
    pub feedback: EngineFeedback,
    pub ledger: AccountLedger,
    pub seen_intent_ids: BTreeSet<String>,
    #[serde(default)]
    pub options: EngineOptions,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StepResult {
    pub evidence: LifecycleEvidence,
    pub feedback: EngineFeedback,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReplayResult {
    pub final_account: AccountState,
    pub evidence: Vec<LifecycleEvidence>,
}

/// Owns a request while its state is temporarily moved out of the engine.
///
/// Keeping the request in this guard means an early `Result` return, or even
/// an unwind from a user-supplied decision runner, restores the prior state
/// before the engine is observed again.
struct RequestStateGuard<'a> {
    target: &'a mut Value,
    request: Option<ResearchRequest>,
}

impl<'a> RequestStateGuard<'a> {
    const fn new(target: &'a mut Value, request: ResearchRequest) -> Self {
        Self {
            target,
            request: Some(request),
        }
    }

    const fn request(&self) -> &ResearchRequest {
        self.request
            .as_ref()
            .expect("request state guard must own a request")
    }

    fn finish(mut self) -> ResearchRequest {
        let mut request = self
            .request
            .take()
            .expect("request state guard must own a request");
        *self.target = std::mem::replace(&mut request.state, Value::Null);
        request
    }
}

impl Drop for RequestStateGuard<'_> {
    fn drop(&mut self) {
        if let Some(request) = self.request.take() {
            *self.target = request.state;
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum MarginRefreshState {
    Disabled,
    Available,
    Missing(String),
}

impl MarginRefreshState {
    const fn allows_new_risk(&self) -> bool {
        matches!(self, Self::Disabled | Self::Available)
    }

    fn blocker(&self) -> Option<&str> {
        match self {
            Self::Missing(reason) => Some(reason),
            Self::Disabled | Self::Available => None,
        }
    }
}

pub struct Engine<P, C, M, X> {
    config: EngineConfig,
    options: EngineOptions,
    pricing: P,
    costs: C,
    margin: M,
    execution: X,
    ledger: AccountLedger,
    last_sequence: Option<u64>,
    research_state: Value,
    feedback: EngineFeedback,
    evidence: Vec<LifecycleEvidence>,
    seen_intent_ids: BTreeSet<String>,
}

impl<P, C, M, X> Engine<P, C, M, X>
where
    P: PricingPolicy,
    C: CostModel,
    M: MarginProvider,
    X: ExecutionModel,
{
    /// Creates a new empty engine.
    ///
    /// # Errors
    ///
    /// Returns an error when initial capital is negative.
    pub fn new(
        config: EngineConfig,
        pricing: P,
        costs: C,
        margin: M,
        execution: X,
    ) -> Result<Self, EngineError> {
        if config.initial_cash.0 < 0 {
            return Err(EngineError::InvalidConfig(
                "initial cash must be non-negative".to_owned(),
            ));
        }
        let ledger = AccountLedger::new(config.initial_cash);
        let account = AccountState {
            cash: config.initial_cash,
            reserved_margin: Money::ZERO,
            realized_pnl: Money::ZERO,
            unrealized_pnl: Money::ZERO,
            fees_paid: Money::ZERO,
            equity: config.initial_cash,
            positions: Vec::new(),
        };
        Ok(Self {
            config,
            options: EngineOptions::default(),
            pricing,
            costs,
            margin,
            execution,
            ledger,
            last_sequence: None,
            research_state: Value::Object(serde_json::Map::new()),
            feedback: EngineFeedback {
                sequence: 0,
                outcomes: Vec::new(),
                account,
                blockers: Vec::new(),
            },
            evidence: Vec::new(),
            seen_intent_ids: BTreeSet::new(),
        })
    }

    /// Restores an engine from a versioned checkpoint.
    ///
    /// # Errors
    ///
    /// Returns an error when the checkpoint contract version is unsupported.
    pub fn from_snapshot(
        snapshot: EngineSnapshot,
        pricing: P,
        costs: C,
        margin: M,
        execution: X,
    ) -> Result<Self, EngineError> {
        if snapshot.schema_version != CONTRACT_VERSION {
            return Err(EngineError::InvalidEvent(format!(
                "unsupported checkpoint schema {}",
                snapshot.schema_version
            )));
        }
        Ok(Self {
            config: snapshot.config,
            pricing,
            costs,
            margin,
            execution,
            ledger: snapshot.ledger,
            last_sequence: snapshot.last_sequence,
            research_state: snapshot.research_state,
            feedback: snapshot.feedback,
            evidence: Vec::new(),
            seen_intent_ids: snapshot.seen_intent_ids,
            options: snapshot.options,
        })
    }

    /// Creates an engine with explicit optional funded-replay extensions.
    ///
    /// [`Self::new`] remains the compatibility constructor with all
    /// extensions disabled.
    ///
    /// # Errors
    ///
    /// Returns an error when initial capital is negative.
    pub fn new_with_options(
        config: EngineConfig,
        options: EngineOptions,
        pricing: P,
        costs: C,
        margin: M,
        execution: X,
    ) -> Result<Self, EngineError> {
        // `new` owns the initial-capital validation shared by both
        // constructors.
        let mut engine = Self::new(config, pricing, costs, margin, execution)?;
        engine.options = options;
        Ok(engine)
    }

    /// Returns this engine with explicit optional extensions enabled.
    #[must_use]
    pub const fn with_options(mut self, options: EngineOptions) -> Self {
        self.options = options;
        self
    }

    /// Replaces optional extensions on an existing engine.
    pub const fn set_options(&mut self, options: EngineOptions) {
        self.options = options;
    }

    /// Convenience builder for funded margin-utilization admission.
    #[must_use]
    pub const fn with_risk_limits(mut self, limits: RiskLimits) -> Self {
        self.options.risk_limits = Some(limits);
        self
    }

    /// Enables required authoritative portfolio-margin refreshes.
    #[must_use]
    pub const fn with_authoritative_portfolio_margin(mut self) -> Self {
        self.options.portfolio_margin = PortfolioMarginMode::Required;
        self
    }

    /// Processes the next causal event through research and economic execution.
    ///
    /// # Errors
    ///
    /// Returns an error for ordering, contract, process, serialization, or arithmetic failures.
    // Keeping the transaction in one function makes its all-or-nothing state
    // transition auditable; splitting it would obscure the rollback boundary.
    #[allow(clippy::too_many_lines)]
    pub fn process_next(
        &mut self,
        event: SealedEvent,
        runner: &mut impl DecisionRunner,
    ) -> Result<StepResult, EngineError> {
        self.validate_event(&event)?;
        let sequence = event.sequence;
        let feedback_feature_hash = stable_hash(&event.research_payload)?;
        let feedback_context_hash = stable_hash(&self.feedback)?;
        let feedback = self.feedback.clone();
        let mut request = ResearchRequest {
            schema_version: CONTRACT_VERSION.to_owned(),
            input: event,
            state: Value::Null,
            feedback,
            sequence,
            feedback_context_hash,
            feedback_feature_hash,
        };
        // Build every potentially allocating request field before taking the
        // engine state. The move itself is then guarded before any fallible
        // request processing so an unwind cannot strand a null state.
        request.state = std::mem::take(&mut self.research_state);
        let (request_result, request) = {
            let guard = RequestStateGuard::new(&mut self.research_state, request);
            let result = (|| -> Result<(String, ResearchResponse, String), EngineError> {
                let request_hash = stable_hash(guard.request())?;
                let response = runner
                    .decide(guard.request())
                    .map_err(EngineError::Research)?;
                validate_response(&response)?;
                let response_hash = stable_hash(&response)?;
                Ok((request_hash, response, response_hash))
            })();
            let request = guard.finish();
            (result, request)
        };
        let event = request.input;
        let (request_hash, response, response_hash) = request_result?;

        let mut ledger = self.ledger.clone();
        // Revalue the working ledger before any new-risk admission. The
        // request above deliberately used the prior feedback unchanged so
        // strategy feedback retains its one-event-lag contract.
        let require_current_marks = self.options.max_accounting_mark_age_ns.is_some()
            || self.options.risk_limits.is_some()
            || self.options.portfolio_margin == PortfolioMarginMode::Required;
        if require_current_marks {
            ledger.apply_marks(&event.quotes, event.available_at_ns);
        }
        let mut margin_state = self.refresh_portfolio_margin(&event, &mut ledger)?;
        let mut marks_allow_new_risk =
            self.marks_allow_new_risk(&ledger, &event, require_current_marks);
        let mut margin_blockers = Vec::new();
        if let Some(reason) = margin_state.blocker() {
            margin_blockers.push(reason.to_owned());
        }
        let mut outcomes = Vec::with_capacity(response.actions.len());
        let mut pending_intent_ids = BTreeSet::new();
        for intent in &response.actions {
            validate_intent(intent, &self.seen_intent_ids, &mut pending_intent_ids)?;
            outcomes.push(self.execute_intent(
                &event,
                intent,
                &mut ledger,
                margin_state.allows_new_risk(),
                marks_allow_new_risk,
            )?);

            // Refresh between intents so a close can release capacity before a
            // later open in the same response. Conversely, a missing refresh
            // remains a hard gate for every subsequent new-risk intent.
            if require_current_marks {
                ledger.apply_marks(&event.quotes, event.available_at_ns);
            }
            ledger.release_authoritative_if_flat();
            margin_state = self.refresh_portfolio_margin(&event, &mut ledger)?;
            marks_allow_new_risk =
                self.marks_allow_new_risk(&ledger, &event, require_current_marks);
            if let Some(reason) = margin_state.blocker()
                && !margin_blockers.iter().any(|blocker| blocker == reason)
            {
                margin_blockers.push(reason.to_owned());
            }
        }
        ledger.apply_marks(&event.quotes, event.available_at_ns);
        ledger.release_authoritative_if_flat();
        let post_margin = self.refresh_portfolio_margin(&event, &mut ledger)?;
        if let Some(reason) = post_margin.blocker()
            && !margin_blockers.iter().any(|blocker| blocker == reason)
        {
            margin_blockers.push(reason.to_owned());
        }
        let mut blockers = ledger.accounting_mark_blockers_with_max_age(
            &event.quotes,
            event.available_at_ns,
            self.options.max_accounting_mark_age_ns,
        );
        for reason in margin_blockers {
            if !blockers.iter().any(|blocker| blocker == &reason) {
                blockers.push(reason);
            }
        }
        let account = ledger.state()?;
        let feedback = EngineFeedback {
            sequence: event.sequence,
            outcomes: outcomes.clone(),
            account: account.clone(),
            blockers,
        };
        let state_hash = stable_hash(&(&response.state, &ledger, event.sequence))?;
        let evidence = LifecycleEvidence {
            event_id: event.event_id,
            sequence: event.sequence,
            request_hash,
            response_hash,
            state_hash,
            outcomes,
            account,
        };

        self.ledger = ledger;
        self.last_sequence = Some(event.sequence);
        self.research_state = response.state;
        self.feedback = feedback.clone();
        self.evidence.push(evidence.clone());
        for intent_id in pending_intent_ids {
            self.seen_intent_ids.insert(intent_id);
        }
        Ok(StepResult { evidence, feedback })
    }

    fn marks_allow_new_risk(
        &self,
        ledger: &AccountLedger,
        event: &SealedEvent,
        require_current_marks: bool,
    ) -> bool {
        !require_current_marks
            || ledger
                .accounting_mark_blockers_with_max_age(
                    &event.quotes,
                    event.available_at_ns,
                    self.options.max_accounting_mark_age_ns,
                )
                .is_empty()
    }

    /// Runs a convenience replay as repeated calls to [`Self::process_next`].
    ///
    /// # Errors
    ///
    /// Returns the first error produced by the authoritative one-event reducer.
    pub fn run_replay(
        &mut self,
        events: impl IntoIterator<Item = SealedEvent>,
        runner: &mut impl DecisionRunner,
    ) -> Result<ReplayResult, EngineError> {
        for event in events {
            self.process_next(event, runner)?;
        }
        Ok(ReplayResult {
            final_account: self.ledger.state()?,
            evidence: self.evidence.clone(),
        })
    }

    #[must_use]
    pub fn snapshot(&self) -> EngineSnapshot {
        EngineSnapshot {
            schema_version: CONTRACT_VERSION.to_owned(),
            config: self.config,
            last_sequence: self.last_sequence,
            research_state: self.research_state.clone(),
            feedback: self.feedback.clone(),
            ledger: self.ledger.clone(),
            seen_intent_ids: self.seen_intent_ids.clone(),
            options: self.options,
        }
    }

    /// Returns the current public account projection.
    ///
    /// # Errors
    ///
    /// Returns an error if account projection arithmetic overflows.
    pub fn account(&self) -> Result<AccountState, EngineError> {
        self.ledger.state()
    }

    /// Takes all accumulated lifecycle evidence without cloning it.
    ///
    /// Streaming callers should invoke this periodically (or after each
    /// step) so the reducer does not retain the full replay in memory.
    pub fn take_evidence(&mut self) -> Vec<LifecycleEvidence> {
        std::mem::take(&mut self.evidence)
    }

    /// Drains accumulated lifecycle evidence without cloning it.
    pub fn drain_evidence(&mut self) -> std::vec::Drain<'_, LifecycleEvidence> {
        self.evidence.drain(..)
    }

    fn validate_event(&self, event: &SealedEvent) -> Result<(), EngineError> {
        if event.schema_version != CONTRACT_VERSION {
            return Err(EngineError::InvalidEvent(format!(
                "unsupported schema {}",
                event.schema_version
            )));
        }
        if let Some(previous) = self.last_sequence {
            let expected = previous
                .checked_add(1)
                .ok_or(EngineError::ArithmeticOverflow)?;
            if event.sequence != expected {
                return Err(EngineError::InvalidEvent(format!(
                    "expected sequence {expected}, received {}",
                    event.sequence
                )));
            }
        }
        if event.available_at_ns > event.sealed_at_ns || event.sealed_at_ns > event.decision_at_ns {
            return Err(EngineError::InvalidEvent(
                "event violates available <= sealed <= decision ordering".to_owned(),
            ));
        }
        for (instrument_key, quote) in &event.quotes {
            if quote.instrument_id.is_empty()
                || instrument_key != &quote.instrument_id
                || quote.source_id.is_empty()
                || quote.allowed_uses.is_empty()
                || quote.observed_at_ns > quote.available_at_ns
                || quote.available_at_ns > event.available_at_ns
                || matches!((quote.bid, quote.ask), (Some(bid), Some(ask)) if bid.0 > ask.0)
            {
                return Err(EngineError::InvalidEvent(format!(
                    "quote timing or identity invalid for {}",
                    quote.instrument_id
                )));
            }
        }
        for (basket_key, fact) in &event.margin_facts {
            if fact.required.0 < 0
                || fact.fact_id.is_empty()
                || fact.source_id.is_empty()
                || basket_key != &fact.basket_key
                || fact.observed_at_ns > fact.available_at_ns
                || fact.available_at_ns > event.available_at_ns
            {
                return Err(EngineError::InvalidEvent(format!(
                    "margin fact timing or value invalid for {}",
                    fact.fact_id
                )));
            }
        }
        Ok(())
    }

    fn refresh_portfolio_margin(
        &self,
        event: &SealedEvent,
        ledger: &mut AccountLedger,
    ) -> Result<MarginRefreshState, EngineError> {
        if self.options.portfolio_margin == PortfolioMarginMode::Disabled {
            return Ok(MarginRefreshState::Disabled);
        }
        let account = ledger.state()?;
        match self.margin.refresh_portfolio_margin(event, &account) {
            Ok(PortfolioMargin::Available(total)) => {
                ledger.set_authoritative_margin(total)?;
                Ok(MarginRefreshState::Available)
            }
            Ok(PortfolioMargin::Missing | PortfolioMargin::Unsupported) => Ok(
                MarginRefreshState::Missing("missing current portfolio margin".to_owned()),
            ),
            Err(reason) => Ok(MarginRefreshState::Missing(format!(
                "missing current portfolio margin: {reason}"
            ))),
        }
    }

    // Intent execution deliberately keeps pricing, admission, fills, and
    // ledger mutation in one transaction-shaped routine.
    #[allow(clippy::too_many_lines)]
    fn execute_intent(
        &mut self,
        event: &SealedEvent,
        intent: &TradeIntent,
        ledger: &mut AccountLedger,
        margin_allows_new_risk: bool,
        marks_allow_new_risk: bool,
    ) -> Result<ExecutionOutcome, EngineError> {
        if let Some(reason) = action_rejection(ledger, intent) {
            return Ok(rejected(intent, reason));
        }
        if matches!(intent.action, IntentAction::Open) && !margin_allows_new_risk {
            return Ok(rejected(
                intent,
                "missing current portfolio margin".to_owned(),
            ));
        }
        if matches!(intent.action, IntentAction::Open) && !marks_allow_new_risk {
            return Ok(rejected(
                intent,
                "missing current accounting mark".to_owned(),
            ));
        }

        let priced = match intent
            .legs
            .iter()
            .map(|leg| self.pricing.price(event, leg))
            .collect::<Result<Vec<_>, _>>()
        {
            Ok(priced) => priced,
            Err(reason) => return Ok(rejected(intent, reason)),
        };

        let account = ledger.state()?;
        let required_margin = if matches!(intent.action, IntentAction::Open) {
            match self.margin.required_margin(event, intent, &account) {
                Ok(margin) if margin.0 >= 0 => margin,
                Ok(margin) => return Err(EngineError::InvalidMargin(margin)),
                Err(reason) => return Ok(rejected(intent, reason)),
            }
        } else {
            Money::ZERO
        };
        if matches!(intent.action, IntentAction::Open)
            && let Some(reason) =
                self.open_admission_rejection(event, intent, &priced, ledger, required_margin)?
        {
            return Ok(rejected(intent, reason));
        }

        let raw = self.execution.execute(event, intent, &priced);
        if let Some(reason) = validate_raw_execution(intent, &raw) {
            return Ok(rejected(intent, reason));
        }
        let (total_requested, total_filled) = fill_totals(intent, &raw)?;
        if intent.atomic && total_filled != 0 && total_filled != total_requested {
            return Ok(rejected(
                intent,
                "atomic basket cannot be partially filled".to_owned(),
            ));
        }
        if total_filled == 0 {
            return Ok(ExecutionOutcome {
                intent_id: intent.intent_id.clone(),
                strategy_position_id: intent.strategy_position_id.clone(),
                status: if raw.reason.is_some() {
                    OutcomeStatus::Rejected
                } else {
                    OutcomeStatus::Deferred
                },
                fills: Vec::new(),
                reason: raw.reason,
            });
        }

        let executed_margin = if matches!(intent.action, IntentAction::Open) {
            let executed = intent_for_fills(intent, &raw.fills)?;
            match self.margin.required_margin(event, &executed, &account) {
                Ok(margin) if margin.0 >= 0 => margin,
                Ok(margin) => return Err(EngineError::InvalidMargin(margin)),
                Err(reason) => return Ok(rejected(intent, reason)),
            }
        } else {
            Money::ZERO
        };

        let fills = apply_costs_with_context(event, intent, &raw.fills, &self.costs)?;
        if matches!(intent.action, IntentAction::Open) {
            let required_capital = filled_capital_required(&fills, executed_margin)?;
            let available_capital = ledger
                .cash()
                .checked_sub(ledger.reserved_margin())
                .ok_or(EngineError::ArithmeticOverflow)?;
            if required_capital.0 > available_capital.0 {
                return Ok(rejected(
                    intent,
                    "insufficient available capital after execution".to_owned(),
                ));
            }
        }
        for fill in &fills {
            ledger.apply_fill(&intent.strategy_position_id, fill)?;
        }
        let gross_after = ledger.gross_quantity(&intent.strategy_position_id);
        match intent.action {
            IntentAction::Open => {
                ledger.reserve(&intent.strategy_position_id, executed_margin)?;
            }
            IntentAction::Close | IntentAction::Reduce | IntentAction::Flatten => {
                if gross_after == 0 {
                    ledger.set_reservation(&intent.strategy_position_id, Money::ZERO)?;
                }
            }
        }
        Ok(ExecutionOutcome {
            intent_id: intent.intent_id.clone(),
            strategy_position_id: intent.strategy_position_id.clone(),
            status: if total_filled == total_requested {
                OutcomeStatus::Filled
            } else {
                OutcomeStatus::PartiallyFilled
            },
            fills,
            reason: raw.reason,
        })
    }

    fn open_admission_rejection(
        &self,
        event: &SealedEvent,
        intent: &TradeIntent,
        priced: &[crate::PricedLeg],
        ledger: &AccountLedger,
        required_margin: Money,
    ) -> Result<Option<String>, EngineError> {
        let capital = capital_required(event, intent, priced, required_margin, &self.costs)?;
        let available_capital = ledger
            .cash()
            .checked_sub(ledger.reserved_margin())
            .ok_or(EngineError::ArithmeticOverflow)?;
        if capital.total.0 > available_capital.0 {
            return Ok(Some("insufficient available capital".to_owned()));
        }
        if let Some(limits) = self.options.risk_limits
            && let Some(reason) =
                risk_limit_rejection(ledger, required_margin, capital.estimated_fees, limits)?
        {
            return Ok(Some(reason));
        }
        Ok(None)
    }
}

fn validate_response(response: &ResearchResponse) -> Result<(), EngineError> {
    if response.schema_version != CONTRACT_VERSION {
        return Err(EngineError::InvalidResearchResponse(format!(
            "unsupported schema {}",
            response.schema_version
        )));
    }
    if !response.artifact_consumed
        || response.runner_id.is_empty()
        || response.bundle_hash.is_empty()
    {
        return Err(EngineError::InvalidResearchResponse(
            "research artifact identity is incomplete".to_owned(),
        ));
    }
    Ok(())
}

fn validate_intent(
    intent: &TradeIntent,
    seen: &BTreeSet<String>,
    pending: &mut BTreeSet<String>,
) -> Result<(), EngineError> {
    if intent.schema_version != CONTRACT_VERSION {
        return Err(EngineError::InvalidIntent(format!(
            "unsupported schema {}",
            intent.schema_version
        )));
    }
    if intent.intent_id.is_empty()
        || intent.decision_id.is_empty()
        || intent.strategy_position_id.is_empty()
        || intent.basket_key.is_empty()
        || intent.legs.is_empty()
    {
        return Err(EngineError::InvalidIntent(
            "identity and legs are required".to_owned(),
        ));
    }
    if seen.contains(&intent.intent_id) || !pending.insert(intent.intent_id.clone()) {
        return Err(EngineError::InvalidIntent(format!(
            "duplicate intent id {}",
            intent.intent_id
        )));
    }
    let mut instruments = BTreeSet::new();
    for leg in &intent.legs {
        if leg.instrument_id.is_empty() || leg.quantity == 0 {
            return Err(EngineError::InvalidIntent(
                "leg identity and positive quantity are required".to_owned(),
            ));
        }
        if !instruments.insert(&leg.instrument_id) {
            return Err(EngineError::InvalidIntent(format!(
                "duplicate instrument {} in one intent",
                leg.instrument_id
            )));
        }
    }
    Ok(())
}

fn action_rejection(ledger: &AccountLedger, intent: &TradeIntent) -> Option<String> {
    let positions = ledger.position_quantities(&intent.strategy_position_id);
    if matches!(intent.action, IntentAction::Open) {
        for leg in &intent.legs {
            let current = positions.get(&leg.instrument_id).copied().unwrap_or(0);
            let extends_position = current == 0
                || matches!(
                    (current.signum(), leg.side),
                    (1, Side::Buy) | (-1, Side::Sell)
                );
            if !extends_position {
                return Some(format!(
                    "open intent would reduce existing position {}",
                    leg.instrument_id
                ));
            }
        }
        return None;
    }
    if positions.is_empty() {
        return Some("lifecycle intent has no owned position".to_owned());
    }
    for leg in &intent.legs {
        let current = positions.get(&leg.instrument_id).copied().unwrap_or(0);
        let reducing = matches!(
            (current.signum(), leg.side),
            (1, Side::Sell) | (-1, Side::Buy)
        );
        if current == 0 || !reducing || leg.quantity > current.unsigned_abs() {
            return Some(format!(
                "lifecycle intent does not reduce owned position {}",
                leg.instrument_id
            ));
        }
    }
    match intent.action {
        IntentAction::Close | IntentAction::Flatten => {
            if intent.legs.len() != positions.len()
                || intent.legs.iter().any(|leg| {
                    positions
                        .get(&leg.instrument_id)
                        .is_none_or(|quantity| leg.quantity != quantity.unsigned_abs())
                })
            {
                return Some("close or flatten must cover the complete owned position".to_owned());
            }
        }
        IntentAction::Reduce => {
            let gross: u128 = positions
                .values()
                .map(|quantity| u128::from(quantity.unsigned_abs()))
                .sum();
            let reduction: u128 = intent.legs.iter().map(|leg| u128::from(leg.quantity)).sum();
            if reduction >= gross {
                return Some("reduce must leave a non-zero owned position".to_owned());
            }
        }
        IntentAction::Open => {}
    }
    None
}

fn validate_raw_execution(intent: &TradeIntent, raw: &crate::RawExecution) -> Option<String> {
    let mut quantities = BTreeMap::<&str, u128>::new();
    for fill in &raw.fills {
        let Some(leg) = intent
            .legs
            .iter()
            .find(|leg| leg.instrument_id == fill.instrument_id)
        else {
            return Some(format!(
                "execution returned unknown instrument {}",
                fill.instrument_id
            ));
        };
        if leg.side != fill.side || fill.quantity == 0 || fill.quantity > leg.quantity {
            return Some(format!(
                "execution exceeded or contradicted intent for {}",
                fill.instrument_id
            ));
        }
        if fill.price.0 <= 0 {
            return Some(format!(
                "execution returned non-positive price for {}",
                fill.instrument_id
            ));
        }
        if let Some(limit) = leg.limit_price {
            let outside_limit = match fill.side {
                Side::Buy => fill.price.0 > limit.0,
                Side::Sell => fill.price.0 < limit.0,
            };
            if outside_limit {
                return Some(format!(
                    "execution price violated limit for {}",
                    fill.instrument_id
                ));
            }
        }
        let total = quantities.entry(&fill.instrument_id).or_default();
        *total += u128::from(fill.quantity);
        if *total > u128::from(leg.quantity) {
            return Some(format!(
                "execution exceeded intended quantity for {}",
                fill.instrument_id
            ));
        }
    }
    None
}

fn fill_totals(
    intent: &TradeIntent,
    raw: &crate::RawExecution,
) -> Result<(u128, u128), EngineError> {
    let requested = intent.legs.iter().try_fold(0_u128, |total, leg| {
        total
            .checked_add(u128::from(leg.quantity))
            .ok_or(EngineError::ArithmeticOverflow)
    })?;
    let filled = raw.fills.iter().try_fold(0_u128, |total, fill| {
        total
            .checked_add(u128::from(fill.quantity))
            .ok_or(EngineError::ArithmeticOverflow)
    })?;
    Ok((requested, filled))
}

fn rejected(intent: &TradeIntent, reason: String) -> ExecutionOutcome {
    ExecutionOutcome {
        intent_id: intent.intent_id.clone(),
        strategy_position_id: intent.strategy_position_id.clone(),
        status: OutcomeStatus::Rejected,
        fills: Vec::new(),
        reason: Some(reason),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct CapitalRequirement {
    total: Money,
    estimated_fees: Money,
}

fn capital_required(
    event: &SealedEvent,
    intent: &TradeIntent,
    priced: &[crate::PricedLeg],
    required_margin: Money,
    costs: &impl CostModel,
) -> Result<CapitalRequirement, EngineError> {
    let fee_context = FeeContext::estimate(event, intent);
    let estimated_fees = priced.iter().try_fold(Money::ZERO, |total, leg| {
        let fee = costs.fee_with_context(&fee_context, leg)?;
        if fee.0 < 0 {
            return Err(EngineError::InvalidCost(fee));
        }
        total
            .checked_add(fee)
            .ok_or(EngineError::ArithmeticOverflow)
    })?;
    let cash_outflow = priced.iter().try_fold(Money::ZERO, |total, leg| {
        let quantity = i64::try_from(leg.quantity).map_err(|_| EngineError::ArithmeticOverflow)?;
        let signed_quantity = match leg.side {
            Side::Buy => quantity,
            Side::Sell => -quantity,
        };
        total
            .checked_add(checked_product(leg.price, signed_quantity)?)
            .ok_or(EngineError::ArithmeticOverflow)
    })?;
    let total = net_external_capital(required_margin, cash_outflow, estimated_fees)?;
    Ok(CapitalRequirement {
        total,
        estimated_fees,
    })
}

fn filled_capital_required(
    fills: &[backtest_contracts::Fill],
    required_margin: Money,
) -> Result<Money, EngineError> {
    let fees = fills.iter().try_fold(Money::ZERO, |total, fill| {
        total
            .checked_add(fill.fee)
            .ok_or(EngineError::ArithmeticOverflow)
    })?;
    let cash_outflow = fills.iter().try_fold(Money::ZERO, |total, fill| {
        let quantity = i64::try_from(fill.quantity).map_err(|_| EngineError::ArithmeticOverflow)?;
        let signed_quantity = match fill.side {
            Side::Buy => quantity,
            Side::Sell => -quantity,
        };
        total
            .checked_add(checked_product(fill.price, signed_quantity)?)
            .ok_or(EngineError::ArithmeticOverflow)
    })?;
    net_external_capital(required_margin, cash_outflow, fees)
}

fn net_external_capital(
    required_margin: Money,
    cash_outflow: Money,
    fees: Money,
) -> Result<Money, EngineError> {
    required_margin
        .checked_add(cash_outflow)
        .and_then(|value| value.checked_add(fees))
        .map(|value| Money(value.0.max(0)))
        .ok_or(EngineError::ArithmeticOverflow)
}

fn risk_limit_rejection(
    ledger: &AccountLedger,
    required_margin: Money,
    estimated_fees: Money,
    limits: RiskLimits,
) -> Result<Option<String>, EngineError> {
    let account = ledger.state()?;
    let projected_equity = account
        .equity
        .checked_sub(estimated_fees)
        .ok_or(EngineError::ArithmeticOverflow)?;
    if projected_equity.0 <= 0 {
        return Ok(Some(
            "risk limit: current marked equity is non-positive".to_owned(),
        ));
    }
    let projected_margin = ledger
        .reserved_margin()
        .checked_add(required_margin)
        .ok_or(EngineError::ArithmeticOverflow)?;
    let lhs = i128::from(projected_margin.0)
        .checked_mul(10_000)
        .ok_or(EngineError::ArithmeticOverflow)?;
    let rhs = i128::from(projected_equity.0)
        .checked_mul(i128::from(limits.max_margin_utilization_bps))
        .ok_or(EngineError::ArithmeticOverflow)?;
    if lhs > rhs {
        return Ok(Some(format!(
            "risk limit: margin utilization exceeds {} bps",
            limits.max_margin_utilization_bps
        )));
    }
    Ok(None)
}

fn intent_for_fills(
    intent: &TradeIntent,
    fills: &[crate::RawFill],
) -> Result<TradeIntent, EngineError> {
    let mut quantities = BTreeMap::<String, u64>::new();
    for fill in fills {
        let current = quantities.get(&fill.instrument_id).copied().unwrap_or(0);
        quantities.insert(
            fill.instrument_id.clone(),
            current
                .checked_add(fill.quantity)
                .ok_or(EngineError::ArithmeticOverflow)?,
        );
    }
    let legs = intent
        .legs
        .iter()
        .filter_map(|leg| {
            quantities.get(&leg.instrument_id).map(|quantity| {
                let mut executed = leg.clone();
                executed.quantity = *quantity;
                executed
            })
        })
        .collect();
    let mut executed = intent.clone();
    executed.legs = legs;
    Ok(executed)
}

fn stable_hash(value: &impl Serialize) -> Result<String, EngineError> {
    let mut writer = BufWriter::with_capacity(64 * 1024, DigestWriter(Sha256::new()));
    serde_json::to_writer(&mut writer, value)?;
    writer.flush().map_err(serde_json::Error::io)?;
    let digest_writer = writer
        .into_inner()
        .map_err(|error| serde_json::Error::io(error.into_error()))?;
    Ok(format!("{:x}", digest_writer.0.finalize()))
}

struct DigestWriter(Sha256);

impl Write for DigestWriter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.update(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod stable_hash_tests {
    use super::*;

    #[test]
    fn streaming_hash_matches_encoded_sha256() {
        let value = serde_json::json!({"ids": ["a", "b"], "sequence": 42, "nested": {"x": true}});
        let expected = format!("{:x}", Sha256::digest(serde_json::to_vec(&value).unwrap()));
        assert_eq!(stable_hash(&value).unwrap(), expected);
    }

    #[test]
    fn streaming_hash_matches_across_buffer_boundaries() {
        for length in [0, 63, 65_535, 65_536, 65_537, 200_000] {
            let value = serde_json::json!({"ids": "quote\"\\\nλ".repeat(length), "money": i64::MAX, "none": null});
            let expected = format!("{:x}", Sha256::digest(serde_json::to_vec(&value).unwrap()));
            assert_eq!(stable_hash(&value).unwrap(), expected);
        }
    }

    #[test]
    fn streaming_hash_propagates_serialization_failure() {
        struct Invalid;
        impl Serialize for Invalid {
            fn serialize<S: serde::Serializer>(&self, _serializer: S) -> Result<S::Ok, S::Error> {
                Err(serde::ser::Error::custom("intentional fixture failure"))
            }
        }
        assert!(matches!(
            stable_hash(&Invalid),
            Err(EngineError::Serialization(_))
        ));
    }
}

#[cfg(test)]
mod request_state_tests {
    use super::*;

    fn request(state: Value) -> ResearchRequest {
        ResearchRequest {
            schema_version: CONTRACT_VERSION.to_owned(),
            input: SealedEvent {
                schema_version: CONTRACT_VERSION.to_owned(),
                event_id: "event".to_owned(),
                sequence: 0,
                decision_at_ns: 0,
                available_at_ns: 0,
                sealed_at_ns: 0,
                quotes: BTreeMap::new(),
                margin_facts: BTreeMap::new(),
                research_payload: Value::Null,
            },
            state,
            feedback: EngineFeedback {
                sequence: 0,
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
            sequence: 0,
            feedback_context_hash: String::new(),
            feedback_feature_hash: String::new(),
        }
    }

    #[test]
    fn moved_request_state_restores_after_failed_request_path() {
        let original = serde_json::json!({"large_state": [1, 2, 3], "sequence": 41});
        let mut engine_state = original.clone();
        let request = request(engine_state.clone());
        let result = {
            let guard = RequestStateGuard::new(&mut engine_state, request);
            let result = (|| -> Result<(), EngineError> {
                let _ = stable_hash(guard.request())?;
                Err(EngineError::Research("runner failed".to_owned()))
            })();
            let request = guard.finish();
            drop(request);
            result
        };
        assert!(result.is_err());
        assert_eq!(engine_state, original);
    }

    #[test]
    fn moved_request_state_restores_when_runner_unwinds() {
        let original = serde_json::json!({"sequence": 42});
        let mut engine_state = original.clone();
        let request = request(engine_state.clone());
        let unwound = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let guard = RequestStateGuard::new(&mut engine_state, request);
            let _ = guard.request();
            panic!("runner panic");
        }));
        assert!(unwound.is_err());
        assert_eq!(engine_state, original);
    }
}
