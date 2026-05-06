# Order Lifecycle

End-to-end recipe: full order state machine on SPY — place, modify, cancel, partial fill, executions query.

## What this shows

- Allocating a fresh `orderId` from `next_valid_id`.
- Placing a limit order far from market and observing `Submitted` status.
- Verifying it appears in `req_open_orders` with the expected `permId`.
- Modifying the same `orderId` and confirming `permId` is **stable across modify**.
- Walking the price toward market to trigger a fill, then querying `req_executions`.
- Cancelling a fresh order and observing the `Cancelled` terminal status.
- Flattening any residual position before disconnecting.

## Run it

```bash
IB_USERNAME=... IB_PASSWORD=... python examples/ex96_order_lifecycle.py
```

> Use the paper account. Never run order examples against a live account.

## Source

```python
{{#include ../../../../examples/ex96_order_lifecycle.py}}
```
