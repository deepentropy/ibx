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

## Protobuf forms

The official client gives each request a second form that takes one protobuf message (`reqMarketDataProtoBuf(const protobuf::MarketDataRequest&)`), and each answer a callback with the answer as a protobuf message, called before the plain callback (`orderStatusProtoBuf(const protobuf::OrderStatus&)`). The library has both as an option of the build:

```bash
cmake -S cpp -B build/cpp -DCMAKE_BUILD_TYPE=Release -DIBX_PROTOBUF=ON -DIBX_PROTO_DIR=/path/to/the/official/client/source/proto
cmake --build build/cpp
```

`IBX_PROTO_DIR` is the folder of the `.proto` files of the official client (10.51 or later). They are read where they are: `protoc` writes their C++ sources into the build folder and they are compiled into the library. The build needs protobuf with `protoc` (found by `find_package(Protobuf)`) and sets C++17, which current protobuf asks for. The messages are in the namespace `protobuf`. A program that links `ibx` gets the definition `IBX_PROTOBUF`, the headers of the messages and the protobuf library with it.

What a program gets:

- **The 81 protobuf requests.** Each reads its message and makes the plain request: same checks, same answers. A field that is not set has the value that stands for none (-1 for a request id, 0 for an order id, the unset value of a number, an empty text, no). `startApiProtoBuf` does nothing: the session is started by `eConnect`. `reqConfigProtoBuf` and `updateConfigProtoBuf` are declared and do nothing.
- **The 81 protobuf callbacks.** Each plain callback is preceded by the protobuf callback of its answer, with the fields set as the Python client sets them, which is how the reference sets them: a number an answer gives is set, a text when not empty, a flag when yes; a field of an object (a contract, an order, an execution...) is left out when it has the value the object starts with.
  - A price whose size follows it has one `tickPriceProtoBuf` message that carries the size; that size has no message of its own.
  - The bars of an answer come in one `historicalDataProtoBuf` message before the `historicalData` callback of each bar, the rows of a scan in one `scannerDataProtoBuf` message before their `scannerData` callbacks.
  - The message of `tickReqParamsProtoBuf` carries the last price and last size precisions of the contract, which the plain callback does not have.
  - An error the client makes itself (not connected, already connected...) has no message, as in the official client; an error of the servers has its `errorProtoBuf`.
  - `orderBoundProtoBuf`, `rerouteMarketDataRequestProtoBuf`, `rerouteMarketDepthRequestProtoBuf`, `configResponseProtoBuf` and `updateConfigResponseProtoBuf` are declared and never called.
- **Refusals.** What the client has no counterpart for is refused with an error callback (code 321) and nothing is sent: `exerciseOptionsProtoBuf` with a manual order time, a customer account or the professional customer flag, and an order with a condition of a kind the official client does not have.

It costs nothing when it is not used. Without the option nothing of it is compiled. With it, the protobuf callbacks of `EWrapper` are pure virtual, as in the official header, and a class that derives from `EWrapper` gets every message. `DefaultEWrapper` implements them empty and notes each one that reaches it: the message of a protobuf callback a program does not override is built once, then no more, and a callback costs one test more (about half a nanosecond on a tick). An override must therefore not call the `DefaultEWrapper` callback it replaces.

## Tests

```bash
cmake -S cpp -B build/cpp -DIBX_BUILD_TESTS=ON
cmake --build build/cpp
ctest --test-dir build/cpp
```

- `ibx_tests`: every field of every structure and every parameter of every callback crosses to the right place, and the scenarios recorded from the reference gateway are replayed through the C++ client (no network).
- `ibx_proto_tests` (with `IBX_PROTOBUF`): each protobuf callback comes before its plain callback with its fields set, no message is built for a callback that is not used, and each protobuf request makes the plain request of its message. With the option, `ibx_tests` replays the recorded scenarios a second time with every request made through its protobuf form.
- `live_paper`: a session of about 45 requests on the paper account, every callback written as a row; with `IBX_PROTOBUF`, every protobuf callback too, with its message as JSON.

## The C entry layer

The classes sit on a C layer of the Rust engine (`cpp/include/ibx_capi.h`, built with the `capi` feature): a session handle, one function per request, and a table of callbacks. Another language can bind to it directly.
