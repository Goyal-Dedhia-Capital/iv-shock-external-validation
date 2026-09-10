use std::io::{self, BufRead, Write};

use exp046_r2_core_policy::{Config, Runner};
use serde_json::Value;

fn main() {
    let mut arguments = std::env::args().skip(1);
    let config = match (
        arguments.next().as_deref(),
        arguments.next(),
        arguments.next(),
    ) {
        (None, None, None) => Ok(Config::default()),
        (Some("--config-json"), Some(value), None) => Config::from_json_str(&value),
        _ => Err("usage: exp046-policy [--config-json JSON]".to_owned()),
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
        let response = match serde_json::from_str::<backtest_contracts::ResearchRequest>(&line)
            .map_err(|error| error.to_string())
            .and_then(|request| runner.process_request(request, &bundle))
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
