# C++ API

The C++ library gives the classes of the official C++ client, on the Rust engine: a program written for the official client compiles against it and logs in to the servers itself, with no local gateway.

- `EClientSocket`, `EClient`: the requests.
- `EWrapper`, `DefaultEWrapper`: the callbacks. `EWrapper_declare(EWrapper_methods_override)` declares them in a class, as with the official headers.
- `EReader`, `EReaderOSSignal`: the loop that gives the callbacks, on the thread that calls `processMsgs()`.
- `Contract`, `ContractDetails`, `Order`, `OrderState`, `Execution`, `Bar`, `Decimal`, the order conditions and the other structures, with the names, types and starting values of the official ones.

The names and signatures are those of the official C++ client 10.51.01: 89 callbacks, 80 requests and 37 structures are compared with its headers by `cpp/tests/check_signatures.py`.

## Build

The library is built with CMake; cargo builds the Rust engine on the way.

```bash
cmake -S cpp -B build/cpp -DCMAKE_BUILD_TYPE=Release
cmake --build build/cpp
```

In a CMake project:

```cmake
add_subdirectory(path/to/ibx/cpp)
target_link_libraries(my_program PRIVATE ibx)
```

`ibx` is a static library with the classes; it links to the Rust engine, a shared library (`ibx.dll`, `libibx.so`) that must be beside the program (`ibx_copy_core(my_program)` copies it there). A C++14 compiler is needed (MSVC, GCC, Clang), and a Rust toolchain.

## Logging in

The official `eConnect(host, port, clientId)` compiles and works: `host` and `port` name a local gateway there and are kept only for `host()` and `port()`. The login takes the user name and the password of the environment (`IB_USERNAME`, `IB_PASSWORD`), the server of `IB_HOST` when set. Paper or live: `IB_PAPER` when set (`0` or `false`: live), else the port (4001 and 7496: live), else paper.

The login can also be given in full:

```cpp
ibx::Login login;
login.username = "...";
login.password = "...";
login.paper = true;
client.eConnect(login, clientId);
```

A live login waits for the second factor: the approval on the phone, or the code that `login.codeProvider` returns for a typed factor.

## What differs from the official client

- There is no socket to a local gateway: `asyncEConnect`, `setConnectOptions`, `setOptionalCapabilities` and `startApi` are kept and do nothing; the session is started by `eConnect`, which calls `connectAck` before it returns.
- `EReader::processMsgs()` gives the callbacks on the calling thread. An exception a callback throws is thrown again from `processMsgs()`, after the other answers of the pass were given.
- `Decimal` is the same 64-bit decimal with the same functions, implemented in the library: no other library is needed.
- `reqFundamentalData`, `cancelFundamentalData` and the callback `fundamentalData` exist here; the official client no longer has them.
- `exerciseOptions` sends nothing yet, as in the Python client. The callbacks `tickEFP`, `winError`, `rerouteMktDataReq`, `rerouteMktDepthReq` and `orderBound` are declared and never called.
- An order whose conditions are joined by "or", or with `conditionsIncludeOvernight`, is refused with an error callback: the client does not send what it cannot send as given.
- The wire-encoding hooks of the official classes (`readExternal`, `writeExternal`, the `Encode...` helpers) and the constants of the session levels are not provided.

## Tests

```bash
cmake -S cpp -B build/cpp -DIBX_BUILD_TESTS=ON
cmake --build build/cpp
ctest --test-dir build/cpp
```

- `ibx_tests`: every field of every structure and every parameter of every callback crosses to the right place, and the scenarios recorded from the reference gateway are replayed through the C++ client (no network).
- `live_paper`: a session of about 45 requests on the paper account, every callback written as a row.

## The C entry layer

The classes sit on a C layer of the Rust engine (`cpp/include/ibx_capi.h`, built with the `capi` feature): a session handle, one function per request, and a table of callbacks. Another language can bind to it directly.
