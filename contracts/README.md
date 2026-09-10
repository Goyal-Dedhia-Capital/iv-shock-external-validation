# Data-owner intake checklist

Copy `source_contract.example.json` to `source_contract.local.json` and resolve
every applicable field using provider documentation or a written owner ruling.
Do not guess from column names.

Before the first run, confirm:

- native one-second coverage and whether rows are snapshots or changed ticks;
- the full expiry request needed for expiry-rank and DTE analysis;
- the minimum expiry count returned by the provider's independent inventory
  endpoint or query metadata, plus a short evidence identity;
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

Provider column names are resolved only through the mappings in the source
contract. The preflight records that resolved mapping in its manifest so a
local alias cannot silently select a different field.

The normalized adapter-to-policy JSON contract is fully specified by
`strategy_packet.schema.json`; its enclosing persistent request/response wire
protocol is `research_protocol.schema.json`. Final-portfolio packets must also provide the
event-time chain evidence on every leg (`expiry_minute`, `represented`), the
represented same-expiry strike inventory, and the source-hashed official lot
authority. The Rust policy revalidates those facts before emitting an intent.

`exchange_calendar.example.json` is a separate strategy-run gate. The firm
strategy owner must replace its `PENDING` authority and identity and enumerate
holidays and special sessions before chronological books run. The data owner is
not responsible for validating detector or book logic.

Holiday entries are `YYYY-MM-DD` strings. Each special-session entry must use:

```json
{
  "date": "YYYY-MM-DD",
  "open": "HH:MM:00",
  "close": "HH:MM:00",
  "eligible_last_minute": "HH:MM:00",
  "intraday_breaks": [{"start": "HH:MM:00", "end": "HH:MM:00"}]
}
```

Set `status` to `READY` only after the authority, identity, holidays, special
sessions, and regular hours are complete. The Rust launcher rehashes this exact
file and rejects wrong dates, weekends, holidays, breaks, and close-boundary
minutes before strategy state changes.
