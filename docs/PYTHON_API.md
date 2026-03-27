# Python API Reference (v0.4.2)

*Auto-generated from source — do not edit.*

## EClient Methods

### Connection

| Method | Description |
|--------|-------------|
| `new` | Create a new EClient (or EWrapper) instance. |
| `connect` | Connect to IB and start the engine. |
| `disconnect` | Disconnect from IB. |
| `is_connected` | Check if connected. |
| `run` | Run the event loop. |
| `get_account_id` | Get the account ID. |

### Account & Portfolio

| Method | Description |
|--------|-------------|
| `req_pnl` | Request P&L updates for the account. |
| `cancel_pnl` | Cancel P&L subscription. |
| `req_pnl_single` | Request P&L for a single position. |
| `cancel_pnl_single` | Cancel single-position P&L subscription. |
| `req_account_summary` | Request account summary. |
| `cancel_account_summary` | Cancel account summary. |
| `req_positions` | Request all positions. |
| `cancel_positions` | Cancel positions. |
| `req_account_updates` | Request account updates. |
| `req_managed_accts` | Request managed accounts list. |
| `req_account_updates_multi` | Request account updates for multiple accounts/models. |
| `cancel_account_updates_multi` | Cancel multi-account updates. |
| `req_positions_multi` | Request positions across multiple accounts/models. |
| `cancel_positions_multi` | Cancel multi-account positions. |
| `account_snapshot` | Read account state snapshot. Returns a dict with all account values. |

### Orders

| Method | Description |
|--------|-------------|
| `place_order` | Place an order. |
| `cancel_order` | Cancel an order. |
| `req_global_cancel` | Cancel all orders globally. |
| `req_ids` | Request next valid order ID. |
| `next_order_id` | Get the next order ID (local counter, auto-increments). |
| `req_open_orders` | Request all open orders for this client. |
| `req_all_open_orders` | Request all open orders across all clients. |
| `req_auto_open_orders` | Automatically bind future orders to this client. |
| `req_executions` | Request execution reports. |
| `req_completed_orders` | Request completed orders. |

### Market Data

| Method | Description |
|--------|-------------|
| `set_news_providers` | Set news provider codes for per-contract news ticks (e.g. "BRFG*BRFUPDN"). |
| `req_mkt_data` | Request market data for a contract. |
| `cancel_mkt_data` | Cancel market data subscription. |
| `req_tick_by_tick_data` | Request tick-by-tick data. |
| `cancel_tick_by_tick_data` | Cancel tick-by-tick data. |
| `req_market_data_type` | Set market data type (1=live, 2=frozen, 3=delayed, 4=delayed-frozen). |
| `req_mkt_depth` | Request market depth (L2 order book). |
| `cancel_mkt_depth` | Cancel market depth. |
| `req_real_time_bars` | Request real-time 5-second bars. |
| `cancel_real_time_bars` | Cancel real-time bars. |
| `quote` | Zero-copy SeqLock quote read by req_id. Returns a dict with bid, ask, last, bid_size, ask_size, last_size, volume, high, low, open, close, or None if the req_id is not mapped. |
| `quote_by_instrument` | Zero-copy SeqLock quote read by InstrumentId. Returns a dict with bid, ask, last, bid_size, ask_size, last_size, volume, high, low, open, close, or None if not connected. |

### Reference Data

| Method | Description |
|--------|-------------|
| `req_historical_data` | Request historical bar data. |
| `cancel_historical_data` | Cancel historical data. |
| `req_head_time_stamp` | Request head timestamp. |
| `cancel_head_time_stamp` | Cancel head timestamp request. |
| `req_contract_details` | Request contract details. |
| `req_mkt_depth_exchanges` | Request available exchanges for market depth. |
| `req_matching_symbols` | Search for matching symbols. |
| `req_scanner_subscription` | Request scanner subscription. |
| `cancel_scanner_subscription` | Cancel scanner subscription. |
| `req_scanner_parameters` | Request scanner parameters XML. |
| `req_news_article` | Request a news article. |
| `req_historical_news` | Request historical news. |
| `req_fundamental_data` | Request fundamental data. |
| `cancel_fundamental_data` | Cancel fundamental data. |
| `req_historical_ticks` | Request historical tick data. |
| `req_market_rule` | Request market rule details. |
| `req_histogram_data` | Request histogram data. |
| `cancel_histogram_data` | Cancel histogram data. |
| `req_historical_schedule` | Request historical trading schedule. |

