# Historical Bars

One day of 5-minute bars of SPY.

## What this shows

- `reqHistoricalData` with a duration, a bar size and what to show.
- `historicalData` once per bar, then `historicalDataEnd`.
- A volume is a `Decimal`: `DecimalFunctions::decimalStringToDisplay` writes it.

## Run it

```bash
cmake -S cpp -B build/cpp -DCMAKE_BUILD_TYPE=Release
cmake --build build/cpp --target hello_bar_data
IB_USERNAME=... IB_PASSWORD=... build/cpp/examples/hello_bar_data
```

## Source

```cpp
{{#include ../../../../../cpp/examples/hello_bar_data.cpp}}
```

The session every example uses:

```cpp
{{#include ../../../../../cpp/examples/session.h}}
```
