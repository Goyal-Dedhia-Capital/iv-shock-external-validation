# Rust source provenance

- `crates/contracts/src/lib.rs` is vendored from
  `Goyal-Dedhia-Capital/generic-backtest-engine` commit
  `5da3e2e483af8568e8cc5b60c2427ea036436205` so the external runner does not
  require access to the engine repository. Source SHA-256:
  `7d7cee06a8bc00d283c9071feef9f797289b2bb0adf988b079d3c17ccfa0eb5f`.
- `crates/iv_shock_core` is the governed six-detector persistent core inherited
  from the H5 `validation/iv_shocks_v1/rust` implementation. S0 is the frozen
  replication detector; S1-S5 remain externally blocked until their threshold
  policies are promoted. The source worktree was at commit
  `2386afde1162c2415c00d5bcdaeacaaca8cfcc47`; the inherited files were
  untracked research artifacts with source SHA-256 values
  `5f898d8c9405660eb0b6e35c345fc1e9c4fd9c2b34373e39d441581ae1bbd647`
  (`src/lib.rs`),
  `0ff4d774a4250f3f3afae6d4e7c51b46d80fc19f9acf37a8a092144f08b3f79c`
  (`src/main.rs`), and
  `d4302780b6a3e9e88351149e9a55bb74100fcc621ec12f30be69b9fa9906ebcc`
  (`tests/protocol.rs`).
- `crates/sequential_books` is the governed H5 research selection/reservation
  kernel inherited from `validation/iv_shocks_v1/books`, source SHA-256
  `6d02175dccdef33f63fd2f907e9bf70142e7018d697ac0e4d8d596e3b80969ed`.
- `crates/strategy_policy` inherits the feedback-aware funded lifecycle from
  H3 EXP019 at firm-research-workspace commit
  `cefddd1053da2f0df6330e439babf5b0cd203166`, source SHA-256
  `bb287c1c3f0f04051182ad96d2739ec095925d4eaf77860d8dfad62ae4f7db68`.
  It is generalized to all ten H3/H5 family books and rejects candidates whose
  entry-known source fields do not match the frozen registry.

Vendoring is intentional: the data owner receives the exact strategy-facing
contracts and research logic without needing another private repository.

- `models/exp032_reference_model_bundle.json` is the verified EXP032 external
  coefficient bundle, SHA-256
  `158d5bc92d2da250174649bfd1f1a4c7961d96bda14e9467f4adcb844ffb0b20`.
  It contains 90 family-month fits and no raw market data or result tape. The
  executable Rust scorer is limited to the bundle's 2024-07 through 2025-12
  date range and rejects any missing or non-prior fit.
