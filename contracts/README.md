# Data-owner intake checklist

Copy `source_contract.example.json` to `source_contract.local.json` and resolve
every applicable field using provider documentation or a written owner ruling.
Do not guess from column names.

Before the first run, confirm:

- native one-second coverage and whether rows are snapshots or changed ticks;
- the full expiry request needed for expiry-rank and DTE analysis;
- timestamp and availability semantics;
- the actual session close and special-session calendar;
- spot availability;
- forward, TTM, IV, and Greek calculation methods;
- volume, OI, and `-1` semantics;
- bid/ask field direction, depth, freshness, and observation authority;
- historical lot-size authority.

The local contract is ignored by Git because it may reveal provider details.
After review, publish a sanitized signed contract without credentials or local
paths under a new non-local filename.
