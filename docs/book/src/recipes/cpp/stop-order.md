# Send a Stop Order

Place a BUY stop order on AAPL far above the market, watch it acknowledged, then cancel it.

## What this shows

- An `Order` of type `STP` with its stop price in `auxPrice`.
- `orderStatus` up to `Cancelled`.

> Paper account only: the example sends no order when the account of the login is not a paper one. The price is far from the market, so the order does not fill.

## Run it

```bash
cmake -S cpp -B build/cpp -DCMAKE_BUILD_TYPE=Release
cmake --build build/cpp --target hello_stop_order
IB_USERNAME=... IB_PASSWORD=... build/cpp/examples/hello_stop_order
```

## Source

```cpp
{{#include ../../../../../cpp/examples/hello_stop_order.cpp}}
```

The session every example uses:

```cpp
{{#include ../../../../../cpp/examples/session.h}}
```
