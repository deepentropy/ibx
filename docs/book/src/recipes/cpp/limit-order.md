# Send a Limit Order

Place a BUY limit order on AAPL far below the market, watch it acknowledged, then cancel it.

## What this shows

- The order id of `nextValidId`.
- Building an `Order`: a quantity is a `Decimal` (`DecimalFunctions::stringToDecimal`).
- `orderStatus`: `PreSubmitted` or `Submitted`, then `Cancelled` after `cancelOrder`.

> Paper account only: the example sends no order when the account of the login is not a paper one. The price is far from the market, so the order does not fill.

## Run it

```bash
cmake -S cpp -B build/cpp -DCMAKE_BUILD_TYPE=Release
cmake --build build/cpp --target hello_limit_order
IB_USERNAME=... IB_PASSWORD=... build/cpp/examples/hello_limit_order
```

## Source

```cpp
{{#include ../../../../../cpp/examples/hello_limit_order.cpp}}
```

The session every example uses:

```cpp
{{#include ../../../../../cpp/examples/session.h}}
```
