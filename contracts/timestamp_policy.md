# Timestamp and availability policy

The data owner must declare whether a source timestamp is a bar start, bar end,
exchange event time, receive time, or API publication time. A strategy may use a
row only after its declared `available_at` time. If availability differs from
the event timestamp, the source contract must name an explicit availability
column; the normalizer preserves it end to end.

## Minute replication

For a source proven to contain one-second OHLC bars:

- minute open: first eligible open;
- minute high: maximum eligible high;
- minute low: minimum eligible low;
- minute close: last eligible close;
- quote, spot, forward, IV, Greeks, and OI: last non-null value within the minute;
- incremental volume: sum within the minute;
- cumulative volume: last minus the value immediately preceding the minute,
  under a separately tested implementation.

Nothing is carried across a missing minute in the primary lane. A minute is
eligible for the sealed-minute artifact only when every expected source-second
slot is represented for that contract. Partial minutes remain in the audit
artifact with `minute_complete=false`. A shock at minute `t` cannot create an
entry earlier than the frozen `t+1` boundary.

## High-resolution extensions

Rolling 60-second innovations and adjacent-second innovations use distinct
variant IDs. Neither inherits the replication label.
