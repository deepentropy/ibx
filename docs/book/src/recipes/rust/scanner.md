# Market Scanner

Subscribe to the `TOP_PERC_GAIN` scanner over US major stocks and print the top 10 results.

## What this shows

- Calling `req_scanner_subscription` with a `ScannerSubscription` (instrument / location / scan code / rows).
- Reading `scanner_data` rows (rank + `ContractDetails`) until `scanner_data_end`.
- Cancelling cleanly with `cancel_scanner_subscription` before disconnecting.

## Run it

```bash
IB_USERNAME=... IB_PASSWORD=... cargo run --example hello_scanner
```

## Source

```rust
{{#include ../../../../../examples/hello_scanner.rs}}
```
