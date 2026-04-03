# Python API Reference (v0.4.4)

*Auto-generated from source — do not edit.*

## Table of Contents

- [EClient: Connection](#connection)
- [EClient: Account & Portfolio](#account--portfolio)
- [EClient: Orders](#orders)
- [EClient: Market Data](#market-data)
- [EClient: Reference Data](#reference-data)
- [EClient: Gateway-Local & Stubs](#gateway-local--stubs)
- [EWrapper Callbacks](#ewrapper-callbacks)

## Connection

#### `new`

Create a new EClient (or EWrapper) instance.

```python
def new(wrapper))
```

| Parameter | Type | Description |
|-----------|------|-------------|
| `wrapper` | `PyObject` | Wrapper callback receiver for synchronous delivery. |

---

#### `connect`

Connect to IB and start the engine.

```python
def connect(host="cdc1.ibllc.com".to_string(), port=0, client_id=0, username="".to_string(), password="".to_string(), paper=true, core_id=None))
```

| Parameter | Type | Description |
|-----------|------|-------------|
| `host` | `str` | Server hostname. |
| `port` | `int` | Port number (unused — ibx connects directly). |
| `client_id` | `int` | Client ID (unused — single-client engine). |
| `username` | `str` | Account username. |
| `password` | `str` | Account password. |
| `paper` | `bool` | If `true`, connect to paper trading. |
| `core_id` | `usize or None` | CPU core affinity for the hot loop thread. |

---

#### `disconnect`

Disconnect from IB.

```python
def disconnect()
```

---

#### `is_connected`

Check if connected.

```python
def is_connected()
```

---

#### `run`

Run the event loop.

```python
def run()
```

---

#### `get_account_id`

Get the account ID.

```python
def get_account_id()
```

---

## Account & Portfolio

#### `req_pnl`

Request P&L updates for the account.

```python
def req_pnl(req_id, account, model_code=""))
```

| Parameter | Type | Description |
|-----------|------|-------------|
| `req_id` | `int` | Request identifier. Used to match responses to requests. |
| `account` | `str` | Account ID. |
| `model_code` | `str` | Model portfolio code (empty for default). |

---

#### `cancel_pnl`

Cancel P&L subscription.

```python
def cancel_pnl(req_id)
```

| Parameter | Type | Description |
|-----------|------|-------------|
| `req_id` | `int` | Request identifier. Used to match responses to requests. |

---

#### `req_pnl_single`

Request P&L for a single position.

```python
def req_pnl_single(req_id, account, model_code, con_id))
```

| Parameter | Type | Description |
|-----------|------|-------------|
| `req_id` | `int` | Request identifier. Used to match responses to requests. |
| `account` | `str` | Account ID. |
| `model_code` | `str` | Model portfolio code (empty for default). |
| `con_id` | `int` | Contract ID. Unique per instrument. |

---

#### `cancel_pnl_single`

Cancel single-position P&L subscription.

```python
def cancel_pnl_single(req_id)
```

| Parameter | Type | Description |
|-----------|------|-------------|
| `req_id` | `int` | Request identifier. Used to match responses to requests. |

---

#### `req_account_summary`

Request account summary.

```python
def req_account_summary(req_id, group_name, tags))
```

| Parameter | Type | Description |
|-----------|------|-------------|
| `req_id` | `int` | Request identifier. Used to match responses to requests. |
| `group_name` | `str` | Account group name (e.g. `"All"`). |
| `tags` | `str` | Comma-separated account tags: `"NetLiquidation,BuyingPower,..."`. |

---

#### `cancel_account_summary`

Cancel account summary.

```python
def cancel_account_summary(req_id)
```

| Parameter | Type | Description |
|-----------|------|-------------|
| `req_id` | `int` | Request identifier. Used to match responses to requests. |

---

#### `req_positions`

Request all positions.

```python
def req_positions()
```

---

#### `cancel_positions`

Cancel positions.

```python
def cancel_positions()
```

---

#### `req_account_updates`

Request account updates.

```python
def req_account_updates(subscribe, _acct_code=""))
```

| Parameter | Type | Description |
|-----------|------|-------------|
| `subscribe` | `bool` | `true` to start updates, `false` to stop. |
| `acct_code` | `str` | Account code (e.g. `"DU1234567"`). |

---

#### `req_managed_accts`

Request managed accounts list.

```python
def req_managed_accts()
```

---

#### `req_account_updates_multi`

Request account updates for multiple accounts/models.

```python
def req_account_updates_multi(req_id, account, model_code, ledger_and_nlv=false))
```

| Parameter | Type | Description |
|-----------|------|-------------|
| `req_id` | `int` | Request identifier. Used to match responses to requests. |
| `account` | `str` | Account ID. |
| `model_code` | `str` | Model portfolio code (empty for default). |
| `ledger_and_nlv` | `bool` | If `true`, include ledger and NLV data. |

---

#### `cancel_account_updates_multi`

Cancel multi-account updates.

```python
def cancel_account_updates_multi(req_id)
```

| Parameter | Type | Description |
|-----------|------|-------------|
| `req_id` | `int` | Request identifier. Used to match responses to requests. |

---

#### `req_positions_multi`

Request positions across multiple accounts/models.

```python
def req_positions_multi(req_id, account, model_code))
```

| Parameter | Type | Description |
|-----------|------|-------------|
| `req_id` | `int` | Request identifier. Used to match responses to requests. |
| `account` | `str` | Account ID. |
| `model_code` | `str` | Model portfolio code (empty for default). |

---

#### `cancel_positions_multi`

Cancel multi-account positions.

```python
def cancel_positions_multi(req_id)
```

| Parameter | Type | Description |
|-----------|------|-------------|
| `req_id` | `int` | Request identifier. Used to match responses to requests. |

---

#### `account_snapshot`

Read account state snapshot. Returns a dict with all account values.

```python
def account_snapshot()
```

---

## Orders

#### `place_order`

Place an order.

```python
def place_order(order_id, contract, order)
```

| Parameter | Type | Description |
|-----------|------|-------------|
| `order_id` | `int` | Order identifier. Must be unique per session. |
| `contract` | `Contract` | Contract specification (symbol, secType, exchange, currency, etc.). |
| `order` | `Order` | Order parameters (action, quantity, type, price, TIF, etc.). |

---

#### `cancel_order`

Cancel an order.

```python
def cancel_order(order_id, manual_order_cancel_time=""))
```

| Parameter | Type | Description |
|-----------|------|-------------|
| `order_id` | `int` | Order identifier. Must be unique per session. |
| `manual_order_cancel_time` | `str` | Manual cancel time (empty for immediate). |

---

#### `req_global_cancel`

Cancel all orders globally.

```python
def req_global_cancel()
```

---

#### `req_ids`

Request next valid order ID.

```python
def req_ids(num_ids=1))
```

| Parameter | Type | Description |
|-----------|------|-------------|
| `num_ids` | `int` | Number of IDs to reserve (unused). |

---

#### `next_order_id`

Get the next order ID (local counter, auto-increments).

```python
def next_order_id()
```

---

#### `req_open_orders`

Request all open orders for this client.

```python
def req_open_orders()
```

---

#### `req_all_open_orders`

Request all open orders across all clients.

```python
def req_all_open_orders()
```

---

#### `req_auto_open_orders`

Automatically bind future orders to this client.

```python
def req_auto_open_orders(b_auto_bind))
```

| Parameter | Type | Description |
|-----------|------|-------------|
| `b_auto_bind` | `bool` | If `true`, auto-bind future orders to this client. |

---

#### `req_executions`

Request execution reports.

```python
def req_executions(req_id, exec_filter=None))
```

| Parameter | Type | Description |
|-----------|------|-------------|
| `req_id` | `int` | Request identifier. Used to match responses to requests. |
| `exec_filter` | `PyObject or None` |  |

---

#### `req_completed_orders`

Request completed orders.

```python
def req_completed_orders(api_only=false))
```

| Parameter | Type | Description |
|-----------|------|-------------|
| `api_only` | `bool` |  |

---

## Market Data

#### `set_news_providers`

Set news provider codes for per-contract news ticks (e.g. "BRFG*BRFUPDN").

```python
def set_news_providers(providers))
```

| Parameter | Type | Description |
|-----------|------|-------------|
| `providers` | `str` | News provider list. |

---

#### `req_mkt_data`

Request market data for a contract.

```python
def req_mkt_data(req_id, contract, generic_tick_list="", snapshot=false, regulatory_snapshot=false, mkt_data_options=Vec::new()))
```

| Parameter | Type | Description |
|-----------|------|-------------|
| `req_id` | `int` | Request identifier. Used to match responses to requests. |
| `contract` | `Contract` | Contract specification (symbol, secType, exchange, currency, etc.). |
| `generic_tick_list` | `str` | Comma-separated generic tick IDs (e.g. `"233"` for RT volume). |
| `snapshot` | `bool` | If `true`, delivers one quote then auto-cancels. |
| `regulatory_snapshot` | `bool` | If `true`, request a regulatory snapshot (additional fees may apply). |
| `mkt_data_options` | `list` |  |

---

#### `cancel_mkt_data`

Cancel market data subscription.

```python
def cancel_mkt_data(req_id)
```

| Parameter | Type | Description |
|-----------|------|-------------|
| `req_id` | `int` | Request identifier. Used to match responses to requests. |

---

#### `req_tick_by_tick_data`

Request tick-by-tick data.

```python
def req_tick_by_tick_data(req_id, contract, tick_type, number_of_ticks=0, ignore_size=false))
```

| Parameter | Type | Description |
|-----------|------|-------------|
| `req_id` | `int` | Request identifier. Used to match responses to requests. |
| `contract` | `Contract` | Contract specification (symbol, secType, exchange, currency, etc.). |
| `tick_type` | `str` | Tick type ID or tick-by-tick type string. |
| `number_of_ticks` | `int` | Maximum number of ticks to return. |
| `ignore_size` | `bool` | If `true`, ignore size in tick-by-tick data. |

---

#### `cancel_tick_by_tick_data`

Cancel tick-by-tick data.

```python
def cancel_tick_by_tick_data(req_id)
```

| Parameter | Type | Description |
|-----------|------|-------------|
| `req_id` | `int` | Request identifier. Used to match responses to requests. |

---

#### `req_market_data_type`

Set market data type (1=live, 2=frozen, 3=delayed, 4=delayed-frozen).

```python
def req_market_data_type(market_data_type)
```

| Parameter | Type | Description |
|-----------|------|-------------|
| `market_data_type` | `int` | 1=live, 2=frozen, 3=delayed, 4=delayed-frozen. |

---

#### `req_mkt_depth`

Request market depth (L2 order book).

```python
def req_mkt_depth(req_id, contract, num_rows=5, is_smart_depth=false, mkt_depth_options=Vec::new()))
```

| Parameter | Type | Description |
|-----------|------|-------------|
| `req_id` | `int` | Request identifier. Used to match responses to requests. |
| `contract` | `Contract` | Contract specification (symbol, secType, exchange, currency, etc.). |
| `num_rows` | `int` | Number of order book rows to subscribe to. |
| `is_smart_depth` | `bool` | If `true`, aggregate depth from multiple exchanges via SMART. |
| `mkt_depth_options` | `list` |  |

---

#### `cancel_mkt_depth`

Cancel market depth.

```python
def cancel_mkt_depth(req_id, is_smart_depth=false))
```

| Parameter | Type | Description |
|-----------|------|-------------|
| `req_id` | `int` | Request identifier. Used to match responses to requests. |
| `is_smart_depth` | `bool` | If `true`, aggregate depth from multiple exchanges via SMART. |

---

#### `req_real_time_bars`

Request real-time 5-second bars.

```python
def req_real_time_bars(req_id, contract, bar_size=5, what_to_show="TRADES", use_rth=0, real_time_bars_options=Vec::new()))
```

| Parameter | Type | Description |
|-----------|------|-------------|
| `req_id` | `int` | Request identifier. Used to match responses to requests. |
| `contract` | `Contract` | Contract specification (symbol, secType, exchange, currency, etc.). |
| `bar_size` | `int` | Bar size: `"1 min"`, `"5 mins"`, `"1 hour"`, `"1 day"`, etc. |
| `what_to_show` | `str` | Data type: `"TRADES"`, `"MIDPOINT"`, `"BID"`, `"ASK"`, `"BID_ASK"`, etc. |
| `use_rth` | `int` | If `true`, only return data from Regular Trading Hours. |
| `real_time_bars_options` | `list` |  |

---

#### `cancel_real_time_bars`

Cancel real-time bars.

```python
def cancel_real_time_bars(req_id)
```

| Parameter | Type | Description |
|-----------|------|-------------|
| `req_id` | `int` | Request identifier. Used to match responses to requests. |

---

#### `quote`

Zero-copy SeqLock quote read by req_id. Returns a dict with bid, ask, last, bid_size, ask_size, last_size, volume, high, low, open, close, or None if the req_id is not mapped.

```python
def quote(req_id)
```

| Parameter | Type | Description |
|-----------|------|-------------|
| `req_id` | `int` | Request identifier. Used to match responses to requests. |

---

#### `quote_by_instrument`

Zero-copy SeqLock quote read by InstrumentId. Returns a dict with bid, ask, last, bid_size, ask_size, last_size, volume, high, low, open, close, or None if not connected.

```python
def quote_by_instrument(instrument)
```

| Parameter | Type | Description |
|-----------|------|-------------|
| `instrument` | `int` | Instrument type for scanner (e.g. `"STK"`, `"FUT"`). |

---

## Reference Data

#### `req_historical_data`

Request historical bar data.

```python
def req_historical_data(req_id, contract, end_date_time, duration_str, bar_size_setting, what_to_show, use_rth, format_date=1, keep_up_to_date=false, chart_options=Vec::new()))
```

| Parameter | Type | Description |
|-----------|------|-------------|
| `req_id` | `int` | Request identifier. Used to match responses to requests. |
| `contract` | `Contract` | Contract specification (symbol, secType, exchange, currency, etc.). |
| `end_date_time` | `str` | End date/time in `"YYYYMMDD HH:MM:SS"` format, or empty for now. |
| `duration_str` | `str` | Duration string, e.g. `"1 D"`, `"1 W"`, `"1 M"`, `"1 Y"`. |
| `bar_size_setting` | `str` | Bar size: `"1 min"`, `"5 mins"`, `"1 hour"`, `"1 day"`, etc. |
| `what_to_show` | `str` | Data type: `"TRADES"`, `"MIDPOINT"`, `"BID"`, `"ASK"`, `"BID_ASK"`, etc. |
| `use_rth` | `int` | If `true`, only return data from Regular Trading Hours. |
| `format_date` | `int` | Date format: 1=`"YYYYMMDD HH:MM:SS"`, 2=Unix seconds. |
| `keep_up_to_date` | `bool` | If `true`, continue receiving updates after initial history. |
| `chart_options` | `list` |  |

---

#### `cancel_historical_data`

Cancel historical data.

```python
def cancel_historical_data(req_id)
```

| Parameter | Type | Description |
|-----------|------|-------------|
| `req_id` | `int` | Request identifier. Used to match responses to requests. |

---

#### `req_head_time_stamp`

Request head timestamp.

```python
def req_head_time_stamp(req_id, contract, what_to_show, use_rth, format_date=1))
```

| Parameter | Type | Description |
|-----------|------|-------------|
| `req_id` | `int` | Request identifier. Used to match responses to requests. |
| `contract` | `Contract` | Contract specification (symbol, secType, exchange, currency, etc.). |
| `what_to_show` | `str` | Data type: `"TRADES"`, `"MIDPOINT"`, `"BID"`, `"ASK"`, `"BID_ASK"`, etc. |
| `use_rth` | `int` | If `true`, only return data from Regular Trading Hours. |
| `format_date` | `int` | Date format: 1=`"YYYYMMDD HH:MM:SS"`, 2=Unix seconds. |

---

#### `cancel_head_time_stamp`

Cancel head timestamp request.

```python
def cancel_head_time_stamp(req_id)
```

| Parameter | Type | Description |
|-----------|------|-------------|
| `req_id` | `int` | Request identifier. Used to match responses to requests. |

---

#### `req_contract_details`

Request contract details.

```python
def req_contract_details(req_id, contract)
```

| Parameter | Type | Description |
|-----------|------|-------------|
| `req_id` | `int` | Request identifier. Used to match responses to requests. |
| `contract` | `Contract` | Contract specification (symbol, secType, exchange, currency, etc.). |

---

#### `req_mkt_depth_exchanges`

Request available exchanges for market depth.

```python
def req_mkt_depth_exchanges()
```

---

#### `req_matching_symbols`

Search for matching symbols.

```python
def req_matching_symbols(req_id, pattern)
```

| Parameter | Type | Description |
|-----------|------|-------------|
| `req_id` | `int` | Request identifier. Used to match responses to requests. |
| `pattern` | `str` | Symbol search pattern. |

---

#### `req_scanner_subscription`

Request scanner subscription.

```python
def req_scanner_subscription(req_id, subscription, scanner_subscription_options=Vec::new()))
```

| Parameter | Type | Description |
|-----------|------|-------------|
| `req_id` | `int` | Request identifier. Used to match responses to requests. |
| `subscription` | `PyObject` | Scanner subscription parameters. |
| `scanner_subscription_options` | `list` |  |

---

#### `cancel_scanner_subscription`

Cancel scanner subscription.

```python
def cancel_scanner_subscription(req_id)
```

| Parameter | Type | Description |
|-----------|------|-------------|
| `req_id` | `int` | Request identifier. Used to match responses to requests. |

---

#### `req_scanner_parameters`

Request scanner parameters XML.

```python
def req_scanner_parameters()
```

---

#### `req_news_article`

Request a news article.

```python
def req_news_article(req_id, provider_code, article_id, news_article_options=Vec::new()))
```

| Parameter | Type | Description |
|-----------|------|-------------|
| `req_id` | `int` | Request identifier. Used to match responses to requests. |
| `provider_code` | `str` | News provider code (e.g. `"BRFG"`). |
| `article_id` | `str` | News article identifier. |
| `news_article_options` | `list` |  |

---

#### `req_historical_news`

Request historical news.

```python
def req_historical_news(req_id, con_id, provider_codes, start_date_time, end_date_time, total_results, historical_news_options=Vec::new()))
```

| Parameter | Type | Description |
|-----------|------|-------------|
| `req_id` | `int` | Request identifier. Used to match responses to requests. |
| `con_id` | `int` | Contract ID. Unique per instrument. |
| `provider_codes` | `str` | Pipe-separated news provider codes. |
| `start_date_time` | `str` | Start date/time for tick query. |
| `end_date_time` | `str` | End date/time in `"YYYYMMDD HH:MM:SS"` format, or empty for now. |
| `total_results` | `int` | Maximum number of news results. |
| `historical_news_options` | `list` |  |

---

#### `req_fundamental_data`

Request fundamental data.

```python
def req_fundamental_data(req_id, contract, report_type, fundamental_data_options=Vec::new()))
```

| Parameter | Type | Description |
|-----------|------|-------------|
| `req_id` | `int` | Request identifier. Used to match responses to requests. |
| `contract` | `Contract` | Contract specification (symbol, secType, exchange, currency, etc.). |
| `report_type` | `str` | Report type: `"ReportSnapshot"`, `"ReportsFinSummary"`, `"RESC"`, etc. |
| `fundamental_data_options` | `list` |  |

---

#### `cancel_fundamental_data`

Cancel fundamental data.

```python
def cancel_fundamental_data(req_id)
```

| Parameter | Type | Description |
|-----------|------|-------------|
| `req_id` | `int` | Request identifier. Used to match responses to requests. |

---

#### `req_historical_ticks`

Request historical tick data.

```python
def req_historical_ticks(req_id, contract, start_date_time="", end_date_time="", number_of_ticks=1000, what_to_show="TRADES", use_rth=1, ignore_size=false, misc_options=Vec::new()))
```

| Parameter | Type | Description |
|-----------|------|-------------|
| `req_id` | `int` | Request identifier. Used to match responses to requests. |
| `contract` | `Contract` | Contract specification (symbol, secType, exchange, currency, etc.). |
| `start_date_time` | `str` | Start date/time for tick query. |
| `end_date_time` | `str` | End date/time in `"YYYYMMDD HH:MM:SS"` format, or empty for now. |
| `number_of_ticks` | `int` | Maximum number of ticks to return. |
| `what_to_show` | `str` | Data type: `"TRADES"`, `"MIDPOINT"`, `"BID"`, `"ASK"`, `"BID_ASK"`, etc. |
| `use_rth` | `int` | If `true`, only return data from Regular Trading Hours. |
| `ignore_size` | `bool` | If `true`, ignore size in tick-by-tick data. |
| `misc_options` | `list` |  |

---

#### `req_market_rule`

Request market rule details.

```python
def req_market_rule(market_rule_id)
```

| Parameter | Type | Description |
|-----------|------|-------------|
| `market_rule_id` | `int` | Market rule ID. |

---

#### `req_histogram_data`

Request histogram data.

```python
def req_histogram_data(req_id, contract, use_rth, time_period))
```

| Parameter | Type | Description |
|-----------|------|-------------|
| `req_id` | `int` | Request identifier. Used to match responses to requests. |
| `contract` | `Contract` | Contract specification (symbol, secType, exchange, currency, etc.). |
| `use_rth` | `bool` | If `true`, only return data from Regular Trading Hours. |
| `time_period` | `str` | Histogram time period. |

---

#### `cancel_histogram_data`

Cancel histogram data.

```python
def cancel_histogram_data(req_id)
```

| Parameter | Type | Description |
|-----------|------|-------------|
| `req_id` | `int` | Request identifier. Used to match responses to requests. |

---

#### `req_historical_schedule`

Request historical trading schedule.

```python
def req_historical_schedule(req_id, contract, end_date_time="", duration_str="1 M", use_rth=true))
```

| Parameter | Type | Description |
|-----------|------|-------------|
| `req_id` | `int` | Request identifier. Used to match responses to requests. |
| `contract` | `Contract` | Contract specification (symbol, secType, exchange, currency, etc.). |
| `end_date_time` | `str` | End date/time in `"YYYYMMDD HH:MM:SS"` format, or empty for now. |
| `duration_str` | `str` | Duration string, e.g. `"1 D"`, `"1 W"`, `"1 M"`, `"1 Y"`. |
| `use_rth` | `bool` | If `true`, only return data from Regular Trading Hours. |

---

## Gateway-Local & Stubs

#### `calculate_implied_volatility`

Calculate option implied volatility. Not yet implemented.

```python
def calculate_implied_volatility(req_id, contract, option_price, under_price, implied_vol_options=Vec::new()))
```

| Parameter | Type | Description |
|-----------|------|-------------|
| `req_id` | `int` | Request identifier. Used to match responses to requests. |
| `contract` | `Contract` | Contract specification (symbol, secType, exchange, currency, etc.). |
| `option_price` | `float` | Option market price. |
| `under_price` | `float` | Underlying asset price. |
| `implied_vol_options` | `list` |  |

---

#### `calculate_option_price`

Calculate option theoretical price. Not yet implemented.

```python
def calculate_option_price(req_id, contract, volatility, under_price, opt_prc_options=Vec::new()))
```

| Parameter | Type | Description |
|-----------|------|-------------|
| `req_id` | `int` | Request identifier. Used to match responses to requests. |
| `contract` | `Contract` | Contract specification (symbol, secType, exchange, currency, etc.). |
| `volatility` | `float` | Implied volatility. |
| `under_price` | `float` | Underlying asset price. |
| `opt_prc_options` | `list` |  |

---

#### `cancel_calculate_implied_volatility`

Cancel implied volatility calculation.

```python
def cancel_calculate_implied_volatility(req_id)
```

| Parameter | Type | Description |
|-----------|------|-------------|
| `req_id` | `int` | Request identifier. Used to match responses to requests. |

---

#### `cancel_calculate_option_price`

Cancel option price calculation.

```python
def cancel_calculate_option_price(req_id)
```

| Parameter | Type | Description |
|-----------|------|-------------|
| `req_id` | `int` | Request identifier. Used to match responses to requests. |

---

#### `exercise_options`

Exercise options. Not yet implemented.

```python
def exercise_options(req_id, contract, exercise_action, exercise_quantity, account, _override))
```

| Parameter | Type | Description |
|-----------|------|-------------|
| `req_id` | `int` | Request identifier. Used to match responses to requests. |
| `contract` | `Contract` | Contract specification (symbol, secType, exchange, currency, etc.). |
| `exercise_action` | `int` | 1=exercise, 2=lapse. |
| `exercise_quantity` | `int` | Number of contracts to exercise. |
| `account` | `str` | Account ID. |
| `override` | `int` | Override flag for exercise. |

---

#### `req_sec_def_opt_params`

Request option chain parameters. Not yet implemented.

```python
def req_sec_def_opt_params(req_id, underlying_symbol, fut_fop_exchange="", underlying_sec_type="STK", underlying_con_id=0))
```

| Parameter | Type | Description |
|-----------|------|-------------|
| `req_id` | `int` | Request identifier. Used to match responses to requests. |
| `underlying_symbol` | `str` | Underlying symbol (e.g. `"AAPL"`). |
| `fut_fop_exchange` | `str` | Exchange for futures/FOP options. |
| `underlying_sec_type` | `str` | Underlying security type (e.g. `"STK"`). |
| `underlying_con_id` | `int` | Underlying contract ID. |

---

#### `req_news_bulletins`

Subscribe to news bulletins.

```python
def req_news_bulletins(all_msgs=true))
```

| Parameter | Type | Description |
|-----------|------|-------------|
| `all_msgs` | `bool` | If `true`, receive all existing bulletins on subscribe. |

---

#### `cancel_news_bulletins`

Cancel news bulletin subscription.

```python
def cancel_news_bulletins()
```

---

#### `req_current_time`

Request current server time.

```python
def req_current_time()
```

---

#### `request_fa`

Request FA data. Not yet implemented.

```python
def request_fa(_fa_data_type)
```

| Parameter | Type | Description |
|-----------|------|-------------|
| `fa_data_type` | `int` | FA data type (1=Groups, 2=Profiles, 3=Aliases). |

---

#### `replace_fa`

Replace FA data. Not yet implemented.

```python
def replace_fa(req_id, fa_data_type, cxml))
```

| Parameter | Type | Description |
|-----------|------|-------------|
| `req_id` | `int` | Request identifier. Used to match responses to requests. |
| `fa_data_type` | `int` | FA data type (1=Groups, 2=Profiles, 3=Aliases). |
| `cxml` | `str` | FA XML configuration data. |

---

#### `query_display_groups`

Query display groups.

```python
def query_display_groups(req_id)
```

| Parameter | Type | Description |
|-----------|------|-------------|
| `req_id` | `int` | Request identifier. Used to match responses to requests. |

---

#### `subscribe_to_group_events`

Subscribe to display group events.

```python
def subscribe_to_group_events(req_id, group_id)
```

| Parameter | Type | Description |
|-----------|------|-------------|
| `req_id` | `int` | Request identifier. Used to match responses to requests. |
| `group_id` | `int` | Display group ID. |

---

#### `unsubscribe_from_group_events`

Unsubscribe from display group events.

```python
def unsubscribe_from_group_events(req_id)
```

| Parameter | Type | Description |
|-----------|------|-------------|
| `req_id` | `int` | Request identifier. Used to match responses to requests. |

---

#### `update_display_group`

Update display group.

```python
def update_display_group(req_id, contract_info)
```

| Parameter | Type | Description |
|-----------|------|-------------|
| `req_id` | `int` | Request identifier. Used to match responses to requests. |
| `contract_info` | `str` | Display group contract info string. |

---

#### `req_smart_components`

Request SMART routing component exchanges.

```python
def req_smart_components(req_id, bbo_exchange)
```

| Parameter | Type | Description |
|-----------|------|-------------|
| `req_id` | `int` | Request identifier. Used to match responses to requests. |
| `bbo_exchange` | `str` | BBO exchange for smart component lookup (e.g. `"SMART"`). |

---

#### `req_news_providers`

Request available news providers.

```python
def req_news_providers()
```

---

#### `req_soft_dollar_tiers`

Request soft dollar tiers.

```python
def req_soft_dollar_tiers(req_id)
```

| Parameter | Type | Description |
|-----------|------|-------------|
| `req_id` | `int` | Request identifier. Used to match responses to requests. |

---

#### `req_family_codes`

Request family codes.

```python
def req_family_codes()
```

---

#### `set_server_log_level`

Set server log level (1=error..5=trace).

```python
def set_server_log_level(log_level=2))
```

| Parameter | Type | Description |
|-----------|------|-------------|
| `log_level` | `int` | Log level: 1=error, 2=warn, 3=info, 4=debug, 5=trace. |

---

#### `req_user_info`

Request user info (white branding ID).

```python
def req_user_info(req_id)
```

| Parameter | Type | Description |
|-----------|------|-------------|
| `req_id` | `int` | Request identifier. Used to match responses to requests. |

---

#### `req_wsh_meta_data`

Request Wall Street Horizon metadata. Not yet implemented.

```python
def req_wsh_meta_data(req_id)
```

| Parameter | Type | Description |
|-----------|------|-------------|
| `req_id` | `int` | Request identifier. Used to match responses to requests. |

---

#### `req_wsh_event_data`

Request Wall Street Horizon event data. Not yet implemented.

```python
def req_wsh_event_data(req_id, wsh_event_data=None))
```

| Parameter | Type | Description |
|-----------|------|-------------|
| `req_id` | `int` | Request identifier. Used to match responses to requests. |
| `wsh_event_data` | `PyObject or None` |  |

---

## EWrapper Callbacks

#### `new`

Create a new EClient (or EWrapper) instance.

| Parameter | Type | Description |
|-----------|------|-------------|
| `args` | `Bound<'_, pyo3::types::PyTuple>` |  |
| `kwargs` | `Bound<'_, pyo3::types::PyDict> or None` |  |

---

#### `connect_ack`

Connection acknowledged.

---

#### `connection_closed`

Connection has been closed.

---

#### `next_valid_id`

Next valid order ID from the server.

| Parameter | Type | Description |
|-----------|------|-------------|
| `order_id` | `int` | Order identifier. Must be unique per session. |

---

#### `managed_accounts`

Comma-separated list of managed account IDs.

| Parameter | Type | Description |
|-----------|------|-------------|
| `accounts_list` | `str` | Comma-separated account IDs. |

---

#### `error`

Error or informational message from the server.

| Parameter | Type | Description |
|-----------|------|-------------|
| `req_id` | `int` | Request identifier. Used to match responses to requests. |
| `error_code` | `int` | Error code. |
| `error_string` | `str` | Error message. |
| `advanced_order_reject_json` | `str` | JSON with advanced rejection details. |

---

#### `current_time`

Current server time (Unix seconds).

| Parameter | Type | Description |
|-----------|------|-------------|
| `time` | `int` | Tick timestamp (Unix seconds). |

---

#### `tick_price`

Price tick update (bid, ask, last, etc.).

| Parameter | Type | Description |
|-----------|------|-------------|
| `req_id` | `int` | Request identifier. Used to match responses to requests. |
| `tick_type` | `int` | Tick type ID or tick-by-tick type string. |
| `price` | `float` | Tick price. |
| `attrib` | `PyObject` | Tick attributes. |

---

#### `tick_size`

Size tick update (bid size, ask size, volume, etc.).

| Parameter | Type | Description |
|-----------|------|-------------|
| `req_id` | `int` | Request identifier. Used to match responses to requests. |
| `tick_type` | `int` | Tick type ID or tick-by-tick type string. |
| `size` | `float` | Tick size. |

---

#### `tick_string`

String tick (e.g. last trade timestamp).

| Parameter | Type | Description |
|-----------|------|-------------|
| `req_id` | `int` | Request identifier. Used to match responses to requests. |
| `tick_type` | `int` | Tick type ID or tick-by-tick type string. |
| `value` | `str` | Account value. |

---

#### `tick_generic`

Generic numeric tick value.

| Parameter | Type | Description |
|-----------|------|-------------|
| `req_id` | `int` | Request identifier. Used to match responses to requests. |
| `tick_type` | `int` | Tick type ID or tick-by-tick type string. |
| `value` | `float` | Account value. |

---

#### `tick_snapshot_end`

Snapshot delivery complete; subscription auto-cancelled.

| Parameter | Type | Description |
|-----------|------|-------------|
| `req_id` | `int` | Request identifier. Used to match responses to requests. |

---

#### `market_data_type`

Market data type changed (1=live, 2=frozen, 3=delayed, 4=delayed-frozen).

| Parameter | Type | Description |
|-----------|------|-------------|
| `req_id` | `int` | Request identifier. Used to match responses to requests. |
| `market_data_type` | `int` | 1=live, 2=frozen, 3=delayed, 4=delayed-frozen. |

---

#### `order_status`

Order status update (filled, remaining, avg price, etc.).

| Parameter | Type | Description |
|-----------|------|-------------|
| `order_id` | `int` | Order identifier. Must be unique per session. |
| `status` | `str` | Order status string (`"Submitted"`, `"Filled"`, `"Cancelled"`, etc.). |
| `filled` | `float` | Cumulative filled quantity. |
| `remaining` | `float` | Remaining quantity. |
| `avg_fill_price` | `float` | Average fill price. |
| `perm_id` | `int` | Permanent order ID assigned by the server. |
| `parent_id` | `int` | Parent order ID (0 if no parent). |
| `last_fill_price` | `float` | Price of the last fill. |
| `client_id` | `int` | Client ID (unused — single-client engine). |
| `why_held` | `str` | Reason the order is held (e.g. `"locate"`). |
| `mkt_cap_price` | `float` | Market cap price for the order. |

---

#### `open_order`

Open order details (contract, order, state).

| Parameter | Type | Description |
|-----------|------|-------------|
| `order_id` | `int` | Order identifier. Must be unique per session. |
| `contract` | `PyObject` | Contract specification (symbol, secType, exchange, currency, etc.). |
| `order` | `PyObject` | Order parameters (action, quantity, type, price, TIF, etc.). |
| `order_state` | `PyObject` | Order state (status, margin, commission info). |

---

#### `open_order_end`

End of open orders list.

---

#### `exec_details`

Execution fill details.

| Parameter | Type | Description |
|-----------|------|-------------|
| `req_id` | `int` | Request identifier. Used to match responses to requests. |
| `contract` | `PyObject` | Contract specification (symbol, secType, exchange, currency, etc.). |
| `execution` | `PyObject` | Execution details (exec_id, time, price, shares, etc.). |

---

#### `exec_details_end`

End of execution details list.

| Parameter | Type | Description |
|-----------|------|-------------|
| `req_id` | `int` | Request identifier. Used to match responses to requests. |

---

#### `commission_report`

Commission report for an execution.

| Parameter | Type | Description |
|-----------|------|-------------|
| `commission_report` | `PyObject` |  |

---

#### `update_account_value`

Account value update (key/value/currency).

| Parameter | Type | Description |
|-----------|------|-------------|
| `key` | `str` | Account value key (e.g. `"NetLiquidation"`, `"BuyingPower"`). |
| `value` | `str` | Account value. |
| `currency` | `str` | Currency code (e.g. `"USD"`). |
| `account_name` | `str` | Account identifier. |

---

#### `update_portfolio`

Portfolio position update.

| Parameter | Type | Description |
|-----------|------|-------------|
| `contract` | `PyObject` | Contract specification (symbol, secType, exchange, currency, etc.). |
| `position` | `float` | Book position (row index) or position size. |
| `market_price` | `float` | Current market price. |
| `market_value` | `float` | Current market value of position. |
| `average_cost` | `float` | Average cost basis. |
| `unrealized_pnl` | `float` | Unrealized profit/loss. |
| `realized_pnl` | `float` | Realized profit/loss. |
| `account_name` | `str` | Account identifier. |

---

#### `update_account_time`

Account update timestamp.

| Parameter | Type | Description |
|-----------|------|-------------|
| `timestamp` | `str` | Timestamp string. |

---

#### `account_download_end`

Account data delivery complete.

| Parameter | Type | Description |
|-----------|------|-------------|
| `account` | `str` | Account ID. |

---

#### `account_summary`

Account summary tag/value entry.

| Parameter | Type | Description |
|-----------|------|-------------|
| `req_id` | `int` | Request identifier. Used to match responses to requests. |
| `account` | `str` | Account ID. |
| `tag` | `str` | Account tag name (e.g. `"NetLiquidation"`). |
| `value` | `str` | Account value. |
| `currency` | `str` | Currency code (e.g. `"USD"`). |

---

#### `account_summary_end`

End of account summary.

| Parameter | Type | Description |
|-----------|------|-------------|
| `req_id` | `int` | Request identifier. Used to match responses to requests. |

---

#### `position`

Position entry (account, contract, size, avg cost).

| Parameter | Type | Description |
|-----------|------|-------------|
| `account` | `str` | Account ID. |
| `contract` | `PyObject` | Contract specification (symbol, secType, exchange, currency, etc.). |
| `pos` | `float` | Position size (decimal shares). |
| `avg_cost` | `float` | Average cost per share. |

---

#### `position_end`

End of positions list.

---

#### `pnl`

Account P&L update (daily, unrealized, realized).

| Parameter | Type | Description |
|-----------|------|-------------|
| `req_id` | `int` | Request identifier. Used to match responses to requests. |
| `daily_pnl` | `float` | Daily profit/loss. |
| `unrealized_pnl` | `float` | Unrealized profit/loss. |
| `realized_pnl` | `float` | Realized profit/loss. |

---

#### `pnl_single`

Single-position P&L update.

| Parameter | Type | Description |
|-----------|------|-------------|
| `req_id` | `int` | Request identifier. Used to match responses to requests. |
| `pos` | `float` | Position size (decimal shares). |
| `daily_pnl` | `float` | Daily profit/loss. |
| `unrealized_pnl` | `float` | Unrealized profit/loss. |
| `realized_pnl` | `float` | Realized profit/loss. |
| `value` | `float` | Account value. |

---

#### `historical_data`

Historical OHLCV bar.

| Parameter | Type | Description |
|-----------|------|-------------|
| `req_id` | `int` | Request identifier. Used to match responses to requests. |
| `bar` | `PyObject` | Bar data (date, open, high, low, close, volume, wap, bar_count). |

---

#### `historical_data_end`

End of historical data delivery.

| Parameter | Type | Description |
|-----------|------|-------------|
| `req_id` | `int` | Request identifier. Used to match responses to requests. |
| `start` | `str` | Period start date/time. |
| `end` | `str` | Period end date/time. |

---

#### `historical_data_update`

Real-time bar update (keep_up_to_date=true).

| Parameter | Type | Description |
|-----------|------|-------------|
| `req_id` | `int` | Request identifier. Used to match responses to requests. |
| `bar` | `PyObject` | Bar data (date, open, high, low, close, volume, wap, bar_count). |

---

#### `head_timestamp`

Earliest available data timestamp.

| Parameter | Type | Description |
|-----------|------|-------------|
| `req_id` | `int` | Request identifier. Used to match responses to requests. |
| `head_timestamp` | `str` | Earliest available data timestamp string. |

---

#### `contract_details`

Contract definition details.

| Parameter | Type | Description |
|-----------|------|-------------|
| `req_id` | `int` | Request identifier. Used to match responses to requests. |
| `contract_details` | `PyObject` |  |

---

#### `contract_details_end`

End of contract details.

| Parameter | Type | Description |
|-----------|------|-------------|
| `req_id` | `int` | Request identifier. Used to match responses to requests. |

---

#### `symbol_samples`

Matching symbol search results.

| Parameter | Type | Description |
|-----------|------|-------------|
| `req_id` | `int` | Request identifier. Used to match responses to requests. |
| `contract_descriptions` | `PyObject` |  |

---

#### `tick_by_tick_all_last`

Tick-by-tick last trade.

| Parameter | Type | Description |
|-----------|------|-------------|
| `req_id` | `int` | Request identifier. Used to match responses to requests. |
| `tick_type` | `int` | Tick type ID or tick-by-tick type string. |
| `time` | `int` | Tick timestamp (Unix seconds). |
| `price` | `float` | Tick price. |
| `size` | `float` | Tick size. |
| `tick_attrib_last` | `PyObject` |  |
| `exchange` | `str` | Exchange name. |
| `special_conditions` | `str` | Special trade conditions. |

---

#### `tick_by_tick_bid_ask`

Tick-by-tick bid/ask quote.

| Parameter | Type | Description |
|-----------|------|-------------|
| `req_id` | `int` | Request identifier. Used to match responses to requests. |
| `time` | `int` | Tick timestamp (Unix seconds). |
| `bid_price` | `float` | Bid price. |
| `ask_price` | `float` | Ask price. |
| `bid_size` | `float` | Bid size. |
| `ask_size` | `float` | Ask size. |
| `tick_attrib_bid_ask` | `PyObject` |  |

---

#### `tick_by_tick_mid_point`

Tick-by-tick midpoint.

| Parameter | Type | Description |
|-----------|------|-------------|
| `req_id` | `int` | Request identifier. Used to match responses to requests. |
| `time` | `int` | Tick timestamp (Unix seconds). |
| `mid_point` | `float` | Midpoint price. |

---

#### `scanner_data`

Scanner result entry (rank, contract, distance).

| Parameter | Type | Description |
|-----------|------|-------------|
| `req_id` | `int` | Request identifier. Used to match responses to requests. |
| `rank` | `int` | Scanner result rank (0-based). |
| `contract_details` | `PyObject` |  |
| `distance` | `str` | Scanner distance metric. |
| `benchmark` | `str` | Scanner benchmark. |
| `projection` | `str` | Scanner projection. |
| `legs_str` | `str` | Combo legs description. |

---

#### `scanner_data_end`

End of scanner results.

| Parameter | Type | Description |
|-----------|------|-------------|
| `req_id` | `int` | Request identifier. Used to match responses to requests. |

---

#### `scanner_parameters`

Scanner parameters XML.

| Parameter | Type | Description |
|-----------|------|-------------|
| `xml` | `str` | XML string. |

---

#### `news_providers`

Available news providers list.

| Parameter | Type | Description |
|-----------|------|-------------|
| `news_providers` | `PyObject` |  |

---

#### `news_article`

Full news article text.

| Parameter | Type | Description |
|-----------|------|-------------|
| `req_id` | `int` | Request identifier. Used to match responses to requests. |
| `article_type` | `int` | Article type: 0=plain text, 1=HTML. |
| `article_text` | `str` | Full article body. |

---

#### `historical_news`

Historical news headline.

| Parameter | Type | Description |
|-----------|------|-------------|
| `req_id` | `int` | Request identifier. Used to match responses to requests. |
| `time` | `str` | Tick timestamp (Unix seconds). |
| `provider_code` | `str` | News provider code (e.g. `"BRFG"`). |
| `article_id` | `str` | News article identifier. |
| `headline` | `str` | News headline text. |

---

#### `historical_news_end`

End of historical news.

| Parameter | Type | Description |
|-----------|------|-------------|
| `req_id` | `int` | Request identifier. Used to match responses to requests. |
| `has_more` | `bool` | If `true`, more results available. |

---

#### `tick_news`

Per-contract news tick.

| Parameter | Type | Description |
|-----------|------|-------------|
| `ticker_id` | `int` | Ticker/request ID. |
| `time_stamp` | `int` | Timestamp string. |
| `provider_code` | `str` | News provider code (e.g. `"BRFG"`). |
| `article_id` | `str` | News article identifier. |
| `headline` | `str` | News headline text. |
| `extra_data` | `str` | Additional tick data. |

---

#### `update_mkt_depth`

L2 book update (single exchange).

| Parameter | Type | Description |
|-----------|------|-------------|
| `req_id` | `int` | Request identifier. Used to match responses to requests. |
| `position` | `int` | Book position (row index) or position size. |
| `operation` | `int` | Book operation: 0=insert, 1=update, 2=delete. |
| `side` | `int` | Book side: 0=ask, 1=bid. Or order side `"BOT"`/`"SLD"`. |
| `price` | `float` | Tick price. |
| `size` | `float` | Tick size. |

---

#### `update_mkt_depth_l2`

L2 book update (with market maker).

| Parameter | Type | Description |
|-----------|------|-------------|
| `req_id` | `int` | Request identifier. Used to match responses to requests. |
| `position` | `int` | Book position (row index) or position size. |
| `market_maker` | `str` | Market maker ID. |
| `operation` | `int` | Book operation: 0=insert, 1=update, 2=delete. |
| `side` | `int` | Book side: 0=ask, 1=bid. Or order side `"BOT"`/`"SLD"`. |
| `price` | `float` | Tick price. |
| `size` | `float` | Tick size. |
| `is_smart_depth` | `bool` | If `true`, aggregate depth from multiple exchanges via SMART. |

---

#### `mkt_depth_exchanges`

Available exchanges for market depth.

| Parameter | Type | Description |
|-----------|------|-------------|
| `depth_mkt_data_descriptions` | `PyObject` |  |

---

#### `real_time_bar`

Real-time 5-second OHLCV bar.

| Parameter | Type | Description |
|-----------|------|-------------|
| `req_id` | `int` | Request identifier. Used to match responses to requests. |
| `date` | `int` | Bar date string. |
| `open` | `float` | Open price. |
| `high` | `float` | High price. |
| `low` | `float` | Low price. |
| `close` | `float` | Close price. |
| `volume` | `float` | Volume. |
| `wap` | `float` | Volume-weighted average price. |
| `count` | `int` | Trade count. |

---

#### `historical_ticks`

Historical tick data (Last, BidAsk, or Midpoint).

| Parameter | Type | Description |
|-----------|------|-------------|
| `req_id` | `int` | Request identifier. Used to match responses to requests. |
| `ticks` | `PyObject` | Historical tick data. |
| `done` | `bool` | If `true`, all ticks have been delivered. |

---

#### `historical_ticks_bid_ask`

Historical bid/ask ticks.

| Parameter | Type | Description |
|-----------|------|-------------|
| `req_id` | `int` | Request identifier. Used to match responses to requests. |
| `ticks` | `PyObject` | Historical tick data. |
| `done` | `bool` | If `true`, all ticks have been delivered. |

---

#### `historical_ticks_last`

Historical last-trade ticks.

| Parameter | Type | Description |
|-----------|------|-------------|
| `req_id` | `int` | Request identifier. Used to match responses to requests. |
| `ticks` | `PyObject` | Historical tick data. |
| `done` | `bool` | If `true`, all ticks have been delivered. |

---

#### `tick_option_computation`

Option implied vol / greeks computation.

| Parameter | Type | Description |
|-----------|------|-------------|
| `req_id` | `int` | Request identifier. Used to match responses to requests. |
| `tick_type` | `int` | Tick type ID or tick-by-tick type string. |
| `tick_attrib` | `int` |  |
| `implied_vol` | `float` | Implied volatility. |
| `delta` | `float` | Option delta. |
| `opt_price` | `float` | Option theoretical price. |
| `pv_dividend` | `float` | Present value of dividends. |
| `gamma` | `float` | Option gamma. |
| `vega` | `float` | Option vega. |
| `theta` | `float` | Option theta. |
| `und_price` | `float` | Underlying price. |

---

#### `security_definition_option_parameter`

Option chain parameters (strikes, expirations).

| Parameter | Type | Description |
|-----------|------|-------------|
| `req_id` | `int` | Request identifier. Used to match responses to requests. |
| `exchange` | `str` | Exchange name. |
| `underlying_con_id` | `int` | Underlying contract ID. |
| `trading_class` | `str` | Trading class. |
| `multiplier` | `str` | Contract multiplier. |
| `expirations` | `PyObject` | Available expiration dates. |
| `strikes` | `PyObject` | Available strike prices. |

---

#### `security_definition_option_parameter_end`

End of option chain parameters.

| Parameter | Type | Description |
|-----------|------|-------------|
| `req_id` | `int` | Request identifier. Used to match responses to requests. |

---

#### `fundamental_data`

Fundamental data (XML/JSON).

| Parameter | Type | Description |
|-----------|------|-------------|
| `req_id` | `int` | Request identifier. Used to match responses to requests. |
| `data` | `str` | Raw data string (XML/JSON). |

---

#### `update_news_bulletin`

News bulletin message.

| Parameter | Type | Description |
|-----------|------|-------------|
| `msg_id` | `int` | Bulletin message ID. |
| `msg_type` | `int` | Bulletin message type (1=regular, 2=exchange). |
| `message` | `str` | Bulletin message text. |
| `orig_exchange` | `str` | Originating exchange. |

---

#### `receive_fa`

Financial advisor data received.

| Parameter | Type | Description |
|-----------|------|-------------|
| `fa_data_type` | `int` | FA data type (1=Groups, 2=Profiles, 3=Aliases). |
| `xml` | `str` | XML string. |

---

#### `replace_fa_end`

Financial advisor replace complete.

| Parameter | Type | Description |
|-----------|------|-------------|
| `req_id` | `int` | Request identifier. Used to match responses to requests. |
| `text` | `str` | Informational text. |

---

#### `position_multi`

Multi-account position entry.

| Parameter | Type | Description |
|-----------|------|-------------|
| `req_id` | `int` | Request identifier. Used to match responses to requests. |
| `account` | `str` | Account ID. |
| `model_code` | `str` | Model portfolio code (empty for default). |
| `contract` | `PyObject` | Contract specification (symbol, secType, exchange, currency, etc.). |
| `pos` | `float` | Position size (decimal shares). |
| `avg_cost` | `float` | Average cost per share. |

---

#### `position_multi_end`

End of multi-account positions.

| Parameter | Type | Description |
|-----------|------|-------------|
| `req_id` | `int` | Request identifier. Used to match responses to requests. |

---

#### `account_update_multi`

Multi-account value update.

| Parameter | Type | Description |
|-----------|------|-------------|
| `req_id` | `int` | Request identifier. Used to match responses to requests. |
| `account` | `str` | Account ID. |
| `model_code` | `str` | Model portfolio code (empty for default). |
| `key` | `str` | Account value key (e.g. `"NetLiquidation"`, `"BuyingPower"`). |
| `value` | `str` | Account value. |
| `currency` | `str` | Currency code (e.g. `"USD"`). |

---

#### `account_update_multi_end`

End of multi-account updates.

| Parameter | Type | Description |
|-----------|------|-------------|
| `req_id` | `int` | Request identifier. Used to match responses to requests. |

---

#### `display_group_list`

Display group list.

| Parameter | Type | Description |
|-----------|------|-------------|
| `req_id` | `int` | Request identifier. Used to match responses to requests. |
| `groups` | `str` | FA group definitions. |

---

#### `display_group_updated`

Display group updated.

| Parameter | Type | Description |
|-----------|------|-------------|
| `req_id` | `int` | Request identifier. Used to match responses to requests. |
| `contract_info` | `str` | Display group contract info string. |

---

#### `market_rule`

Market rule: price increment schedule.

| Parameter | Type | Description |
|-----------|------|-------------|
| `market_rule_id` | `int` | Market rule ID. |
| `price_increments` | `PyObject` | Price increment rules `[{low_edge, increment}]`. |

---

#### `smart_components`

SMART routing component exchanges.

| Parameter | Type | Description |
|-----------|------|-------------|
| `req_id` | `int` | Request identifier. Used to match responses to requests. |
| `smart_component_map` | `PyObject` |  |

---

#### `soft_dollar_tiers`

Soft dollar tier list.

| Parameter | Type | Description |
|-----------|------|-------------|
| `req_id` | `int` | Request identifier. Used to match responses to requests. |
| `tiers` | `PyObject` | Soft dollar tier list. |

---

#### `family_codes`

Family codes linking related accounts.

| Parameter | Type | Description |
|-----------|------|-------------|
| `family_codes` | `PyObject` |  |

---

#### `histogram_data`

Price distribution histogram.

| Parameter | Type | Description |
|-----------|------|-------------|
| `req_id` | `int` | Request identifier. Used to match responses to requests. |
| `items` | `PyObject` | Histogram entries `[(price, count)]`. |

---

#### `user_info`

User info (white branding ID).

| Parameter | Type | Description |
|-----------|------|-------------|
| `req_id` | `int` | Request identifier. Used to match responses to requests. |
| `white_branding_id` | `str` | White branding ID (empty for standard accounts). |

---

#### `wsh_meta_data`

Wall Street Horizon metadata.

| Parameter | Type | Description |
|-----------|------|-------------|
| `req_id` | `int` | Request identifier. Used to match responses to requests. |
| `data_json` | `str` |  |

---

#### `wsh_event_data`

Wall Street Horizon event data.

| Parameter | Type | Description |
|-----------|------|-------------|
| `req_id` | `int` | Request identifier. Used to match responses to requests. |
| `data_json` | `str` |  |

---

#### `completed_order`

Completed (filled/cancelled) order details.

| Parameter | Type | Description |
|-----------|------|-------------|
| `contract` | `PyObject` | Contract specification (symbol, secType, exchange, currency, etc.). |
| `order` | `PyObject` | Order parameters (action, quantity, type, price, TIF, etc.). |
| `order_state` | `PyObject` | Order state (status, margin, commission info). |

---

#### `completed_orders_end`

End of completed orders list.

---

#### `order_bound`

Order bound to a perm ID.

| Parameter | Type | Description |
|-----------|------|-------------|
| `order_id` | `int` | Order identifier. Must be unique per session. |
| `api_client_id` | `int` |  |
| `api_order_id` | `int` |  |

---

#### `tick_req_params`

Tick parameters: min tick size, BBO exchange, snapshot permissions.

| Parameter | Type | Description |
|-----------|------|-------------|
| `ticker_id` | `int` | Ticker/request ID. |
| `min_tick` | `float` | Minimum tick size. |
| `bbo_exchange` | `str` | BBO exchange for smart component lookup (e.g. `"SMART"`). |
| `snapshot_permissions` | `int` | Snapshot permissions bitmask. |

---

#### `bond_contract_details`

Bond contract details.

| Parameter | Type | Description |
|-----------|------|-------------|
| `req_id` | `int` | Request identifier. Used to match responses to requests. |
| `contract_details` | `PyObject` |  |

---

#### `delta_neutral_validation`

Delta-neutral validation response.

| Parameter | Type | Description |
|-----------|------|-------------|
| `req_id` | `int` | Request identifier. Used to match responses to requests. |
| `delta_neutral_contract` | `PyObject` |  |

---

#### `historical_schedule`

Historical trading schedule (exchange hours).

| Parameter | Type | Description |
|-----------|------|-------------|
| `req_id` | `int` | Request identifier. Used to match responses to requests. |
| `start_date_time` | `str` | Start date/time for tick query. |
| `end_date_time` | `str` | End date/time in `"YYYYMMDD HH:MM:SS"` format, or empty for now. |
| `time_zone` | `str` | Timezone string (e.g. `"US/Eastern"`). |
| `sessions` | `PyObject` | Trading sessions `[(ref_date, open, close)]`. |

---
