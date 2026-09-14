# Compatibility status

Capability status for the Rust client, the Python bindings, and the engine
underneath both. Status is assigned from a named artifact — a test, a script,
or a recorded server response — not from code inspection.

| Status | Definition |
| :---: | --- |
| ✅ Supported | Implemented; exercised against IBKR production servers; the response is parsed and delivered to the caller |
| 🔬 Implemented | Implemented and unit-tested; the request reaches the server, but the response has not been observed end to end |
| ⛔ Unavailable | Not carried by the protocol; the call returns an error stating the reason |
| ✅ Documented | A property of the venue rather than a call: established by observation against IBKR production servers, with nothing for this client to implement |

Verification runs against a paper account on IBKR production servers, and the order path has additionally been run once end to end on a funded account during regular hours. Where a market or data set is not entitled to that account, the entitlement response is recorded as the result.

| | |
| --- | --- |
| Requests | 80. Every one either does what it says or reports why it cannot — none returns success having sent nothing |
| Order fields | 154. 118 are sent; 29 have no field in the protocol to carry them and the call says so rather than dropping them; 6 are what the venue fills on the way back, which an order does not carry out; 1 is acted on here rather than sent |
| Rust and Python | the same request produces the same call on both, compared against live responses |
| Tests | 3,606 offline, and 197 more that live in the suites run against a broker session |

## API surface

| Measure | Count |
| --- | --- |
| Canonical calls | 78 |
| Served, Rust | 77 |
| Served, Python | 77 |
| Taken and not applied, Rust | 1 |
| Taken and not applied, Python | 1 |
| Canonical callbacks | 85 |
| Calls where the two surfaces differ | 0 |
| Callbacks where the two surfaces differ | 0 |

Counted from the source by `scripts/gen_api_docs.py` and checked against this
table by `scripts/check_status_counts.py`, both of which CI runs. A figure
here that the source stops supporting fails the build rather than standing. The two surfaces carry the same calls and the same callbacks: a
program written against either finds the same thing, and a call that cannot be
served says so on both rather than being absent from one.

### Calls not served

Every call that exists with the expected signature and reports, through the
error callback, that it cannot be served. Taken from the generated coverage
matrix, which CI checks against the source.

None report through the error callback. One is taken and not applied:
`set_server_log_level`. The session holds no log level of its own and the
protocol carries no message asking the venue to change one, so what a caller
states is written to this client's log and reaches nothing else. It is counted
with the calls that are not served, because a caller who set one and expected
the venue to act on it got neither — and it says so in its own documentation
rather than on the error callback, which a program calling it as a matter of
course would hear on every call.

The two that stood here before it — the advisor configuration request and its
replacement — are not among them, and neither is their answer: the request goes
to the venue like every other call, and the venue's reply is read and handed
over on `receive_fa`, `replace_fa_end` and the error callback. What is
unverified is the content of that reply, which needs an advisor account to see,
and the status row below says so.

## Test inventory

| Suite | Count | Requires credentials |
| --- | ---: | :---: |
| Rust unit and integration | 2,710 | No |
| Rust, live | 9 | Yes |
| Python | 896 | No |
| Python, live | 137 | Yes |
| Paper compatibility suite (154 phases) | 51 tests | Yes |

Counted rather than stated: `scripts/check_status_counts.py` names every test
in each suite and fails the gate when this table disagrees with it, so a figure
here cannot go quietly out of date as the suites grow.


45 of the 47 capabilities are verified against IBKR production servers. Of the
other two, advisor configuration reaches the server and needs an advisor
account to see what it answers, and session lifetime is a property of the venue
rather than a call this client makes.

Every figure above is measured on each commit, and the build fails if one moves.

---

## Client surfaces

