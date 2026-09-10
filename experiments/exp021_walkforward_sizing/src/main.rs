use std::collections::HashMap;
use std::fs;
use std::io::{self, BufRead, Write};

use backtest_contracts::ResearchRequest;
use exp019_funded_best_family_portfolio::{Config, Runner};
use exp020_funded_spot_filter::{SpotState, gate_request};
use sha2::{Digest, Sha256};

fn load_multipliers() -> Result<HashMap<String, u64>, String> {
    let path = std::env::var("EXP021_MULTIPLIERS_JSON")
        .map_err(|_| "EXP021_MULTIPLIERS_JSON is required".to_owned())?;
    let bytes = fs::read(&path).map_err(|error| format!("cannot read {path}: {error}"))?;
    let expected = std::env::var("EXP021_MULTIPLIERS_SHA256")
        .map_err(|_| "EXP021_MULTIPLIERS_SHA256 is required".to_owned())?;
    let actual = format!("{:x}", Sha256::digest(&bytes));
    if actual != expected {
        return Err("multiplier tape SHA-256 mismatch".to_owned());
    }
    let values: HashMap<String, u64> = serde_json::from_slice(&bytes)
        .map_err(|error| format!("invalid multiplier tape: {error}"))?;
    if values.values().any(|value| !matches!(value, 1 | 2)) {
        return Err("multipliers must be exactly one or two".to_owned());
    }
    Ok(values)
}

fn scale_request(
    request: &mut ResearchRequest,
    multipliers: &HashMap<String, u64>,
) -> Result<(), String> {
    let candidates = request
        .input
        .research_payload
        .get_mut("candidates")
        .and_then(serde_json::Value::as_array_mut)
        .ok_or_else(|| "minute packet candidates must be an array".to_owned())?;
    for candidate in candidates {
        let object = candidate
            .as_object_mut()
            .ok_or_else(|| "candidate must be an object".to_owned())?;
        let candidate_id = object
            .get("candidate_id")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| "candidate_id must be a string".to_owned())?;
        let scenario = object
            .get("scenario")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| "candidate scenario must be a string".to_owned())?;
        let book = object
            .get("book")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| "candidate book must be a string".to_owned())?;
        let strategy_position_id = format!("exp019|{scenario}|{book}|{candidate_id}");
        let multiplier = *multipliers.get(&strategy_position_id).unwrap_or(&1);
        if multiplier == 1 {
            continue;
        }
        let quantity = object
            .get("quantity")
            .and_then(serde_json::Value::as_u64)
            .ok_or_else(|| "candidate quantity must be unsigned".to_owned())?;
        object.insert(
            "quantity".to_owned(),
            serde_json::Value::from(
                quantity
                    .checked_mul(multiplier)
                    .ok_or_else(|| "candidate quantity overflow".to_owned())?,
            ),
        );
        let legs = object
            .get_mut("legs")
            .and_then(serde_json::Value::as_array_mut)
            .ok_or_else(|| "candidate legs must be an array".to_owned())?;
        for leg in legs {
            let leg = leg
                .as_object_mut()
                .ok_or_else(|| "candidate leg must be an object".to_owned())?;
            let quantity = leg
                .get("quantity")
                .and_then(serde_json::Value::as_u64)
                .ok_or_else(|| "leg quantity must be unsigned".to_owned())?;
            leg.insert(
                "quantity".to_owned(),
                serde_json::Value::from(
                    quantity
                        .checked_mul(multiplier)
                        .ok_or_else(|| "leg quantity overflow".to_owned())?,
                ),
            );
        }
    }
    Ok(())
}

fn main() {
    let multipliers = match load_multipliers() {
        Ok(values) => values,
        Err(error) => {
            eprintln!("{error}");
            std::process::exit(2);
        }
    };
    let mut arguments = std::env::args().skip(1);
    let config = match (
        arguments.next().as_deref(),
        arguments.next(),
        arguments.next(),
    ) {
        (None, None, None) => Ok(Config::default()),
        (Some("--config-json"), Some(value), None) => Config::from_json_str(&value),
        _ => Err("usage: exp021-policy [--config-json JSON]".to_owned()),
    };
    let mut runner = match config.map(Runner::new) {
        Ok(runner) => runner,
        Err(error) => {
            eprintln!("{error}");
            std::process::exit(2);
        }
    };
    let bundle = match std::env::var("IV_SHOCK_RESEARCH_BUNDLE_HASH") {
        Ok(value) => value,
        Err(_) => {
            eprintln!("IV_SHOCK_RESEARCH_BUNDLE_HASH is required");
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
                gate_request(&mut request, SpotState::Down)?;
                scale_request(&mut request, &multipliers)?;
                runner.process_request(request, &bundle)
            });
        let mut response = match response {
            Ok(response) => serde_json::to_value(response).expect("response serializes"),
            Err(error) => serde_json::json!({"error": error}),
        };
        if let Some(object) = response.as_object_mut() {
            object.insert(
                "runner_id".to_owned(),
                serde_json::Value::String("exp021-walkforward-sizing:spot-iv-crowding".to_owned()),
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
    use super::scale_request;
    use backtest_contracts::{
        AccountState, CONTRACT_VERSION, EngineFeedback, Money, ResearchRequest, SealedEvent,
    };
    use serde_json::json;
    use std::collections::{BTreeMap, HashMap};

    fn request() -> ResearchRequest {
        ResearchRequest {
            schema_version: CONTRACT_VERSION.to_owned(),
            sequence: 0,
            input: SealedEvent {
                schema_version: CONTRACT_VERSION.to_owned(),
                event_id: "event".to_owned(),
                sequence: 0,
                decision_at_ns: 0,
                available_at_ns: 0,
                sealed_at_ns: 0,
                quotes: BTreeMap::new(),
                margin_facts: BTreeMap::new(),
                research_payload: json!({"kind":"minute","candidates":[
                    {"candidate_id":"known","scenario":"baseline","book":"H3_F1","quantity":50,"legs":[{"quantity":50}]},
                    {"candidate_id":"unknown","scenario":"baseline","book":"H3_F1","quantity":25,"legs":[{"quantity":25}]}
                ]}),
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

    #[test]
    fn scales_only_named_candidates_and_every_leg() {
        let mut request = request();
        scale_request(
            &mut request,
            &HashMap::from([("exp019|baseline|H3_F1|known".to_owned(), 2)]),
        )
        .unwrap();
        let rows = request.input.research_payload["candidates"]
            .as_array()
            .unwrap();
        assert_eq!(rows[0]["quantity"], 100);
        assert_eq!(rows[0]["legs"][0]["quantity"], 100);
        assert_eq!(rows[1]["quantity"], 25);
        assert_eq!(rows[1]["legs"][0]["quantity"], 25);
    }
}
