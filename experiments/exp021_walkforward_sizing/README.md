# EXP021 — causal walk-forward sizing

## Question

After a base sequential pass accepts H3_F1, H3_F2 and H5_F2 trades, does a
strictly-prior family-local Ridge score improve results by assigning 2x to the
top fitted-score third and 1x to every other trade? The score never skips a
trade. January–June 2024 is a fixed 1x warm-up.

This experiment uses **three independent, non-compounding ₹10 lakh accounts**.
It must not be added as though it were one shared ₹10 lakh portfolio.

## Inputs

`accepted_features.parquet` must contain the entry-known feature columns listed
in `run_walkforward.py`, `strategy_position_id`, candidate identity and the
realized `net_micro` label from the completed base replay. Labels are used only
by later monthly fits; each test month trains strictly before its first day.

## Run

```bash
uv run python experiments/exp021_walkforward_sizing/run_walkforward.py \
  --input /absolute/path/to/base-feature-directory \
  --output /absolute/path/to/exp021-fit

uv run python experiments/exp021_walkforward_sizing/write_multipliers.py \
  --predictions /absolute/path/to/exp021-fit/predictions.parquet \
  --output /absolute/path/to/exp021-fit/multipliers.json

scripts/run-exp021 \
  --calendar contracts/exchange_calendar.local.json \
  --source-contract contracts/source_contract.local.json \
  --stage execution \
  --multipliers /absolute/path/to/exp021-fit/multipliers.json \
  < /absolute/path/to/base-policy-requests.jsonl \
  > /absolute/path/to/exp021-policy-responses.jsonl
```

`scripts/run-exp021` validates and hashes the source contract and calendar,
binds the clean Git revision, config and exact multiplier tape, then starts the
Rust policy. The engine must run each family in its own account, apply its own
capital admission and return feedback after every intent.

`reference_model_bundle.json` is the immutable firm reference
(`2c7db51363dc9bb79ad4d3adcbf8cdd5e5cb40f74338af76589c7f065df2d937`).
It is a diagnostic comparator, not permission to score dates from the friend's
dataset without a strictly-prior refit.
