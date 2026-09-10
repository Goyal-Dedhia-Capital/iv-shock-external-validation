//! Dataset-specific signed CE/PE SPAN snapshot adapter for EXP-019.
//!
//! This module is intentionally compiled by the replay harness, not by the
//! policy crate.  A snapshot is installed before an event is passed to the
//! generic engine.  The policy therefore never receives SPAN fields in its
//! `research_payload`.

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::rc::Rc;

use backtest_contracts::{AccountState, Money, SealedEvent, Side, TradeIntent};
use backtest_engine::{MarginProvider, PortfolioMargin};
use serde::{Deserialize, Serialize};

pub const SNAPSHOT_SCHEMA: &str = "exp012.dataset-span-snapshot-v1";
pub const CALCULATOR_VERSION: &str = "exp019-dataset-span-signed-option-portfolio-v1";
pub const MICRO_RUPEES_PER_RUPEE: f64 = 1_000_000.0;
#[allow(dead_code)]
pub const CAPITAL_TEN_LAKH_MICRO_RUPEES: i64 = 1_000_000_000_000;

/// The strict dataset mode requires a real SPAN effective timestamp and the
/// release minute gate.  Slot simulation is a separate, explicitly modelled
/// mode: it may use the policy slot while retaining unknown/non-authoritative
/// timing labels.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DatasetSpanMode {
    Strict,
    SlotSimulation,
}

impl DatasetSpanMode {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Strict => "strict",
            Self::SlotSimulation => "slot_simulation",
        }
    }

    pub fn parse(value: &str) -> Result<Self, String> {
        match value {
            "strict" | "dataset_strict" => Ok(Self::Strict),
            "slot_simulation" | "dataset_slot_simulation" | "assumed" | "assumed_mode" => {
                Ok(Self::SlotSimulation)
            }
            other => Err(format!(
                "invalid dataset SPAN mode {other}; expected strict or slot_simulation"
            )),
        }
    }
}

/// One release row.  Numeric SPAN fields are optional in the wire type so a
/// malformed/missing row is representable and fails closed during validation;
/// the feeder writes Float32 values as JSON numbers for eligible rows.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SpanSnapshotContract {
    #[serde(alias = "instrument_id")]
    pub contract_id: String,
    pub span_s1: Option<f64>,
    pub span_s2: Option<f64>,
    pub span_s3: Option<f64>,
    pub span_s4: Option<f64>,
    pub span_s5: Option<f64>,
    pub span_s6: Option<f64>,
    pub span_s7: Option<f64>,
    pub span_s8: Option<f64>,
    pub span_s9: Option<f64>,
    pub span_s10: Option<f64>,
    pub span_s11: Option<f64>,
    pub span_s12: Option<f64>,
    pub span_s13: Option<f64>,
    pub span_s14: Option<f64>,
    pub span_s15: Option<f64>,
    pub span_s16: Option<f64>,
    pub span_price: Option<f64>,
    pub span_cvf: Option<f64>,
    pub span_symbol: String,
    pub span_option_type: String,
    pub span_strike: f64,
    pub span_resolved_expiry: String,
    pub source_date: String,
    pub selected_slot: String,
    pub source_slot: String,
    pub policy_effective_at_ist: Option<String>,
    pub source_sha256: String,
    pub span_available: bool,
    pub margin_eligible: bool,
    pub span_date_slot_eligible: bool,
    pub span_minute_eligible: bool,
    pub span_effective_ts_ist: Option<String>,
    pub effective_time_source: String,
}

impl SpanSnapshotContract {
    fn arrays(&self) -> Option<[f64; 16]> {
        Some([
            self.span_s1?,
            self.span_s2?,
            self.span_s3?,
            self.span_s4?,
            self.span_s5?,
            self.span_s6?,
            self.span_s7?,
            self.span_s8?,
            self.span_s9?,
            self.span_s10?,
            self.span_s11?,
            self.span_s12?,
            self.span_s13?,
            self.span_s14?,
            self.span_s15?,
            self.span_s16?,
        ])
    }
}

/// Event-level envelope metadata plus all SPAN rows needed by held/current
/// contracts.  Rows are kept outside `SealedEvent` by the feeder and harness.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SpanSnapshot {
    pub schema_version: String,
    #[serde(default)]
    pub event_minute: Option<i64>,
    #[serde(default)]
    pub spot: Option<f64>,
    #[serde(default)]
    pub previous_close_spot: Option<f64>,
    #[serde(default)]
    pub previous_close_source: Option<String>,
    pub source_date: String,
    pub selected_slot: String,
    pub source_slot: String,
    #[serde(default)]
    pub policy_effective_at_ist: Option<String>,
    pub source_sha256: String,
    pub span_available: bool,
    pub margin_eligible: bool,
    pub span_date_slot_eligible: bool,
    pub span_minute_eligible: bool,
    #[serde(default)]
    pub span_effective_ts_ist: Option<String>,
    pub effective_time_source: String,
    #[serde(alias = "rows")]
    pub contracts: Vec<SpanSnapshotContract>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SpanMarginBreakdown {
    pub margin_rupees: f64,
    pub scan_scenarios: [f64; 16],
    pub m_span: f64,
    pub credit_sum: f64,
    pub long_option_value: f64,
    pub net_option_value: f64,
    pub elm_required: f64,
    pub reference_spot: f64,
}

#[derive(Debug, Clone)]
pub struct DatasetSpanMargin {
    mode: DatasetSpanMode,
    state: Rc<RefCell<Option<SpanSnapshot>>>,
}

