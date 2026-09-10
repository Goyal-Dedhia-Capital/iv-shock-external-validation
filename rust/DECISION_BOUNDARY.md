# Persistent research decision boundary

The executable detector and sequential strategy will be a persistent Rust
process. It reads one JSON object per line and writes one JSON object per line.

Request:

```json
{
  "input": {},
  "state": {},
  "feedback": {},
  "sequence": 0
}
```

Response:

```json
{
  "artifact_consumed": true,
  "runner_id": "...",
  "bundle_hash": "...",
  "state": {},
  "intents": []
}
```

The process must complete and flush each response before reading the next
request. Research code cannot read historical files or price fills. The feeder
owns ordered market inputs; the execution engine owns fills, costs, margin and
accounting. A later commit will port the exact detector only after the independent
source contract and minute preparation pass, avoiding an unverified mapping of
the friend's IV and timestamp fields.
