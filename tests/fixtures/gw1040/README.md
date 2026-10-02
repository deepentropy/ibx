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

## Files (28/09/2026; depth slices 02/10/2026)

| Folder | Session | Scenarios |
|---|---|---|
| `20260926` | closed | account_summary, account_updates, bracket, cancel_unknown, connect_only, hist_keep_up_to_date, lmt_cancel, modify_cancelled, oca_group, pnl, scanner_two |
| `20260926b` | closed | bracket, hist_keep_up_to_date, pnl, scanner_two |
| `20260928` | overnight | overnight_tif: OVERNIGHT, OVERNIGHT + DAY and includeOvernight orders |
| `20260928` | pre-open | premarket_order_types: STP / TRAIL with outsideRth, IOC / FOK, TIF values, customerAccount refusal, OPG, delayed market data, modify of a filled order |
| `20260928` | RTH | rth_order_types: TRAIL MIT / TRAIL LIT / PASSV REL / RPI / PEG BEST / PEG BENCH, overnight cases in RTH, SPY call spread (combo, refused 460) |
| `20260928` | RTH | depth_single_iex: AAPL depth on IEX alone, 5 rows (slice: the request, its farm entries, acknowledgements, definitions, depth frames and the first 610 depth callbacks) |
| `20260928` | RTH | depth_smart: AAPL SmartDepth, 50 rows (slice: first 862 callbacks, with tail deletes), then AXTI SmartDepth, 10 rows (whole) |

The `6010` (orderRef) values in the order frames are labels chosen by the recording scripts.