impl DatasetSpanMargin {
    pub fn new(mode: DatasetSpanMode) -> Self {
        Self {
            mode,
            state: Rc::new(RefCell::new(None)),
        }
    }

    #[allow(dead_code)]
    pub const fn mode(&self) -> DatasetSpanMode {
        self.mode
    }

    /// Install one event's snapshot before calling generic-engine
    /// `process_next`.  Structural identity errors are hard failures; timing
    /// ineligibility is retained as a fail-closed margin state so closes can
    /// still be admitted by the engine.
    pub fn update_snapshot(
        &self,
        snapshot: SpanSnapshot,
        event: &SealedEvent,
    ) -> Result<(), String> {
        validate_snapshot(&snapshot, event)?;
        *self.state.borrow_mut() = Some(snapshot);
        Ok(())
    }

    pub fn clear_snapshot(&self) {
        *self.state.borrow_mut() = None;
    }

    /// Restore the last already-validated snapshot from the engine harness's
    /// bound checkpoint. This is used only for an intra-stream resume before
    /// the next source slot update arrives.
    pub fn restore_snapshot(&self, snapshot: Option<SpanSnapshot>) {
        *self.state.borrow_mut() = snapshot;
    }

    #[allow(dead_code)]
    pub fn snapshot(&self) -> Option<SpanSnapshot> {
        self.state.borrow().clone()
    }

    pub fn calculate_for_account(
        &self,
        account: &AccountState,
    ) -> Result<SpanMarginBreakdown, String> {
        let snapshot = self
            .state
            .borrow()
            .clone()
            .ok_or_else(|| "missing dataset SPAN snapshot".to_owned())?;
        let quantities = quantities_from_account(account)?;
        calculate_margin(&snapshot, self.mode, &quantities)
    }

    fn current_and_projected(
        &self,
        account: &AccountState,
        intent: &TradeIntent,
    ) -> Result<(Money, Money), String> {
        let snapshot = self
            .state
            .borrow()
            .clone()
            .ok_or_else(|| "missing dataset SPAN snapshot".to_owned())?;
        let current_quantities = quantities_from_account(account)?;
        let projected_quantities = projected_quantities(account, intent)?;
        let current = if current_quantities.is_empty() {
            Money::ZERO
        } else {
            money_from_breakdown(calculate_margin(&snapshot, self.mode, &current_quantities)?)?
        };
        let projected = if projected_quantities.is_empty() {
            Money::ZERO
        } else {
            money_from_breakdown(calculate_margin(
                &snapshot,
                self.mode,
                &projected_quantities,
            )?)?
        };
        Ok((current, projected))
    }
}

impl MarginProvider for DatasetSpanMargin {
    fn required_margin(
        &self,
        _event: &SealedEvent,
        intent: &TradeIntent,
        account: &AccountState,
    ) -> Result<Money, String> {
        let (current, projected) = self.current_and_projected(account, intent)?;
        let incremental = projected
            .checked_sub(current)
            .ok_or_else(|| "dataset SPAN incremental margin overflow".to_owned())?;
        Ok(if incremental.0 < 0 {
            Money::ZERO
        } else {
            incremental
        })
    }

    fn refresh_portfolio_margin(
        &self,
        _event: &SealedEvent,
        account: &AccountState,
    ) -> Result<PortfolioMargin, String> {
        if account
            .positions
            .iter()
            .all(|position| position.quantity == 0)
        {
            return Ok(PortfolioMargin::Available(Money::ZERO));
        }
        match self.calculate_for_account(account) {
            Ok(breakdown) => Ok(PortfolioMargin::Available(money_from_breakdown(breakdown)?)),
            Err(_) => Ok(PortfolioMargin::Missing),
        }
    }
}

