# Streaming Ticks

Ten seconds of the top of book of SPY.

## What this shows

- `reqMktData` and `cancelMktData`.
- `tickPrice` and `tickSize`, with the kind of each tick.

## Run it

```bash
cmake -S cpp -B build/cpp -DCMAKE_BUILD_TYPE=Release
cmake --build build/cpp --target hello_tick_data
IB_USERNAME=... IB_PASSWORD=... build/cpp/examples/hello_tick_data
```

## Source

```cpp
{{#include ../../../../../cpp/examples/hello_tick_data.cpp}}
```

The session every example uses:

```cpp
{{#include ../../../../../cpp/examples/session.h}}
```
