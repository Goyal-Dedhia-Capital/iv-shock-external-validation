# Code verification without historical tape parity

The data owner is not responsible for reproducing the firm's historical event
tapes. Strategy correctness is owned and tested in this repository.

Run one command after every change:

```bash
scripts/check
```

It runs Python source-contract and causal-resampling tests, validates every
committed JSON registry, formats and lints the complete Rust workspace with
warnings denied, and runs all Rust contract, detector, scheduling, family,
lifecycle, feedback, and restart tests. It also builds and tests the standalone
EXP021 and EXP046 policy packages from their locked dependency graphs while
placing compiler output outside the checkout.

The fixed tests cover hand-checkable detector formulas, strictly prior
calibration, quiet-gap behavior, frozen scheduling, near-ATM/rank/DTE family
boundaries, deterministic recipient ordering, ten independent books, inverse
close legs, rejected-open release, rejected-close retry, partial-fill halt,
transactional error rollback, exact previous-sequence feedback, and fresh
process restart equivalence. It also covers typed S0 evidence matching,
bundle-mismatch rejection, planned-exit coverage rejection, and per-book
session-end quarantine without overnight retries. The shared Rust calendar guard independently
rehashes the frozen calendar, validates its authority and timezone, rejects
weekends/holidays, honors special sessions, and enforces half-open minute
windows before any strategy process mutates state.

The data owner's validation obligation is narrower:

1. Complete `contracts/source_contract.local.json` and a non-PENDING exchange
   calendar from the examples.
2. Export the proprietary source outside this checkout and provide the
   normalized input path; keep provider code and credentials private.
3. Run `scripts/check`, then `scripts/preflight` on one full session.
4. Send the compact failure JSON and immutable manifest for any failure. Do not
   change detector thresholds, family labels, clocks, or lifecycle rules to
   make a source pass.

Real-data checks then test mapping and coverage: row conservation, timestamp
availability, expiry inventory, quote authority, IV provenance, duplicates,
missingness, and source-calendar identity. They do not redefine the strategy.

The executable authorizes qualifying frozen-S0 events through both the ten-book
single-leg baseline and the later five-family final-candidate mode. Final mode
rehashes and scores the frozen EXP032 monthly model, applies causal filters,
validates authoritative one-lot sizing plus event-time expiry/chain identity
and outward/vertical basket geometry, and emits atomic multileg intents.
S1-S5 and new one-second detector definitions remain diagnostic specifications.