fn validate_snapshot(snapshot: &SpanSnapshot, event: &SealedEvent) -> Result<(), String> {
    if snapshot.schema_version != SNAPSHOT_SCHEMA {
        return Err(format!(
            "unsupported dataset SPAN snapshot schema {}",
            snapshot.schema_version
        ));
    }
    let event_date = event
        .research_payload
        .get("session_date")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| "dataset SPAN event session_date is required".to_owned())?;
    if snapshot.source_date != event_date {
        return Err(format!(
            "dataset SPAN source date {} differs from event date {event_date}",
            snapshot.source_date
        ));
    }
    let expected_minute = event
        .research_payload
        .get("bar_minute")
        .and_then(serde_json::Value::as_i64)
        .ok_or_else(|| "dataset SPAN event bar_minute is required".to_owned())?;
    let actual_minute = snapshot
        .event_minute
        .ok_or_else(|| "dataset SPAN snapshot event_minute is required".to_owned())?;
    if expected_minute != actual_minute {
        return Err("dataset SPAN event minute mismatch".to_owned());
    }
    if !is_sha256(&snapshot.source_sha256)
        || snapshot.selected_slot.trim().is_empty()
        || snapshot.source_slot.trim().is_empty()
        || snapshot.effective_time_source.trim().is_empty()
    {
        return Err("dataset SPAN source/slot metadata is incomplete".to_owned());
    }
    if let Some(spot) = snapshot.spot {
        require_positive_finite(spot, "dataset SPAN spot")?;
    }
    if let Some(previous) = snapshot.previous_close_spot {
        require_positive_finite(previous, "dataset SPAN previous close spot")?;
        if snapshot
            .previous_close_source
            .as_deref()
            .is_none_or(str::is_empty)
        {
            return Err("dataset SPAN previous close source is required".to_owned());
        }
    }
    if let Some(value) = snapshot.policy_effective_at_ist.as_deref() {
        let (_, timestamp_ns) = parse_ist_timestamp(value)?;
        if timestamp_ns > event.available_at_ns {
            return Err("dataset SPAN policy effective time is newer than event".to_owned());
        }
    }
    if let Some(value) = snapshot.span_effective_ts_ist.as_deref() {
        let (_, timestamp_ns) = parse_ist_timestamp(value)?;
        if timestamp_ns > event.available_at_ns {
            return Err("dataset SPAN effective time is newer than event".to_owned());
        }
    }
    let source_date = parse_date(&snapshot.source_date)?;
    let mut contract_ids = BTreeSet::new();
    for contract in &snapshot.contracts {
        if !contract_ids.insert(contract.contract_id.clone()) {
            return Err(format!(
                "duplicate dataset SPAN contract {}",
                contract.contract_id
            ));
        }
        // Unavailable rows are retained as explicit evidence but may have
        // null/placeholder metadata in the source release.  They cannot be
        // used for margin and are therefore not rejected here.
        if !contract.span_available {
            continue;
        }
        if contract.contract_id.trim().is_empty()
            || contract.span_symbol.trim().is_empty()
            || contract.span_option_type.trim().is_empty()
            || !is_sha256(&contract.source_sha256)
            || contract.source_sha256 != snapshot.source_sha256
            || contract.source_date != snapshot.source_date
            || contract.selected_slot != snapshot.selected_slot
            || contract.source_slot != snapshot.source_slot
        {
            return Err(format!(
                "dataset SPAN metadata mismatch for {}",
                contract.contract_id
            ));
        }
        if !contract.span_strike.is_finite() || contract.span_strike <= 0.0 {
            return Err(format!("invalid SPAN strike for {}", contract.contract_id));
        }
        let expiry = parse_date(&contract.span_resolved_expiry)?;
        if expiry < source_date {
            return Err(format!("expired dataset SPAN row {}", contract.contract_id));
        }
        let expected_id =
            format_contract_id(expiry, contract.span_strike, &contract.span_option_type);
        if contract.contract_id != expected_id {
            return Err(format!(
                "dataset SPAN contract identity mismatch for {} (expected {expected_id})",
                contract.contract_id
            ));
        }
        if contract.span_cvf.is_some_and(|value| value != 1.0) {
            return Err(format!(
                "dataset SPAN CVF must equal 1 for {}",
                contract.contract_id
            ));
        }
        if contract
            .arrays()
            .is_some_and(|array| array.iter().any(|value| !value.is_finite()))
        {
            return Err(format!(
                "non-finite SPAN array for {}",
                contract.contract_id
            ));
        }
        if contract.span_available && contract.margin_eligible && contract.arrays().is_none() {
            return Err(format!(
                "missing required SPAN array for {}",
                contract.contract_id
            ));
        }
        if let Some(value) = contract.span_effective_ts_ist.as_deref() {
            let (_, timestamp_ns) = parse_ist_timestamp(value)?;
            if timestamp_ns > event.available_at_ns {
                return Err(format!(
                    "dataset SPAN row effective time is newer than event for {}",
                    contract.contract_id
                ));
            }
        }
        if let Some(value) = contract.policy_effective_at_ist.as_deref() {
            let (_, timestamp_ns) = parse_ist_timestamp(value)?;
            if timestamp_ns > event.available_at_ns {
                return Err(format!(
                    "dataset SPAN row policy time is newer than event for {}",
                    contract.contract_id
                ));
            }
        }
    }
    Ok(())
}

fn snapshot_usable(snapshot: &SpanSnapshot, mode: DatasetSpanMode) -> Result<(), String> {
    if !snapshot.span_available || !snapshot.margin_eligible || !snapshot.span_date_slot_eligible {
        return Err("dataset SPAN snapshot is unavailable or date/slot-ineligible".to_owned());
    }
    match mode {
        DatasetSpanMode::Strict => {
            if !snapshot.span_minute_eligible || snapshot.span_effective_ts_ist.is_none() {
                return Err(
                    "dataset SPAN strict mode requires effective timestamp and minute eligibility"
                        .to_owned(),
                );
            }
            if unknown_effective_time_source(&snapshot.effective_time_source) {
                return Err("dataset SPAN strict mode rejects unknown effective time".to_owned());
            }
        }
        DatasetSpanMode::SlotSimulation => {
            if snapshot.policy_effective_at_ist.is_none() {
                return Err(
                    "dataset SPAN slot simulation requires policy effective time".to_owned(),
                );
            }
        }
    }
    Ok(())
}

fn unknown_effective_time_source(value: &str) -> bool {
    let normalized = value.trim().to_ascii_lowercase();
    normalized.is_empty()
        || normalized == "unknown"
        || normalized == "unknown_effective_time"
        || normalized == "unknown-effective-time"
}

fn quantities_from_account(account: &AccountState) -> Result<BTreeMap<String, i64>, String> {
    let mut quantities = BTreeMap::new();
    for position in &account.positions {
        if position.quantity == 0 {
            continue;
        }
        let entry = quantities
            .entry(position.instrument_id.clone())
            .or_insert(0_i64);
        *entry = entry
            .checked_add(position.quantity)
            .ok_or_else(|| "dataset SPAN signed quantity overflow".to_owned())?;
    }
    quantities.retain(|_, quantity| *quantity != 0);
    Ok(quantities)
}

