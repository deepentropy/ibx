# Request Account PnL

The profit and loss of the account, for five seconds.

## What this shows

- `reqPnL` for the account `managedAccounts` gave.
- `pnl`: the day's, the unrealized and the realized values.
- `cancelPnL`.

## Run it

```bash
cmake -S cpp -B build/cpp -DCMAKE_BUILD_TYPE=Release
cmake --build build/cpp --target hello_pnl
IB_USERNAME=... IB_PASSWORD=... build/cpp/examples/hello_pnl
```

## Source

```cpp
{{#include ../../../../../cpp/examples/hello_pnl.cpp}}
```

The session every example uses:

```cpp
{{#include ../../../../../cpp/examples/session.h}}
```
