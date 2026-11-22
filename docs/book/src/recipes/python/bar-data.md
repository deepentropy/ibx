# Historical Bars

Fetch one trading day of 5-minute SPY bars and print the first and last.

## What this shows

- Building a `Contract` with `con_id` so the request is unambiguous.
- Calling `req_historical_data` with duration / bar size / what-to-show.
- Driving the callback loop on a daemon thread with `EClient.run`.
- Reading OHLCV out of the bar object.

## Many requests in a row

Observed on the paper account on 22/11/2026, market closed:

- Bars under 30 seconds (5 and 15 seconds seen): about the first 33 requests of a burst are answered at once
  (about 0.12 s each). The server then holds the next ones, with no error, and gives their bars on its own
  6-second clock: about 4, 4, 3, 2, then 1 every 6 seconds, then a 12-second gap, about 25 requests a minute.
  Every such request of the session counts, whatever the contract. About 5 minutes without one start the count again.
- Bars of 30 seconds, 1 minute and more, and contract details do not count: 80 one-minute
  requests were all answered in 9 seconds.
- A contract given with its contract id saves the lookup of its description, about 0.1 s a request.
- To see where the time of each request goes, turn on the debug log of `ibx::engine::hot_loop::hmds`
  (`RUST_LOG=ibx::engine::hot_loop::hmds=debug`). Each request ends with one line: the time in the client
  (pacing, contract lookup), on the server (to its first part, then to the first bars), the transfer and the reading.
  A held request shows a first part at once and its bars seconds later.

## Run it

```bash
IB_USERNAME=... IB_PASSWORD=... python examples/hello_bar_data.py
```

## Source

```python
{{#include ../../../../../examples/hello_bar_data.py}}
```
