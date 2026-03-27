# Rust API Reference (v0.4.2)

*Auto-generated from source — do not edit.*

## Table of Contents

- [EClient: Connection](#connection)
- [EClient: Account & Portfolio](#account--portfolio)
- [EClient: Orders](#orders)
- [EClient: Market Data](#market-data)
- [EClient: Reference Data](#reference-data)
- [EClient: Gateway-Local & Stubs](#gateway-local--stubs)
- [Wrapper Callbacks](#wrapper-callbacks)

## Connection

#### `connect`

Connect to IB and start the engine.

```rust
pub fn connect(config: &EClientConfig) -> Result<Self, Box<dyn std::error::Error>>
```

| Parameter | Type | Description |
|-----------|------|-------------|
| `config` | `&EClientConfig` | Connection configuration (username, password, host, paper, core_id). |

**Returns:** `Result<Self, Box<dyn std::error::Error>>`

---

#### `from_parts`

Construct from pre-built components (for testing or custom setups).

```rust
pub fn from_parts( shared: Arc<SharedState>, control_tx: Sender<ControlCommand>, handle: thread::JoinHandle<()>, account_id: String, ) -> Self
```

| Parameter | Type | Description |
|-----------|------|-------------|
| `shared` | `Arc<SharedState>` | Shared state handle. |
| `control_tx` | `Sender<ControlCommand>` | Control channel sender. |
| `handle` | `thread::JoinHandle<(` | Background thread handle. |

**Returns:** `Self`

---

#### `map_req_instrument`

Map a reqId to an InstrumentId (for testing without a live engine).

```rust
pub fn map_req_instrument(&self, req_id: i64, instrument: InstrumentId)
```

| Parameter | Type | Description |
|-----------|------|-------------|
| `req_id` | `i64` | Request identifier. Used to match responses to requests. |
| `instrument` | `InstrumentId` | Instrument type for scanner (e.g. `"STK"`, `"FUT"`). |

---

#### `seed_instrument`

Pre-seed a con_id → InstrumentId mapping (for testing without a live engine).

```rust
pub fn seed_instrument(&self, con_id: i64, instrument: InstrumentId)
```

| Parameter | Type | Description |
|-----------|------|-------------|
| `con_id` | `i64` | Contract ID. Unique per instrument. |
| `instrument` | `InstrumentId` | Instrument type for scanner (e.g. `"STK"`, `"FUT"`). |

---

#### `is_connected`

Check if the client is connected.

```rust
pub fn is_connected(&self) -> bool
```

**Returns:** `bool`

---

#### `disconnect`

Disconnect from IB.  Sends `Shutdown` to the hot loop, waits for the background thread to exit, and marks the client as disconnected.

```rust
pub fn disconnect(&self)
```

---

## Account & Portfolio

#### `req_positions`

Request positions. Immediately delivers all positions via wrapper callbacks, then calls position_end.

```rust
pub fn req_positions(&self, wrapper: &mut impl Wrapper)
```

| Parameter | Type | Description |
|-----------|------|-------------|
| `wrapper` | `&mut impl Wrapper` | Wrapper callback receiver for synchronous delivery. |

---

#### `req_pnl`

Subscribe to account PnL updates.

```rust
pub fn req_pnl(&self, req_id: i64, _account: &str, _model_code: &str)
```

| Parameter | Type | Description |
|-----------|------|-------------|
| `req_id` | `i64` | Request identifier. Used to match responses to requests. |
| `account` | `&str` | Account ID. |
| `model_code` | `&str` | Model portfolio code (empty for default). |

---

#### `cancel_pnl`

Cancel PnL subscription.

```rust
pub fn cancel_pnl(&self, req_id: i64)
```

| Parameter | Type | Description |
|-----------|------|-------------|
| `req_id` | `i64` | Request identifier. Used to match responses to requests. |

---

#### `req_pnl_single`

Subscribe to single-position PnL updates.

```rust
pub fn req_pnl_single(&self, req_id: i64, _account: &str, _model_code: &str, con_id: i64)
```

| Parameter | Type | Description |
|-----------|------|-------------|
| `req_id` | `i64` | Request identifier. Used to match responses to requests. |
| `account` | `&str` | Account ID. |
| `model_code` | `&str` | Model portfolio code (empty for default). |
| `con_id` | `i64` | Contract ID. Unique per instrument. |

---

#### `cancel_pnl_single`

Cancel single-position PnL subscription.

```rust
pub fn cancel_pnl_single(&self, req_id: i64)
```

| Parameter | Type | Description |
|-----------|------|-------------|
| `req_id` | `i64` | Request identifier. Used to match responses to requests. |

---

#### `req_account_summary`

Request account summary.

```rust
pub fn req_account_summary(&self, req_id: i64, _group: &str, tags: &str)
```

| Parameter | Type | Description |
|-----------|------|-------------|
| `req_id` | `i64` | Request identifier. Used to match responses to requests. |
| `group` | `&str` | Account group name (e.g. `"All"`). |
| `tags` | `&str` | Comma-separated account tags: `"NetLiquidation,BuyingPower,..."`. |

---

#### `cancel_account_summary`

Cancel account summary.

```rust
pub fn cancel_account_summary(&self, req_id: i64)
```

| Parameter | Type | Description |
|-----------|------|-------------|
| `req_id` | `i64` | Request identifier. Used to match responses to requests. |

---

#### `req_account_updates`

Subscribe to account updates.

```rust
pub fn req_account_updates(&self, subscribe: bool, _acct_code: &str)
```

| Parameter | Type | Description |
|-----------|------|-------------|
| `subscribe` | `bool` | `true` to start updates, `false` to stop. |
| `acct_code` | `&str` | Account code (e.g. `"DU1234567"`). |

---

#### `cancel_positions`

Cancel positions subscription.

```rust
pub fn cancel_positions(&self)
```

---

#### `req_managed_accts`

Request managed accounts.

```rust
pub fn req_managed_accts(&self, wrapper: &mut impl Wrapper)
```

| Parameter | Type | Description |
|-----------|------|-------------|
| `wrapper` | `&mut impl Wrapper` | Wrapper callback receiver for synchronous delivery. |

---

#### `req_account_updates_multi`

Request account updates for multiple accounts/models.

```rust
pub fn req_account_updates_multi( &self, _req_id: i64, _account: &str, _model_code: &str, _ledger_and_nlv: bool, wrapper: &mut impl Wrapper, )
```

| Parameter | Type | Description |
|-----------|------|-------------|
| `req_id` | `i64` | Request identifier. Used to match responses to requests. |
| `account` | `&str` | Account ID. |
| `model_code` | `&str` | Model portfolio code (empty for default). |
| `ledger_and_nlv` | `bool` | If `true`, include ledger and NLV data. |
| `wrapper` | `&mut impl Wrapper` | Wrapper callback receiver for synchronous delivery. |

---

#### `cancel_account_updates_multi`

Cancel multi-account updates.

```rust
pub fn cancel_account_updates_multi(&self, _req_id: i64)
```

| Parameter | Type | Description |
|-----------|------|-------------|
| `req_id` | `i64` | Request identifier. Used to match responses to requests. |

---

#### `req_positions_multi`

Request positions for multiple accounts/models.

```rust
pub fn req_positions_multi( &self, _req_id: i64, _account: &str, _model_code: &str, wrapper: &mut impl Wrapper, )
```

| Parameter | Type | Description |
|-----------|------|-------------|
| `req_id` | `i64` | Request identifier. Used to match responses to requests. |
| `account` | `&str` | Account ID. |
| `model_code` | `&str` | Model portfolio code (empty for default). |
| `wrapper` | `&mut impl Wrapper` | Wrapper callback receiver for synchronous delivery. |

---

#### `cancel_positions_multi`

Cancel multi-account positions.

```rust
pub fn cancel_positions_multi(&self, _req_id: i64)
```

| Parameter | Type | Description |
|-----------|------|-------------|
| `req_id` | `i64` | Request identifier. Used to match responses to requests. |

---

#### `account`

Read account state snapshot.

```rust
pub fn account(&self) -> AccountState
```

**Returns:** `AccountState`

---

## Orders

#### `place_order`

Place an order.

```rust
pub fn place_order(&self, order_id: i64, contract: &Contract, order: &Order) -> Result<(), String>
```

| Parameter | Type | Description |
|-----------|------|-------------|
| `order_id` | `i64` | Order identifier. Must be unique per session. |
| `contract` | `&Contract` | Contract specification (symbol, secType, exchange, currency, etc.). |
| `order` | `&Order` | Order parameters (action, quantity, type, price, TIF, etc.). |

**Returns:** `Result<(), String>`

---

#### `cancel_order`

Cancel an order.

```rust
pub fn cancel_order(&self, order_id: i64, _manual_order_cancel_time: &str) -> Result<(), String>
```

| Parameter | Type | Description |
|-----------|------|-------------|
| `order_id` | `i64` | Order identifier. Must be unique per session. |
| `manual_order_cancel_time` | `&str` | Manual cancel time (empty for immediate). |

**Returns:** `Result<(), String>`

---

#### `req_global_cancel`

Cancel all orders.

```rust
pub fn req_global_cancel(&self) -> Result<(), String>
```

**Returns:** `Result<(), String>`

---

#### `req_ids`

Request next valid order ID.

```rust
pub fn req_ids(&self, wrapper: &mut impl Wrapper)
```

| Parameter | Type | Description |
|-----------|------|-------------|
| `wrapper` | `&mut impl Wrapper` | Wrapper callback receiver for synchronous delivery. |

---

#### `next_order_id`

Get the next order ID (local counter).

```rust
pub fn next_order_id(&self) -> i64
```

**Returns:** `i64`

---

#### `req_open_orders`

Request open orders for this client.

```rust
pub fn req_open_orders(&self, wrapper: &mut impl Wrapper)
```

| Parameter | Type | Description |
|-----------|------|-------------|
| `wrapper` | `&mut impl Wrapper` | Wrapper callback receiver for synchronous delivery. |

---

#### `req_all_open_orders`

Request all open orders.

```rust
pub fn req_all_open_orders(&self, wrapper: &mut impl Wrapper)
```

| Parameter | Type | Description |
|-----------|------|-------------|
| `wrapper` | `&mut impl Wrapper` | Wrapper callback receiver for synchronous delivery. |

---

#### `req_completed_orders`

Request completed orders. Immediately delivers all archived completed orders, then calls `completed_orders_end`.

```rust
pub fn req_completed_orders(&self, wrapper: &mut impl Wrapper)
```

| Parameter | Type | Description |
|-----------|------|-------------|
| `wrapper` | `&mut impl Wrapper` | Wrapper callback receiver for synchronous delivery. |

---

#### `req_auto_open_orders`

Automatically bind future orders to this client.

```rust
pub fn req_auto_open_orders(&self, _b_auto_bind: bool)
```

| Parameter | Type | Description |
|-----------|------|-------------|
| `b_auto_bind` | `bool` | If `true`, auto-bind future orders to this client. |

---

#### `req_executions`

Request execution reports. Replays stored executions (optionally filtered), firing `exec_details` + `commission_report` for each, then `exec_details_end`.

```rust
pub fn req_executions(&self, req_id: i64, filter: &ExecutionFilter, wrapper: &mut impl Wrapper)
```

| Parameter | Type | Description |
|-----------|------|-------------|
| `req_id` | `i64` | Request identifier. Used to match responses to requests. |
| `filter` | `&ExecutionFilter` | Execution filter (client_id, acct_code, time, symbol, sec_type, exchange, side). |
| `wrapper` | `&mut impl Wrapper` | Wrapper callback receiver for synchronous delivery. |

---

#### `parse_algo_params`

Parse algo strategy and TagValue params into internal AlgoParams.

```rust
pub fn parse_algo_params(strategy: &str, params: &[TagValue]) -> Result<AlgoParams, String>
```

| Parameter | Type | Description |
|-----------|------|-------------|
| `strategy` | `&str` | Algo strategy name (e.g. `"Vwap"`, `"Twap"`). |
| `params` | `&[TagValue]` | Algo parameter list. |

**Returns:** `Result<AlgoParams, String>`

---

## Market Data

#### `req_mkt_data`

Subscribe to market data. When `snapshot` is true, delivers the first available quote then calls `tick_snapshot_end` and auto-cancels the subscription.

```rust
pub fn req_mkt_data( &self, req_id: i64, contract: &Contract, generic_tick_list: &str, snapshot: bool, _regulatory_snapshot: bool, ) -> Result<(), String>
```

| Parameter | Type | Description |
|-----------|------|-------------|
| `req_id` | `i64` | Request identifier. Used to match responses to requests. |
| `contract` | `&Contract` | Contract specification (symbol, secType, exchange, currency, etc.). |
| `generic_tick_list` | `&str` | Comma-separated generic tick IDs (e.g. `"233"` for RT volume). |
| `snapshot` | `bool` | If `true`, delivers one quote then auto-cancels. |
| `regulatory_snapshot` | `bool` | If `true`, request a regulatory snapshot (additional fees may apply). |

**Returns:** `Result<(), String>`

---

#### `cancel_mkt_data`

Cancel market data.

```rust
pub fn cancel_mkt_data(&self, req_id: i64) -> Result<(), String>
```

| Parameter | Type | Description |
|-----------|------|-------------|
| `req_id` | `i64` | Request identifier. Used to match responses to requests. |

**Returns:** `Result<(), String>`

---

#### `req_tick_by_tick_data`

Subscribe to tick-by-tick data.

```rust
pub fn req_tick_by_tick_data( &self, req_id: i64, contract: &Contract, tick_type: &str, _number_of_ticks: i32, _ignore_size: bool, ) -> Result<(), String>
```

| Parameter | Type | Description |
|-----------|------|-------------|
| `req_id` | `i64` | Request identifier. Used to match responses to requests. |
| `contract` | `&Contract` | Contract specification (symbol, secType, exchange, currency, etc.). |
| `tick_type` | `&str` | Tick type ID or tick-by-tick type string. |
| `number_of_ticks` | `i32` | Maximum number of ticks to return. |
| `ignore_size` | `bool` | If `true`, ignore size in tick-by-tick data. |

**Returns:** `Result<(), String>`

---

#### `cancel_tick_by_tick_data`

Cancel tick-by-tick data.

```rust
pub fn cancel_tick_by_tick_data(&self, req_id: i64) -> Result<(), String>
```

| Parameter | Type | Description |
|-----------|------|-------------|
| `req_id` | `i64` | Request identifier. Used to match responses to requests. |

**Returns:** `Result<(), String>`

---

#### `req_mkt_depth`

Subscribe to market depth (L2 order book).

```rust
pub fn req_mkt_depth( &self, req_id: i64, contract: &Contract, num_rows: i32, is_smart_depth: bool, ) -> Result<(), String>
```

| Parameter | Type | Description |
|-----------|------|-------------|
| `req_id` | `i64` | Request identifier. Used to match responses to requests. |
| `contract` | `&Contract` | Contract specification (symbol, secType, exchange, currency, etc.). |
| `num_rows` | `i32` | Number of order book rows to subscribe to. |
| `is_smart_depth` | `bool` | If `true`, aggregate depth from multiple exchanges via SMART. |

**Returns:** `Result<(), String>`

---

#### `cancel_mkt_depth`

Cancel market depth.

```rust
pub fn cancel_mkt_depth(&self, req_id: i64) -> Result<(), String>
```

| Parameter | Type | Description |
|-----------|------|-------------|
| `req_id` | `i64` | Request identifier. Used to match responses to requests. |

**Returns:** `Result<(), String>`

---

#### `req_real_time_bars`

Subscribe to real-time 5-second bars.

```rust
pub fn req_real_time_bars( &self, req_id: i64, contract: &Contract, _bar_size: i32, what_to_show: &str, use_rth: bool, ) -> Result<(), String>
```

| Parameter | Type | Description |
|-----------|------|-------------|
| `req_id` | `i64` | Request identifier. Used to match responses to requests. |
| `contract` | `&Contract` | Contract specification (symbol, secType, exchange, currency, etc.). |
| `bar_size` | `i32` | Bar size: `"1 min"`, `"5 mins"`, `"1 hour"`, `"1 day"`, etc. |
| `what_to_show` | `&str` | Data type: `"TRADES"`, `"MIDPOINT"`, `"BID"`, `"ASK"`, `"BID_ASK"`, etc. |
| `use_rth` | `bool` | If `true`, only return data from Regular Trading Hours. |

**Returns:** `Result<(), String>`

---

#### `cancel_real_time_bars`

Cancel real-time bars.

```rust
pub fn cancel_real_time_bars(&self, req_id: i64) -> Result<(), String>
```

| Parameter | Type | Description |
|-----------|------|-------------|
| `req_id` | `i64` | Request identifier. Used to match responses to requests. |

**Returns:** `Result<(), String>`

---

#### `req_market_data_type`

Set market data type preference (1=live, 2=frozen, 3=delayed, 4=delayed-frozen).

```rust
pub fn req_market_data_type(&self, market_data_type: i32)
```

| Parameter | Type | Description |
|-----------|------|-------------|
| `market_data_type` | `i32` | 1=live, 2=frozen, 3=delayed, 4=delayed-frozen. |

---

#### `set_news_providers`

Set news provider codes for per-contract news ticks.

```rust
pub fn set_news_providers(&self, providers: &str)
```

| Parameter | Type | Description |
|-----------|------|-------------|
| `providers` | `&str` | News provider list. |

---

#### `quote`

Zero-copy SeqLock quote read. Maps reqId → InstrumentId → SeqLock. Returns `None` if the reqId is not mapped to a subscription.

```rust
pub fn quote(&self, req_id: i64) -> Option<Quote>
```

| Parameter | Type | Description |
|-----------|------|-------------|
| `req_id` | `i64` | Request identifier. Used to match responses to requests. |

**Returns:** `Option<Quote>`

---

#### `quote_by_instrument`

Direct SeqLock read by InstrumentId (for callers who track IDs themselves).

```rust
pub fn quote_by_instrument(&self, instrument: InstrumentId) -> Quote
```

| Parameter | Type | Description |
|-----------|------|-------------|
| `instrument` | `InstrumentId` | Instrument type for scanner (e.g. `"STK"`, `"FUT"`). |

**Returns:** `Quote`

---

## Reference Data

#### `req_historical_data`

Request historical data.

```rust
pub fn req_historical_data( &self, req_id: i64, contract: &Contract, end_date_time: &str, duration: &str, bar_size: &str, what_to_show: &str, use_rth: bool, _format_date: i32, _keep_up_to_date: bool, ) -> Result<(), String>
```

| Parameter | Type | Description |
|-----------|------|-------------|
| `req_id` | `i64` | Request identifier. Used to match responses to requests. |
| `contract` | `&Contract` | Contract specification (symbol, secType, exchange, currency, etc.). |
| `end_date_time` | `&str` | End date/time in `"YYYYMMDD HH:MM:SS"` format, or empty for now. |
| `duration` | `&str` | Duration string, e.g. `"1 D"`, `"1 W"`, `"1 M"`, `"1 Y"`. |
| `bar_size` | `&str` | Bar size: `"1 min"`, `"5 mins"`, `"1 hour"`, `"1 day"`, etc. |
| `what_to_show` | `&str` | Data type: `"TRADES"`, `"MIDPOINT"`, `"BID"`, `"ASK"`, `"BID_ASK"`, etc. |
| `use_rth` | `bool` | If `true`, only return data from Regular Trading Hours. |
| `format_date` | `i32` | Date format: 1=`"YYYYMMDD HH:MM:SS"`, 2=Unix seconds. |
| `keep_up_to_date` | `bool` | If `true`, continue receiving updates after initial history. |

**Returns:** `Result<(), String>`

---

#### `cancel_historical_data`

Cancel historical data.

```rust
pub fn cancel_historical_data(&self, req_id: i64) -> Result<(), String>
```

| Parameter | Type | Description |
|-----------|------|-------------|
| `req_id` | `i64` | Request identifier. Used to match responses to requests. |

**Returns:** `Result<(), String>`

---

#### `req_head_time_stamp`

Request head timestamp.

```rust
pub fn req_head_time_stamp( &self, req_id: i64, contract: &Contract, what_to_show: &str, use_rth: bool, _format_date: i32, ) -> Result<(), String>
```

| Parameter | Type | Description |
|-----------|------|-------------|
| `req_id` | `i64` | Request identifier. Used to match responses to requests. |
| `contract` | `&Contract` | Contract specification (symbol, secType, exchange, currency, etc.). |
| `what_to_show` | `&str` | Data type: `"TRADES"`, `"MIDPOINT"`, `"BID"`, `"ASK"`, `"BID_ASK"`, etc. |
| `use_rth` | `bool` | If `true`, only return data from Regular Trading Hours. |
| `format_date` | `i32` | Date format: 1=`"YYYYMMDD HH:MM:SS"`, 2=Unix seconds. |

**Returns:** `Result<(), String>`

---

#### `req_contract_details`

Request contract details.

```rust
pub fn req_contract_details(&self, req_id: i64, contract: &Contract) -> Result<(), String>
```

| Parameter | Type | Description |
|-----------|------|-------------|
| `req_id` | `i64` | Request identifier. Used to match responses to requests. |
| `contract` | `&Contract` | Contract specification (symbol, secType, exchange, currency, etc.). |

**Returns:** `Result<(), String>`

---

#### `req_mkt_depth_exchanges`

Request available exchanges for market depth.

```rust
pub fn req_mkt_depth_exchanges(&self) -> Result<(), String>
```

**Returns:** `Result<(), String>`

---

#### `req_matching_symbols`

Request matching symbols.

```rust
pub fn req_matching_symbols(&self, req_id: i64, pattern: &str) -> Result<(), String>
```

| Parameter | Type | Description |
|-----------|------|-------------|
| `req_id` | `i64` | Request identifier. Used to match responses to requests. |
| `pattern` | `&str` | Symbol search pattern. |

**Returns:** `Result<(), String>`

---

#### `cancel_head_time_stamp`

Cancel head timestamp request.

```rust
pub fn cancel_head_time_stamp(&self, req_id: i64) -> Result<(), String>
```

| Parameter | Type | Description |
|-----------|------|-------------|
| `req_id` | `i64` | Request identifier. Used to match responses to requests. |

**Returns:** `Result<(), String>`

---

#### `req_market_rule`

Request market rule by ID. Looks up cached market rules delivered during connection init.

```rust
pub fn req_market_rule(&self, market_rule_id: i32, wrapper: &mut impl crate::api::wrapper::Wrapper)
```

| Parameter | Type | Description |
|-----------|------|-------------|
| `market_rule_id` | `i32` | Market rule ID. |
| `wrapper` | `&mut impl crate::api::wrapper::Wrapper` | Wrapper callback receiver for synchronous delivery. |

---

#### `req_news_bulletins`

Subscribe to news bulletins.

```rust
pub fn req_news_bulletins(&self, _all_msgs: bool)
```

| Parameter | Type | Description |
|-----------|------|-------------|
| `all_msgs` | `bool` | If `true`, receive all existing bulletins on subscribe. |

---

#### `cancel_news_bulletins`

Cancel news bulletin subscription.

```rust
pub fn cancel_news_bulletins(&self)
```

---

#### `req_scanner_parameters`

Request scanner parameters XML.

```rust
pub fn req_scanner_parameters(&self) -> Result<(), String>
```

**Returns:** `Result<(), String>`

---

#### `req_scanner_subscription`

Subscribe to a market scanner.

```rust
pub fn req_scanner_subscription( &self, req_id: i64, instrument: &str, location_code: &str, scan_code: &str, max_items: u32, ) -> Result<(), String>
```

| Parameter | Type | Description |
|-----------|------|-------------|
| `req_id` | `i64` | Request identifier. Used to match responses to requests. |
| `instrument` | `&str` | Instrument type for scanner (e.g. `"STK"`, `"FUT"`). |
| `location_code` | `&str` | Scanner location (e.g. `"STK.US.MAJOR"`). |
| `scan_code` | `&str` | Scanner code (e.g. `"TOP_PERC_GAIN"`, `"HIGH_OPT_IMP_VOLAT"`). |
| `max_items` | `u32` | Maximum number of scanner results. |

**Returns:** `Result<(), String>`

---

#### `cancel_scanner_subscription`

Cancel a scanner subscription.

```rust
pub fn cancel_scanner_subscription(&self, req_id: i64) -> Result<(), String>
```

| Parameter | Type | Description |
|-----------|------|-------------|
| `req_id` | `i64` | Request identifier. Used to match responses to requests. |

**Returns:** `Result<(), String>`

---

#### `req_historical_news`

Request historical news headlines.

```rust
pub fn req_historical_news( &self, req_id: i64, con_id: i64, provider_codes: &str, start_time: &str, end_time: &str, max_results: u32, ) -> Result<(), String>
```

| Parameter | Type | Description |
|-----------|------|-------------|
| `req_id` | `i64` | Request identifier. Used to match responses to requests. |
| `con_id` | `i64` | Contract ID. Unique per instrument. |
| `provider_codes` | `&str` | Pipe-separated news provider codes. |
| `start_time` | `&str` | Start date/time for news query. |
| `end_time` | `&str` | End date/time for news query. |
| `max_results` | `u32` | Maximum number of results. |

**Returns:** `Result<(), String>`

---

#### `req_news_article`

Request a news article by provider and article ID.

```rust
pub fn req_news_article(&self, req_id: i64, provider_code: &str, article_id: &str) -> Result<(), String>
```

| Parameter | Type | Description |
|-----------|------|-------------|
| `req_id` | `i64` | Request identifier. Used to match responses to requests. |
| `provider_code` | `&str` | News provider code (e.g. `"BRFG"`). |
| `article_id` | `&str` | News article identifier. |

**Returns:** `Result<(), String>`

---

#### `req_fundamental_data`

Request fundamental data (e.g. ReportSnapshot, ReportsFinSummary).

```rust
pub fn req_fundamental_data(&self, req_id: i64, contract: &Contract, report_type: &str) -> Result<(), String>
```

| Parameter | Type | Description |
|-----------|------|-------------|
| `req_id` | `i64` | Request identifier. Used to match responses to requests. |
| `contract` | `&Contract` | Contract specification (symbol, secType, exchange, currency, etc.). |
| `report_type` | `&str` | Report type: `"ReportSnapshot"`, `"ReportsFinSummary"`, `"RESC"`, etc. |

**Returns:** `Result<(), String>`

---

#### `cancel_fundamental_data`

Cancel fundamental data.

```rust
pub fn cancel_fundamental_data(&self, req_id: i64) -> Result<(), String>
```

| Parameter | Type | Description |
|-----------|------|-------------|
| `req_id` | `i64` | Request identifier. Used to match responses to requests. |

**Returns:** `Result<(), String>`

---

#### `req_histogram_data`

Request price histogram data.

```rust
pub fn req_histogram_data(&self, req_id: i64, contract: &Contract, use_rth: bool, period: &str) -> Result<(), String>
```

| Parameter | Type | Description |
|-----------|------|-------------|
| `req_id` | `i64` | Request identifier. Used to match responses to requests. |
| `contract` | `&Contract` | Contract specification (symbol, secType, exchange, currency, etc.). |
| `use_rth` | `bool` | If `true`, only return data from Regular Trading Hours. |
| `period` | `&str` | Histogram period, e.g. `"1week"`, `"1month"`. |

**Returns:** `Result<(), String>`

---

#### `cancel_histogram_data`

Cancel histogram data.

```rust
pub fn cancel_histogram_data(&self, req_id: i64) -> Result<(), String>
```

| Parameter | Type | Description |
|-----------|------|-------------|
| `req_id` | `i64` | Request identifier. Used to match responses to requests. |

**Returns:** `Result<(), String>`

---

#### `req_historical_ticks`

Request historical tick data.

```rust
pub fn req_historical_ticks( &self, req_id: i64, contract: &Contract, start_date_time: &str, end_date_time: &str, number_of_ticks: i32, what_to_show: &str, use_rth: bool, ) -> Result<(), String>
```

| Parameter | Type | Description |
|-----------|------|-------------|
| `req_id` | `i64` | Request identifier. Used to match responses to requests. |
| `contract` | `&Contract` | Contract specification (symbol, secType, exchange, currency, etc.). |
| `start_date_time` | `&str` | Start date/time for tick query. |
| `end_date_time` | `&str` | End date/time in `"YYYYMMDD HH:MM:SS"` format, or empty for now. |
| `number_of_ticks` | `i32` | Maximum number of ticks to return. |
| `what_to_show` | `&str` | Data type: `"TRADES"`, `"MIDPOINT"`, `"BID"`, `"ASK"`, `"BID_ASK"`, etc. |
| `use_rth` | `bool` | If `true`, only return data from Regular Trading Hours. |

**Returns:** `Result<(), String>`

---

#### `req_historical_schedule`

Request historical trading schedule.

```rust
pub fn req_historical_schedule( &self, req_id: i64, contract: &Contract, end_date_time: &str, duration: &str, use_rth: bool, ) -> Result<(), String>
```

| Parameter | Type | Description |
|-----------|------|-------------|
| `req_id` | `i64` | Request identifier. Used to match responses to requests. |
| `contract` | `&Contract` | Contract specification (symbol, secType, exchange, currency, etc.). |
| `end_date_time` | `&str` | End date/time in `"YYYYMMDD HH:MM:SS"` format, or empty for now. |
| `duration` | `&str` | Duration string, e.g. `"1 D"`, `"1 W"`, `"1 M"`, `"1 Y"`. |
| `use_rth` | `bool` | If `true`, only return data from Regular Trading Hours. |

**Returns:** `Result<(), String>`

---

## Gateway-Local & Stubs

#### `req_smart_components`

Request smart routing components for a BBO exchange. Gateway-local — returns component exchanges from init data.

```rust
pub fn req_smart_components(&self, req_id: i64, _bbo_exchange: &str, wrapper: &mut impl Wrapper)
```

| Parameter | Type | Description |
|-----------|------|-------------|
| `req_id` | `i64` | Request identifier. Used to match responses to requests. |
| `bbo_exchange` | `&str` | BBO exchange for smart component lookup (e.g. `"SMART"`). |
| `wrapper` | `&mut impl Wrapper` | Wrapper callback receiver for synchronous delivery. |

---

#### `req_news_providers`

Request available news providers. Gateway-local — returns provider list from init data.

```rust
pub fn req_news_providers(&self, wrapper: &mut impl Wrapper)
```

| Parameter | Type | Description |
|-----------|------|-------------|
| `wrapper` | `&mut impl Wrapper` | Wrapper callback receiver for synchronous delivery. |

---

#### `req_current_time`

Request current server time. Returns local system time (no server round-trip).

```rust
pub fn req_current_time(&self, wrapper: &mut impl Wrapper)
```

| Parameter | Type | Description |
|-----------|------|-------------|
| `wrapper` | `&mut impl Wrapper` | Wrapper callback receiver for synchronous delivery. |

---

#### `request_fa`

Request FA data. Not yet implemented.

```rust
pub fn request_fa(&self, _fa_data_type: i32)
```

| Parameter | Type | Description |
|-----------|------|-------------|
| `fa_data_type` | `i32` | FA data type (1=Groups, 2=Profiles, 3=Aliases). |

---

#### `replace_fa`

Replace FA data. Not yet implemented.

```rust
pub fn replace_fa(&self, _req_id: i64, _fa_data_type: i32, _cxml: &str)
```

| Parameter | Type | Description |
|-----------|------|-------------|
| `req_id` | `i64` | Request identifier. Used to match responses to requests. |
| `fa_data_type` | `i32` | FA data type (1=Groups, 2=Profiles, 3=Aliases). |
| `cxml` | `&str` | FA XML configuration data. |

---

#### `query_display_groups`

Query display groups. Not yet implemented.

```rust
pub fn query_display_groups(&self, _req_id: i64)
```

| Parameter | Type | Description |
|-----------|------|-------------|
| `req_id` | `i64` | Request identifier. Used to match responses to requests. |

---

#### `subscribe_to_group_events`

Subscribe to display group events. Not yet implemented.

```rust
pub fn subscribe_to_group_events(&self, _req_id: i64, _group_id: i32)
```

| Parameter | Type | Description |
|-----------|------|-------------|
| `req_id` | `i64` | Request identifier. Used to match responses to requests. |
| `group_id` | `i32` | Display group ID. |

---

#### `unsubscribe_from_group_events`

Unsubscribe from display group events. Not yet implemented.

```rust
pub fn unsubscribe_from_group_events(&self, _req_id: i64)
```

| Parameter | Type | Description |
|-----------|------|-------------|
| `req_id` | `i64` | Request identifier. Used to match responses to requests. |

---

#### `update_display_group`

Update display group. Not yet implemented.

```rust
pub fn update_display_group(&self, _req_id: i64, _contract_info: &str)
```

| Parameter | Type | Description |
|-----------|------|-------------|
| `req_id` | `i64` | Request identifier. Used to match responses to requests. |
| `contract_info` | `&str` | Display group contract info string. |

---

#### `req_soft_dollar_tiers`

Request soft dollar tiers. Gateway-local — returns tiers parsed from CCP logon tag 6560.

```rust
pub fn req_soft_dollar_tiers(&self, req_id: i64, wrapper: &mut impl Wrapper)
```

| Parameter | Type | Description |
|-----------|------|-------------|
| `req_id` | `i64` | Request identifier. Used to match responses to requests. |
| `wrapper` | `&mut impl Wrapper` | Wrapper callback receiver for synchronous delivery. |

---

#### `req_family_codes`

Request family codes. Gateway-local — returns codes parsed from CCP logon tag 6823.

```rust
pub fn req_family_codes(&self, wrapper: &mut impl Wrapper)
```

| Parameter | Type | Description |
|-----------|------|-------------|
| `wrapper` | `&mut impl Wrapper` | Wrapper callback receiver for synchronous delivery. |

---

#### `set_server_log_level`

Set server log level.

```rust
pub fn set_server_log_level(&self, log_level: i32)
```

| Parameter | Type | Description |
|-----------|------|-------------|
| `log_level` | `i32` | Log level: 1=error, 2=warn, 3=info, 4=debug, 5=trace. |

---

#### `req_user_info`

Request user info. Gateway-local — returns whiteBrandingId from CCP logon.

```rust
pub fn req_user_info(&self, req_id: i64, wrapper: &mut impl Wrapper)
```

| Parameter | Type | Description |
|-----------|------|-------------|
| `req_id` | `i64` | Request identifier. Used to match responses to requests. |
| `wrapper` | `&mut impl Wrapper` | Wrapper callback receiver for synchronous delivery. |

---

#### `req_wsh_meta_data`

Request WSH metadata. Not yet implemented.

```rust
pub fn req_wsh_meta_data(&self, _req_id: i64)
```

| Parameter | Type | Description |
|-----------|------|-------------|
| `req_id` | `i64` | Request identifier. Used to match responses to requests. |

---

#### `req_wsh_event_data`

Request WSH event data. Not yet implemented.

```rust
pub fn req_wsh_event_data(&self, _req_id: i64)
```

| Parameter | Type | Description |
|-----------|------|-------------|
| `req_id` | `i64` | Request identifier. Used to match responses to requests. |

---

## Wrapper Callbacks

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
| `order_id` | `i64` | Order identifier. Must be unique per session. |

---

#### `managed_accounts`

Comma-separated list of managed account IDs.

| Parameter | Type | Description |
|-----------|------|-------------|
| `accounts_list` | `&str` | Comma-separated account IDs. |

---

#### `error`

Error or informational message from the server.

| Parameter | Type | Description |
|-----------|------|-------------|
| `req_id` | `i64` | Request identifier. Used to match responses to requests. |
| `error_code` | `i64` | Error code. |
| `error_string` | `&str` | Error message. |
| `advanced_order_reject_json` | `&str` | JSON with advanced rejection details. |

---

#### `current_time`

Current server time (Unix seconds).

| Parameter | Type | Description |
|-----------|------|-------------|
| `time` | `i64` | Tick timestamp (Unix seconds). |

---

#### `tick_price`

Price tick update (bid, ask, last, etc.).

| Parameter | Type | Description |
|-----------|------|-------------|
| `req_id` | `i64` | Request identifier. Used to match responses to requests. |
| `tick_type` | `i32` | Tick type ID or tick-by-tick type string. |
| `price` | `f64` | Tick price. |
| `attrib` | `&TickAttrib` | Tick attributes. |

---

#### `tick_size`

Size tick update (bid size, ask size, volume, etc.).

| Parameter | Type | Description |
|-----------|------|-------------|
| `req_id` | `i64` | Request identifier. Used to match responses to requests. |
| `tick_type` | `i32` | Tick type ID or tick-by-tick type string. |
| `size` | `f64` | Tick size. |

---

#### `tick_string`

String tick (e.g. last trade timestamp).

| Parameter | Type | Description |
|-----------|------|-------------|
| `req_id` | `i64` | Request identifier. Used to match responses to requests. |
| `tick_type` | `i32` | Tick type ID or tick-by-tick type string. |
| `value` | `&str` | Account value. |

---

#### `tick_generic`

Generic numeric tick value.

| Parameter | Type | Description |
|-----------|------|-------------|
| `req_id` | `i64` | Request identifier. Used to match responses to requests. |
| `tick_type` | `i32` | Tick type ID or tick-by-tick type string. |
| `value` | `f64` | Account value. |

---

#### `tick_snapshot_end`

Snapshot delivery complete; subscription auto-cancelled.

| Parameter | Type | Description |
|-----------|------|-------------|
| `req_id` | `i64` | Request identifier. Used to match responses to requests. |

---

#### `market_data_type`

Market data type changed (1=live, 2=frozen, 3=delayed, 4=delayed-frozen).

| Parameter | Type | Description |
|-----------|------|-------------|
| `req_id` | `i64` | Request identifier. Used to match responses to requests. |
| `market_data_type` | `i32` | 1=live, 2=frozen, 3=delayed, 4=delayed-frozen. |

---

#### `order_status`

Order status update (filled, remaining, avg price, etc.).

| Parameter | Type | Description |
|-----------|------|-------------|
| `order_id` | `i64` | Order identifier. Must be unique per session. |
| `status` | `&str` | Order status string (`"Submitted"`, `"Filled"`, `"Cancelled"`, etc.). |
| `filled` | `f64` | Cumulative filled quantity. |
| `remaining` | `f64` | Remaining quantity. |
| `avg_fill_price` | `f64` | Average fill price. |
| `perm_id` | `i64` | Permanent order ID assigned by the server. |
| `parent_id` | `i64` | Parent order ID (0 if no parent). |
| `last_fill_price` | `f64` | Price of the last fill. |
| `client_id` | `i64` | Client ID (unused — single-client engine). |
| `why_held` | `&str` | Reason the order is held (e.g. `"locate"`). |
| `mkt_cap_price` | `f64` | Market cap price for the order. |

---

#### `open_order`

Open order details (contract, order, state).

| Parameter | Type | Description |
|-----------|------|-------------|
| `order_id` | `i64` | Order identifier. Must be unique per session. |
| `contract` | `&Contract` | Contract specification (symbol, secType, exchange, currency, etc.). |
| `order` | `&Order` | Order parameters (action, quantity, type, price, TIF, etc.). |
| `order_state` | `&OrderState` | Order state (status, margin, commission info). |

---

#### `open_order_end`

End of open orders list.

---

#### `exec_details`

Execution fill details.

| Parameter | Type | Description |
|-----------|------|-------------|
| `req_id` | `i64` | Request identifier. Used to match responses to requests. |
| `contract` | `&Contract` | Contract specification (symbol, secType, exchange, currency, etc.). |
| `execution` | `&Execution` | Execution details (exec_id, time, price, shares, etc.). |

---

#### `exec_details_end`

End of execution details list.

| Parameter | Type | Description |
|-----------|------|-------------|
| `req_id` | `i64` | Request identifier. Used to match responses to requests. |

---

#### `commission_report`

Commission report for an execution.

| Parameter | Type | Description |
|-----------|------|-------------|
| `report` | `&CommissionReport` | Commission report (exec_id, commission, currency, realized P&L). |

---

#### `update_account_value`

Account value update (key/value/currency).

| Parameter | Type | Description |
|-----------|------|-------------|
| `key` | `&str` | Account value key (e.g. `"NetLiquidation"`, `"BuyingPower"`). |
| `value` | `&str` | Account value. |
| `currency` | `&str` | Currency code (e.g. `"USD"`). |
| `account_name` | `&str` | Account identifier. |

---

#### `update_portfolio`

Portfolio position update.

| Parameter | Type | Description |
|-----------|------|-------------|
| `contract` | `&Contract` | Contract specification (symbol, secType, exchange, currency, etc.). |
| `position` | `f64` | Book position (row index) or position size. |
| `market_price` | `f64` | Current market price. |
| `market_value` | `f64` | Current market value of position. |
| `average_cost` | `f64` | Average cost basis. |
| `unrealized_pnl` | `f64` | Unrealized profit/loss. |
| `realized_pnl` | `f64` | Realized profit/loss. |
| `account_name` | `&str` | Account identifier. |

---

#### `update_account_time`

Account update timestamp.

| Parameter | Type | Description |
|-----------|------|-------------|
| `timestamp` | `&str` | Timestamp string. |

---

#### `account_download_end`

Account data delivery complete.

| Parameter | Type | Description |
|-----------|------|-------------|
| `account` | `&str` | Account ID. |

---

#### `account_summary`

Account summary tag/value entry.

| Parameter | Type | Description |
|-----------|------|-------------|
| `req_id` | `i64` | Request identifier. Used to match responses to requests. |
| `account` | `&str` | Account ID. |
| `tag` | `&str` | Account tag name (e.g. `"NetLiquidation"`). |
| `value` | `&str` | Account value. |
| `currency` | `&str` | Currency code (e.g. `"USD"`). |

---

#### `account_summary_end`

End of account summary.

| Parameter | Type | Description |
|-----------|------|-------------|
| `req_id` | `i64` | Request identifier. Used to match responses to requests. |

---

#### `position`

Position entry (account, contract, size, avg cost).

| Parameter | Type | Description |
|-----------|------|-------------|
| `account` | `&str` | Account ID. |
| `contract` | `&Contract` | Contract specification (symbol, secType, exchange, currency, etc.). |
| `pos` | `f64` | Position size (decimal shares). |
| `avg_cost` | `f64` | Average cost per share. |

---

#### `position_end`

End of positions list.

---

#### `pnl`

Account P&L update (daily, unrealized, realized).

| Parameter | Type | Description |
|-----------|------|-------------|
| `req_id` | `i64` | Request identifier. Used to match responses to requests. |
| `daily_pnl` | `f64` | Daily profit/loss. |
| `unrealized_pnl` | `f64` | Unrealized profit/loss. |
| `realized_pnl` | `f64` | Realized profit/loss. |

---

#### `pnl_single`

Single-position P&L update.

| Parameter | Type | Description |
|-----------|------|-------------|
| `req_id` | `i64` | Request identifier. Used to match responses to requests. |
| `pos` | `f64` | Position size (decimal shares). |
| `daily_pnl` | `f64` | Daily profit/loss. |
| `unrealized_pnl` | `f64` | Unrealized profit/loss. |
| `realized_pnl` | `f64` | Realized profit/loss. |
| `value` | `f64` | Account value. |

---

#### `historical_data`

Historical OHLCV bar.

| Parameter | Type | Description |
|-----------|------|-------------|
| `req_id` | `i64` | Request identifier. Used to match responses to requests. |
| `bar` | `&BarData` | Bar data (date, open, high, low, close, volume, wap, bar_count). |

---

#### `historical_data_end`

End of historical data delivery.

| Parameter | Type | Description |
|-----------|------|-------------|
| `req_id` | `i64` | Request identifier. Used to match responses to requests. |
| `start` | `&str` | Period start date/time. |
| `end` | `&str` | Period end date/time. |

---

#### `historical_data_update`

Real-time bar update (keep_up_to_date=true).

| Parameter | Type | Description |
|-----------|------|-------------|
| `req_id` | `i64` | Request identifier. Used to match responses to requests. |
| `bar` | `&BarData` | Bar data (date, open, high, low, close, volume, wap, bar_count). |

---

#### `head_timestamp`

Earliest available data timestamp.

| Parameter | Type | Description |
|-----------|------|-------------|
| `req_id` | `i64` | Request identifier. Used to match responses to requests. |
| `head_timestamp` | `&str` | Earliest available data timestamp string. |

---

#### `contract_details`

Contract definition details.

| Parameter | Type | Description |
|-----------|------|-------------|
| `req_id` | `i64` | Request identifier. Used to match responses to requests. |
| `details` | `&ContractDetails` | Contract details object. |

---

#### `contract_details_end`

End of contract details.

| Parameter | Type | Description |
|-----------|------|-------------|
| `req_id` | `i64` | Request identifier. Used to match responses to requests. |

---

#### `symbol_samples`

Matching symbol search results.

| Parameter | Type | Description |
|-----------|------|-------------|
| `req_id` | `i64` | Request identifier. Used to match responses to requests. |
| `descriptions` | `&[ContractDescription]` | Array of matching contract descriptions. |

---

#### `tick_by_tick_all_last`

Tick-by-tick last trade.

| Parameter | Type | Description |
|-----------|------|-------------|
| `req_id` | `i64` | Request identifier. Used to match responses to requests. |
| `tick_type` | `i32` | Tick type ID or tick-by-tick type string. |
| `time` | `i64` | Tick timestamp (Unix seconds). |
| `price` | `f64` | Tick price. |
| `size` | `f64` | Tick size. |
| `attrib` | `&TickAttribLast` | Tick attributes. |
| `exchange` | `&str` | Exchange name. |
| `special_conditions` | `&str` | Special trade conditions. |

---

#### `tick_by_tick_bid_ask`

Tick-by-tick bid/ask quote.

| Parameter | Type | Description |
|-----------|------|-------------|
| `req_id` | `i64` | Request identifier. Used to match responses to requests. |
| `time` | `i64` | Tick timestamp (Unix seconds). |
| `bid_price` | `f64` | Bid price. |
| `ask_price` | `f64` | Ask price. |
| `bid_size` | `f64` | Bid size. |
| `ask_size` | `f64` | Ask size. |
| `attrib` | `&TickAttribBidAsk` | Tick attributes. |

---

#### `tick_by_tick_mid_point`

Tick-by-tick midpoint.

| Parameter | Type | Description |
|-----------|------|-------------|
| `req_id` | `i64` | Request identifier. Used to match responses to requests. |
| `time` | `i64` | Tick timestamp (Unix seconds). |
| `mid_point` | `f64` | Midpoint price. |

---

#### `scanner_data`

Scanner result entry (rank, contract, distance).

| Parameter | Type | Description |
|-----------|------|-------------|
| `req_id` | `i64` | Request identifier. Used to match responses to requests. |
| `rank` | `i32` | Scanner result rank (0-based). |
| `details` | `&ContractDetails` | Contract details object. |
| `distance` | `&str` | Scanner distance metric. |
| `benchmark` | `&str` | Scanner benchmark. |
| `projection` | `&str` | Scanner projection. |
| `legs_str` | `&str` | Combo legs description. |

---

#### `scanner_data_end`

End of scanner results.

| Parameter | Type | Description |
|-----------|------|-------------|
| `req_id` | `i64` | Request identifier. Used to match responses to requests. |

---

#### `scanner_parameters`

Scanner parameters XML.

| Parameter | Type | Description |
|-----------|------|-------------|
| `xml` | `&str` | XML string. |

---

#### `update_news_bulletin`

News bulletin message.

| Parameter | Type | Description |
|-----------|------|-------------|
| `msg_id` | `i64` | Bulletin message ID. |
| `msg_type` | `i32` | Bulletin message type (1=regular, 2=exchange). |
| `message` | `&str` | Bulletin message text. |
| `orig_exchange` | `&str` | Originating exchange. |

---

#### `tick_news`

Per-contract news tick.

| Parameter | Type | Description |
|-----------|------|-------------|
| `ticker_id` | `i64` | Ticker/request ID. |
| `timestamp` | `i64` | Timestamp string. |
| `provider_code` | `&str` | News provider code (e.g. `"BRFG"`). |
| `article_id` | `&str` | News article identifier. |
| `headline` | `&str` | News headline text. |
| `extra_data` | `&str` | Additional tick data. |

---

#### `historical_news`

Historical news headline.

| Parameter | Type | Description |
|-----------|------|-------------|
| `req_id` | `i64` | Request identifier. Used to match responses to requests. |
| `time` | `&str` | Tick timestamp (Unix seconds). |
| `provider_code` | `&str` | News provider code (e.g. `"BRFG"`). |
| `article_id` | `&str` | News article identifier. |
| `headline` | `&str` | News headline text. |

---

#### `historical_news_end`

End of historical news.

| Parameter | Type | Description |
|-----------|------|-------------|
| `req_id` | `i64` | Request identifier. Used to match responses to requests. |
| `has_more` | `bool` | If `true`, more results available. |

---

#### `news_article`

Full news article text.

| Parameter | Type | Description |
|-----------|------|-------------|
| `req_id` | `i64` | Request identifier. Used to match responses to requests. |
| `article_type` | `i32` | Article type: 0=plain text, 1=HTML. |
| `article_text` | `&str` | Full article body. |

---

#### `real_time_bar`

Real-time 5-second OHLCV bar.

| Parameter | Type | Description |
|-----------|------|-------------|
| `req_id` | `i64` | Request identifier. Used to match responses to requests. |
| `date` | `i64` | Bar date string. |
| `open` | `f64` | Open price. |
| `high` | `f64` | High price. |
| `low` | `f64` | Low price. |
| `close` | `f64` | Close price. |
| `volume` | `f64` | Volume. |
| `wap` | `f64` | Volume-weighted average price. |
| `count` | `i32` | Trade count. |

---

#### `historical_ticks`

Historical tick data (Last, BidAsk, or Midpoint).

| Parameter | Type | Description |
|-----------|------|-------------|
| `req_id` | `i64` | Request identifier. Used to match responses to requests. |
| `ticks` | `&HistoricalTickData` | Historical tick data. |
| `done` | `bool` | If `true`, all ticks have been delivered. |

---

#### `histogram_data`

Price distribution histogram.

| Parameter | Type | Description |
|-----------|------|-------------|
| `req_id` | `i64` | Request identifier. Used to match responses to requests. |
| `items` | `&[(f64, i64` | Histogram entries `[(price, count)]`. |

---

#### `market_rule`

Market rule: price increment schedule.

| Parameter | Type | Description |
|-----------|------|-------------|
| `market_rule_id` | `i64` | Market rule ID. |
| `price_increments` | `&[PriceIncrement]` | Price increment rules `[{low_edge, increment}]`. |

---

#### `completed_order`

Completed (filled/cancelled) order details.

| Parameter | Type | Description |
|-----------|------|-------------|
| `contract` | `&Contract` | Contract specification (symbol, secType, exchange, currency, etc.). |
| `order` | `&Order` | Order parameters (action, quantity, type, price, TIF, etc.). |
| `order_state` | `&OrderState` | Order state (status, margin, commission info). |

---

#### `completed_orders_end`

End of completed orders list.

---

#### `historical_schedule`

Historical trading schedule (exchange hours).

| Parameter | Type | Description |
|-----------|------|-------------|
| `req_id` | `i64` | Request identifier. Used to match responses to requests. |
| `start_date_time` | `&str` | Start date/time for tick query. |
| `end_date_time` | `&str` | End date/time in `"YYYYMMDD HH:MM:SS"` format, or empty for now. |
| `time_zone` | `&str` | Timezone string (e.g. `"US/Eastern"`). |
| `sessions` | `&[(String, String, String` | Trading sessions `[(ref_date, open, close)]`. |

---

#### `fundamental_data`

Fundamental data (XML/JSON).

| Parameter | Type | Description |
|-----------|------|-------------|
| `req_id` | `i64` | Request identifier. Used to match responses to requests. |
| `data` | `&str` | Raw data string (XML/JSON). |

---

#### `update_mkt_depth`

L2 book update (single exchange).

| Parameter | Type | Description |
|-----------|------|-------------|
| `req_id` | `i64` | Request identifier. Used to match responses to requests. |
| `position` | `i32` | Book position (row index) or position size. |
| `operation` | `i32` | Book operation: 0=insert, 1=update, 2=delete. |
| `side` | `i32` | Book side: 0=ask, 1=bid. Or order side `"BOT"`/`"SLD"`. |
| `price` | `f64` | Tick price. |
| `size` | `f64` | Tick size. |

---

#### `update_mkt_depth_l2`

L2 book update (with market maker).

| Parameter | Type | Description |
|-----------|------|-------------|
| `req_id` | `i64` | Request identifier. Used to match responses to requests. |
| `position` | `i32` | Book position (row index) or position size. |
| `market_maker` | `&str` | Market maker ID. |
| `operation` | `i32` | Book operation: 0=insert, 1=update, 2=delete. |
| `side` | `i32` | Book side: 0=ask, 1=bid. Or order side `"BOT"`/`"SLD"`. |
| `price` | `f64` | Tick price. |
| `size` | `f64` | Tick size. |
| `is_smart_depth` | `bool` | If `true`, aggregate depth from multiple exchanges via SMART. |

---

#### `mkt_depth_exchanges`

Available exchanges for market depth.

| Parameter | Type | Description |
|-----------|------|-------------|
| `descriptions` | `&[crate::types::DepthMktDataDescription]` | Array of matching contract descriptions. |

---

#### `tick_req_params`

Tick parameters: min tick size, BBO exchange, snapshot permissions.

| Parameter | Type | Description |
|-----------|------|-------------|
| `ticker_id` | `i64` | Ticker/request ID. |
| `min_tick` | `f64` | Minimum tick size. |
| `bbo_exchange` | `&str` | BBO exchange for smart component lookup (e.g. `"SMART"`). |
| `snapshot_permissions` | `i64` | Snapshot permissions bitmask. |

---

#### `smart_components`

SMART routing component exchanges.

| Parameter | Type | Description |
|-----------|------|-------------|
| `req_id` | `i64` | Request identifier. Used to match responses to requests. |
| `components` | `&[crate::types::SmartComponent]` | Smart routing component exchanges. |

---

#### `news_providers`

Available news providers list.

| Parameter | Type | Description |
|-----------|------|-------------|
| `providers` | `&[crate::types::NewsProvider]` | News provider list. |

---

#### `soft_dollar_tiers`

Soft dollar tier list.

| Parameter | Type | Description |
|-----------|------|-------------|
| `req_id` | `i64` | Request identifier. Used to match responses to requests. |
| `tiers` | `&[crate::types::SoftDollarTier]` | Soft dollar tier list. |

---

#### `family_codes`

Family codes linking related accounts.

| Parameter | Type | Description |
|-----------|------|-------------|
| `codes` | `&[crate::types::FamilyCode]` | Family code list. |

---

#### `user_info`

User info (white branding ID).

| Parameter | Type | Description |
|-----------|------|-------------|
| `req_id` | `i64` | Request identifier. Used to match responses to requests. |
| `white_branding_id` | `&str` | White branding ID (empty for standard accounts). |

---
