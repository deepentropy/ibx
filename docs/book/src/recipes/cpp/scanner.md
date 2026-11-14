# Market Scanner

The top gainers among the US stocks.

## What this shows

- Filling a `ScannerSubscription`.
- `scannerData` once per row, then `scannerDataEnd`.
- `cancelScannerSubscription`.

## Run it

```bash
cmake -S cpp -B build/cpp -DCMAKE_BUILD_TYPE=Release
cmake --build build/cpp --target hello_scanner
IB_USERNAME=... IB_PASSWORD=... build/cpp/examples/hello_scanner
```

## Source

```cpp
{{#include ../../../../../cpp/examples/hello_scanner.cpp}}
```

The session every example uses:

```cpp
{{#include ../../../../../cpp/examples/session.h}}
```
