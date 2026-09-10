# Pinned generic-engine source dependencies

The standalone historical policies need APIs that differ between their
published engine revisions. The minimum source crates are vendored so a clean
checkout builds without access to another private repository:

- `generic-backtest-engine-6ce0a23`: commit
  `6ce0a234130160e5fa0c39dc5115f0d878ceeb7f`, used by EXP021.
- `generic-backtest-engine-c3825ab`: commit
  `c3825ab856d4e480bd993c6c68a28ae4f69d602e`, used by EXP046.

Only `contracts`, `engine`, and `research-process` source plus their Cargo
manifests/locks are retained. No build output or historical run artifact is
vendored. Strategy logic still emits `TradeIntent`; the engine crates retain
pricing, fill, margin, cost and accounting ownership.