fn projected_quantities(
    account: &AccountState,
    intent: &TradeIntent,
) -> Result<BTreeMap<String, i64>, String> {
    let mut quantities = quantities_from_account(account)?;
    for leg in &intent.legs {
        let quantity = i64::try_from(leg.quantity)
            .map_err(|_| "dataset SPAN intent quantity exceeds i64".to_owned())?;
        let signed = match leg.side {
            Side::Buy => quantity,
            Side::Sell => quantity
                .checked_neg()
                .ok_or_else(|| "dataset SPAN signed quantity overflow".to_owned())?,
        };
        let entry = quantities.entry(leg.instrument_id.clone()).or_insert(0_i64);
        *entry = entry
            .checked_add(signed)
            .ok_or_else(|| "dataset SPAN projected quantity overflow".to_owned())?;
    }
    quantities.retain(|_, quantity| *quantity != 0);
    Ok(quantities)
}

fn calculate_margin(
    snapshot: &SpanSnapshot,
    mode: DatasetSpanMode,
    quantities: &BTreeMap<String, i64>,
) -> Result<SpanMarginBreakdown, String> {
    if quantities.is_empty() {
        return Ok(SpanMarginBreakdown {
            margin_rupees: 0.0,
            scan_scenarios: [0.0; 16],
            m_span: 0.0,
            credit_sum: 0.0,
            long_option_value: 0.0,
            net_option_value: 0.0,
            elm_required: 0.0,
            reference_spot: snapshot.spot.unwrap_or(0.0),
        });
    }
    snapshot_usable(snapshot, mode)?;
    let spot = snapshot
        .spot
        .ok_or_else(|| "dataset SPAN spot is missing".to_owned())?;
    require_positive_finite(spot, "dataset SPAN spot")?;
    let reference_spot = snapshot
        .previous_close_spot
        .ok_or_else(|| "dataset SPAN previous close proxy is required for ELM".to_owned())?;
    require_positive_finite(reference_spot, "dataset SPAN ELM reference spot")?;
    let evaluation_date = parse_date(&snapshot.source_date)?;
    let mut scenarios = [0.0_f64; 16];
    let mut credit_sum = 0.0;
    let mut long_option_value = 0.0;
    let mut elm_required = 0.0;
    let mut has_margin_risk_leg = false;
    for (instrument_id, quantity) in quantities {
        let contract = snapshot
            .contracts
            .iter()
            .find(|contract| contract.contract_id == *instrument_id)
            .ok_or_else(|| format!("missing dataset SPAN snapshot for {instrument_id}"))?;
        if !contract.span_available
            || !contract.margin_eligible
            || !contract.span_date_slot_eligible
        {
            return Err(format!(
                "dataset SPAN row is ineligible for {instrument_id}"
            ));
        }
        match mode {
            DatasetSpanMode::Strict
                if !contract.span_minute_eligible
                    || contract.span_effective_ts_ist.is_none()
                    || unknown_effective_time_source(&contract.effective_time_source) =>
            {
                return Err(format!(
                    "dataset SPAN strict timing is unavailable for {instrument_id}"
                ));
            }
            DatasetSpanMode::SlotSimulation if contract.policy_effective_at_ist.is_none() => {
                return Err(format!(
                    "dataset SPAN policy timing is unavailable for {instrument_id}"
                ));
            }
            _ => {}
        }
        if contract.span_symbol != "NIFTY"
            || !matches!(contract.span_option_type.as_str(), "CE" | "PE")
        {
            return Err(format!(
                "unsupported portfolio leg {instrument_id}; only NIFTY CE/PE is supported"
            ));
        }
        let arrays = contract
            .arrays()
            .ok_or_else(|| format!("missing required SPAN array for {instrument_id}"))?;
        if contract.span_cvf != Some(1.0) {
            return Err(format!("dataset SPAN CVF must equal 1 for {instrument_id}"));
        }
        let quantity_f = *quantity as f64;
        for (index, value) in arrays.iter().enumerate() {
            scenarios[index] += quantity_f * *value;
        }
        let span_price = contract
            .span_price
            .ok_or_else(|| format!("missing SPAN price for {instrument_id}"))?;
        require_nonnegative_finite(span_price, "SPAN credit price")?;
        if *quantity < 0 {
            has_margin_risk_leg = true;
            credit_sum += (-quantity_f) * span_price;
            let expiry = parse_date(&contract.span_resolved_expiry)?;
            elm_required += elm_for_short_option(
                reference_spot,
                contract.span_strike,
                &contract.span_option_type,
                expiry,
                evaluation_date,
                -quantity_f,
            );
        } else {
            long_option_value += quantity_f * span_price;
        }
    }
    if scenarios.iter().any(|value| !value.is_finite()) {
        return Err("dataset SPAN scenario arithmetic is non-finite".to_owned());
    }
    let m_span = scenarios
        .iter()
        .copied()
        .fold(f64::NEG_INFINITY, f64::max)
        .max(0.0);
    // Premium cash flows are accounted by the generic ledger.  SPAN reserves
    // only the remaining risk: scan risk less net current option value, plus
    // ELM on short options.  A pure long option book therefore reserves zero
    // while its premium and fees still consume cash through engine accounting.
    let net_option_value = long_option_value - credit_sum;
    let s_net = if has_margin_risk_leg {
        (m_span - net_option_value).max(0.0)
    } else {
        0.0
    };
    let margin_rupees = s_net + elm_required;
    let result = SpanMarginBreakdown {
        margin_rupees,
        scan_scenarios: scenarios,
        m_span,
        credit_sum,
        long_option_value,
        net_option_value,
        elm_required,
        reference_spot,
    };
    Ok(result)
}

