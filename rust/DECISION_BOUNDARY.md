# Persistent research decision boundary

No source-preflight manifest by itself authorizes this process to trade or
backtest.
Every chronological runner must additionally require a non-`PENDING`
exchange-calendar file, hash that exact file into its run manifest, and fail
closed when the local session date is an unlisted weekend, holiday, or
unsupported special session. Calendar windows are half-open minute-start
intervals; holding horizons use elapsed calendar minutes unless the frozen
variant says otherwise.

`scripts/run-strategy` supplies the file path and its SHA-256. Each Rust binary
rehashes and parses the same file before reading stdin, then validates every
declared session and event minute. The policy runner validates entry minutes
and the declared eligible session end. Planned exits beyond it become counted
`planned_session_end_ineligible` coverage rejections instead of packet errors.

The repository now contains three persistent Rust processes:

- `iv-shock-decision`: frozen S0 plus separately labelled S1-S5 diagnostics;
- `iv-shock-sequential-books`: source scheduling and full-chain recipient views;
- `iv-shock-strategy-policy`: feedback-aware ten-book single-leg baseline plus
  the five-family outward/vertical final-candidate mode.

The detector binary is diagnostics-only. It rejects direct intent requests;
only a qualifying S0 diagnostic (support at least 200, exact threshold 2,
post-quiet qualification) may enter the funded policy. S1-S5 remain diagnostic
sensitivities until their prior-only rate-matching rules are frozen.
Every policy candidate must match the typed S0 diagnostic by source contract,
event minute, sign, score, threshold, support, and quiet status.

An explicit session-end packet quarantines active or pending positions in the
affected family book, records `unresolved_at_session_end`, and emits no
overnight retry. Other family books remain independent. Checkpoints bind the
exact code/calendar/source bundle and reject a different bundle on restore.

Each process reads one canonical `ResearchRequest` JSON object per line and
writes one canonical `ResearchResponse` per line.

The funded policy candidate accepts only entry-known fields. It rejects family
labels that disagree with source side, shock sign, surface, DTE, represented
expiry rank, near-ATM moneyness, H3 formation lane, or recipient-relative bin.
It also rejects H3 buys, H5 sells, future endpoint fields, duplicate IDs,
out-of-order feedback, and non-chronological minutes.

Final-candidate mode additionally requires the exact five-family registry,
same-expiry 300/500-point PE vertical geometry or deterministic H3 outward-1
CE selection, event-time spot/skew/volume predicates, and a positive score
recomputed inside Rust from the SHA-bound EXP032 family-month model fit.

The exact request payload schema is in `contracts/strategy_packet.schema.json`;
the surrounding protocol types are defined in `rust/crates/contracts`. A
minimal complete request envelope is:

```json
{
  "schema_version": "gdc.generic-backtest.v1",
  "input": {
    "schema_version": "gdc.generic-backtest.v1",
    "event_id": "event-0",
    "sequence": 0,
    "decision_at_ns": 0,
    "available_at_ns": 0,
    "sealed_at_ns": 0,
    "quotes": {},
    "margin_facts": {},
    "research_payload": {"kind": "minute", "minute": 0, "session_date": "2024-07-01", "candidates": [], "detector_diagnostics": []}
  },
  "state": {},
  "feedback": {"sequence": 0, "outcomes": [], "account": {"cash": 0, "reserved_margin": 0, "realized_pnl": 0, "unrealized_pnl": 0, "fees_paid": 0, "equity": 0, "positions": []}, "blockers": []},
  "sequence": 0,
  "feedback_context_hash": "sha256-context",
  "feedback_feature_hash": "sha256-features"
}
```

Response:

```json
{
  "artifact_consumed": true,
  "runner_id": "...",
  "bundle_hash": "...",
  "state": {},
  "actions": []
}
```

The process completes and flushes each response before reading the next
request. Research code cannot read historical files or price fills. The feeder
owns ordered market inputs; the execution engine owns fills, costs, margin and
accounting. The detector and policy are already present; the independent data
owner only completes the source mapping and exchange-calendar contract.