| Surface | Status | Verification |
| --- | :---: | --- |
| `EClient` / `EWrapper` (TWS API shape) | ✅ Supported | `tests/ib_paper_compat`, `tests/python/test_compat_tier1..3.py` |
| `ib_async`, unmodified | ✅ Supported | Their `IB` on this engine through `ibx.ib_async.attach` — their events, async variants and types, with no gateway. All 67 transport calls their library makes are carried and gated, and their own suite runs against it; see the note below. `tests/python/test_ib_async_transport.py`, `tests/ib_async_upstream/conftest.py` |
| `ibx.IB` (ib_async shape) | ✅ Supported | 90/90 methods present; `tests/python/test_ib_facade.py`, `scripts/sdk_sweep.py` |
| `ibx::api::Client` (Rust) | ✅ Supported | 80/80 callable; 5 return an error saying why: two name a handshake with a local process there is none of, one withdraws a contract lookup that does not stream and so has nothing to withdraw, and two ask for a running profit, whose figures arrive on a callback as they change — which is not something a call that answers once can hand back. Counted over the request surface the gate reads, which is the binding's — the two carry the same calls, and the row beside this one is what states that they do |
| Gateway settings | ✅ Supported | 14 settings carried, 10 recorded as having no counterpart, both lists the same on either client; `tests/python/test_gateway_settings.py`, `tests/python/test_settings_parity.py`; session opened under a stated build and time zone |
| Rust/Python equivalence | ✅ Supported | 4 static gates (settings, order fields, surface, error behaviour) plus `scripts/conformance.py --compare`, which compares 10 server responses across both clients |

**The upstream `ib_async` suite.** Its three transport tests behave against this
engine as they do against a gateway. Two pass. The third asserts a `RequestError`
carrying code 321, which their wrapper cannot raise: it lists 321 among the codes
it treats as warnings, and a warning never ends the request it belongs to. That
test fails the same way against any server.

## Market data

| Capability | Status | Verification |
| --- | :---: | --- |
| Top of book | ✅ Supported | Streaming and snapshot; US equities and FX; `scripts/sdk_sweep.py`, `tests/python/test_live_quotes.py`. Concurrent subscribers on one contract share one wire subscription |
| Market depth (L2) | ✅ Supported | A book is asked for once, at the venue named, and every level names it; inserts, updates and deletes are delivered as the venue states them, ten levels a side through ib_async's own ticker. Which venues answer depends on the account's entitlements — see the note below. `tests/python/test_live_depth.py`, `tests/python/test_ib_async_depth.py`, `src/bin/capture_depth.rs` |
| Historical bars | ✅ Supported | 9 markets in one session (`src/bin/capture_global.rs`); `keepUpToDate` verified in `tests/python/test_historical_and_scanner.py` |
| Historical ticks and schedules | ✅ Supported | `scripts/sdk_sweep.py`; unsupported tick types return an error rather than substituting another series |
| Tick-by-tick quotes | ✅ Supported | FX and US equities, concurrent streams, each record carrying its request id; `tests/python/test_live_python_wrappers.py` |
| Tick-by-tick trades | ✅ Supported | 67,785 trades over a 20-minute session; 327 in the first twenty seconds of one subscription. A stream is asked for by the venue's id for the contract, which is resolved first when the caller states a description, and by the name the caller used: `Last` and `AllLast` are two queries the venue answers apart, and which trades are on which is the venue's to say — measured on a liquid future, 104 and 139 records over two windows of the same length |
| Real-time bars | ✅ Supported | Five-second bars streaming during regular hours, each carrying open, high, low, close and volume, alongside a book on the same session and through ib_async's own `reqRealTimeBars` |
| Trading halt status | ✅ Supported | Tick 437 decoded from status mask and status index; `src/bin/capture_status.rs` |
| Tick attributes | ✅ Supported | Per-trade `unreported` and `pastLimit` observed to vary within one stream |
| Venue map behind the exchange mask | ✅ Supported | Asked for beside the quote and answered at regular trading hours with 18 venues, each with the letter the mask's bits refer to. Outside those hours the server states none, and a venue's letter is empty until it does |

