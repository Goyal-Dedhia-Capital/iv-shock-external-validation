# H3/H5 external-validation execution plan

## Outcome

Validate the H3 short-response and H5 long-response IV-shock lineage on an
independent one-second NIFTY option source. Separate minute-clock replication,
one-second extensions, descriptive response, executable quote overlays, and
funded sequential books.

## First useful table

One source session, S0, one H3 family and one H5 family:

| lane | candidates | sequential trades | entry coverage | exit coverage | gross | spread | fees | net |
|---|---:|---:|---:|---:|---:|---:|---:|---:|

The public Asjad CSV may exercise schema code but cannot provide this validation:
it is one minute, one date, one expiry, and has no prior-session calibration.

## Stages and gates

1. **Source contract.** Data owner resolves every research field and, for the
   executable lane, every quote field. Duplicate keys or ambiguous availability
   fail closed.
2. **One-session preparation.** Normalize the full expiry inventory and causally
   seal one-minute bars. Audit row conservation, missingness, quote coverage,
   expiry inventory, and timestamps.
3. **Firm-owned S0 verification slice.** Run one H3 and one H5 family through
   the persistent decision boundary. Verify scores, event identity, recipient
   selection, schedules, fees, determinism, and uninterrupted/resumed equality.
   The data owner does not reproduce or approve historical result tapes.
4. **Five-session slice.** Exercise strictly chronological state and five
   independent books. No full-period launch follows a failed lifecycle or
   conservation invariant.
5. **Calibration audit.** Warm at least 60 strictly prior sessions and prove no
   current-session data enters thresholds.
6. **External evaluation.** Run 2024, 2025, and available 2026 separately. The
   actual source range controls these labels; absent dates are never invented.
7. **Resolution extensions.** Rolling 60-second and adjacent-second innovations
   receive new IDs. Evaluate 1/5/15/60-second entry delays.
8. **Layers.** Add quote execution, fees, liquidity/freshness, market regimes,
   structures, score gates, sizing, and portfolio construction one layer at a
   time, always reporting its incremental coverage and distribution change.

## Shared preparation and parallel work

`python/run_local.py` is the shared source reader and normalized minute producer.
The H3 and H5 pilot consumers must both read the same hashed sealed-minute
artifact. A focused integration test will assert that a warm second consumer
does not reread the source or rebuild the artifact.

Calibration and each sequential book remain ordered. After a detector's causal
event tape exists, independent family, price, cost, and report views may run in
parallel. Use date-partitioned Parquet, lazy Polars projections, dictionary-coded
IDs, and compact event/trade tapes. Do not materialize the full chain once per
variant.

## Result contract

Each variant reports population and clock identity, coverage funnel, overlapping
and sequential counts, gross/net distributions, PF, wins/zeros, tail quantiles,
drawdown, monthly/yearly stability, best-date concentration, unresolved
positions, and cross-family exposure. Costs are applied per fill before any
after-cost statistic is recomputed.

## Completion criteria

- Source contract and query identity committed; secrets and raw data absent.
- Exact input/config/code hashes in every run manifest.
- A versioned exchange calendar, including holidays and special sessions, is
  frozen before sequential runs; observed quotes never define session bounds.
- Causality, restart, dedup, ordering, fill/fee and conservation tests pass.
- Replication and extension identities never mix.
- S1-S5 retain their historical no-completed-full-grid label.
- Independent review finds no unresolved correctness issue in the exact result
  commit.
