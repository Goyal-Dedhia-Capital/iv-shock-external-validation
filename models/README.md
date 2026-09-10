# Frozen model artifacts

`exp032_reference_model_bundle.json` contains only transforms and ridge
coefficients, not market data, trades, labels or credentials. Its SHA-256 is
`158d5bc92d2da250174649bfd1f1a4c7961d96bda14e9467f4adcb844ffb0b20`.

The bundle holds 90 strictly-prior family-month fits for July 2024 through
December 2025. The Rust policy rehashes it, validates the EXP032 schema and
causal training boundary, recomputes the score from the 19 entry-known
features, and admits only scores above the stored prior-training threshold.

This is the frozen strategy artifact for an exact external replication. It is
not valid outside its covered months. A newly fitted bundle is a new experiment
and must use a distinct ID, immutable hash and strictly-prior training data.
