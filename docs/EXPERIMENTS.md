# H3/H5 experiment catalogue

This catalogue separates historical research lineage from code that can be run on the independent one-second source. Historical IDs are never reused. A row marked **executable** has a maintained entrypoint in this repository; **specification** means the definition is retained for interpretation or a later sensitivity and cannot create funded intents.

The three finalized historical strategy books are documented separately in
`experiments/`: EXP021 causal sizing, EXP032/033 final five-family portfolio,
and EXP046 R2 OTM score-tier core. They are alternatives with different account
semantics, not layers to add together automatically.

## Executable validation experiments

| External ID | What it answers | Strategy | Entry point |
|---|---|---|---|
| `EXT-S0-DETECTOR` | Do frozen 2-MAD IV shocks reproduce on causally sealed one-minute data? | S0, 60 strictly prior sessions, support ≥200, 15-minute quiet gap | `scripts/run-strategy --runner detector` |
| `EXT-H3-BASELINE` | Do the five selected short-response families survive sequential execution? | H3_F1–F5, single CE short, five independent books | `scripts/run-strategy --runner policy` |
| `EXT-H5-BASELINE` | Do the five selected long-response families survive sequential execution? | H5_F1–F5, single PE long, five independent books | `scripts/run-strategy --runner policy` |
| `EXT-FINAL-STRUCTURES` | Do the later structure, causal filters and frozen score transfer together? | H3_F4 outward-1 CE; H5 F1/F2/F3/F5 300/500-point PE verticals | final policy with both `variants/final_portfolio_policy_config.json` and the reference model bundle |
| `EXT-FINAL-SCORE` | Does the frozen prior-month ridge score transfer? | Exact EXP032 family-month coefficients; admit only `rank_score_micro > 0` | final policy plus `models/exp032_reference_model_bundle.json` |
| `EXT-FINAL-FUNDED` | Do those intents survive executable quotes, costs and shared capital? | One shared non-compounding ₹10 lakh engine account | external engine integration; this repository emits and verifies intents but does not vendor the engine |
| `EXT-EXP021-SIZING` | Does causal 1x/2x sizing help three accepted books? | H3_F1, H3_F2 and H5_F2; score changes size but never admission | `scripts/run-exp021` after the documented monthly fit |
| `EXT-EXP046-R2` | Does the separate promoted R2 OTM call-short book transfer? | 31–60 DTE, 250–500-point OTM CE, 30-minute hold, Q1 reject/Q4–Q5 2x | `scripts/run-exp046` |

The reference model is deliberately frozen. It may score only dates covered by its exact family-month fits, July 2024 through December 2025. A different period requires a new, separately identified strictly-prior model bundle; it must not silently reuse the reference coefficients.

## Final five-family portfolio

| Family | Shock family inherited from baseline | Basket | Causal prefilter |
|---|---|---|---|
| H3_F4 | CE positive, coherent, source DTE 0–30 | Sell nearest represented CE one step farther from ATM than the selected anchor | 30-minute spot return ≤0; minimum leg event volume ≤0 |
| H5_F1 | CE positive, coherent, source DTE 31–60 | Buy selected PE; sell same-expiry PE 500 points lower | 30-minute spot return ≤0 |
| H5_F2 | CE positive, coherent, source DTE 0–30 | Buy selected PE; sell same-expiry PE 300 points lower | spot ≤0; risk reversal >0; minimum leg event volume ≤0 |
| H5_F3 | PE negative, coherent, source DTE 0–30 | Buy selected PE; sell same-expiry PE 300 points lower | spot ≤0; minimum leg event volume ≤0 |
| H5_F5 | PE positive, idiosyncratic, source DTE 31–60 | Buy selected PE; sell same-expiry PE 500 points lower | spot ≤0; risk reversal >0; minimum leg event volume ≤0 |

Missing features or legs fail closed. The Rust policy verifies leg side, option type, same expiry, exact width, equal quantity, H3 outward ordering, typed S0 evidence, and the positive score gate. It emits each basket atomically. Fill prices, costs, margin and P&L remain engine-owned.

## Historical H3 lineage

| IDs | Purpose | External status |
|---|---|---|
| EXP004–009 | Detector definitions, tradability, horizons, RAW/PCHIP response and canonical S0 response | Lineage for `EXT-S0-DETECTOR`; PCHIP remains research-only for formation and never fills |
| EXP010–013 | Mechanism, cross-expiry response, funded five shorts and actual sequential five-book research | Lineage for `EXT-H3-BASELINE` |
| EXP014 | Ten-family causal feature diagnostics | Specification and feature provenance |
| EXP015–018 | Multileg structures, tier confirmation, liquidity and entry-price sensitivities | Structure/coverage lineage |
| EXP019–020 | Funded family portfolio and spot filter | Lifecycle and spot-filter lineage |
| EXP021–025 | Walk-forward sizing, ATM distance, liquidity, native geometry, feature/risk/sizing | Model and final-structure lineage |
| EXP026–030 | Portfolio construction, funded/overlapping books and ranked overlap | Portfolio comparison lineage; overlapping research is not a sequential funded result |
| EXP031–033 | Score admission, final score-gated book and Groww/capital sensitivity | Lineage for `EXT-FINAL-SCORE` and `EXT-FINAL-FUNDED` |
| EXP034 | Entry-volume liquidity diagnostic | Sensitivity only; it does not rewrite the final policy |
| EXP035–040 | OTM/cross-expiry R2 discovery, features, margin, tradability and score priority | Lineage for the separate EXP046 book |
| EXP041–045 | R2 vertical, rho and score-only challengers | Rejected or intermediate sensitivities; not silently included in promoted R2 |
| EXP046 | R2 shock-threshold screen and promoted score-tier core | Executable separately as `EXT-EXP046-R2` |

## Historical H5 lineage

| ID | Purpose | External status |
|---|---|---|
| EXP001 | Long response across the option chain | Lineage for H5 family discovery |
| VALIDATION_S0 | Frozen S0 long-family and sequential layers | Lineage for `EXT-H5-BASELINE` |
| VALIDATION_S1_S5 | Alternative detector pilots | Diagnostic specification only; no completed full grid |

## Interpretation boundaries

Native CLOSE replication, confirmed bid/ask execution and PCHIP sensitivities are different result lanes. The reference Groww fee schedule and SPAN are modeled sensitivities, not broker-certified historical facts. No result is a live-trading rule, and no funded claim is allowed until the independent source passes execution-stage admission and the generic engine reconciles every fill, fee, position and cash movement.
