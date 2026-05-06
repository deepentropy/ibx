# Streaming L2 Market Depth

End-to-end recipe: subscribe to Level-2 market depth for two tickers (AAPL + TSLA), maintain per-ticker bid/ask books from `update_mkt_depth_l2` callbacks, and print the book on each update.

## What this shows

- Connecting and waiting for `next_valid_id`.
- Subscribing to L2 with `req_mkt_depth` for multiple contracts.
- Building an in-memory book from `update_mkt_depth_l2` events (`insert` / `update` / `delete` ops).
- Cleanly cancelling subscriptions and disconnecting.

## Run it

```bash
IB_USERNAME=... IB_PASSWORD=... cargo run --example l2_aapl_tsla
```

Optional:

```bash
IB_HOST=cdc1.ibllc.com DURATION_SECS=30 cargo run --example l2_aapl_tsla
```

## Source

```rust
{{#include ../../../../examples/l2_aapl_tsla.rs}}
```
