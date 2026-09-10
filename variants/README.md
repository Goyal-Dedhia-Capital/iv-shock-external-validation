# Variant registry

- `h3_families.json` and `h5_families.json` describe the executable single-leg baselines enforced in Rust.
- `final_portfolio_policy_config.json` selects the executable later five-family portfolio.
- `final_candidate_policy.json` is the full machine-readable policy and interpretation specification for that executable boundary.
- `detectors.json` and `comparison_grid.json` retain S1–S5 and resolution sensitivities. Only S0 is authorized to create policy candidates.
- `experiment_registry.json` preserves historical IDs and lineage; it is not a list of runnable commands or completed profitability claims.

The executable EXP021 and EXP046 launch configurations live beside their code
under `experiments/`. They are intentionally absent from the root final-five
config because their account and score semantics differ.

The Rust binaries do not dynamically trust these files. Their executable invariants are compiled and tested; the JSON files make the same policies inspectable to humans and orchestration code.
