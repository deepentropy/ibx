# Login

Log in to the paper system, read the account and the next valid order id, log out.

## What this shows

- Creating an `EClientSocket` with its wrapper and its `EReaderOSSignal`.
- `eConnect`: the login goes to the servers with the credentials of the environment.
- The loop `waitForSignal()` / `processMsgs()` that gives the callbacks.
- `managedAccounts` and `nextValidId`, the first callbacks of every session.

## Run it

```bash
cmake -S cpp -B build/cpp -DCMAKE_BUILD_TYPE=Release
cmake --build build/cpp --target hello_login
IB_USERNAME=... IB_PASSWORD=... build/cpp/examples/hello_login
```

## Source

```cpp
{{#include ../../../../../cpp/examples/hello_login.cpp}}
```

The session every example uses:

```cpp
{{#include ../../../../../cpp/examples/session.h}}
```