### Gateway-Local & Stubs

| Method | Description |
|--------|-------------|
| `calculate_implied_volatility` | Calculate option implied volatility. Not yet implemented. |
| `calculate_option_price` | Calculate option theoretical price. Not yet implemented. |
| `cancel_calculate_implied_volatility` | Cancel implied volatility calculation. |
| `cancel_calculate_option_price` | Cancel option price calculation. |
| `exercise_options` | Exercise options. Not yet implemented. |
| `req_sec_def_opt_params` | Request option chain parameters. Not yet implemented. |
| `req_news_bulletins` | Subscribe to news bulletins. |
| `cancel_news_bulletins` | Cancel news bulletin subscription. |
| `req_current_time` | Request current server time. |
| `request_fa` | Request FA data. Not yet implemented. |
| `replace_fa` | Replace FA data. Not yet implemented. |
| `query_display_groups` | Query display groups. |
| `subscribe_to_group_events` | Subscribe to display group events. |
| `unsubscribe_from_group_events` | Unsubscribe from display group events. |
| `update_display_group` | Update display group. |
| `req_smart_components` | Request SMART routing component exchanges. |
| `req_news_providers` | Request available news providers. |
| `req_soft_dollar_tiers` | Request soft dollar tiers. |
| `req_family_codes` | Request family codes. |
| `set_server_log_level` | Set server log level (1=error..5=trace). |
| `req_user_info` | Request user info (white branding ID). |
| `req_wsh_meta_data` | Request Wall Street Horizon metadata. Not yet implemented. |
| `req_wsh_event_data` | Request Wall Street Horizon event data. Not yet implemented. |

## EWrapper Callbacks

