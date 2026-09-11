mod costs;
mod execution;
mod margin;

use std::env;
use std::fs::{self, File};
use std::io::{BufRead, BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};

use backtest_contracts::{Money, ResearchRequest, ResearchResponse, SealedEvent};
use backtest_engine::{CrossSpreadPricing, DecisionRunner, Engine, EngineConfig};
use backtest_research_process::{ProcessRunner, ProcessSpec};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::costs::{BROKER_ID, SCHEDULE_ID, SCHEDULE_SHA256, ZerodhaCosts};
use crate::execution::{ExecutionConfig, LiquidityExecution};
use crate::margin::{EventMargin, MarginMode};

const RUN_SCHEMA: &str = "gdc.iv-shock.external-funded-run.v1";

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RunConfig {
    schema_version: String,
    input_events_jsonl: PathBuf,
    strategy_program: PathBuf,
    #[serde(default)]
    strategy_args: Vec<String>,
    output_dir: PathBuf,
    initial_cash_micro: i64,
    margin_mode: MarginMode,
    execution: ExecutionConfig,
}

#[derive(Debug, Serialize)]
struct StepRecord {
    schema_version: &'static str,
    sequence: u64,
    event_id: String,
    response: ResearchResponse,
    lifecycle: backtest_contracts::LifecycleEvidence,
    blockers: Vec<String>,
}

#[derive(Debug, Serialize)]
struct Summary {
    schema_version: &'static str,
    stream_consumed: bool,
    execution_complete: bool,
    event_count: u64,
    initial_cash_micro: i64,
    input_sha256: String,
    strategy_sha256: String,
    broker_id: &'static str,
    cost_schedule_id: &'static str,
    cost_schedule_sha256: &'static str,
    execution: ExecutionConfig,
    margin_mode: MarginMode,
    final_account: backtest_contracts::AccountState,
}

struct CapturingRunner {
    inner: ProcessRunner,
    last: Option<ResearchResponse>,
}

impl DecisionRunner for CapturingRunner {
    fn decide(&mut self, request: &ResearchRequest) -> Result<ResearchResponse, String> {
        let response = self.inner.decide(request)?;
        self.last = Some(response.clone());
        Ok(response)
    }
}

