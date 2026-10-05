# Gateway 1040 reference scenarios

Scenarios recorded from the official IB Gateway 1040 on a paper account, for the scenario replay tests
(ibx#487) and the fixture tree of ibx#484. Each file holds one scenario with its four legs in one time order.

## Layout

`scenarios/<yyyymmdd>/<scenario>.jsonl`, one JSON object per line:

- Line 1, `"type": "header"`:
  - `format` (`four-leg/1`), `scenario`, `gateway` / `gateway_version`;
  - `capture_date` (dd/mm/yyyy), `market_session` (`overnight`, `pre-open`, `RTH`, `after-hours`, `closed`, taken
    from New York time at the first record, with `market_session_at`);
  - `contracts` (symbol, secType, conId seen in the scenario), `api_conn`, `seq_range`, `counts` per leg;
  - `script_version`, `notes`.
- Then one line per record, in the gateway's own order (`seq`):
  - `leg`: `api_out` (API client to gateway), `fix_out` (gateway to IB server), `fix_in` (IB server to gateway),
    `api_in` (gateway to API client);
  - `seq`, `nanos` (ns since the recorder started), `conn` (`api:<port>`, `CCP`, `usfarm`, `ushmds`, ...), `hook`
    (where the record was taken), `raw_b64` (bytes), and decoded fields (FIX tags as `[tag, value]` pairs, API fields
    split on NUL).

All legs come from one recorder inside the gateway JVM with one sequence counter, so the order between legs is the
order in which the gateway handled them. An `api_in` record may hold several callbacks written in one socket write;
it always comes after the `fix_in` that caused it.

## Masking

- Account id: `DUXXXXXXX`. IB username: `{user}`. Machine fingerprint: `{hwid}|XX:XX:XX:XX:XX:XX`.
- Masked in raw and decoded forms. When a mask changes the length of a text FIX frame, `9=`, `95=` and `10=` are
  recomputed; protobuf lengths are rewritten. Binary and NS frames get masks of the same length.
- Check before adding a file: no match for `\b(DU|DF|U|F)[0-9]{6,8}\b` (outside `8349` signatures), no MAC address,
  no username.

## Files (28/09/2026; depth slices 02/10/2026; combos 03/10/2026; global cancel 04/10/2026; 05/10/2026)

| Folder | Session | Scenarios |
|---|---|---|
| `20260926` | closed | account_summary, account_updates, bracket, cancel_unknown, connect_only, hist_keep_up_to_date, lmt_cancel, modify_cancelled, oca_group, pnl, scanner_two |
| `20260926b` | closed | bracket, hist_keep_up_to_date, pnl, scanner_two |
| `20260928` | overnight | overnight_tif: OVERNIGHT, OVERNIGHT + DAY and includeOvernight orders |
| `20260928` | pre-open | premarket_order_types: STP / TRAIL with outsideRth, IOC / FOK, TIF values, customerAccount refusal, OPG, delayed market data, modify of a filled order |
| `20260928` | RTH | rth_order_types: TRAIL MIT / TRAIL LIT / PASSV REL / RPI / PEG BEST / PEG BENCH, overnight cases in RTH, SPY call spread (combo, refused 460) |
| `20260928` | RTH | depth_single_iex: AAPL depth on IEX alone, 5 rows (slice: the request, its farm entries, acknowledgements, definitions, depth frames and the first 610 depth callbacks) |
| `20260928` | RTH | option_chain_aapl: reqSecDefOptParams AAPL STK (slice of i192_f2_option_future_lookup: the request, the derivative query, the stock leg lookup, the chain query and answer, the 41 rows and the end) |
| `20260928` | RTH | depth_smart: AAPL SmartDepth, 50 rows (slice: first 862 callbacks, with tail deletes), then AXTI SmartDepth, 10 rows (whole) |
| `20260928` | RTH | depth_smart_status: the same two SmartDepth requests (slice: requests and cancels, component definition lookups and replies, depth and top-of-book entries, acknowledgements, refusals, API errors with the 2152 warnings; no depth frames) |
| `20260926` | closed | i105_combo_stock_smart: the first combo order of the session (SMART, SPY / QQQ, BAG symbol SPY,QQQ): the set-up requests, then 478 |
| `20260926b` | closed | i105_combo_stock_smart, i105_combo_leg_prices, i105_combo_directed: combo orders of the same session (no set-up request), price change and cancel, per-leg prices, a combo on ARCA refused with 200 |
| `20260930` | RTH | i105_combo_fill: a SMART stock combo bought and sold, its set-up, the fill of the combo and of each leg |
| `20261002` | pre-open | b1_432_hist_ticks: historical ticks of AAPL and EUR.USD (start, end, both, no zone, refusals, AGGTRADES); b1_429_keep_up_to_date: four keepUpToDate requests, their updates, cancels and refusals; b1_431_hist_format: formatDate 1 and 2 bars, historicalDataEnd strings, head timestamps, cancel of an unknown request |
| `20261002` | pre-open | b1_cleanup: reqAllOpenOrders and reqPositions, both answered from the gateway's state (no frame) |
| `20261005` | RTH | b2_generic: generic ticks (AAPL with sixteen of them, SPY with mdoff, an invalid list, EUR.USD with 233 on the cash farm, MNQ with 588 on the futures farm); b2_mkt_errors: two ids on AAPL, 7203 refused (354 with type 1, with 233, 10167 and delayed data with type 3, type 1 again); b2_tbt: tick-by-tick types, past ticks, ignoreSize, EUR.USD on the cash farm, an unknown type; b2_rtbars: real-time bars shared by four requests, empty bars, the cancel of one; b2_trail: plain TRAIL orders and their trail stop prices |
| `20261001` | RTH | global_cancel_replayed: 8 orders of earlier sessions in the logon replay (150=A 20=3 39=A), then reqAllOpenOrders, reqGlobalCancel (8 cancels tagged ALL, in the book's order) and reqAllOpenOrders of client 193 (slice: the replay's order reports and the API connection; the scenario connections in between left out) |

## Decoded API side (`<name>.api.jsonl`, ibx#487)

Next to each scenario, its API messages decoded by the official client library, for the scenario replay
(`test_support::scenario`, tests/scenario_replay.rs): line 1 a header (`format` `four-leg-api/1`, `source`,
`decoder`, `machine_zone`: the zone of the machine that ran the gateway, the replay runs in it), then one line per API record: `seq`, `leg`, `msg_name`, and `request` (`api_out`) or `callbacks`
(`api_in`) in the form of the codec fixtures below. Made by `scripts/codec_fixtures.py --scenarios`; a scenario
recorded again needs only this script run again.

The `6010` (orderRef) values in the order frames are labels chosen by the recording scripts.

## Codec fixtures (`codec/`, ibx#486)

Slices of the scenarios above (and of other recorded sessions) for the golden codec tests (`src/golden/`), one file
per scenario, `codec/1` format, made by `scripts/codec_fixtures.py`:

- line 1, the header: `scenario`, `area`, `source` (the scenario file), `capture_date`, `market_session`,
  `machine_zone` (the zone of the machine that ran the gateway: a time without a zone is read in it; the replays run
  in it, whatever the zone of the machine running the tests), `notes`;
- then the kept records in their order (`seq`, `leg`, `conn`, `msg_type` or `msg_name`, `raw_b64`), with the API side
  decoded by the official client library: `request` on an `api_out` record (for placeOrder the order and the contract
  as the library reads them back, only the fields that differ from a new object), `callbacks` on an `api_in` record
  (the wrapper calls the library makes for the message).

The decode tests send the recorded server frames to ibx (the farm request ids and lookup ids replaced by ibx's) and
compare ibx's callbacks with `callbacks`; the encode tests make the `request` again and compare ibx's messages with the
recorded ones after `test_support::normalise`.