**Depth entitlements.** On this account IEX answers — its Level II is fee-waived
— and returns 227 levels on one SPY subscription; NASDAQ and CME refuse by name,
and the refusal reaches the caller. A book asked for on no particular venue is
acknowledged and produces nothing, which is what an account with no aggregate
entitlement is answered with.

## Orders

| Capability | Status | Verification |
| --- | :---: | --- |
| 24 order types | ✅ Supported | `whatIf` preview accepted by the server for each; `tests/ib_paper_compat`. Every type this client places is previewed as itself |
| Order fields | ✅ Supported | An order has 154 fields. 118 are sent. 29 have no field in this protocol to carry them, and each says so on itself rather than being quietly ignored. 6 more are what the venue fills on the way back, which an order does not carry out. One is acted on here rather than sent: an order held back is kept until one in its family transmits, which is what the counterpart this replaces does with it. A check on every commit fails if a field starts being dropped |
| Non-US markets | ✅ Supported | Previews accepted on DE, NL, GB, CH, AU, CA, US equities and FX; JP and HK rejected for lot size, which is the exchange rule and is surfaced to the caller |
| Modify, cancel, global cancel | ✅ Supported | `scripts/sdk_lifecycle.py` (place → modify → cancel), `tests/ib_paper_compat` Phase 9 / 9b |
| Brackets, OCA, combos | ✅ Supported | Per-leg pricing; leg order validated by server rejection of the inverted spread; `src/bin/capture_combo.rs` |
| Conditions | ✅ Supported | All 6 types (price, volume, percent change, margin, execution, time) accepted and held by the server; `tests/ib_paper_compat` Phase 60 |
| Order acceptance | ✅ Supported | Every change to an order answers with the order as this client sent it and the status it is now in, which is the pair the protocol answers a change with. 45 orders placed, modified and withdrawn over a 15-cycle session, every one reaching Cancelled, with no error |
| Executions and fills | ✅ Supported | Fill reported and position reconciled; execution report retains server fields including unnamed tags; `tests/ib_paper_compat` Phase 97 |
| A round trip on a funded account | ✅ Supported | One same-day option bought and sold on a funded account during regular hours: limit in, filled, limit out, position and account values reconciled, and nothing left open. Run once by hand rather than by a phase, a funded account being one the suite may not trade — so unlike every row beside it this one records a session rather than something a reader can repeat |
| Option exercise and lapse | ✅ Supported | Both submitted for a resolved option contract; server response 399 *"You have not got the number of options requested to be exercised"* delivered to the caller |

## Account

| Capability | Status | Verification |
| --- | :---: | --- |
| Account values and summary | ✅ Supported | 135 values, each in the currency the server states; subscribed at connect; `tests/python/test_account_updates_and_pnl.py` |
| Positions and P&L | ✅ Supported | Delivered on login and after each fill; `tests/python/test_account_updates_and_pnl.py` |
| Managed accounts | ✅ Supported | `tests/python/test_account_updates_and_pnl.py` |
| Financial advisor configuration | 🔬 Implemented | Request reaches the server on both clients; the response requires an advisor account |

## Reference data

| Capability | Status | Verification |
| --- | :---: | --- |
| Contract details | ✅ Supported | 11 of 12 resolved across 9 countries; the 12th matched 2 contracts and returned an ambiguity error rather than a selection; `src/bin/capture_global.rs` |
| Option chains, symbol search | ✅ Supported | `scripts/sdk_sweep.py` |
| Scanners, fundamentals | ✅ Supported | 697 KB scanner parameter set, fundamental report; `tests/python/test_historical_and_scanner.py` |
| News | ✅ Supported | 117 providers parsed. Headline retrieval requires a news subscription; this account holds none, and every provider returns an empty result set |
| Exchange directory | ✅ Supported | 203 exchanges, in the two sections the venue states them in: shares and derivatives. What each carries and which group each aggregates into are not stated by the venue and are not stated here |
| Corporate events calendar | ✅ Supported | 43 event types with their field schemas, 179 KB, over the security-definition connection; an event query is answered with a well-formed result and can be withdrawn. Event content needs a subscription — see the note below. `src/bin/capture_calendar.rs` |
| Implied volatility, option price | ✅ Supported | The venue computes the model and publishes it per option on a subscription of its own, and that is what a caller asking for volatility or greeks is given. Solved here is only a hypothetical the caller supplies — a price, or a volatility — because the protocol carries no request that carries one; solved against the venue's own model it reproduces the venue's price to the cent on 2 contracts. `src/bin/capture_option_model.rs` |

