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

- The data owner implements `python/marketdata_adapter.py`, completes
  `contracts/source_contract.example.json`, and runs all proprietary data
  access locally.
- The normalized feeder owns timestamps, source provenance, contract identity,
  resampling, and coverage.
- The persistent strategy process owns causal detector state and emits canonical
  trade intents.
- The execution engine owns fills, fees, margin, positions, and accounting.

## First local run

```bash
uv sync --dev
uv run pytest -q

cp contracts/source_contract.example.json contracts/source_contract.local.json
# Resolve every PENDING field in the local file. The local file is gitignored.

uv run python python/run_local.py \
  --input /absolute/path/to/one-second-export.csv \
  --source-contract contracts/source_contract.local.json \
  --cache-dir /absolute/path/to/local-cache \
  --manifest-out outputs/manifests/first-session.json
```

The first real-data output is a one-session schema and minute-aggregation audit.
The detector and sequential-book run begins only after this audit passes.

## Validation order

1. One session, full represented expiry inventory: schema and causal resampling.
2. One session: S0, one H3 family, and one H5 family.
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
- H3/H5 detector and family registry: implemented as specifications.
- Friend-owned API adapter: interface only; private API implementation pending.
- One-second source contract: pending data-owner completion.
- Real-data audit and detector parity: pending the private source.

Private external validation of H3/H5 IV-shock research on independent high-frequency option data
