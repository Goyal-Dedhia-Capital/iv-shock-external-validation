use std::collections::{BTreeMap, BTreeSet};

use backtest_contracts::{
    AccountState, CONTRACT_VERSION, EngineFeedback, ExecutionOutcome, IntentAction,
    LifecycleEvidence, MarginStatus, Money, OutcomeStatus, ResearchRequest, ResearchResponse,
    SealedEvent, Side, TradeIntent,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use thiserror::Error;

use crate::accounting::checked_product;
use crate::policies::apply_costs;
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

/// Cash treatment used when admitting a new basket.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OpeningCapitalPolicy {
    /// Legacy behavior: sell proceeds in the same basket offset buy premium.
    #[default]
    NetBasketCashflow,
    /// Conservative broker-style behavior: gross buy premium and the
    /// admission reserve must be funded without same-basket sell proceeds.
    GrossBuyPremium,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum MarginRefreshPolicy {
    #[default]
    Disabled,
    Enabled,
}

#[allow(clippy::trivially_copy_pass_by_ref)]
const fn is_margin_refresh_disabled(policy: &MarginRefreshPolicy) -> bool {
    matches!(policy, MarginRefreshPolicy::Disabled)
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
    #[error("invalid held margin refresh: {0}")]
    InvalidMarginRefresh(String),
    #[error("invalid cost amount: {0:?}")]
    InvalidCost(Money),
    #[error("invalid engine configuration: {0}")]
    InvalidConfig(String),
    #[error("serialization failed: {0}")]
    Serialization(#[from] serde_json::Error),
}

/// Optional admission limit for funded replays.
///
/// Controls the capital base used for funded admission.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CapitalBasis {
    #[default]
    MarkedEquity,
    NonCompoundingInitial,
}

/// The ceiling compares projected marked margin with the selected capital base
/// after estimated fees, so premium cash is not treated as extra equity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct RiskLimits {
    pub max_margin_utilization_bps: u32,
    #[serde(default)]
    pub capital_basis: CapitalBasis,
}

impl RiskLimits {
    #[must_use]
    pub const fn new(max_margin_utilization_bps: u32) -> Self {
        Self {
            max_margin_utilization_bps,
            capital_basis: CapitalBasis::MarkedEquity,
        }
    }