fn elm_for_short_option(
    reference_spot: f64,
    strike: f64,
    option_type: &str,
    expiry: SimpleDate,
    evaluation_date: SimpleDate,
    units: f64,
) -> f64 {
    let mut rate: f64 = 0.02;
    if expiry > add_months(evaluation_date, 9) {
        rate = rate.max(0.05);
    }
    let deep_otm = match option_type {
        "CE" => strike > 1.10 * reference_spot,
        "PE" => strike < 0.90 * reference_spot,
        _ => false,
    };
    if deep_otm {
        rate = rate.max(0.03);
    }
    // NSE expiry-day +2% is applicable from 2024-11-20 onward.  Apply this
    // after the long/deep floors so deep-OTM expiry-day remains 5%.
    if evaluation_date >= SimpleDate::new(2024, 11, 20) && expiry == evaluation_date {
        rate += 0.02;
    }
    rate * reference_spot * units
}

fn money_from_breakdown(breakdown: SpanMarginBreakdown) -> Result<Money, String> {
    Ok(Money(conservative_micro_rupees(breakdown.margin_rupees)?))
}

fn conservative_micro_rupees(rupees: f64) -> Result<i64, String> {
    require_nonnegative_finite(rupees, "dataset SPAN margin")?;
    if rupees == 0.0 {
        return Ok(0);
    }
    let micros = (rupees * MICRO_RUPEES_PER_RUPEE).ceil();
    if !micros.is_finite() || micros > i64::MAX as f64 {
        return Err("dataset SPAN margin exceeds Money range".to_owned());
    }
    Ok(micros as i64)
}

fn require_positive_finite(value: f64, label: &str) -> Result<(), String> {
    if value.is_finite() && value > 0.0 {
        Ok(())
    } else {
        Err(format!("{label} must be positive and finite"))
    }
}

fn require_nonnegative_finite(value: f64, label: &str) -> Result<(), String> {
    if value.is_finite() && value >= 0.0 {
        Ok(())
    } else {
        Err(format!("{label} must be finite and non-negative"))
    }
}

