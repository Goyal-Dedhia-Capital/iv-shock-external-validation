# External funded and execution-overlay runner

This runner answers one narrow question: after the Rust H3/H5 policy has emitted
an intent, does that intent survive observed bid/ask crossing, adverse slippage,
the frozen Zerodha options fee schedule, capacity evidence, margin and account
constraints?

It consumes chronological generic-engine `SealedEvent` JSONL. The event's
`research_payload` is the exact policy packet. Each quote is owner-attested and
has separate execution/accounting authority. Every liquidity fact must contain
`source_kind: observed`; synthetic or PCHIP data cannot execute.

## Economic lanes

`capacity_mode` has three meanings:

- `unlimited_quote`: requires the executable side of the quote but makes no
  depth claim. This is the most optimistic spread-crossing lane.
- `top_of_book`: requires owner-attested size on the aggressive side and rejects
  an atomic basket when that size is insufficient.
- `bar_volume_sensitivity`: caps quantity to the declared fraction of traded
  bar volume. It is an activity sensitivity, not observed queue depth.

Adverse slippage is the larger of `slippage_bps` and `slippage_ticks`, applied
once after crossing the spread. Zerodha charges then apply per fill in integer
micro-rupees. The cost schedule is
`zerodha_options_nse_backtest_v1`, SHA-256
`80f00b252f4bf365779c3c93b1436e20af3a8fba938d51957d99ba837934b9d3`.

`margin_mode` is deliberately explicit:

- `premium_only`: admits pure long-option opens; any opening sell leg fails.
- `authoritative`: requires a causal basket margin fact for every opening
  short leg and a `__portfolio__` total at every event. Missing facts block new
  risk while allowing closes.
- `unfunded_zero_sensitivity`: applies zero collateral only to isolate
  spread/slippage/fee decay. Its results are never funded returns.

## Run

Build the policy and runner from a clean reviewed revision, then copy and edit
the tracked example outside Git:

```bash
cargo build --locked --release --manifest-path rust/Cargo.toml \
  -p iv-shock-strategy-policy --bin iv-shock-strategy-policy
cp contracts/funded_run.example.json /absolute/path/to/funded_run.local.json
scripts/run-funded /absolute/path/to/funded_run.local.json
scripts/report-funded \
  --steps /absolute/path/to/output/steps.jsonl \
  --summary /absolute/path/to/output/summary.json \
  --output /absolute/path/to/output/distributions.json
```

The output directory must not exist. A successful run contains `steps.jsonl`,
`final_snapshot.json`, and `summary.json`; a failed run retains `failure.json`.
The summary binds the exact input and strategy hashes, cost kernel, execution
lane, margin lane, initial capital and final account. `stream_consumed` means
the input ended without a host error; `execution_complete` is true only when
the final account is flat.

## Result interpretation

Run separate immutable directories for zero extra slippage and each chosen
tick/bps sensitivity. Compare gross cash P&L, fees and net P&L from the same
completed positions. Rejections and unresolved positions remain coverage; they
are not zero-return trades. The distribution report includes N, mean, median,
P05/P25/P75/P95, win/zero rates, profit factor, maximum sequential drawdown,
daily P&L and largest positive-date concentration for every family.

Without owner-attested quote semantics, this runner cannot claim executable
fills. Without bid/ask size or depth, it cannot claim scalable liquidity.
Without authoritative historical margin, H3 can be measured only in the
explicit unfunded execution-sensitivity lane.
