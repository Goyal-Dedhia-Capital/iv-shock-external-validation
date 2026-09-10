//! Causal 30-minute spot-state gate layered over the frozen EXP019 policy.

use std::io::{self, BufRead, Write};

use backtest_contracts::ResearchRequest;
use exp019_funded_best_family_portfolio::{Config, Runner};

pub const FEATURE: &str = "spot_return_30m_pct";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SpotState {
    Down,
    Up,
}

impl SpotState {
    #[must_use]
    pub const fn accepts(self, value: Option<f64>) -> bool {
        match (self, value) {
            (Self::Down, Some(value)) => value.is_finite() && value <= 0.0,
            (Self::Up, Some(value)) => value.is_finite() && value > 0.0,
            (_, None) => false,
        }
    }

    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Down => "spot_down_30m_le_zero",
            Self::Up => "spot_up_30m_gt_zero",
        }
    }
}

/// Apply the entry-known spot gate without changing candidate ordering or clocks.
/// Missing and non-finite feature values fail closed.
///
/// # Errors
///
/// Returns an error if the packet is not the canonical minute/candidate shape.
pub fn gate_request(request: &mut ResearchRequest, state: SpotState) -> Result<(), String> {
    let packet = request
        .input
        .research_payload
        .as_object_mut()
        .ok_or_else(|| "research payload must be an object".to_owned())?;
    if packet.get("kind").and_then(serde_json::Value::as_str) != Some("minute") {
        return Err("research payload must be a minute packet".to_owned());
    }
    let candidates = packet
        .get_mut("candidates")
        .and_then(serde_json::Value::as_array_mut)
        .ok_or_else(|| "minute packet candidates must be an array".to_owned())?;
    for candidate in candidates {
        let object = candidate
            .as_object_mut()
            .ok_or_else(|| "candidate must be an object".to_owned())?;
        let already_eligible = object
            .get("entry_eligible")
            .and_then(serde_json::Value::as_bool)
            .ok_or_else(|| "candidate entry_eligible must be boolean".to_owned())?;
        let value = object
            .get("features")
            .and_then(serde_json::Value::as_object)
            .and_then(|features| features.get(FEATURE))
            .and_then(serde_json::Value::as_f64);
        object.insert(
            "entry_eligible".to_owned(),
            serde_json::Value::Bool(already_eligible && state.accepts(value)),
        );
    }
    Ok(())
}

/// Run one persistent canonical JSONL decision process.
///
/// # Panics
///
/// Panics only if the typed research response unexpectedly cannot serialize.
pub fn run_policy(state: SpotState) {
    let mut arguments = std::env::args().skip(1);
    let config = match (
        arguments.next().as_deref(),
        arguments.next(),
        arguments.next(),
    ) {
        (None, None, None) => Ok(Config::default()),
        (Some("--config-json"), Some(value), None) => Config::from_json_str(&value),
        _ => Err("usage: exp020-policy [--config-json JSON]".to_owned()),
    };
    let mut runner = match config.map(Runner::new) {
        Ok(runner) => runner,
        Err(error) => {
            eprintln!("{error}");
            std::process::exit(2);
        }
    };
    let stdin = io::stdin();
    let mut stdout = io::BufWriter::new(io::stdout().lock());
    for line in stdin.lock().lines() {
        let line = match line {
            Ok(line) if !line.trim().is_empty() => line,
            Ok(_) => continue,
            Err(error) => {
                eprintln!("stdin error: {error}");
                break;
            }
        };
        let response = serde_json::from_str::<ResearchRequest>(&line)
            .map_err(|error| error.to_string())
            .and_then(|mut request| {
                gate_request(&mut request, state)?;
                runner.process_request(request, "exp020-funded-spot-filter-v1")
            });
        let mut response = match response {
            Ok(response) => serde_json::to_value(response).expect("response serializes"),
            Err(error) => serde_json::json!({"error": error}),
        };
        if let Some(object) = response.as_object_mut() {
            object.insert(
                "runner_id".to_owned(),
                serde_json::Value::String(format!("exp020-funded-spot-filter:{}", state.label())),
            );
        }
        if serde_json::to_writer(&mut stdout, &response).is_err()
            || writeln!(stdout).is_err()
            || stdout.flush().is_err()
        {
            break;
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::{SpotState, gate_request};
    use backtest_contracts::{
        AccountState, CONTRACT_VERSION, EngineFeedback, Money, ResearchRequest, SealedEvent,
    };
    use serde_json::{Value, json};

    fn request(value: &Value, eligible: bool) -> ResearchRequest {
        ResearchRequest {
            schema_version: CONTRACT_VERSION.to_owned(),
            sequence: 0,
            input: SealedEvent {
                schema_version: CONTRACT_VERSION.to_owned(),
                event_id: "event".to_owned(),
                sequence: 0,
                decision_at_ns: 60,
                available_at_ns: 60,
                sealed_at_ns: 60,
                quotes: BTreeMap::default(),
                margin_facts: BTreeMap::default(),
                research_payload: json!({
                    "kind":"minute", "minute":1,
                    "candidates":[{"entry_eligible":eligible,"features":{"spot_return_30m_pct":value.clone()}}]
                }),
            },
            state: json!({}),
            feedback: EngineFeedback {
                sequence: 0,
                account: AccountState {
                    cash: Money::ZERO,
                    reserved_margin: Money::ZERO,
                    realized_pnl: Money::ZERO,
                    unrealized_pnl: Money::ZERO,
                    fees_paid: Money::ZERO,
                    equity: Money::ZERO,
                    positions: vec![],
                },
                outcomes: vec![],
                blockers: vec![],
            },
            feedback_context_hash: "context".to_owned(),
            feedback_feature_hash: "features".to_owned(),
        }
    }

    fn eligible(request: &ResearchRequest) -> bool {
        request.input.research_payload["candidates"][0]["entry_eligible"]
            .as_bool()
            .unwrap()
    }

    #[test]
    fn zero_belongs_only_to_down_state() {
        let mut down = request(&json!(0.0), true);
        let mut up = down.clone();
        gate_request(&mut down, SpotState::Down).unwrap();
        gate_request(&mut up, SpotState::Up).unwrap();
        assert!(eligible(&down));
        assert!(!eligible(&up));
    }

    #[test]
    fn positive_belongs_only_to_up_state() {
        let mut down = request(&json!(0.01), true);
        let mut up = down.clone();
        gate_request(&mut down, SpotState::Down).unwrap();
        gate_request(&mut up, SpotState::Up).unwrap();
        assert!(!eligible(&down));
        assert!(eligible(&up));
    }

    #[test]
    fn missing_and_preexisting_rejection_fail_closed() {
        let mut missing = request(&Value::Null, true);
        gate_request(&mut missing, SpotState::Down).unwrap();
        assert!(!eligible(&missing));
        let mut rejected = request(&json!(-1.0), false);
        gate_request(&mut rejected, SpotState::Down).unwrap();
        assert!(!eligible(&rejected));
    }
}
