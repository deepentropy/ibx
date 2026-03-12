# ibapi-Compatible API Gap Analysis

**Date**: 2026-03-12
**Scope**: IBX Python compat layer (`src/python/compat/`) vs official ibapi `EClient`/`EWrapper`

## Legend
- **IMPLEMENTED**: EClient request + EWrapper callback both exposed in Python compat
- **CALLBACK ONLY**: EWrapper callback exists but no EClient request method exposed
- **RUST CORE ONLY**: Rust control module has the logic but not wired to Python compat
- **MISSING**: Not implemented anywhere

---

## Coverage Summary

| Category | EClient Methods | EWrapper Callbacks | Status |
|---|---|---|---|
| Connection | 4/5 | 4/4 | Almost complete |
| Market Data (L1) | 3/4 | 6/7 | Missing `req_market_data_type` |
| Tick-by-Tick (L3) | 2/2 | 3/3 | Complete |
| Market Depth (L2) | 0/2 | 2/3 | Callback only, no requests |
| Historical Bars | 3/5 | 3/4 | Missing ticks, real-time bars |
| Orders | 4/6 | 3/5 | Missing open orders query |
| Account & Portfolio | 4/6 | 8/10 | Missing multi-account |
| Positions | 2/2 | 2/4 | Missing multi-model positions |
| P&L | 4/4 | 2/2 | Complete |
| Contract Details | 1/2 | 3/3 | Missing option chain params |
| Scanner | 0/3 | 3/3 | Callback only (Rust core has logic) |
| News | 0/5 | 5/6 | Callback only (Rust core has logic) |
| Fundamental Data | 0/2 | 0/1 | Rust core has logic, not exposed |
| Real-Time Bars | 0/2 | 0/1 | Missing |
| Options/Greeks | 0/5 | 1/2 | Only tick_option_computation callback |
| FA (Financial Advisor) | 0/2 | 0/2 | Missing |
| Display Groups | 0/4 | 0/2 | Missing |
| Advanced Features | 0/7 | 0/7 | Missing |

**Overall: ~23/61 EClient methods (38%), ~45/65 EWrapper callbacks (69%)**

---

## Detailed Gap List

### TIER 1 — High Impact (commonly used by ibapi users)

| # | EClient Method | EWrapper Callback | Status | Notes |
|---|---|---|---|---|
| 1 | `req_market_data_type(market_data_type)` | `market_data_type()` ✅ | **MISSING REQUEST** | Needed to switch live/delayed/frozen data. Very common. |
| 2 | `req_mkt_depth(req_id, contract, num_rows, is_smart_depth, opts)` | `update_mkt_depth()` ✅ / `update_mkt_depth_l2()` ✅ | **MISSING REQUEST** | L2 order book. Callbacks already exist. |
| 3 | `cancel_mkt_depth(req_id, is_smart_depth)` | — | **MISSING** | Pairs with above. |
| 4 | `req_open_orders()` | `open_order()` ✅ / `open_order_end()` ✅ | **MISSING REQUEST** | Callbacks exist. Users need to query open orders. |
| 5 | `req_all_open_orders()` | (same callbacks) | **MISSING REQUEST** | Master client variant. |
| 6 | `req_executions(req_id, filter)` | `exec_details()` ✅ / `exec_details_end()` ✅ | **MISSING REQUEST** | Callbacks exist. Needed for fill history. |
| 7 | `req_historical_ticks(req_id, contract, start, end, count, what, rth, ignore_size, opts)` | `historical_ticks()` / `historical_ticks_bid_ask()` / `historical_ticks_last()` | **MISSING** | Time & Sales data. Different from bars. |
| 8 | `req_real_time_bars(req_id, contract, bar_size, what, rth, opts)` | `real_time_bar()` | **MISSING** | 5-second bars. Popular for live trading. |
| 9 | `cancel_real_time_bars(req_id)` | — | **MISSING** | Pairs with above. |
| 10 | `cancel_head_time_stamp(req_id)` | — | **MISSING** | Pairs with existing `req_head_time_stamp`. |
| 11 | `req_sec_def_opt_params(req_id, symbol, exchange, sec_type, con_id)` | `security_definition_option_parameter()` / `_end()` | **MISSING** | Option chain parameters. Essential for options trading. |
| 12 | `req_matching_symbols(req_id, pattern)` | `symbol_samples()` ✅ | **RUST CORE ONLY** | `build_matching_symbols_request` exists in control/contracts.rs. Callback exists. Just need to wire the request. |
| 13 | `req_current_time()` | `current_time()` ✅ | **MISSING REQUEST** | Callback exists. Simple to add. |

### TIER 2 — Medium Impact (used by advanced users)

