# Repository instructions

- This is a private external-validation repository for firm research.
- Never commit raw market data, API credentials, local source contracts, caches,
  candidate tapes, full ledgers, or account secrets.
- Preserve causal availability: no future observation, endpoint availability,
  or realized return may affect signal formation, ordering, or admission.
- Keep minute replication and one-second extensions under distinct variant IDs.
- Treat PCHIP as synthetic research/accounting data, never as a fill price.
- Research decisions must use the persistent Rust JSONL-to-TradeIntent boundary.
  Python owns only source adaptation, normalization, preparation, and reporting.
- Never overwrite a completed result or mutate its manifest. Create a new run ID.
- Commit only on a non-default branch and publish only to
  `Goyal-Dedhia-Capital`.
