# Executable historical strategy books

These are three distinct strategies. Do not pool their accounts or reuse their
model identities.

| Strategy | Directory | Account | Purpose |
|---|---|---|---|
| EXP021 | `exp021_walkforward_sizing/` | three independent ₹10 lakh books | test whether a prior-only score should size accepted H3_F1, H3_F2 and H5_F2 trades at 1x or 2x |
| EXP032/033 | root `rust/crates/strategy_policy` | one shared ₹10 lakh account | test the final score-gated H3_F4 plus four H5 vertical portfolio |
| EXP046 | `exp046_r2_core/` | one separate ₹10 lakh account | test the promoted R2 OTM call-short score-tier book |

The root policy uses the current normalized packet contract. EXP021 and EXP046
preserve their published persistent JSONL policy boundaries and pinned generic
engine revisions. All three emit `TradeIntent`; fills, fees, margin, cash and
P&L belong to the engine.

No raw data, execution tape, ledger, checkpoint, binary or credential belongs
in this repository.
