use std::io::{self, BufRead, Write};

use backtest_contracts::{CONTRACT_VERSION, ResearchRequest, ResearchResponse};
use serde_json::json;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let stdin = io::stdin();
    let mut stdout = io::BufWriter::new(io::stdout().lock());
    for line in stdin.lock().lines() {
        let request: ResearchRequest = serde_json::from_str(&line?)?;
        let response = ResearchResponse {
            schema_version: CONTRACT_VERSION.to_owned(),
            artifact_consumed: true,
            runner_id: "jsonl-contract-fixture".to_owned(),
            bundle_hash: "fixture-bundle".to_owned(),
            state: json!({"sequence": request.sequence}),
            actions: Vec::new(),
        };
        serde_json::to_writer(&mut stdout, &response)?;
        stdout.write_all(b"\n")?;
        stdout.flush()?;
    }
    Ok(())
}