| # | EClient Method | EWrapper Callback | Status | Notes |
|---|---|---|---|---|
| 14 | `req_scanner_subscription(req_id, sub, opts)` | `scanner_data()` ✅ / `scanner_data_end()` ✅ | **RUST CORE ONLY** | Scanner logic exists in control/scanner.rs. Callbacks exist. |
| 15 | `cancel_scanner_subscription(req_id)` | — | **RUST CORE ONLY** | `build_scanner_cancel_xml` exists. |
| 16 | `req_scanner_parameters()` | `scanner_parameters()` ✅ | **RUST CORE ONLY** | `build_scanner_params_request` exists. Callback exists. |
| 17 | `req_news_providers()` | `news_providers()` ✅ | **MISSING REQUEST** | Callback exists. |
| 18 | `req_news_article(req_id, provider, article_id, opts)` | `news_article()` ✅ | **RUST CORE ONLY** | `build_article_request_xml` exists. Callback exists. |
| 19 | `req_historical_news(req_id, con_id, provider, start, end, limit, opts)` | `historical_news()` ✅ / `historical_news_end()` ✅ | **RUST CORE ONLY** | `build_historical_news_xml` exists. Callbacks exist. |
| 20 | `req_fundamental_data(req_id, contract, report_type, opts)` | `fundamental_data()` | **RUST CORE ONLY** | Full logic in control/fundamental.rs. Missing EWrapper callback. |
| 21 | `cancel_fundamental_data(req_id)` | — | **MISSING** | Pairs with above. |
| 22 | `calculate_implied_volatility(req_id, contract, price, under_price, opts)` | (uses `tick_option_computation` ✅) | **MISSING** | Options pricing. |
| 23 | `calculate_option_price(req_id, contract, vol, under_price, opts)` | (uses `tick_option_computation` ✅) | **MISSING** | Options pricing. |
| 24 | `cancel_calculate_implied_volatility(req_id)` | — | **MISSING** | |
| 25 | `cancel_calculate_option_price(req_id)` | — | **MISSING** | |
| 26 | `exercise_options(req_id, contract, action, quantity, account, override)` | — | **MISSING** | Option exercise. |
| 27 | `req_news_bulletins(all_msgs)` | `update_news_bulletin()` | **MISSING** | |
| 28 | `cancel_news_bulletins()` | — | **MISSING** | |
| 29 | `req_managed_accts()` | `managed_accounts()` ✅ | **MISSING REQUEST** | Callback exists (called automatically at connect). |
| 30 | `req_account_updates_multi(req_id, account, model, ledger)` | `account_update_multi()` / `_end()` | **MISSING** | Multi-account variant. |
| 31 | `cancel_account_updates_multi(req_id)` | — | **MISSING** | |
| 32 | `req_positions_multi(req_id, account, model)` | `position_multi()` / `_end()` | **MISSING** | Multi-model positions. |
| 33 | `cancel_positions_multi(req_id)` | — | **MISSING** | |

### TIER 3 — Low Impact (niche/rarely used)

| # | EClient Method | EWrapper Callback | Status |
|---|---|---|---|
| 34 | `request_fa(fa_data_type)` | `receive_fa()` | **MISSING** |
| 35 | `replace_fa(req_id, fa_data_type, xml)` | `replace_fa_end()` | **MISSING** |
| 36 | `query_display_groups(req_id)` | `display_group_list()` | **MISSING** |
| 37 | `subscribe_to_group_events(req_id, group_id)` | `display_group_updated()` | **MISSING** |
| 38 | `unsubscribe_from_group_events(req_id)` | — | **MISSING** |
| 39 | `update_display_group(req_id, contract_info)` | — | **MISSING** |
| 40 | `req_market_rule(market_rule_id)` | `market_rule()` | **MISSING** |
| 41 | `req_smart_components(req_id, bbo_exchange)` | `smart_components()` | **MISSING** |
| 42 | `req_soft_dollar_tiers(req_id)` | `soft_dollar_tiers()` | **MISSING** |
| 43 | `req_family_codes()` | `family_codes()` | **MISSING** |
| 44 | `req_histogram_data(req_id, contract, rth, period)` | `histogram_data()` | **MISSING** |
| 45 | `cancel_histogram_data(req_id)` | — | **MISSING** |
| 46 | `set_server_log_level(level)` | — | **MISSING** |
| 47 | `req_user_info(req_id)` | `user_info()` | **MISSING** |
| 48 | `req_wsh_meta_data(req_id)` | `wsh_meta_data()` | **MISSING** |
| 49 | `req_wsh_event_data(req_id, ...)` | `wsh_event_data()` | **MISSING** |
| 50 | — | `completed_order()` / `completed_orders_end()` | **MISSING CALLBACK** |
| 51 | — | `order_bound()` | **MISSING CALLBACK** |
| 52 | — | `tick_req_params()` | **MISSING CALLBACK** |
| 53 | — | `bond_contract_details()` | **MISSING CALLBACK** |
| 54 | — | `delta_neutral_validation()` | **MISSING CALLBACK** |
| 55 | — | `historical_schedule()` | **MISSING CALLBACK** |
| 56 | — | `mkt_depth_exchanges()` | **MISSING CALLBACK** |

---

## Quick Wins (lowest effort to wire up)

These have Rust core logic AND/OR EWrapper callbacks already. Only need EClient request method in Python compat:

1. **`req_market_data_type`** — trivial, just sets a flag
2. **`req_mkt_depth` / `cancel_mkt_depth`** — callbacks already exist
3. **`req_open_orders`** — callbacks already exist
4. **`req_executions`** — callbacks already exist
5. **`req_current_time`** — callback already exists
6. **`req_matching_symbols`** — Rust parser + callback both exist
7. **`req_scanner_*`** — full Rust core logic + callbacks exist
8. **`req_news_*`** — full Rust core logic + callbacks exist
9. **`cancel_head_time_stamp`** — trivial cancel

---

## Existing Implementation Notes

### What's well-covered:
- Connection lifecycle (connect/disconnect/reconnect)
- L1 market data (req/cancel/all tick types)
- Tick-by-tick data (last, bid/ask, midpoint)
- Historical bars + head timestamp
- Full order management (place/cancel/modify/global cancel + 30+ order types)
- Account updates, summary, positions
- P&L (portfolio + single position)
- Contract details lookup
- All order condition types (price, time, margin, execution, volume, pct change)
- Algo strategies (VWAP, TWAP, ArrivalPx, ClosePx, DarkIce, PctVol, Adaptive)

### Python compat `req_account_updates` is a no-op:
Line 554 in client.rs shows `_subscribe` and `_acct_code` are unused — this may be intentionally handled via the CCP connection, but worth verifying it works for ibapi users who call it.
