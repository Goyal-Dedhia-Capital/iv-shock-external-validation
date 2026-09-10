use std::io::{self, BufRead, Write};
use std::path::Path;

use backtest_contracts::ResearchRequest;
use iv_shock_calendar_guard::CalendarGuard;
use iv_shock_decision::{InputPacket, Runner, RunnerConfig};

fn calendar() -> io::Result<CalendarGuard> {
    let path = std::env::var("IV_SHOCK_CALENDAR_PATH").map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "IV_SHOCK_CALENDAR_PATH is required",
        )
    })?;
    let hash = std::env::var("IV_SHOCK_CALENDAR_SHA256").map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "IV_SHOCK_CALENDAR_SHA256 is required",
        )
    })?;
    std::env::var("IV_SHOCK_SOURCE_CONTRACT_SHA256").map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "IV_SHOCK_SOURCE_CONTRACT_SHA256 is required",
        )
    })?;
    CalendarGuard::load(Path::new(&path), &hash).map_err(io::Error::other)
}

fn validate_calendar(guard: &CalendarGuard, request: &ResearchRequest) -> io::Result<()> {
    let payload = request
        .input
        .research_payload
        .get("input")
        .unwrap_or(&request.input.research_payload)
        .clone();
    let packet: InputPacket = serde_json::from_value(payload).map_err(io::Error::other)?;
    if matches!(
        packet.kind.to_ascii_lowercase().as_str(),
        "session_start" | "minute_packet" | "session_end"
    ) {
        let date = packet.session_date.as_deref().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "strategy packet session_date missing",
            )
        })?;
        if packet.kind.eq_ignore_ascii_case("minute_packet") {
            let minute = packet.ts_minute.ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    "minute packet timestamp missing",
                )
            })?;
            guard
                .validate_minute(date, minute)
                .map_err(io::Error::other)?;
        } else {
            guard
                .validate_session_date(date)
                .map_err(io::Error::other)?;
        }
    }
    Ok(())
}

fn main() -> io::Result<()> {
    let guard = calendar()?;
    let bundle_hash = std::env::var("IV_SHOCK_RESEARCH_BUNDLE_HASH").map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "IV_SHOCK_RESEARCH_BUNDLE_HASH is required",
        )
    })?;
    let stdin = io::stdin();
    let mut stdout = io::BufWriter::new(io::stdout().lock());
    let mut runner = Runner::new(RunnerConfig {
        bundle_hash,
        ..RunnerConfig::default()
    });
    for line in stdin.lock().lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let request: ResearchRequest = serde_json::from_str(&line)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        validate_calendar(&guard, &request)?;
        let response = runner
            .process_canonical_request(request)
            .map_err(io::Error::other)?;
        serde_json::to_writer(&mut stdout, &response)?;
        writeln!(stdout)?;
        stdout.flush()?;
    }
    Ok(())
}
