# IV-shock external validation

Private, contract-first validation of the firm's H3 short-response and H5
long-response IV-shock research on an independent high-frequency NIFTY options
dataset.

This repository contains strategy specifications, normalized interfaces,
causal resampling code, compact fixtures, and result contracts. It must not
contain proprietary raw option data, API credentials, or mutable local caches.

## Research lanes

1. **Minute replication:** causally aggregate the independent one-second source
   to sealed one-minute bars, then reproduce the frozen H3/H5 timing and
   selection semantics.
2. **High-resolution extension:** evaluate rolling 60-second and native
   one-second innovations with separately labelled entry-delay sensitivities.
3. **Execution overlay:** add confirmed bid/ask authority, broker costs, and
   sequential account constraints without changing the descriptive baseline.

The high-resolution lanes are new sensitivities. They are never reported as
replications of the frozen minute-clock studies.

## Ownership boundary

- The data owner exports proprietary data outside this checkout, completes an
  ignored local source contract, and runs all private data access locally.
- The normalized feeder owns timestamps, source provenance, contract identity,
  resampling, and coverage.
- The persistent strategy process owns causal detector state and emits canonical
  trade intents.
- The execution engine owns fills, fees, margin, positions, and accounting.

The two standalone historical policies compile against minimal source snapshots
of their exact published engine revisions under `vendor/`; this avoids a hidden
cross-repository credential dependency. No engine binary or run output is stored.

## First local run

```bash
uv sync --dev
scripts/check

cp contracts/source_contract.example.json contracts/source_contract.local.json
# Resolve every PENDING field in the local file. The local file is gitignored.

scripts/preflight \
  --input /absolute/path/to/one-second-export.csv \
  --source-contract contracts/source_contract.local.json \
  --cache-dir /absolute/path/to/local-cache \
  --manifest-out outputs/manifests/first-session.json
```

After the firm accepts that manifest and freezes a `READY` calendar, launch a
runner through the binding guard (never call a release binary directly):

```bash
scripts/run-strategy \
  --runner detector \
  --calendar /absolute/path/to/exchange-calendar.json \
  --source-contract contracts/source_contract.local.json \
  --stage research
```

Use `--runner books` for full-chain scheduling/recipient views and `--runner
policy` for the feedback-aware ten-family single-leg lifecycle. The launcher
rejects placeholders or dirty strategy code, validates the complete
stage-specific source contract, and binds the Git revision,
calendar SHA-256, and source-contract SHA-256 into the run bundle identity.

The preflight command returns `READY`, `READY_WITH_WARNINGS`, or `BLOCKED` and
records the exact coverage reasons in its immutable manifest. The strategy
launcher instead streams one Rust `ResearchResponse` per input request. A
malformed contract or source returns one compact `FAILED` JSON error instead
of a Python traceback. The data owner fixes only the named mapping or source issue; the firm
strategy owner is responsible for all detector, causality, scheduling, and
accounting verification.

Before any multi-session strategy run, the firm strategy owner also freezes a
versioned exchange calendar from `contracts/exchange_calendar.example.json`.
Observed quote timestamps must never be used to infer holidays or special
session boundaries.

## Validation order

Exact experiment definitions and commands are in
[`docs/EXPERIMENTS.md`](docs/EXPERIMENTS.md) and
[`docs/RUNBOOK.md`](docs/RUNBOOK.md). The three finalized historical strategy
books have separate, runnable handoffs under [`experiments/`](experiments/README.md).

1. One session, full represented expiry inventory: schema and causal resampling.
2. Firm-owned verification: S0, one H3 family, and one H5 family on synthetic
   causal fixtures plus the admitted external session.
3. Five chronological sessions: checkpoint/restart and book conservation.
4. Sixty prior sessions: calibration-state audit.
5. Full 2024, then 2025, then available 2026.
6. Alternative detectors and one-second extensions.
7. Feature, structure, sizing, cost, and combined-portfolio layers.

## Required reporting

Every result must retain population, source policy, detector, clocks, direction,
overlap policy, entry/exit authority, cost profile, coverage, unresolved
positions, trade distribution, drawdown, month/year stability, and best-date
concentration. Planned or pilot-only historical variants remain labelled as such.

## Current status

- Private repository scaffold: implemented.
- H3/H5 detector and family registries: committed specifications enforced by
  the Rust policy boundary.
- Frozen S0 detector and six-detector Rust core: implemented.
- All ten fixed H3/H5 family boundaries: implemented and tested.
- Feedback-aware single-leg lifecycle with ten independent books: implemented.
- Typed S0 detector-to-policy evidence matching, bundle-bound restart, and
  same-session unresolved-position quarantine: implemented.
- Final H3_F4 outward CE and H5 PE vertical structures, causal filters, and
  frozen monthly score evaluation: implemented in the Rust policy.
- Private API export remains data-owner code outside this repository.
- One-second source contract: pending data-owner completion.
- Real-data admission audit is pending the private source. Historical tape
  parity is not assigned to the data owner; strategy correctness is enforced by
  deterministic expected-answer, adversarial, lifecycle, and restart tests in
  `scripts/check`.