| Callback | Description |
|----------|-------------|
| `new` | Create a new EClient (or EWrapper) instance. |
| `connect_ack` | Connection acknowledged. |
| `connection_closed` | Connection has been closed. |
| `next_valid_id` | Next valid order ID from the server. |
| `managed_accounts` | Comma-separated list of managed account IDs. |
| `error` | Error or informational message from the server. |
| `current_time` | Current server time (Unix seconds). |
| `tick_price` | Price tick update (bid, ask, last, etc.). |
| `tick_size` | Size tick update (bid size, ask size, volume, etc.). |
| `tick_string` | String tick (e.g. last trade timestamp). |
| `tick_generic` | Generic numeric tick value. |
| `tick_snapshot_end` | Snapshot delivery complete; subscription auto-cancelled. |
| `market_data_type` | Market data type changed (1=live, 2=frozen, 3=delayed, 4=delayed-frozen). |
| `order_status` | Order status update (filled, remaining, avg price, etc.). |
| `open_order` | Open order details (contract, order, state). |
| `open_order_end` | End of open orders list. |
| `exec_details` | Execution fill details. |
| `exec_details_end` | End of execution details list. |
| `commission_report` | Commission report for an execution. |
| `update_account_value` | Account value update (key/value/currency). |
| `update_portfolio` | Portfolio position update. |
| `update_account_time` | Account update timestamp. |
| `account_download_end` | Account data delivery complete. |
| `account_summary` | Account summary tag/value entry. |
| `account_summary_end` | End of account summary. |
| `position` | Position entry (account, contract, size, avg cost). |
| `position_end` | End of positions list. |
| `pnl` | Account P&L update (daily, unrealized, realized). |
| `pnl_single` | Single-position P&L update. |
| `historical_data` | Historical OHLCV bar. |
| `historical_data_end` | End of historical data delivery. |
| `historical_data_update` | Real-time bar update (keep_up_to_date=true). |
| `head_timestamp` | Earliest available data timestamp. |
| `contract_details` | Contract definition details. |
| `contract_details_end` | End of contract details. |
| `symbol_samples` | Matching symbol search results. |
| `tick_by_tick_all_last` | Tick-by-tick last trade. |
| `tick_by_tick_bid_ask` | Tick-by-tick bid/ask quote. |
| `tick_by_tick_mid_point` | Tick-by-tick midpoint. |
| `scanner_data` | Scanner result entry (rank, contract, distance). |
| `scanner_data_end` | End of scanner results. |
| `scanner_parameters` | Scanner parameters XML. |
| `news_providers` | Available news providers list. |
| `news_article` | Full news article text. |
| `historical_news` | Historical news headline. |
| `historical_news_end` | End of historical news. |
| `tick_news` | Per-contract news tick. |
| `update_mkt_depth` | L2 book update (single exchange). |
| `update_mkt_depth_l2` | L2 book update (with market maker). |
| `mkt_depth_exchanges` | Available exchanges for market depth. |
| `real_time_bar` | Real-time 5-second OHLCV bar. |
| `historical_ticks` | Historical tick data (Last, BidAsk, or Midpoint). |
| `historical_ticks_bid_ask` | Historical bid/ask ticks. |
| `historical_ticks_last` | Historical last-trade ticks. |
| `tick_option_computation` | Option implied vol / greeks computation. |
| `security_definition_option_parameter` | Option chain parameters (strikes, expirations). |
| `security_definition_option_parameter_end` | End of option chain parameters. |
| `fundamental_data` | Fundamental data (XML/JSON). |
| `update_news_bulletin` | News bulletin message. |
| `receive_fa` | Financial advisor data received. |
| `replace_fa_end` | Financial advisor replace complete. |
| `position_multi` | Multi-account position entry. |
| `position_multi_end` | End of multi-account positions. |
| `account_update_multi` | Multi-account value update. |
| `account_update_multi_end` | End of multi-account updates. |
| `display_group_list` | Display group list. |
| `display_group_updated` | Display group updated. |
| `market_rule` | Market rule: price increment schedule. |
| `smart_components` | SMART routing component exchanges. |
| `soft_dollar_tiers` | Soft dollar tier list. |
| `family_codes` | Family codes linking related accounts. |
| `histogram_data` | Price distribution histogram. |
| `user_info` | User info (white branding ID). |
| `wsh_meta_data` | Wall Street Horizon metadata. |
| `wsh_event_data` | Wall Street Horizon event data. |
| `completed_order` | Completed (filled/cancelled) order details. |
| `completed_orders_end` | End of completed orders list. |
| `order_bound` | Order bound to a perm ID. |
| `tick_req_params` | Tick parameters: min tick size, BBO exchange, snapshot permissions. |
| `bond_contract_details` | Bond contract details. |
| `delta_neutral_validation` | Delta-neutral validation response. |
| `historical_schedule` | Historical trading schedule (exchange hours). |

## Full Signatures

<details>
<summary>EClient</summary>

