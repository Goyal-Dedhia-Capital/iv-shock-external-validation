use std::io::{self, BufRead, Write};
use std::path::Path;

use backtest_contracts::ResearchRequest;
use iv_shock_calendar_guard::CalendarGuard;
use iv_shock_strategy_policy::{Config, ModelBundle, Packet, PolicyMode, Runner};
use serde_json::Value;

fn validate_calendar(guard: &CalendarGuard, request: &ResearchRequest) -> Result<(), String> {
    let packet: Packet = serde_json::from_value(request.input.research_payload.clone())
        .map_err(|error| error.to_string())?;
    match packet {
        Packet::Minute {
            minute,
            session_date,
            candidates,
            ..
        } => {
            guard.validate_minute(&session_date, minute)?;
            let session_end = guard.eligible_last_epoch_minute(&session_date)?;
            for candidate in candidates {
                if candidate.date != session_date {
                    return Err("candidate date differs from policy packet date".into());
                }
                if candidate.session_end_minute != session_end {
                    return Err("candidate session end differs from bound exchange calendar".into());
                }
                guard.validate_minute(&session_date, candidate.event_minute)?;
                guard.validate_minute(&session_date, candidate.entry_minute)?;
            }
        }
        Packet::SessionEnd {
            session_date,
            session_end_minute,
        } => {
            let expected = guard
                .eligible_last_epoch_minute(&session_date)?
                .checked_add(1)
                .ok_or_else(|| "session-end boundary overflow".to_owned())?;
            if session_end_minute != expected {
                return Err(
                    "session-end packet must be one minute after the eligible close".into(),
                );
            }
        }
    }
    Ok(())
}

fn apply_model(bundle: &ModelBundle, request: &mut ResearchRequest) -> Result<(), String> {
    let mut packet: Packet = serde_json::from_value(request.input.research_payload.clone())
        .map_err(|error| error.to_string())?;
    if let Packet::Minute { candidates, .. } = &mut packet {
        for candidate in candidates {
            candidate.rank_score_micro = Some(bundle.score(candidate)?);
        }
    }
    request.input.research_payload =
        serde_json::to_value(packet).map_err(|error| error.to_string())?;
    Ok(())
}

fn main() {
    let calendar = std::env::var("IV_SHOCK_CALENDAR_PATH")
        .and_then(|path| std::env::var("IV_SHOCK_CALENDAR_SHA256").map(|hash| (path, hash)))
        .map_err(|_| "calendar path and SHA-256 are required".to_owned())
        .and_then(|(path, hash)| CalendarGuard::load(Path::new(&path), &hash));
    let calendar = match calendar {
        Ok(value) => value,
        Err(error) => {
            eprintln!("{error}");
            std::process::exit(2);
        }
    };
    if std::env::var("IV_SHOCK_SOURCE_CONTRACT_SHA256").is_err() {
        eprintln!("IV_SHOCK_SOURCE_CONTRACT_SHA256 is required");
        std::process::exit(2);
    }
    let bundle_hash = match std::env::var("IV_SHOCK_RESEARCH_BUNDLE_HASH") {
        Ok(value) if !value.is_empty() => value,
        _ => {
            eprintln!("IV_SHOCK_RESEARCH_BUNDLE_HASH is required");
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
        _ => Err("usage: iv-shock-strategy-policy [--config-json JSON]".to_owned()),
    };
    let mode = config.as_ref().ok().map(|value| value.mode);
    let mut runner = match config.map(Runner::new) {
        Ok(runner) => runner,
        Err(error) => {
            eprintln!("{error}");
            std::process::exit(2);
        }
    };
    let model = if mode == Some(PolicyMode::FinalCandidate) {
        std::env::var("IV_SHOCK_MODEL_BUNDLE_PATH")
            .and_then(|path| std::env::var("IV_SHOCK_MODEL_BUNDLE_SHA256").map(|hash| (path, hash)))
            .map_err(|_| "final-candidate mode requires model bundle path and SHA-256".to_owned())
            .and_then(|(path, hash)| ModelBundle::load(Path::new(&path), &hash))
            .map(Some)
    } else {
        Ok(None)
    };
    let model = match model {
        Ok(model) => model,
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
        let response = match serde_json::from_str::<ResearchRequest>(&line)
            .map_err(|error| error.to_string())
            .and_then(|mut request| {
                if let Some(model) = &model {
                    apply_model(model, &mut request)?;
                }
                validate_calendar(&calendar, &request)?;
                Ok(request)
            })
            .and_then(|request| runner.process_request(request, &bundle_hash))
        {
            Ok(response) => serde_json::to_value(response).expect("response serializes"),
            Err(error) => serde_json::json!({"error": error}),
        };
        if serde_json::to_writer(&mut stdout, &response).is_err() {
            break;
        }
        if writeln!(stdout).is_err() || stdout.flush().is_err() {
            break;
        }
    }
}

#[allow(dead_code)]
fn _config_probe(value: &Value) -> Result<Config, String> {
    serde_json::from_value(value.clone()).map_err(|error| error.to_string())
}
