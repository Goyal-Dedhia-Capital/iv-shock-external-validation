# Asjad data-owner handoff

## What this validation tests

The strategy question is whether the independently observed option-chain data
reproduces the H3 short-response and H5 long-response shock opportunities, and
whether the response is large and stable enough to survive real quote crossing,
additional adverse slippage, broker charges, missing quotes, capacity limits,
sequential occupancy and capital/margin admission.

The required attribution is:

```text
detected opportunity
→ data and quote coverage
→ sequentially admissible intent
→ executable-side quote and capacity
→ spread drag
→ additional slippage drag
→ Zerodha charges
→ net trade distribution and account drawdown
```

Missing coverage is never converted to zero P&L. It remains a rejected or
unresolved observation with an explicit reason.

## What the repository contains

- Source contract, timestamp and quote-authority gates.
- An Asjad table adapter that exact-joins authoritative spot and official lot
  size without substituting `forward` for spot or as-of filling either source.
- Causal one-second to sealed one-minute Polars aggregation.
- Rust S0–S5 detector code; only frozen S0 is currently admitted to the
  executable baseline.
- Rust H3_F1–F5 and H5_F1–F5 family boundaries and sequential policy.
- The later H3_F4 plus H5_F1/F2/F3/F5 structure/score policy, EXP021 sizing
  policy and separate EXP046 R2 policy.
- A generic-engine host that crosses bid/ask, applies adverse tick/bps
  slippage, enforces explicit liquidity and margin lanes, charges the frozen
  Zerodha schedule and writes immutable lifecycle evidence.
- A distribution reporter with per-family N, tails, median, win/zero rate,
  profit factor, drawdown, daily/monthly/yearly totals and date concentration.

## What the data owner must export

Keep API credentials and raw archives outside this repository. Export all
represented NIFTY option expiries for every requested session, plus:

- native timestamp and causal availability timestamp, or an explicit statement
  that availability equals the timestamp;
- authoritative NIFTY spot at the same timestamps;
- contract expiry, strike, CE/PE, OHLC, IV, TTM, Greeks, volume and OI;
- official date-effective lot size;
- bid and ask with an explicit statement of whether Asjad `buy_price` is best
  bid and `sell_price` is best ask;
- bid/ask sizes or depth if available;
- historical basket and complete portfolio margin if a funded H3 or vertical
  claim is required.

Traded volume is activity, not displayed depth. If quote sizes are unavailable,
the `unlimited_quote` lane is an optimistic executable-side bound and
`bar_volume_sensitivity` remains a labelled sensitivity.

## Public sample disposition

The published `nifty-08092026-option-chain.csv` is useful only for adapter
development. It has one session, one represented expiry, timestamps one minute
apart, no authoritative spot, no lot-size authority, no availability timestamp
and no quote depth. It cannot warm 60 prior sessions, establish source expiry
rank ≥3, test full-chain moneyness, validate one-second extensions, or support a
funded/liquidity conclusion.

## Run order on the data owner's laptop

1. Clone this repository and run `scripts/bootstrap` and `scripts/check`.
2. Copy `contracts/source_contract.example.json` to the ignored local contract
   and resolve every `PENDING` field from the provider/API owner.
3. Run `scripts/adapt-asjad` to exact-join option, spot and lot exports.
4. Run `scripts/preflight --stage execution` on one full multi-expiry session.
5. Do not continue if the manifest is `BLOCKED`; fix the named data mapping.
6. Produce chronological detector and policy packets from the admitted sealed
   minutes using the firm-owned feeder.
7. Run `scripts/run-funded` in distinct output directories for zero additional
   slippage and each tick/bps/capacity lane.
8. Run `scripts/report-funded` for each lane and compare identical position IDs.
9. Expand from one session to five, then 60 warm-up sessions, then 2024, 2025
   and available 2026 only after deterministic lifecycle and flat-boundary
   checks pass.

## What to send back to the firm

Do not send raw market data, API credentials, private provider code, or local
caches. Send one compact result bundle containing:

- the completed source contract with secrets and machine-local paths removed;
- the adapter and preflight manifests, including source/output hashes and the
  actual first/last session and expiry counts;
- the exact repository commit and `scripts/check` output;
- the feeder manifest binding sealed-minute input, detector responses and
  policy-event output;
- `summary.json` and `distributions.json` from every execution lane;
- rejection/coverage counts for missing IV, spot, entry quote, exit quote,
  quote size and margin;
- unresolved position IDs and their last observed same-session quotes;
- wall time, peak memory and result size for one session and five sessions.

Use separate directories for H3, H5, final structures and EXP046 R2, and for
each quote/slippage/capacity lane. Never merge their P&L before the individual
books reconcile. Large `steps.jsonl` files can stay on the data owner's laptop;
send their SHA-256 values and the compact reports unless the firm requests a
specific diagnostic extract.

## Current remaining integration gate

The repository does not yet contain the normalized-minute → detector packet →
policy-candidate → `SealedEvent` feeder. The adapter and funded engine are
executable and checked, but they are not a substitute for that causal bridge.
No one should claim an end-to-end Asjad result until the bridge is implemented,
tested on a synthetic full-chain session, and exercised on one admitted private
session. The bridge must recompute EXP004 formation features from the external
data and must not import old event or outcome tapes.

The bridge implementation must emit one manifest plus three chronological
interfaces: detector requests/responses, policy candidates with typed detector
evidence, and generic-engine `SealedEvent` packets with observed quote and
liquidity provenance. It must preserve 60 strictly prior sessions, source
expiry rank ≥3, near-ATM source scope, sign-pooled source non-overlap, t+1
entry, the fixed family horizons, and selection before entry/exit availability.