fn is_sha256(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn format_contract_id(expiry: SimpleDate, strike: f64, option_type: &str) -> String {
    let strike = if strike.fract() == 0.0 {
        format!("{:.0}", strike)
    } else {
        format!("{strike}")
    };
    format!(
        "{:04}-{:02}-{:02}|{strike}|{option_type}",
        expiry.year, expiry.month, expiry.day
    )
}

/// Parse the release's ISO-8601 IST timestamps into UTC nanoseconds.  This is
/// deliberately small and only accepts the explicit forms emitted by the
/// feeder (`...+05:30`, `...Z`, with optional fractional seconds).
fn parse_ist_timestamp(value: &str) -> Result<(SimpleDate, i64), String> {
    let (date_text, remainder) = value
        .split_once('T')
        .ok_or_else(|| format!("invalid dataset SPAN timestamp {value}"))?;
    let date = parse_date(date_text)?;
    let (clock, offset_minutes) = if let Some(clock) = remainder.strip_suffix('Z') {
        (clock, 0_i64)
    } else if let Some((clock, offset)) = remainder.rsplit_once('+') {
        (clock, parse_offset_minutes(offset, 1)?)
    } else if let Some((clock, offset)) = remainder.rsplit_once('-') {
        (clock, parse_offset_minutes(offset, -1)?)
    } else {
        return Err(format!("dataset SPAN timestamp lacks timezone {value}"));
    };
    let mut pieces = clock.split(':');
    let hour: i64 = pieces
        .next()
        .and_then(|part| part.parse().ok())
        .ok_or_else(|| format!("invalid dataset SPAN timestamp {value}"))?;
    let minute: i64 = pieces
        .next()
        .and_then(|part| part.parse().ok())
        .ok_or_else(|| format!("invalid dataset SPAN timestamp {value}"))?;
    let seconds = pieces
        .next()
        .ok_or_else(|| format!("invalid dataset SPAN timestamp {value}"))?;
    if pieces.next().is_some() || hour > 23 || minute > 59 {
        return Err(format!("invalid dataset SPAN timestamp {value}"));
    }
    let (second, nanos) = if let Some((whole, fraction)) = seconds.split_once('.') {
        let second: i64 = whole
            .parse()
            .map_err(|_| format!("invalid dataset SPAN timestamp {value}"))?;
        if fraction.is_empty()
            || fraction.len() > 9
            || !fraction.bytes().all(|b| b.is_ascii_digit())
        {
            return Err(format!("invalid dataset SPAN timestamp {value}"));
        }
        let mut padded = fraction.to_owned();
        while padded.len() < 9 {
            padded.push('0');
        }
        let nanos: i64 = padded
            .parse()
            .map_err(|_| format!("invalid dataset SPAN timestamp {value}"))?;
        (second, nanos)
    } else {
        (
            seconds
                .parse()
                .map_err(|_| format!("invalid dataset SPAN timestamp {value}"))?,
            0,
        )
    };
    if second > 59 {
        return Err(format!("invalid dataset SPAN timestamp {value}"));
    }
    let days = days_from_civil(date);
    let seconds = days
        .checked_mul(86_400)
        .and_then(|value| value.checked_add(hour * 3_600 + minute * 60 + second))
        .and_then(|value| value.checked_sub(offset_minutes * 60))
        .ok_or_else(|| "dataset SPAN timestamp overflow".to_owned())?;
    let timestamp_ns = seconds
        .checked_mul(1_000_000_000)
        .and_then(|value| value.checked_add(nanos))
        .ok_or_else(|| "dataset SPAN timestamp overflow".to_owned())?;
    Ok((date, timestamp_ns))
}

fn parse_offset_minutes(value: &str, sign: i64) -> Result<i64, String> {
    let (hours, minutes) = value
        .split_once(':')
        .ok_or_else(|| format!("invalid dataset SPAN timezone offset {value}"))?;
    let hours: i64 = hours
        .parse()
        .map_err(|_| format!("invalid dataset SPAN timezone offset {value}"))?;
    let minutes: i64 = minutes
        .parse()
        .map_err(|_| format!("invalid dataset SPAN timezone offset {value}"))?;
    if hours > 23 || minutes > 59 {
        return Err(format!("invalid dataset SPAN timezone offset {value}"));
    }
    Ok(sign * (hours * 60 + minutes))
}

fn days_from_civil(date: SimpleDate) -> i64 {
    let year = i64::from(date.year) - i64::from(date.month <= 2);
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let year_of_era = year - era * 400;
    let month = i64::from(date.month);
    let day_of_year =
        (153 * (month + if month > 2 { -3 } else { 9 }) + 2) / 5 + i64::from(date.day) - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct SimpleDate {
    year: i32,
    month: u32,
    day: u32,
}

impl SimpleDate {
    const fn new(year: i32, month: u32, day: u32) -> Self {
        Self { year, month, day }
    }
}

fn parse_date(value: &str) -> Result<SimpleDate, String> {
    let mut pieces = value.split('-');
    let year = pieces.next().and_then(|part| part.parse().ok());
    let month = pieces.next().and_then(|part| part.parse().ok());
    let day = pieces.next().and_then(|part| part.parse().ok());
    if pieces.next().is_some() || year.is_none() || month.is_none() || day.is_none() {
        return Err(format!("invalid dataset SPAN date {value}"));
    }
    let result = SimpleDate::new(year.unwrap(), month.unwrap(), day.unwrap());
    if !(1..=12).contains(&result.month)
        || !(1..=days_in_month(result.year, result.month)).contains(&result.day)
    {
        return Err(format!("invalid dataset SPAN date {value}"));
    }
    Ok(result)
}

fn add_months(value: SimpleDate, months: u32) -> SimpleDate {
    let absolute = value.year * 12 + value.month as i32 - 1 + months as i32;
    let year = absolute.div_euclid(12);
    let month = absolute.rem_euclid(12) as u32 + 1;
    SimpleDate::new(year, month, value.day.min(days_in_month(year, month)))
}

fn days_in_month(year: i32, month: u32) -> u32 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if year % 400 == 0 || (year % 4 == 0 && year % 100 != 0) => 29,
        2 => 28,
        _ => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use backtest_contracts::{CONTRACT_VERSION, PositionView};

    const SOURCE_SHA: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

    fn snapshot(
        minute: bool,
        effective: Option<&str>,
        contract: SpanSnapshotContract,
    ) -> SpanSnapshot {
        SpanSnapshot {
            schema_version: SNAPSHOT_SCHEMA.to_owned(),
            event_minute: Some(1),
            spot: Some(20_000.0),
            previous_close_spot: Some(19_900.0),
            previous_close_source: Some("release_previous_session_proxy".to_owned()),
            source_date: "2024-11-20".to_owned(),
            selected_slot: "BOD".to_owned(),
            source_slot: "BOD".to_owned(),
            policy_effective_at_ist: Some("2024-11-20T09:15:00+05:30".to_owned()),
            source_sha256: SOURCE_SHA.to_owned(),
            span_available: true,
            margin_eligible: true,
            span_date_slot_eligible: true,
            span_minute_eligible: minute,
            span_effective_ts_ist: effective.map(str::to_owned),
            effective_time_source: "release_observed".to_owned(),
            contracts: vec![contract],
        }
    }

    fn contract(id: &str, expiry: &str, strike: f64, arrays: [f64; 16]) -> SpanSnapshotContract {
        SpanSnapshotContract {
            contract_id: id.to_owned(),
            span_s1: Some(arrays[0]),
            span_s2: Some(arrays[1]),
            span_s3: Some(arrays[2]),
            span_s4: Some(arrays[3]),
            span_s5: Some(arrays[4]),
            span_s6: Some(arrays[5]),
            span_s7: Some(arrays[6]),
            span_s8: Some(arrays[7]),
            span_s9: Some(arrays[8]),
            span_s10: Some(arrays[9]),
            span_s11: Some(arrays[10]),
            span_s12: Some(arrays[11]),
            span_s13: Some(arrays[12]),
            span_s14: Some(arrays[13]),
            span_s15: Some(arrays[14]),
            span_s16: Some(arrays[15]),
            span_price: Some(100.0),
            span_cvf: Some(1.0),
            span_symbol: "NIFTY".to_owned(),
            span_option_type: "CE".to_owned(),
            span_strike: strike,
            span_resolved_expiry: expiry.to_owned(),
            source_date: "2024-11-20".to_owned(),
            selected_slot: "BOD".to_owned(),
            source_slot: "BOD".to_owned(),
            policy_effective_at_ist: Some("2024-11-20T09:15:00+05:30".to_owned()),
            source_sha256: SOURCE_SHA.to_owned(),
            span_available: true,
            margin_eligible: true,
            span_date_slot_eligible: true,
            span_minute_eligible: false,
            span_effective_ts_ist: None,
            effective_time_source: "unknown".to_owned(),
        }
    }

    fn event() -> SealedEvent {
        SealedEvent {
            schema_version: CONTRACT_VERSION.to_owned(),
            event_id: "e".to_owned(),
            sequence: 0,
            decision_at_ns: 1_732_074_360_000_000_000,
            available_at_ns: 1_732_074_360_000_000_000,
            sealed_at_ns: 1_732_074_360_000_000_000,
            quotes: BTreeMap::new(),
            margin_facts: BTreeMap::new(),
            research_payload: serde_json::json!({"session_date":"2024-11-20","bar_minute":1}),
        }
    }

    fn account(id: &str, quantity: i64) -> AccountState {
        AccountState {
            cash: Money(1_000_000_000_000),
            reserved_margin: Money::ZERO,
            realized_pnl: Money::ZERO,
            unrealized_pnl: Money::ZERO,
            fees_paid: Money::ZERO,
            equity: Money(1_000_000_000_000),
            positions: vec![PositionView {
                strategy_position_id: "p".to_owned(),
                instrument_id: id.to_owned(),
                quantity,
                average_price: Money(1),
                mark_price: Money(1),
                realized_pnl: Money::ZERO,
                unrealized_pnl: Money::ZERO,
            }],
        }
    }

    #[test]
    fn arrays_use_signed_units_without_lot_scaling() {
        let id = "2024-11-21|21000|CE";
        let mut arrays = [0.0; 16];
        arrays[0] = 10.0;
        arrays[1] = -3.0;
        let provider = DatasetSpanMargin::new(DatasetSpanMode::SlotSimulation);
        provider
            .update_snapshot(
                snapshot(false, None, contract(id, "2024-11-21", 21_000.0, arrays)),
                &event(),
            )
            .unwrap();
        let result = provider.calculate_for_account(&account(id, -2)).unwrap();
        assert_eq!(result.scan_scenarios[0], -20.0);
        assert_eq!(result.scan_scenarios[1], 6.0);
    }

    #[test]
    fn strict_unknown_timing_is_missing_but_slot_mode_accepts_policy_time() {
        let id = "2024-11-21|21000|CE";
        let contract = contract(id, "2024-11-21", 21_000.0, [1.0; 16]);
        let strict = DatasetSpanMargin::new(DatasetSpanMode::Strict);
        strict
            .update_snapshot(snapshot(false, None, contract.clone()), &event())
            .unwrap();
        assert_eq!(
            strict
                .refresh_portfolio_margin(&event(), &account(id, -1))
                .unwrap(),
            PortfolioMargin::Missing
        );
        let assumed = DatasetSpanMargin::new(DatasetSpanMode::SlotSimulation);
        assumed
            .update_snapshot(snapshot(false, None, contract), &event())
            .unwrap();
        assert!(matches!(
            assumed
                .refresh_portfolio_margin(&event(), &account(id, -1))
                .unwrap(),
            PortfolioMargin::Available(_)
        ));
    }

    #[test]
    fn elm_long_maturity_uses_five_percent_even_if_not_deep_otm() {
        let id = "2025-12-25|21000|CE";
        let provider = DatasetSpanMargin::new(DatasetSpanMode::SlotSimulation);
        provider
            .update_snapshot(
                snapshot(false, None, contract(id, "2025-12-25", 21_000.0, [0.0; 16])),
                &event(),
            )
            .unwrap();
        let result = provider.calculate_for_account(&account(id, -1)).unwrap();
        assert_eq!(result.elm_required, 0.05 * 19_900.0);
    }

    #[test]
    fn elm_short_maturity_deep_otm_call_uses_three_percent() {
        let id = "2024-12-20|23000|CE";
        let provider = DatasetSpanMargin::new(DatasetSpanMode::SlotSimulation);
        provider
            .update_snapshot(
                snapshot(false, None, contract(id, "2024-12-20", 23_000.0, [0.0; 16])),
                &event(),
            )
            .unwrap();
        let result = provider.calculate_for_account(&account(id, -1)).unwrap();
        assert_eq!(result.elm_required, 0.03 * 19_900.0);
    }

    #[test]
    fn pure_long_option_reserves_zero_margin_without_double_charging_premium() {
        let id = "2024-11-21|19000|PE";
        let mut leg = contract(id, "2024-11-21", 19_000.0, [10.0; 16]);
        leg.span_option_type = "PE".to_owned();
        let provider = DatasetSpanMargin::new(DatasetSpanMode::SlotSimulation);
        provider
            .update_snapshot(snapshot(false, None, leg), &event())
            .unwrap();
        let result = provider.calculate_for_account(&account(id, 50)).unwrap();
        assert_eq!(result.margin_rupees, 0.0);
        assert_eq!(result.elm_required, 0.0);
    }

    #[test]
    fn short_call_preserves_original_scan_credit_and_elm_formula() {
        let id = "2024-11-21|21000|CE";
        let provider = DatasetSpanMargin::new(DatasetSpanMode::SlotSimulation);
        provider
            .update_snapshot(
                snapshot(
                    false,
                    None,
                    contract(id, "2024-11-21", 21_000.0, [-10.0; 16]),
                ),
                &event(),
            )
            .unwrap();
        let result = provider.calculate_for_account(&account(id, -1)).unwrap();
        assert_eq!(result.m_span, 10.0);
        assert_eq!(result.credit_sum, 100.0);
        assert_eq!(result.margin_rupees, 10.0 + 100.0 + 0.02 * 19_900.0);
    }

    #[test]
    fn put_vertical_nets_signed_scenarios_and_current_option_value() {
        let long_id = "2024-11-21|19000|PE";
        let short_id = "2024-11-21|18900|PE";
        let mut long = contract(long_id, "2024-11-21", 19_000.0, [5.0; 16]);
        long.span_option_type = "PE".to_owned();
        long.span_price = Some(120.0);
        let mut short = contract(short_id, "2024-11-21", 18_900.0, [2.0; 16]);
        short.span_option_type = "PE".to_owned();
        short.span_price = Some(80.0);
        let mut value = snapshot(false, None, long);
        value.contracts.push(short);
        let provider = DatasetSpanMargin::new(DatasetSpanMode::SlotSimulation);
        provider.update_snapshot(value, &event()).unwrap();
        let mut portfolio = account(long_id, 50);
        portfolio.positions.push(PositionView {
            strategy_position_id: "p".to_owned(),
            instrument_id: short_id.to_owned(),
            quantity: -50,
            average_price: Money(1),
            mark_price: Money(1),
            realized_pnl: Money::ZERO,
            unrealized_pnl: Money::ZERO,
        });
        let result = provider.calculate_for_account(&portfolio).unwrap();
        assert_eq!(result.m_span, 150.0);
        assert_eq!(result.long_option_value, 6_000.0);
        assert_eq!(result.credit_sum, 4_000.0);
        assert_eq!(result.net_option_value, 2_000.0);
        assert_eq!(result.margin_rupees, 0.02 * 19_900.0 * 50.0);
    }

    #[test]
    fn elm_expiry_day_adds_two_percent_after_deep_otm_floor() {
        let id = "2024-11-20|23000|CE";
        let provider = DatasetSpanMargin::new(DatasetSpanMode::SlotSimulation);
        provider
            .update_snapshot(
                snapshot(false, None, contract(id, "2024-11-20", 23_000.0, [0.0; 16])),
                &event(),
            )
            .unwrap();
        let result = provider.calculate_for_account(&account(id, -1)).unwrap();
        assert_eq!(result.elm_required, 0.05 * 19_900.0);
    }

    #[test]
    fn elm_expiry_day_base_rate_adds_two_percent() {
        let id = "2024-11-20|21000|CE";
        let provider = DatasetSpanMargin::new(DatasetSpanMode::SlotSimulation);
        provider
            .update_snapshot(
                snapshot(false, None, contract(id, "2024-11-20", 21_000.0, [0.0; 16])),
                &event(),
            )
            .unwrap();
        let result = provider.calculate_for_account(&account(id, -1)).unwrap();
        assert_eq!(result.elm_required, 0.04 * 19_900.0);
    }

    #[test]
    fn snapshot_requires_exact_event_minute() {
        let id = "2024-11-21|21000|CE";
        let row = contract(id, "2024-11-21", 21_000.0, [1.0; 16]);
        let provider = DatasetSpanMargin::new(DatasetSpanMode::SlotSimulation);
        let mut missing = snapshot(false, None, row.clone());
        missing.event_minute = None;
        assert!(
            provider
                .update_snapshot(missing, &event())
                .unwrap_err()
                .contains("event_minute is required")
        );
        let mut mismatched = snapshot(false, None, row);
        mismatched.event_minute = Some(2);
        assert!(
            provider
                .update_snapshot(mismatched, &event())
                .unwrap_err()
                .contains("event minute mismatch")
        );
    }

    #[test]
    fn unavailable_unrelated_rows_do_not_hard_fail_but_duplicate_ids_do() {
        let id = "2024-11-21|21000|CE";
        let row = contract(id, "2024-11-21", 21_000.0, [1.0; 16]);
        let mut unavailable = row.clone();
        unavailable.contract_id = "unavailable-placeholder".to_owned();
        unavailable.span_available = false;
        unavailable.span_resolved_expiry = "not-a-date".to_owned();
        unavailable.span_strike = -1.0;
        let mut usable = snapshot(false, None, row.clone());
        usable.contracts.push(unavailable);
        let provider = DatasetSpanMargin::new(DatasetSpanMode::SlotSimulation);
        provider.update_snapshot(usable, &event()).unwrap();

        let mut duplicate = snapshot(false, None, row);
        let mut duplicate_unavailable = duplicate.contracts[0].clone();
        duplicate_unavailable.span_available = false;
        duplicate.contracts.push(duplicate_unavailable);
        assert!(
            provider
                .update_snapshot(duplicate, &event())
                .unwrap_err()
                .contains("duplicate dataset SPAN contract")
        );
    }

    #[test]
    fn zero_span_price_is_valid_but_never_a_fill_price() {
        let id = "2024-11-21|21000|CE";
        let mut row = contract(id, "2024-11-21", 21_000.0, [1.0; 16]);
        row.span_price = Some(0.0);
        let provider = DatasetSpanMargin::new(DatasetSpanMode::SlotSimulation);
        provider
            .update_snapshot(snapshot(false, None, row), &event())
            .unwrap();
        let result = provider.calculate_for_account(&account(id, -1)).unwrap();
        assert_eq!(result.credit_sum, 0.0);
        assert!(result.margin_rupees > 0.0);
    }
}