```python
def new(wrapper))
def connect(host="cdc1.ibllc.com".to_string(), port=0, client_id=0, username="".to_string(), password="".to_string(), paper=true, core_id=None))
def disconnect()
def is_connected()
def run()
def get_account_id()
def req_pnl(req_id, account, model_code=""))
def cancel_pnl(req_id)
def req_pnl_single(req_id, account, model_code, con_id))
def cancel_pnl_single(req_id)
def req_account_summary(req_id, group_name, tags))
def cancel_account_summary(req_id)
def req_positions()
def cancel_positions()
def req_account_updates(subscribe, _acct_code=""))
def req_managed_accts()
def req_account_updates_multi(req_id, account, model_code, ledger_and_nlv=false))
def cancel_account_updates_multi(req_id)
def req_positions_multi(req_id, account, model_code))
def cancel_positions_multi(req_id)
def account_snapshot()
def place_order(order_id, contract, order)
def cancel_order(order_id, manual_order_cancel_time=""))
def req_global_cancel()
def req_ids(num_ids=1))
def next_order_id()
def req_open_orders()
def req_all_open_orders()
def req_auto_open_orders(b_auto_bind))
def req_executions(req_id, exec_filter=None))
def req_completed_orders(api_only=false))
def set_news_providers(providers))
def req_mkt_data(req_id, contract, generic_tick_list="", snapshot=false, regulatory_snapshot=false, mkt_data_options=Vec::new()))
def cancel_mkt_data(req_id)
def req_tick_by_tick_data(req_id, contract, tick_type, number_of_ticks=0, ignore_size=false))
def cancel_tick_by_tick_data(req_id)
def req_market_data_type(market_data_type)
def req_mkt_depth(req_id, contract, num_rows=5, is_smart_depth=false, mkt_depth_options=Vec::new()))
def cancel_mkt_depth(req_id, is_smart_depth=false))
def req_real_time_bars(req_id, contract, bar_size=5, what_to_show="TRADES", use_rth=0, real_time_bars_options=Vec::new()))
def cancel_real_time_bars(req_id)
def quote(req_id)
def quote_by_instrument(instrument)
def req_historical_data(req_id, contract, end_date_time, duration_str, bar_size_setting, what_to_show, use_rth, format_date=1, keep_up_to_date=false, chart_options=Vec::new()))
def cancel_historical_data(req_id)
def req_head_time_stamp(req_id, contract, what_to_show, use_rth, format_date=1))
def cancel_head_time_stamp(req_id)
def req_contract_details(req_id, contract)
def req_mkt_depth_exchanges()
def req_matching_symbols(req_id, pattern)
def req_scanner_subscription(req_id, subscription, scanner_subscription_options=Vec::new()))
def cancel_scanner_subscription(req_id)
def req_scanner_parameters()
def req_news_article(req_id, provider_code, article_id, news_article_options=Vec::new()))
def req_historical_news(req_id, con_id, provider_codes, start_date_time, end_date_time, total_results, historical_news_options=Vec::new()))
def req_fundamental_data(req_id, contract, report_type, fundamental_data_options=Vec::new()))
def cancel_fundamental_data(req_id)
def req_historical_ticks(req_id, contract, start_date_time="", end_date_time="", number_of_ticks=1000, what_to_show="TRADES", use_rth=1, ignore_size=false, misc_options=Vec::new()))
def req_market_rule(market_rule_id)
def req_histogram_data(req_id, contract, use_rth, time_period))
def cancel_histogram_data(req_id)
def req_historical_schedule(req_id, contract, end_date_time="", duration_str="1 M", use_rth=true))
def calculate_implied_volatility(req_id, contract, option_price, under_price, implied_vol_options=Vec::new()))
def calculate_option_price(req_id, contract, volatility, under_price, opt_prc_options=Vec::new()))
def cancel_calculate_implied_volatility(req_id)
def cancel_calculate_option_price(req_id)
def exercise_options(req_id, contract, exercise_action, exercise_quantity, account, _override))
def req_sec_def_opt_params(req_id, underlying_symbol, fut_fop_exchange="", underlying_sec_type="STK", underlying_con_id=0))
def req_news_bulletins(all_msgs=true))
def cancel_news_bulletins()
def req_current_time()
def request_fa(_fa_data_type)
def replace_fa(req_id, fa_data_type, cxml))
def query_display_groups(req_id)
def subscribe_to_group_events(req_id, group_id)
def unsubscribe_from_group_events(req_id)
def update_display_group(req_id, contract_info)
def req_smart_components(req_id, bbo_exchange)
def req_news_providers()
def req_soft_dollar_tiers(req_id)
def req_family_codes()
def set_server_log_level(log_level=2))
def req_user_info(req_id)
def req_wsh_meta_data(req_id)
def req_wsh_event_data(req_id, wsh_event_data=None))
```
</details>

<details>
<summary>EWrapper</summary>