fn sha256(path: &Path) -> Result<String, Box<dyn std::error::Error>> {
    let mut file = File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0_u8; 1024 * 1024];
    loop {
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        hasher.update(&buffer[..count]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

fn read_config(path: &Path) -> Result<RunConfig, Box<dyn std::error::Error>> {
    let config: RunConfig = serde_json::from_reader(BufReader::new(File::open(path)?))?;
    if config.schema_version != RUN_SCHEMA {
        return Err(format!("unsupported run config schema {}", config.schema_version).into());
    }
    if config.initial_cash_micro <= 0 {
        return Err("initial_cash_micro must be positive".into());
    }
    if config.output_dir.exists() {
        return Err(format!(
            "output directory already exists: {}",
            config.output_dir.display()
        )
        .into());
    }
    if !config.input_events_jsonl.is_file() || !config.strategy_program.is_file() {
        return Err("input_events_jsonl and strategy_program must be existing files".into());
    }
    Ok(config)
}

// Keeping immutable output creation, engine construction, streaming, and failure
// evidence together makes the all-or-nothing run boundary easy to audit.
#[allow(clippy::too_many_lines)]
fn run(config: &RunConfig) -> Result<(), Box<dyn std::error::Error>> {
    let input_hash = sha256(&config.input_events_jsonl)?;
    let strategy_hash = sha256(&config.strategy_program)?;
    fs::create_dir(&config.output_dir)?;
    let result = (|| -> Result<Summary, Box<dyn std::error::Error>> {
        let process = ProcessRunner::spawn(&ProcessSpec {
            program: config.strategy_program.to_string_lossy().into_owned(),
            args: config.strategy_args.clone(),
        })?;
        let mut runner = CapturingRunner {
            inner: process,
            last: None,
        };
        let execution = LiquidityExecution::new(config.execution)
            .map_err(|error| format!("invalid execution config: {error}"))?;
        let mut engine = Engine::new(
            EngineConfig {
                initial_cash: Money(config.initial_cash_micro),
            },
            CrossSpreadPricing,
            ZerodhaCosts,
            EventMargin::new(config.margin_mode),
            execution,
        )?;
        if config.margin_mode == MarginMode::Authoritative {
            engine = engine.with_authoritative_portfolio_margin();
        }
        let events = BufReader::new(File::open(&config.input_events_jsonl)?);
        let mut output = BufWriter::new(File::create(config.output_dir.join("steps.jsonl"))?);
        let mut count = 0_u64;
        for (line_number, line) in events.lines().enumerate() {
            let line = line?;
            if line.trim().is_empty() {
                continue;
            }
            let event: SealedEvent = serde_json::from_str(&line)
                .map_err(|error| format!("invalid event at line {}: {error}", line_number + 1))?;
            let event_id = event.event_id.clone();
            let sequence = event.sequence;
            let step = engine.process_next(event, &mut runner)?;
            let response = runner
                .last
                .take()
                .ok_or("strategy response was not captured")?;
            serde_json::to_writer(
                &mut output,
                &StepRecord {
                    schema_version: RUN_SCHEMA,
                    sequence,
                    event_id,
                    response,
                    lifecycle: step.evidence,
                    blockers: step.feedback.blockers,
                },
            )?;
            output.write_all(b"\n")?;
            engine.take_evidence();
            count = count.checked_add(1).ok_or("event count overflow")?;
        }
        output.flush()?;
        let snapshot = engine.snapshot();
        serde_json::to_writer_pretty(
            BufWriter::new(File::create(config.output_dir.join("final_snapshot.json"))?),
            &snapshot,
        )?;
        let final_account = engine.account()?;
        Ok(Summary {
            schema_version: RUN_SCHEMA,
            stream_consumed: true,
            execution_complete: final_account.positions.is_empty(),
            event_count: count,
            initial_cash_micro: config.initial_cash_micro,
            input_sha256: input_hash.clone(),
            strategy_sha256: strategy_hash.clone(),
            broker_id: BROKER_ID,
            cost_schedule_id: SCHEDULE_ID,
            cost_schedule_sha256: SCHEDULE_SHA256,
            execution: config.execution,
            margin_mode: config.margin_mode,
            final_account,
        })
    })();
    match result {
        Ok(summary) => {
            serde_json::to_writer_pretty(
                BufWriter::new(File::create(config.output_dir.join("summary.json"))?),
                &summary,
            )?;
            Ok(())
        }
        Err(error) => {
            let failure = serde_json::json!({
                "schema_version": RUN_SCHEMA,
                "stream_consumed": false,
                "execution_complete": false,
                "error": error.to_string(),
                "input_sha256": input_hash,
                "strategy_sha256": strategy_hash,
            });
            serde_json::to_writer_pretty(
                BufWriter::new(File::create(config.output_dir.join("failure.json"))?),
                &failure,
            )?;
            Err(error)
        }
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = env::args_os();
    let _program = args.next();
    let config = args
        .next()
        .ok_or("usage: external-funded-runner RUN_CONFIG.json")?;
    if args.next().is_some() {
        return Err("usage: external-funded-runner RUN_CONFIG.json".into());
    }
    run(&read_config(Path::new(&config))?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::{BTreeMap, BTreeSet, VecDeque};

    use backtest_contracts::{
        CONTRACT_VERSION, IntentAction, IntentLeg, MarketQuote, QuoteUse, Side, TradeIntent,
    };

    struct QueueRunner(VecDeque<ResearchResponse>);

    impl DecisionRunner for QueueRunner {
        fn decide(&mut self, _request: &ResearchRequest) -> Result<ResearchResponse, String> {
            self.0
                .pop_front()
                .ok_or_else(|| "empty response queue".into())
        }
    }

    fn event(sequence: u64, bid: i64, ask: i64) -> SealedEvent {
        SealedEvent {
            schema_version: CONTRACT_VERSION.into(),
            event_id: format!("e{sequence}"),
            sequence,
            decision_at_ns: i64::try_from(sequence + 1).unwrap(),
            available_at_ns: i64::try_from(sequence + 1).unwrap(),
            sealed_at_ns: i64::try_from(sequence + 1).unwrap(),
            quotes: BTreeMap::from([(
                "x".into(),
                MarketQuote {
                    instrument_id: "x".into(),
                    bid: Some(Money(bid)),
                    ask: Some(Money(ask)),
                    mark: Some(Money(i64::midpoint(bid, ask))),
                    observed_at_ns: i64::try_from(sequence + 1).unwrap(),
                    available_at_ns: i64::try_from(sequence + 1).unwrap(),
                    source_id: "observed_fixture".into(),
                    allowed_uses: BTreeSet::from([QuoteUse::Execution, QuoteUse::Accounting]),
                },
            )]),
            margin_facts: BTreeMap::new(),
            research_payload: serde_json::json!({"execution_liquidity":{"x":{
                "source_kind":"observed","source_id":"observed_fixture",
                "bid_size":10,"ask_size":10,
                "bar_volume":20,"tick_size_micro":50000
            }}}),
        }
    }

    fn response(action: IntentAction, side: Side, suffix: &str) -> ResearchResponse {
        ResearchResponse {
            schema_version: CONTRACT_VERSION.into(),
            artifact_consumed: true,
            runner_id: "fixture".into(),
            bundle_hash: "fixture".into(),
            state: serde_json::json!({"step": suffix}),
            actions: vec![TradeIntent {
                schema_version: CONTRACT_VERSION.into(),
                intent_id: format!("i-{suffix}"),
                decision_id: format!("d-{suffix}"),
                strategy_position_id: "p".into(),
                basket_key: "p".into(),
                action,
                atomic: true,
                legs: vec![IntentLeg {
                    instrument_id: "x".into(),
                    side,
                    quantity: 1,
                    limit_price: None,
                }],
                lineage: BTreeMap::new(),
            }],
        }
    }

    #[test]
    fn engine_path_crosses_spread_applies_slippage_and_costs_once() {
        let execution = LiquidityExecution::new(ExecutionConfig {
            capacity_mode: crate::execution::CapacityMode::TopOfBook,
            slippage_bps: 10,
            slippage_ticks: 0,
            volume_participation_bps: 0,
        })
        .unwrap();
        let mut engine = Engine::new(
            EngineConfig {
                initial_cash: Money(1_000_000_000),
            },
            CrossSpreadPricing,
            ZerodhaCosts,
            EventMargin::new(MarginMode::PremiumOnly),
            execution,
        )
        .unwrap();
        let mut runner = QueueRunner(VecDeque::from([
            response(IntentAction::Open, Side::Buy, "open"),
            response(IntentAction::Close, Side::Sell, "close"),
        ]));
        let opened = engine
            .process_next(event(0, 99_000_000, 100_000_000), &mut runner)
            .unwrap();
        let closed = engine
            .process_next(event(1, 110_000_000, 111_000_000), &mut runner)
            .unwrap();
        assert_eq!(
            opened.evidence.outcomes[0].fills[0].price,
            Money(100_100_000)
        );
        assert_eq!(
            closed.evidence.outcomes[0].fills[0].price,
            Money(109_890_000)
        );
        assert!(opened.evidence.outcomes[0].fills[0].fee.0 > 0);
        assert!(closed.evidence.outcomes[0].fills[0].fee.0 > 0);
        let account = engine.account().unwrap();
        assert!(account.positions.is_empty());
        assert!(account.realized_pnl.0 > 0);
        assert_eq!(
            account.fees_paid.0,
            opened.evidence.outcomes[0].fills[0].fee.0 + closed.evidence.outcomes[0].fills[0].fee.0
        );
    }
}
