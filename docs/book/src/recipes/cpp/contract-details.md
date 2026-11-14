# Request Contract Details

Ask the details of AAPL and print its conId and its primary exchange.

## What this shows

- Building a `Contract` for a stock.
- `reqContractDetails`, `contractDetails` and `contractDetailsEnd`.

## Run it

```bash
cmake -S cpp -B build/cpp -DCMAKE_BUILD_TYPE=Release
cmake --build build/cpp --target hello_contract_details
IB_USERNAME=... IB_PASSWORD=... build/cpp/examples/hello_contract_details
```

## Source

```cpp
{{#include ../../../../../cpp/examples/hello_contract_details.cpp}}
```

The session every example uses:

```cpp
{{#include ../../../../../cpp/examples/session.h}}
```
