# Python API Reference (v0.4.2)

*Auto-generated from source — do not edit.*

## EClient Methods

### Connection

| Method | Description |
|--------|-------------|
| `new` |  |
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
| `cancel_mkt_data` |  |
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
| `calculate_implied_volatility` |  |
| `calculate_option_price` |  |
| `cancel_calculate_implied_volatility` |  |
| `cancel_calculate_option_price` |  |
| `exercise_options` |  |
| `req_sec_def_opt_params` |  |
| `req_news_bulletins` |  |
| `cancel_news_bulletins` |  |
| `req_current_time` |  |
| `request_fa` |  |
| `replace_fa` |  |
| `query_display_groups` |  |
| `subscribe_to_group_events` |  |
| `unsubscribe_from_group_events` |  |
| `update_display_group` |  |
| `req_smart_components` |  |
| `req_news_providers` |  |
| `req_soft_dollar_tiers` |  |
| `req_family_codes` |  |
| `set_server_log_level` |  |
| `req_user_info` |  |
| `req_wsh_meta_data` |  |
| `req_wsh_event_data` |  |

## EWrapper Callbacks

| Callback | Description |
|----------|-------------|
| `new` |  |
| `connect_ack` |  |
| `connection_closed` |  |
| `next_valid_id` |  |
| `managed_accounts` |  |
| `error` |  |
| `current_time` |  |
| `tick_price` |  |
| `tick_size` |  |
| `tick_string` |  |
| `tick_generic` |  |
| `tick_snapshot_end` |  |
| `market_data_type` |  |
| `order_status` |  |
| `open_order` |  |
| `open_order_end` |  |
| `exec_details` |  |
| `exec_details_end` |  |
| `commission_report` |  |
| `update_account_value` |  |
| `update_portfolio` |  |
| `update_account_time` |  |
| `account_download_end` |  |
| `account_summary` |  |
| `account_summary_end` |  |
| `position` |  |
| `position_end` |  |
| `pnl` |  |
| `pnl_single` |  |
| `historical_data` |  |
| `historical_data_end` |  |
| `historical_data_update` |  |
| `head_timestamp` |  |
| `contract_details` |  |
| `contract_details_end` |  |
| `symbol_samples` |  |
| `tick_by_tick_all_last` |  |
| `tick_by_tick_bid_ask` |  |
| `tick_by_tick_mid_point` |  |
| `scanner_data` |  |
| `scanner_data_end` |  |
| `scanner_parameters` |  |
| `news_providers` |  |
| `news_article` |  |
| `historical_news` |  |
| `historical_news_end` |  |
| `tick_news` |  |
| `update_mkt_depth` |  |
| `update_mkt_depth_l2` |  |
| `mkt_depth_exchanges` |  |
| `real_time_bar` |  |
| `historical_ticks` |  |
| `historical_ticks_bid_ask` |  |
| `historical_ticks_last` |  |
| `tick_option_computation` |  |
| `security_definition_option_parameter` |  |
| `security_definition_option_parameter_end` |  |
| `fundamental_data` |  |
| `update_news_bulletin` |  |
| `receive_fa` |  |
| `replace_fa_end` |  |
| `position_multi` |  |
| `position_multi_end` |  |
| `account_update_multi` |  |
| `account_update_multi_end` |  |
| `display_group_list` |  |
| `display_group_updated` |  |
| `market_rule` |  |
| `smart_components` |  |
| `soft_dollar_tiers` |  |
| `family_codes` |  |
| `histogram_data` |  |
| `user_info` |  |
| `wsh_meta_data` |  |
| `wsh_event_data` |  |
| `completed_order` |  |
| `completed_orders_end` |  |
| `order_bound` |  |
| `tick_req_params` |  |
| `bond_contract_details` |  |
| `delta_neutral_validation` |  |
| `historical_schedule` |  |

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