    #[must_use]
    pub const fn non_compounding(max_margin_utilization_bps: u32) -> Self {
        Self {
            max_margin_utilization_bps,
            capital_basis: CapitalBasis::NonCompoundingInitial,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EngineSnapshot {
    pub schema_version: String,
    pub config: EngineConfig,
    #[serde(default, skip_serializing_if = "is_margin_refresh_disabled")]
    pub margin_refresh_policy: MarginRefreshPolicy,
    #[serde(default)]
    pub opening_capital_policy: OpeningCapitalPolicy,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub risk_limits: Option<RiskLimits>,
    pub last_sequence: Option<u64>,
    pub research_state: Value,
    pub feedback: EngineFeedback,
    pub ledger: AccountLedger,
    pub seen_intent_ids: BTreeSet<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StepResult {
    pub evidence: LifecycleEvidence,
    pub feedback: EngineFeedback,
}

#[derive(Debug, Clone, Default)]
struct HeldMarginRefresh {
    status: Option<MarginStatus>,
    blocker: Option<String>,
    block_risk_increasing: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReplayResult {
    pub final_account: AccountState,
    pub evidence: Vec<LifecycleEvidence>,
}

pub struct Engine<P, C, M, X> {
    config: EngineConfig,
    margin_refresh_policy: MarginRefreshPolicy,
    opening_capital_policy: OpeningCapitalPolicy,
    risk_limits: Option<RiskLimits>,
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
            margin_refresh_policy: MarginRefreshPolicy::Disabled,
            opening_capital_policy: OpeningCapitalPolicy::NetBasketCashflow,
            risk_limits: None,
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
                margin: None,
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
        if let Some(limits) = snapshot.risk_limits {
            validate_risk_limits(limits)?;
        }
        Ok(Self {
            config: snapshot.config,
            margin_refresh_policy: snapshot.margin_refresh_policy,
            opening_capital_policy: snapshot.opening_capital_policy,
            risk_limits: snapshot.risk_limits,
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
        })
    }

    /// Enables or disables held-position collateral refresh for subsequent
    /// events. Existing callers remain disabled unless they opt in.
    #[must_use]
    pub const fn with_margin_refresh(mut self, policy: MarginRefreshPolicy) -> Self {
        self.margin_refresh_policy = policy;
        self
    }

    pub const fn set_margin_refresh_policy(&mut self, policy: MarginRefreshPolicy) {
        self.margin_refresh_policy = policy;
    }

    #[must_use]
    pub const fn margin_refresh_policy(&self) -> MarginRefreshPolicy {
        self.margin_refresh_policy
    }

    /// Selects conservative gross-buy-premium admission for new baskets.
    #[must_use]
    pub const fn with_opening_capital_policy(mut self, policy: OpeningCapitalPolicy) -> Self {
        self.opening_capital_policy = policy;
        self
    }

    #[must_use]
    pub const fn opening_capital_policy(&self) -> OpeningCapitalPolicy {
        self.opening_capital_policy
    }

    /// Applies an admission-time marked-margin utilization ceiling.
    ///
    /// # Errors
    ///
    /// Returns an error when the ceiling exceeds 100%.
    pub fn with_risk_limits(mut self, limits: RiskLimits) -> Result<Self, EngineError> {
        validate_risk_limits(limits)?;
        self.risk_limits = Some(limits);
        Ok(self)
    }

    /// Processes the next causal event through research and economic execution.
    ///
    /// # Errors
    ///
    /// Returns an error for ordering, contract, process, serialization, or arithmetic failures.
    pub fn process_next(
        &mut self,
        event: SealedEvent,
        runner: &mut impl DecisionRunner,
    ) -> Result<StepResult, EngineError> {
        self.validate_event(&event)?;
        let mut ledger = self.ledger.clone();
        let mut held_margin = self.refresh_held_margin(&event, &mut ledger)?;
        let mut margin_blockers = held_margin.blocker.iter().cloned().collect::<Vec<_>>();
        let mut request_feedback = self.feedback.clone();
        request_feedback.account = ledger.state()?;
        request_feedback.margin.clone_from(&held_margin.status);
        if let Some(blocker) = held_margin.blocker.as_ref() {
            request_feedback.blockers.push(blocker.clone());
        }
        let feedback_context_hash = stable_hash(&request_feedback)?;
        let request = ResearchRequest {
            schema_version: CONTRACT_VERSION.to_owned(),
            input: event.clone(),
            state: self.research_state.clone(),
            feedback: request_feedback,
            sequence: event.sequence,
            feedback_context_hash,
            feedback_feature_hash: stable_hash(&event.research_payload)?,
        };
        let request_hash = stable_hash(&request)?;
        let response = runner.decide(&request).map_err(EngineError::Research)?;
        validate_response(&response)?;
        let response_hash = stable_hash(&response)?;

        let mut outcomes = Vec::with_capacity(response.actions.len());
        let mut seen_intents = None;
        for intent in &response.actions {
            let seen_intents = seen_intents.get_or_insert_with(|| self.seen_intent_ids.clone());
            validate_intent(intent, seen_intents)?;
            if held_margin.block_risk_increasing && matches!(intent.action, IntentAction::Open) {
                outcomes.push(rejected(
                    intent,
                    "risk-increasing intent blocked while held margin refresh is unavailable"
                        .to_owned(),
                ));
            } else {
                let outcome = self.execute_intent(&event, intent, &mut ledger)?;
                let filled = !outcome.fills.is_empty();
                outcomes.push(outcome);
                if filled {
                    held_margin = self.refresh_held_margin(&event, &mut ledger)?;
                    if let Some(blocker) = held_margin.blocker.as_ref() {
                        margin_blockers.push(blocker.clone());
                    }
                }
            }
        }
        ledger.apply_marks(&event.quotes, event.available_at_ns);
        let mut blockers = margin_blockers;
        blockers.extend(ledger.accounting_mark_blockers(&event.quotes, event.available_at_ns));
        let account = ledger.state()?;
        let feedback = EngineFeedback {
            sequence: event.sequence,
            outcomes: outcomes.clone(),
            account: account.clone(),
            blockers,
            margin: held_margin.status.clone(),
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
            margin: held_margin.status,
        };

        self.ledger = ledger;
        self.last_sequence = Some(event.sequence);
        self.research_state = response.state;
        self.feedback = feedback.clone();
        self.evidence.push(evidence.clone());
        if let Some(seen_intents) = seen_intents {
            self.seen_intent_ids = seen_intents;
        }
        Ok(StepResult { evidence, feedback })
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
            margin_refresh_policy: self.margin_refresh_policy,
            opening_capital_policy: self.opening_capital_policy,
            risk_limits: self.risk_limits,
            last_sequence: self.last_sequence,
            research_state: self.research_state.clone(),
            feedback: self.feedback.clone(),
            ledger: self.ledger.clone(),
            seen_intent_ids: self.seen_intent_ids.clone(),
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

    /// Takes all accumulated lifecycle evidence, leaving the engine ready to
    /// retain only evidence produced by subsequent events.
    #[must_use]
    pub fn take_evidence(&mut self) -> Vec<LifecycleEvidence> {
        std::mem::take(&mut self.evidence)
    }

    /// Streams all accumulated lifecycle evidence without cloning it.
    ///
    /// The returned iterator owns the drained rows, so the engine can continue
    /// processing events while the caller writes or forwards those rows.
    #[must_use]
    pub fn drain_evidence(&mut self) -> std::vec::IntoIter<LifecycleEvidence> {
        self.take_evidence().into_iter()
    }

    fn refresh_held_margin(
        &self,
        event: &SealedEvent,
        ledger: &mut AccountLedger,
    ) -> Result<HeldMarginRefresh, EngineError> {
        if matches!(self.margin_refresh_policy, MarginRefreshPolicy::Disabled) {
            return Ok(HeldMarginRefresh::default());
        }

        let current_margin = ledger.reserved_margin();
        if ledger.state()?.positions.is_empty() {
            let required_margin = ledger.replace_reservations(&BTreeMap::new())?;
            return Ok(HeldMarginRefresh {
                status: Some(MarginStatus {
                    current_margin,
                    required_margin,
                    deficit: Money::ZERO,
                }),
                ..HeldMarginRefresh::default()
            });
        }

        let account = ledger.state()?;
        match self.margin.held_portfolio_margin(event, &account) {
            Ok(Some(required_margin)) => {
                let required_margin = ledger.replace_portfolio_reservation(required_margin)?;
                let mut refresh =
                    held_margin_refresh(current_margin, required_margin, ledger.cash())?;
                self.apply_held_risk_limit(ledger, &mut refresh)?;
                return Ok(refresh);
            }
            Ok(None) => {}
            Err(reason) => {
                return Ok(HeldMarginRefresh {
                    blocker: Some(format!(
                        "held portfolio margin refresh unavailable: {reason}"
                    )),
                    block_risk_increasing: true,
                    ..HeldMarginRefresh::default()
                });
            }
        }

        match self.margin.held_margin(event, &account) {
            Ok(Some(replacements)) => {
                let required_margin = ledger.replace_reservations(&replacements)?;
                let mut refresh =
                    held_margin_refresh(current_margin, required_margin, ledger.cash())?;
                self.apply_held_risk_limit(ledger, &mut refresh)?;
                Ok(refresh)
            }
            Ok(None) => Ok(HeldMarginRefresh {
                blocker: Some("held margin refresh unavailable".to_owned()),
                block_risk_increasing: true,
                ..HeldMarginRefresh::default()
            }),
            Err(reason) => Ok(HeldMarginRefresh {
                blocker: Some(format!("held margin refresh unavailable: {reason}")),
                block_risk_increasing: true,
                ..HeldMarginRefresh::default()
            }),
        }
    }

    fn apply_held_risk_limit(
        &self,
        ledger: &AccountLedger,
        refresh: &mut HeldMarginRefresh,
    ) -> Result<(), EngineError> {
        let (Some(limits), Some(status)) = (self.risk_limits, refresh.status.as_ref()) else {
            return Ok(());
        };
        let account = ledger.state()?;
        let capital_base = match limits.capital_basis {
            CapitalBasis::MarkedEquity => account.equity,
            CapitalBasis::NonCompoundingInitial => {
                Money(account.equity.0.min(ledger.initial_cash().0))
            }
        };
        let margin_breached = capital_base.0 <= 0
            || i128::from(status.required_margin.0)
                .checked_mul(10_000)
                .ok_or(EngineError::ArithmeticOverflow)?
                > i128::from(capital_base.0)
                    .checked_mul(i128::from(limits.max_margin_utilization_bps))
                    .ok_or(EngineError::ArithmeticOverflow)?;
        let total_capital_breached = if matches!(
            self.opening_capital_policy,
            OpeningCapitalPolicy::GrossBuyPremium
        ) {
            status
                .required_margin
                .checked_add(held_long_premium(&account)?)
                .ok_or(EngineError::ArithmeticOverflow)?
                .0
                > capital_base.0
        } else {
            false
        };
        if margin_breached || total_capital_breached {
            let risk_blocker = match (margin_breached, total_capital_breached) {
                (true, true) => format!(
                    "risk limit: held margin utilization exceeds {} bps; held total capital requirement exceeds capital base",
                    limits.max_margin_utilization_bps
                ),
                (true, false) => format!(
                    "risk limit: held margin utilization exceeds {} bps",
                    limits.max_margin_utilization_bps
                ),
                (false, true) => {
                    "risk limit: held total capital requirement exceeds capital base".to_owned()
                }
                (false, false) => unreachable!(),
            };
            refresh.blocker = Some(match refresh.blocker.take() {
                Some(existing) => format!("{existing}; {risk_blocker}"),
                None => risk_blocker,
            });
            refresh.block_risk_increasing = true;
        }
        Ok(())
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

    fn execute_intent(
        &mut self,
        event: &SealedEvent,
        intent: &TradeIntent,
        ledger: &mut AccountLedger,
    ) -> Result<ExecutionOutcome, EngineError> {
        if let Some(reason) = action_rejection(ledger, intent) {
            return Ok(rejected(intent, reason));
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
            && let Some(reason) = self.open_admission_rejection(ledger, required_margin, &priced)?
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

        let fills = apply_costs(raw.fills, &self.costs)?;
        if matches!(intent.action, IntentAction::Open)
            && let Some(reason) =
                self.filled_open_admission_rejection(ledger, executed_margin, &fills)?
        {
            return Ok(rejected(intent, reason));
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
        ledger: &AccountLedger,
        required_margin: Money,
        priced: &[crate::PricedLeg],
    ) -> Result<Option<String>, EngineError> {
        let required_capital = capital_required(
            priced,
            required_margin,
            &self.costs,
            self.opening_capital_policy,
        )?;
        if !has_available_capital(
            ledger,
            required_capital,
            self.risk_limits,
            self.opening_capital_policy,
        )? {
            return Ok(Some("insufficient available capital".to_owned()));
        }
        if let Some(limits) = self.risk_limits {
            return risk_limit_rejection(ledger, required_margin, priced, &self.costs, limits);
        }
        Ok(None)
    }

    fn filled_open_admission_rejection(
        &self,
        ledger: &AccountLedger,
        required_margin: Money,
        fills: &[backtest_contracts::Fill],
    ) -> Result<Option<String>, EngineError> {
        let required_capital = filled_capital_required(
            fills,
            required_margin,
            &self.costs,
            self.opening_capital_policy,
        )?;
        if !has_available_capital(
            ledger,
            required_capital,
            self.risk_limits,
            self.opening_capital_policy,
        )? {
            return Ok(Some(
                "insufficient available capital after execution".to_owned(),
            ));
        }
        if let Some(limits) = self.risk_limits {
            return filled_risk_limit_rejection(ledger, required_margin, fills, limits);
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

fn validate_intent(intent: &TradeIntent, seen: &mut BTreeSet<String>) -> Result<(), EngineError> {
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
    if !seen.insert(intent.intent_id.clone()) {
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

fn has_available_capital(
    ledger: &AccountLedger,
    required_capital: Money,
    limits: Option<RiskLimits>,
    policy: OpeningCapitalPolicy,
) -> Result<bool, EngineError> {
    if matches!(policy, OpeningCapitalPolicy::GrossBuyPremium) {
        let account = ledger.state()?;
        let capital_base = match limits.map(|value| value.capital_basis) {
            Some(CapitalBasis::NonCompoundingInitial) => {
                Money(account.equity.0.min(ledger.initial_cash().0))
            }
            _ => account.equity,
        };
        let held_long_premium = held_long_premium(&account)?;
        let available_capital = capital_base
            .checked_sub(ledger.reserved_margin())
            .and_then(|value| value.checked_sub(held_long_premium))
            .ok_or(EngineError::ArithmeticOverflow)?;
        return Ok(required_capital.0 <= available_capital.0);
    }
    let cash_base = match limits.map(|value| value.capital_basis) {
        Some(CapitalBasis::NonCompoundingInitial) => {
            let equity = ledger.state()?.equity;
            Money(ledger.cash().0.min(ledger.initial_cash().0).min(equity.0))
        }
        _ => ledger.cash(),
    };
    let available_capital = cash_base
        .checked_sub(ledger.reserved_margin())
        .ok_or(EngineError::ArithmeticOverflow)?;
    Ok(required_capital.0 <= available_capital.0)
}

fn held_long_premium(account: &AccountState) -> Result<Money, EngineError> {
    account
        .positions
        .iter()
        .try_fold(Money::ZERO, |total, position| {
            if position.quantity <= 0 {
                return Ok(total);
            }
            total
                .checked_add(checked_product(position.average_price, position.quantity)?)
                .ok_or(EngineError::ArithmeticOverflow)
        })
}

fn risk_limit_rejection(
    ledger: &AccountLedger,
    required_margin: Money,
    priced: &[crate::PricedLeg],
    costs: &impl CostModel,
    limits: RiskLimits,
) -> Result<Option<String>, EngineError> {
    let estimated_fees = priced.iter().try_fold(Money::ZERO, |total, leg| {
        total
            .checked_add(costs.fee(leg)?)
            .ok_or(EngineError::ArithmeticOverflow)
    })?;
    risk_limit_rejection_with_fees(ledger, required_margin, estimated_fees, limits)
}

fn validate_risk_limits(limits: RiskLimits) -> Result<(), EngineError> {
    if limits.max_margin_utilization_bps > 10_000 {
        return Err(EngineError::InvalidConfig(format!(
            "max margin utilization must not exceed 10000 bps, received {}",
            limits.max_margin_utilization_bps
        )));
    }
    Ok(())
}

fn filled_risk_limit_rejection(
    ledger: &AccountLedger,
    required_margin: Money,
    fills: &[backtest_contracts::Fill],
    limits: RiskLimits,
) -> Result<Option<String>, EngineError> {
    let fees = fills.iter().try_fold(Money::ZERO, |total, fill| {
        total
            .checked_add(fill.fee)
            .ok_or(EngineError::ArithmeticOverflow)
    })?;
    risk_limit_rejection_with_fees(ledger, required_margin, fees, limits)
}

fn risk_limit_rejection_with_fees(
    ledger: &AccountLedger,
    required_margin: Money,
    fees: Money,
    limits: RiskLimits,
) -> Result<Option<String>, EngineError> {
    let account = ledger.state()?;
    let projected_equity = account
        .equity
        .checked_sub(fees)
        .ok_or(EngineError::ArithmeticOverflow)?;
    if projected_equity.0 <= 0 {
        return Ok(Some(
            "risk limit: current marked equity is non-positive".to_owned(),
        ));
    }
    let capital_base = match limits.capital_basis {
        CapitalBasis::MarkedEquity => projected_equity,
        CapitalBasis::NonCompoundingInitial => {
            Money(projected_equity.0.min(ledger.initial_cash().0))
        }
    };
    let projected_margin = ledger
        .reserved_margin()
        .checked_add(required_margin)
        .ok_or(EngineError::ArithmeticOverflow)?;
    let lhs = i128::from(projected_margin.0)
        .checked_mul(10_000)
        .ok_or(EngineError::ArithmeticOverflow)?;
    let rhs = i128::from(capital_base.0)
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

fn held_margin_refresh(
    current_margin: Money,
    required_margin: Money,
    cash: Money,
) -> Result<HeldMarginRefresh, EngineError> {
    let deficit = Money(
        required_margin
            .checked_sub(cash)
            .ok_or(EngineError::ArithmeticOverflow)?
            .0
            .max(0),
    );
    let blocker = if deficit.0 > 0 {
        Some(format!(
            "held margin exceeds available cash by {}",
            deficit.0
        ))
    } else {
        None
    };
    Ok(HeldMarginRefresh {
        status: Some(MarginStatus {
            current_margin,
            required_margin,
            deficit,
        }),
        blocker,
        block_risk_increasing: deficit.0 > 0,
    })
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

fn capital_required(
    priced: &[crate::PricedLeg],
    required_margin: Money,
    costs: &impl CostModel,
    policy: OpeningCapitalPolicy,
) -> Result<Money, EngineError> {
    let admission_reserve = priced.iter().try_fold(Money::ZERO, |total, leg| {
        total
            .checked_add(costs.admission_reserve(leg)?)
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
    let gross_buy_premium = priced.iter().try_fold(Money::ZERO, |total, leg| {
        if matches!(leg.side, Side::Sell) {
            return Ok(total);
        }
        let quantity = i64::try_from(leg.quantity).map_err(|_| EngineError::ArithmeticOverflow)?;
        total
            .checked_add(checked_product(leg.price, quantity)?)
            .ok_or(EngineError::ArithmeticOverflow)
    })?;
    external_capital(
        required_margin,
        cash_outflow,
        gross_buy_premium,
        admission_reserve,
        policy,
    )
}

fn filled_capital_required(
    fills: &[backtest_contracts::Fill],
    required_margin: Money,
    costs: &impl CostModel,
    policy: OpeningCapitalPolicy,
) -> Result<Money, EngineError> {
    let admission_reserve = fills.iter().try_fold(Money::ZERO, |total, fill| {
        let leg = crate::PricedLeg {
            instrument_id: fill.instrument_id.clone(),
            side: fill.side,
            quantity: fill.quantity,
            price: fill.price,
        };
        total
            .checked_add(costs.admission_reserve(&leg)?)
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
    let gross_buy_premium = fills.iter().try_fold(Money::ZERO, |total, fill| {
        if matches!(fill.side, Side::Sell) {
            return Ok(total);
        }
        let quantity = i64::try_from(fill.quantity).map_err(|_| EngineError::ArithmeticOverflow)?;
        total
            .checked_add(checked_product(fill.price, quantity)?)
            .ok_or(EngineError::ArithmeticOverflow)
    })?;
    external_capital(
        required_margin,
        cash_outflow,
        gross_buy_premium,
        admission_reserve,
        policy,
    )
}

fn external_capital(
    required_margin: Money,
    cash_outflow: Money,
    gross_buy_premium: Money,
    admission_reserve: Money,
    policy: OpeningCapitalPolicy,
) -> Result<Money, EngineError> {
    // Margin is the incremental post-fill reservation. Project the complete
    // cash effect as well: buys consume cash, while sell proceeds fund the
    // reservation. Clamp credit structures at zero external capital rather
    // than allowing a negative admission requirement.
    let premium = match policy {
        OpeningCapitalPolicy::NetBasketCashflow => cash_outflow,
        OpeningCapitalPolicy::GrossBuyPremium => gross_buy_premium,
    };
    required_margin
        .checked_add(premium)
        .and_then(|value| value.checked_add(admission_reserve))
        .map(|value| Money(value.0.max(0)))
        .ok_or(EngineError::ArithmeticOverflow)
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
    let encoded = serde_json::to_vec(value)?;
    Ok(format!("{:x}", Sha256::digest(encoded)))
}
