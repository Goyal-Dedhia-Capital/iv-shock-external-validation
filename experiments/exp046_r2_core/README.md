# EXP046 — R2 OTM score-tier core

## Question

Does the separate R2 call-short mechanism transfer to the independent source?
The promoted core is one OTM NIFTY CE short, normally 250–500 points above the
event-time ATM, 31–60 calendar DTE, with a 30-minute hold. It requires a
positive coherent CE IV shock with inherited `|z| >= 2`, the causal
spot/volume/DTE gate and the frozen 2024 Ridge score. Q1 is rejected; Q2/Q3 use
one official lot; Q4/Q5 use two subject to engine capital admission.

This strategy owns **one separate, non-compounding ₹10 lakh account**. It is
not one of the EXP032/033 five families.

## Run

The feeder must create the `ResearchRequest` JSONL shape consumed by
`src/lib.rs`, including the complete entry-known feature map used by
`MODEL.json`, and preserve feedback between minutes. End every session with a
`session_end` packet after the final minute; any still-active or pending
position is retained as unresolved and its book is quarantined.

```bash
scripts/run-exp046 \
  --calendar contracts/exchange_calendar.local.json \
  --source-contract contracts/source_contract.local.json \
  --stage execution \
  < /absolute/path/to/r2-policy-requests.jsonl \
  > /absolute/path/to/r2-policy-responses.jsonl
```

The embedded frozen model SHA is
`82e757f6787238222f4595fe106f7238281104e8292f25af79cfcf5c5208a16a`.
Use it first for a transfer diagnostic. A friend-data refit is a new experiment
and must use only prior labels under a new model ID.

The launcher verifies that embedded SHA, the clean Git revision, source
contract, exchange calendar and core-only policy config, and binds all of them
into the run identity emitted by the persistent policy.

The `[1.50, 1.75)` low-z lane and other internal books remain coded historical
sensitivities, but `policy_config.json` launches only promoted
`R2_SCORE_TIER_2X`. They must not be merged into the core result.
