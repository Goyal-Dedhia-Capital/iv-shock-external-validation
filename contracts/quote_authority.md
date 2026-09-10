# Quote and fill authority

The public sample contains `buy_price` and `sell_price`, but those names do not
by themselves establish bid/ask meaning, depth, freshness, or executability.

The source owner must explicitly map the provider fields to canonical `bid` and
`ask` and attest whether they are observed top-of-book quotes. Until then:

- close-to-close results are descriptive gross responses;
- no bid/ask, fill, slippage, capacity, or net-alpha claim is admitted;
- PCHIP prices are never executable;
- missing or crossed quotes fail the executable overlay for that observation.

After authority is established, the primary conservative proxy is long entry at
ask and exit at bid; short entry at bid and exit at ask. Broker fees and any
additional slippage are separate ledger components.
