# Streaming L2 Market Depth

Ten seconds of the market depth of AAPL, five rows a side.

## What this shows

- `reqMktDepth` with the smart depth, and `cancelMktDepth`.
- `updateMktDepthL2`: the row, the operation (insert, update, delete), the side, the price and the size.

## Run it

```bash
cmake -S cpp -B build/cpp -DCMAKE_BUILD_TYPE=Release
cmake --build build/cpp --target hello_l2
IB_USERNAME=... IB_PASSWORD=... build/cpp/examples/hello_l2
```

## Source

```cpp
{{#include ../../../../../cpp/examples/hello_l2.cpp}}
```

The session every example uses:

```cpp
{{#include ../../../../../cpp/examples/session.h}}
```