```python
def new(*_args, **_kwargs))
def connect_ack()
def connection_closed()
def next_valid_id(_order_id)
def managed_accounts(_accounts_list)
def error(_req_id, _error_code, _error_string, _advanced_order_reject_json=""))
def current_time(_time)
def tick_price(_req_id, _tick_type, _price, _attrib)
def tick_size(_req_id, _tick_type, _size)
def tick_string(_req_id, _tick_type, _value)
def tick_generic(_req_id, _tick_type, _value)
def tick_snapshot_end(_req_id)
def market_data_type(_req_id, _market_data_type)
def order_status(_order_id, _status, _filled, _remaining, _avg_fill_price, _perm_id, _parent_id, _last_fill_price, _client_id, _why_held, _mkt_cap_price)
def open_order(_order_id, _contract, _order, _order_state)
def open_order_end()
def exec_details(_req_id, _contract, _execution)
def exec_details_end(_req_id)
def commission_report(_commission_report)
def update_account_value(_key, _value, _currency, _account_name)
def update_portfolio(_contract, _position, _market_price, _market_value, _average_cost, _unrealized_pnl, _realized_pnl, _account_name)
def update_account_time(_timestamp)
def account_download_end(_account)
def account_summary(_req_id, _account, _tag, _value, _currency)
def account_summary_end(_req_id)
def position(_account, _contract, _pos, _avg_cost)
def position_end()
def pnl(_req_id, _daily_pnl, _unrealized_pnl, _realized_pnl)
def pnl_single(_req_id, _pos, _daily_pnl, _unrealized_pnl, _realized_pnl, _value)
def historical_data(_req_id, _bar)
def historical_data_end(_req_id, _start, _end)
def historical_data_update(_req_id, _bar)
def head_timestamp(_req_id, _head_timestamp)
def contract_details(_req_id, _contract_details)
def contract_details_end(_req_id)
def symbol_samples(_req_id, _contract_descriptions)
def tick_by_tick_all_last(_req_id, _tick_type, _time, _price, _size, _tick_attrib_last, _exchange, _special_conditions)
def tick_by_tick_bid_ask(_req_id, _time, _bid_price, _ask_price, _bid_size, _ask_size, _tick_attrib_bid_ask)
def tick_by_tick_mid_point(_req_id, _time, _mid_point)
def scanner_data(_req_id, _rank, _contract_details, _distance, _benchmark, _projection, _legs_str)
def scanner_data_end(_req_id)
def scanner_parameters(_xml)
def news_providers(_news_providers)
def news_article(_req_id, _article_type, _article_text)
def historical_news(_req_id, _time, _provider_code, _article_id, _headline)
def historical_news_end(_req_id, _has_more)
def tick_news(_ticker_id, _time_stamp, _provider_code, _article_id, _headline, _extra_data)
def update_mkt_depth(_req_id, _position, _operation, _side, _price, _size)
def update_mkt_depth_l2(_req_id, _position, _market_maker, _operation, _side, _price, _size, _is_smart_depth)
def mkt_depth_exchanges(_depth_mkt_data_descriptions)
def real_time_bar(_req_id, _date, _open, _high, _low, _close, _volume, _wap, _count)
def historical_ticks(_req_id, _ticks, _done)
def historical_ticks_bid_ask(_req_id, _ticks, _done)
def historical_ticks_last(_req_id, _ticks, _done)
def tick_option_computation(_req_id, _tick_type, _tick_attrib, _implied_vol, _delta, _opt_price, _pv_dividend, _gamma, _vega, _theta, _und_price)
def security_definition_option_parameter(_req_id, _exchange, _underlying_con_id, _trading_class, _multiplier, _expirations, _strikes)
def security_definition_option_parameter_end(_req_id)
def fundamental_data(_req_id, _data)
def update_news_bulletin(_msg_id, _msg_type, _message, _orig_exchange)
def receive_fa(_fa_data_type, _xml)
def replace_fa_end(_req_id, _text)
def position_multi(_req_id, _account, _model_code, _contract, _pos, _avg_cost)
def position_multi_end(_req_id)
def account_update_multi(_req_id, _account, _model_code, _key, _value, _currency)
def account_update_multi_end(_req_id)
def display_group_list(_req_id, _groups)
def display_group_updated(_req_id, _contract_info)
def market_rule(_market_rule_id, _price_increments)
def smart_components(_req_id, _smart_component_map)
def soft_dollar_tiers(_req_id, _tiers)
def family_codes(_family_codes)
def histogram_data(_req_id, _items)
def user_info(_req_id, _white_branding_id)
def wsh_meta_data(_req_id, _data_json)
def wsh_event_data(_req_id, _data_json)
def completed_order(_contract, _order, _order_state)
def completed_orders_end()
def order_bound(_order_id, _api_client_id, _api_order_id)
def tick_req_params(_ticker_id, _min_tick, _bbo_exchange, _snapshot_permissions)
def bond_contract_details(_req_id, _contract_details)
def delta_neutral_validation(_req_id, _delta_neutral_contract)
def historical_schedule(_req_id, _start_date_time, _end_date_time, _time_zone, _sessions)
```
</details>
