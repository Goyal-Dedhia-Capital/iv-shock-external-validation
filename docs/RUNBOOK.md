# Independent laptop runbook

The repository contains the firm strategy. The data owner keeps API credentials, raw data, local contracts, caches and result tapes outside Git.

## 1. Install and verify code

```bash
git clone https://github.com/Goyal-Dedhia-Capital/iv-shock-external-validation.git
cd iv-shock-external-validation
scripts/bootstrap
scripts/check
```

This verifies formulas, causal calibration, family definitions, basket geometry, filters, score arithmetic, ordering, lifecycle, restart and calendar behavior. Historical tape parity is not required.

## 2. Export data outside the repository

Use a private script outside this checkout to export every represented NIFTY option expiry at native one-second resolution. Do not commit that script if it contains provider code or credentials. The repository begins at the normalized CSV/Parquet boundary described in `contracts/source_contract.example.json`.

Copy the tracked templates into ignored local files and replace every `PENDING` value:

```bash
cp contracts/source_contract.example.json contracts/source_contract.local.json
cp contracts/exchange_calendar.example.json contracts/exchange_calendar.local.json
```

Never edit the tracked examples to describe a private source.

## 3. Admit one complete session

```bash
scripts/preflight \
  --input /absolute/path/to/private-export.csv \
  --source-contract contracts/source_contract.local.json \
  --cache-dir /absolute/path/to/private-cache \
  --manifest-out /absolute/path/to/results/first-session-manifest.json
```

Use `--stage execution` in the underlying CLI when bid/ask, lot size and executable quote authority are required. A blocked manifest is a data-contract result, not a reason to weaken the strategy.

## 4. Run the executable baseline

The normalized feeder must stream canonical `ResearchRequest` JSONL. Run the detector, scheduler and policy as persistent processes; do not restart them between minutes.

```bash
scripts/run-strategy \
  --runner detector \
  --calendar contracts/exchange_calendar.local.json \
  --source-contract contracts/source_contract.local.json \
  --stage research \
  < /absolute/path/to/detector-requests.jsonl \
  > /absolute/path/to/detector-responses.jsonl

scripts/run-strategy \
  --runner policy \
  --calendar contracts/exchange_calendar.local.json \
  --source-contract contracts/source_contract.local.json \
  --stage research \
  < /absolute/path/to/baseline-policy-requests.jsonl \
  > /absolute/path/to/baseline-policy-responses.jsonl
```

The baseline policy runs H3_F1–F5 and H5_F1–F5 as ten independent family state machines. One engine instance may consume all intents for a shared account; ten state machines do not imply ten accounts.

## 5. Run the final structure and score policy

```bash
scripts/run-strategy \
  --runner policy \
  --calendar contracts/exchange_calendar.local.json \
  --source-contract contracts/source_contract.local.json \
  --stage execution \
  --policy-config-json "$(cat variants/final_portfolio_policy_config.json)" \
  --model-bundle models/exp032_reference_model_bundle.json \
  < /absolute/path/to/final-policy-requests.jsonl \
  > /absolute/path/to/final-policy-responses.jsonl
```

For each candidate the packet must contain the full entry-known 19-feature vector, typed S0 diagnostic, selected anchor, same-expiry represented strikes, official lot quantity, and every proposed leg. The Rust runner recomputes the monthly ridge score from the SHA-bound reference bundle, validates the final filters and basket geometry, then emits an atomic intent. Future exit availability and realized returns are forbidden inputs.

The typed S0 diagnostic is not a free-form eligibility flag. It must be the
output of the persistent detector process run under the same source/calendar
bundle and must match source contract, event minute, sign, score, threshold,
support and quiet status. The policy validates that match; the feeder retains
the detector response identity in its immutable run manifest.

## 6. Funded execution

Feed the final policy responses to one generic-engine account configured with ₹10,00,000 initial cash and no reset or compounding. Use confirmed bid/ask crossing for the primary executable lane. Apply date-effective costs per fill and report the Groww pinned-2026 schedule separately when it is only a sensitivity. The engine must own capital admission, signed premium flows, fees, margin, marks, fills and reconciliation.

## 7. Run the two separate finalized books

EXP021 and EXP046 are alternatives with different accounts and model logic.
Their commands and input rules are in
`experiments/exp021_walkforward_sizing/README.md` and
`experiments/exp046_r2_core/README.md`. Do not append their P&L to the final
five-family account without a separately specified portfolio experiment.

## Required result files

Retain immutable manifests plus compact coverage, trade and daily-account outputs. Report each family and the combined account with candidate counts, admitted/blocked counts, entry/exit coverage, unresolved positions, gross and net distributions, win/zero rates, PF, tails, drawdown, month/year stability, best-date concentration and cross-family exposure.

## Fail-closed rules

Stop the affected run on wrong hashes, a malformed calendar, missing prior model fit, noncausal timestamps, missing structure legs, wrong strike width, mismatched lot sizes, partial fills, unbalanced positions or unreconciled cash. A missing exact same-session exit quarantines that family book and is reported; it never becomes an overnight retry.