**Corporate events data.** Event content requires a Wall Street Horizon
subscription; this account holds none, so every query — by contract and by
filter — is answered with an empty set. The schema itself is delivered either
way. A query is one message and one answer, so a withdrawal cancels the answer.

---

## Invariants

Three properties are measured on every commit, and the build fails if one
stops holding.

| What is guaranteed | Where it stands |
| --- | --- |
| A call never returns success having sent nothing | 80 requests, none silent |
| A field a caller sets is never quietly ignored | 154 order fields, none dropped |
| A field the server sends is never thrown away | What this client has no name for is kept under its tag number — 49 such fields on an equity definition, 46 on a bond |

A fourth is held by a test rather than a measurement: **no wire parser aborts
on malformed input.** Every parser is given each prefix of a well-formed frame,
that frame with a byte replaced at each position, and runs that are not frames
at all (`tests/malformed_input.rs`).

## Protocol constraints

These are properties of the IBKR protocol, not of this implementation. The
official gateway behaves the same way.

- **29 order fields are not transmitted.** For each, the protocol either
  carries no tag at all, or carries one the server rejects by name
  (`algo_id` → *Invalid value in field # 8016*; `scale_init_fill_qty` →
  *Can not contain field # 6486*). Each field retains the caller's value, so an
  order constructed against another client round-trips unchanged.
- **One market-data subscription per contract on the wire.** The server holds
  one subscription per contract for a session, so callers asking for the same
  contract are multiplexed client-side and each is served from it.
- **A caller's request id is not what the venue is asked under.** Every
  subscription is asked for under an id this client allocates and is mapped
  back to the caller who wanted it. The venue echoes an id back, so one taken
  from the caller cannot be told apart from one allocated here. This client
  allocates the same way, from one upward, and keys its subscriptions on it.
- **A request id states four bytes, and the top quarter of that range is this
  client's own.** A caller numbers requests below `0xC000_0000`; at and above
  it are the ids this client allocates for the questions it asks on a caller's
  behalf, and above `0xF000_0000` the ones the engine asks for itself. An id
  outside what a caller may use is refused by number rather than answered,
  because answered it would be indistinguishable from one of this client's own
  and its reply withheld. An order may be numbered wider — the venue takes a
  wider one — but an order id reused as a request id has to satisfy this, and
  the interface this client mirrors encourages one counter for both.
- **`keepUpToDate` queries are closed on first response.** Continuation is
  provided by folding the 5-second bar stream into the requested bar size.
- **Historical execution reports require a window within 7 days.** A request
  without one is rejected in full.
- **The option-exercise interest rate series is not served.**
  `OptExInterestRate` is accepted as a tick query against an option contract
  and rejected by name against the underlying, and every window tested returns
  an empty result set.
- **A crypto's book and its trade stream are acknowledged and carry nothing.**
  The request sent for a crypto is the request sent for a share and differs
  only in the security type, and that same request against a future is answered
  with the entitlement — so the shape reaches the server and is understood.
  Both increments of the trade stream are acknowledged and produce no records,
  while on the same contract in the same session top of book streams
  continuously and a historical tick request is answered. The server holds the
  data and does not stream it.
- **The fourth integer on tick 437 has no stated meaning.** The tick carries
  four: a status mask, a number, an index naming one status, and one more. The
  protocol establishes no unit for the last two, so any reading of them would be
  invented rather than found; both are carried on the tick as sent, so a caller
  who knows what they mean has them.

## Sessions

An account takes one logon at a time. The venue states this at connect, names
the session already holding the account, and states when that session logged
in.

| Behaviour | Status | Verification |
| --- | :---: | --- |
| The session holding the account is reported to the caller | ✅ Supported | `competing_session()` returns the address, the logon time, and whether this session may trade. A read-only flag from the venue is carried as stated |
| A logon later than this one is another client, and keeps the account | ✅ Supported | A reconnect that finds one reports it and stops; retrying cannot change it. Both times are read from the venue's clock, so two machines' clocks cannot decide it |
| A logon at or before this one is this session's own, still being reaped | ✅ Supported | The reconnect completes over it, which is what an ordinary recovery is |
| The heartbeat is the interval the venue answered with | ✅ Supported | The interval a logon proposes is not what it is held to; the answer is read from the logon response and applied on every reconnect |
| A reconnect follows the venue | ✅ Supported | It uses the hosts this session reached the venue through, on the port the venue named in its redirect, and stops walking hosts when one answers and refuses |
| The first connect knocks on the next door when one does not answer | ✅ Supported | One host per region. A door that answers and refuses ends the walk, so a refused logon is not repeated at every door |
| An order id is counted from what the account is working | ✅ Supported | An order id belongs to the account, not the process, and the venue refuses one while the order under it is still working. Nothing is kept on disk: at each connect the venue replays what the account is working, and ids count from one past the highest of those — from one when nothing is working |
| A session survives losing its connection | ✅ Supported | A dropped connection is rebuilt on the session already open, with no second factor: five forced drops recovered in 2-8s, and an eight hour session rode through its losses unattended |
| A session does not survive its process | ✅ Documented | The venue holds a session for a socket, not for an account: killed without logging out, it was already gone forty seconds later, and a later start is answered with a handshake. A session is therefore bound to the socket that opened it, so a restart is a fresh logon and an uninterrupted session is not. What that costs an account with a second factor has not been measured here; a paper session presents none |
| A session that has ended answers at once | ✅ Supported | Requests made after a terminal loss are refused with 504 immediately, rather than waiting out a timeout each. Every request already answered keeps the venue's answer |

One session, held open for 175 minutes across a market open: 106,053 quotes,
180,433 trades, 95,985 book rows and 4,148 bars, with no unrequested
disconnect and no error other than the venue's answer for a series it does
not hold. `scripts/endurance.py --minutes 175`.

That check takes every subscription out and puts it back each cycle, which no
other check here does, and it is the only one that sees what a long-running
program sees. Run it before believing a change to the quote path — at any hour:
a book is asked for on a currency pair and trades on a crypto pair, both of
which answer when New York is shut, so the two streams it holds to arriving are
answerable outside the session as well as inside it. Which of them answer is a
question about what the account subscribes to rather than about this client: a
book nobody subscribes to is answered with nothing at all on some venues and
refused by name on others, and both read from here as a stream that did not
arrive.

An order's round trip can be checked at any hour: `scripts/order_round_trip.py`
places a limit far under the market on a contract that trades nearly around the
clock, changes its price, withdraws it, and reads back what the venue did with
each step. The paper suite's own order phases name US shares and wait for the
New York session.

## Architectural differences from a gateway process

| Gateway | This client |
| --- | --- |
| Configuration file and settings window | Settings on the client, read back at runtime. 10 gateway settings have no counterpart: no window geometry, no local listening socket, no JVM heap, no message pacing, and a timestamp delivered as the venue states it |
| Local socket for client programs | The client is in-process; there is no socket to connect to, authorise, or keep running |
| Java runtime | None |

## Known limitations

One, and it is this client's own.

- **The carry term in a hypothetical solve is fitted, not read.** The series
  that would state it (`OptExInterestRate`, under the protocol constraints
  above) is not served, so a caller-supplied price or volatility is solved
  against a rate inferred from the venue's own model rather than one the venue
  stated. It absorbs whatever else the two models disagree about: two contracts
  on one underlying and expiry fitted 4.9% and 20.1%. It does not affect the
  volatility or the greeks a caller reads, which are the venue's and are not
  solved here.

## Refusals

A request this client will not send is reported through `error(reqId, code,
message)` and the call returns rather than raising. The code is the one the
TWS API defines for that class: 321 for a request that fails validation, 200
for a contract description that matches nothing, 504 for a call with no
session, and 327 for binding orders entered elsewhere, which is refused for
every client but the one they bind to. Construction and
configuration raise, as does a synchronous call with a return value.

## Calls

| Category | Calls |
| --- | --- |
| **Connection** | `connect`, `disconnect`, `is_connected`, `run`, `get_account_id` |
| **Market data** | `req_mkt_data`, `cancel_mkt_data`, `req_tick_by_tick_data`, `cancel_tick_by_tick_data`, `req_mkt_depth`, `cancel_mkt_depth`, `req_market_data_type` |
| **Orders** | `place_order`, `cancel_order`, `req_global_cancel`, `req_ids`, `req_open_orders`, `req_all_open_orders`, `req_auto_open_orders`, `req_completed_orders`, `req_executions` |
| **Account** | `req_positions`, `cancel_positions`, `req_positions_multi`, `cancel_positions_multi`, `req_account_summary`, `cancel_account_summary`, `req_account_updates`, `req_account_updates_multi`, `cancel_account_updates_multi`, `req_pnl`, `cancel_pnl`, `req_pnl_single`, `cancel_pnl_single`, `req_managed_accts` |
| **Historical** | `req_historical_data`, `cancel_historical_data`, `req_head_time_stamp`, `cancel_head_time_stamp`, `req_historical_ticks`, `req_historical_schedule`, `req_real_time_bars`, `cancel_real_time_bars`, `req_histogram_data`, `cancel_histogram_data` |
| **Reference** | `req_contract_details`, `req_matching_symbols`, `req_sec_def_opt_params`, `req_mkt_depth_exchanges`, `req_market_rule`, `req_smart_components` |
| **Scanner** | `req_scanner_parameters`, `req_scanner_subscription`, `cancel_scanner_subscription` |
| **News** | `req_news_providers`, `req_news_article`, `req_historical_news`, `req_news_bulletins`, `cancel_news_bulletins` |
| **Fundamental** | `req_fundamental_data`, `cancel_fundamental_data` |
| **Options** | `calculate_implied_volatility`, `cancel_calculate_implied_volatility`, `calculate_option_price`, `cancel_calculate_option_price`, `exercise_options` |
| **Other** | `req_current_time`, `req_user_info`, `req_family_codes`, `req_soft_dollar_tiers`, `set_server_log_level`, `req_wsh_meta_data`, `req_wsh_event_data`, `query_display_groups`, `subscribe_to_group_events` |

**Order types.** MKT, LMT, STP, STP LMT, TRAIL, TRAIL LIMIT, MOC, LOC, MTL,
MIT, LIT, MKT PRT, STP PRT, REL, PEG MKT, PEG MID, MIDPRICE, SNAP MKT,
SNAP MID, SNAP PRI, BOX TOP. Algos: VWAP, TWAP, Arrival Price, Close Price,
Dark Ice, PctVol. Conditions: price, volume, percent change, margin, execution
and time. Brackets, one-cancels-all, and combinations with a price per leg.

**Settings.** The gateway's configuration file is replaced by settings on the
client: announced build, time zone, execution-report scope, and others — 14 in
total, readable at runtime. Ten gateway settings have no counterpart and report
why (no window geometry, no local listening socket, no JVM heap, and no message
pacing: nothing here paces outgoing messages, which the gateway ships with off).
Rust: `EClientConfig.gateway`. Python: `ibx.configure()`.
