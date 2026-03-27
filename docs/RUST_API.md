# Rust API Reference (v0.4.2)

*Auto-generated from source — do not edit.*

## EClient Methods

### Connection

| Method | Description |
|--------|-------------|
| `connect` | Connect to IB and start the engine. |
| `from_parts` |  |
| `map_req_instrument` |  |
| `seed_instrument` |  |
| `is_connected` |  |
| `disconnect` | Disconnect from IB.  Sends `Shutdown` to the hot loop, waits for the background thread to exit, and marks the client as disconnected. |

### Account & Portfolio

| Method | Description |
|--------|-------------|
| `req_positions` | Request positions. Immediately delivers all positions via wrapper callbacks, then calls position_end. |
| `req_pnl` | Subscribe to account PnL updates. |
| `cancel_pnl` | Cancel PnL subscription. |
| `req_pnl_single` | Subscribe to single-position PnL updates. |
| `cancel_pnl_single` | Cancel single-position PnL subscription. |
| `req_account_summary` | Request account summary. |
| `cancel_account_summary` | Cancel account summary. |
| `req_account_updates` | Subscribe to account updates. |
| `cancel_positions` | Cancel positions subscription. |
| `req_managed_accts` | Request managed accounts. |
| `req_account_updates_multi` | Request account updates for multiple accounts/models. |
| `cancel_account_updates_multi` | Cancel multi-account updates. |
| `req_positions_multi` | Request positions for multiple accounts/models. |
| `cancel_positions_multi` | Cancel multi-account positions. |
| `account` | Read account state snapshot. |

### Orders

| Method | Description |
|--------|-------------|
| `place_order` | Place an order. |
| `cancel_order` | Cancel an order. |
| `req_global_cancel` | Cancel all orders. |
| `req_ids` | Request next valid order ID. |
| `next_order_id` | Get the next order ID (local counter). |
| `req_open_orders` | Request open orders for this client. |
| `req_all_open_orders` | Request all open orders. |
| `req_completed_orders` | Request completed orders. Immediately delivers all archived completed orders, then calls `completed_orders_end`. |
| `req_auto_open_orders` | Automatically bind future orders to this client. |
| `req_executions` | Request execution reports. Replays stored executions (optionally filtered), firing `exec_details` + `commission_report` for each, then `exec_details_end`. |
| `parse_algo_params` | Parse algo strategy and TagValue params into internal AlgoParams. |

### Market Data

| Method | Description |
|--------|-------------|
| `req_mkt_data` | Subscribe to market data. When `snapshot` is true, delivers the first available quote then calls `tick_snapshot_end` and auto-cancels the subscription. |
| `cancel_mkt_data` | Cancel market data. |
| `req_tick_by_tick_data` | Subscribe to tick-by-tick data. |
| `cancel_tick_by_tick_data` | Cancel tick-by-tick data. |
| `req_mkt_depth` | Subscribe to market depth (L2 order book). |
| `cancel_mkt_depth` | Cancel market depth. |
| `req_real_time_bars` |  |
| `cancel_real_time_bars` |  |
| `req_market_data_type` | Set market data type preference (1=live, 2=frozen, 3=delayed, 4=delayed-frozen). |
| `set_news_providers` | Set news provider codes for per-contract news ticks. |
| `quote` |  |
| `quote_by_instrument` |  |

### Reference Data

| Method | Description |
|--------|-------------|
| `req_historical_data` | Request historical data. |
| `cancel_historical_data` | Cancel historical data. |
| `req_head_time_stamp` | Request head timestamp. |
| `req_contract_details` | Request contract details. |
| `req_mkt_depth_exchanges` | Request available exchanges for market depth. |
| `req_matching_symbols` | Request matching symbols. |
| `cancel_head_time_stamp` | Cancel head timestamp request. |
| `req_market_rule` | Request market rule by ID. Looks up cached market rules delivered during connection init. |
| `req_news_bulletins` | Subscribe to news bulletins. |
| `cancel_news_bulletins` | Cancel news bulletin subscription. |
| `req_scanner_parameters` |  |
| `req_scanner_subscription` |  |
| `cancel_scanner_subscription` |  |
| `req_historical_news` |  |
| `req_news_article` |  |
| `req_fundamental_data` |  |
| `cancel_fundamental_data` |  |
| `req_histogram_data` |  |
| `cancel_histogram_data` |  |
| `req_historical_ticks` |  |
| `req_historical_schedule` |  |

### Gateway-Local & Stubs

| Method | Description |
|--------|-------------|
| `req_smart_components` | Request smart routing components for a BBO exchange. Gateway-local — returns component exchanges from init data. |
| `req_news_providers` | Request available news providers. Gateway-local — returns provider list from init data. |
| `req_current_time` | Request current server time. Returns local system time (no server round-trip). |
| `request_fa` | Request FA data. Not yet implemented. |
| `replace_fa` | Replace FA data. Not yet implemented. |
| `query_display_groups` | Query display groups. Not yet implemented. |
| `subscribe_to_group_events` | Subscribe to display group events. Not yet implemented. |
| `unsubscribe_from_group_events` | Unsubscribe from display group events. Not yet implemented. |
| `update_display_group` | Update display group. Not yet implemented. |
| `req_soft_dollar_tiers` | Request soft dollar tiers. Gateway-local — returns tiers parsed from CCP logon tag 6560. |
| `req_family_codes` | Request family codes. Gateway-local — returns codes parsed from CCP logon tag 6823. |
| `set_server_log_level` | Set server log level. |
| `req_user_info` | Request user info. Gateway-local — returns whiteBrandingId from CCP logon. |
| `req_wsh_meta_data` | Request WSH metadata. Not yet implemented. |
| `req_wsh_event_data` | Request WSH event data. Not yet implemented. |

## Wrapper Callbacks

| Callback | Description |
|----------|-------------|
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
| `update_news_bulletin` |  |
| `tick_news` |  |
| `historical_news` |  |
| `historical_news_end` |  |
| `news_article` |  |
| `real_time_bar` |  |
| `historical_ticks` |  |
| `histogram_data` |  |
| `market_rule` |  |
| `completed_order` |  |
| `completed_orders_end` |  |
| `historical_schedule` |  |
| `fundamental_data` |  |
| `update_mkt_depth` |  |
| `update_mkt_depth_l2` |  |
| `mkt_depth_exchanges` |  |
| `tick_req_params` |  |
| `smart_components` |  |
| `news_providers` |  |
| `soft_dollar_tiers` |  |
| `family_codes` |  |
| `user_info` |  |

## Full Signatures

<details>
<summary>EClient</summary>

```rust
/// Connect to IB and start the engine. pub fn connect(config: &EClientConfig) -> Result<Self, Box<dyn std::error::Error>>
pub fn from_parts( shared: Arc<SharedState>, control_tx: Sender<ControlCommand>, handle: thread::JoinHandle<()>, account_id: String, ) -> Self
pub fn map_req_instrument(&self, req_id: i64, instrument: InstrumentId)
pub fn seed_instrument(&self, con_id: i64, instrument: InstrumentId)
pub fn is_connected(&self) -> bool
/// Disconnect from IB. Sends `Shutdown` to the hot loop, waits for the /// background thread to exit, and marks the client as disconnected. pub fn disconnect(&self)
/// Request positions. Matches `reqPositions` in C++. /// Immediately delivers all positions via wrapper callbacks, then calls position_end. pub fn req_positions(&self, wrapper: &mut impl Wrapper)
/// Subscribe to account PnL updates. Matches `reqPnL` in C++. pub fn req_pnl(&self, req_id: i64, _account: &str, _model_code: &str)
/// Cancel PnL subscription. Matches `cancelPnL` in C++. pub fn cancel_pnl(&self, req_id: i64)
/// Subscribe to single-position PnL updates. Matches `reqPnLSingle` in C++. pub fn req_pnl_single(&self, req_id: i64, _account: &str, _model_code: &str, con_id: i64)
/// Cancel single-position PnL subscription. Matches `cancelPnLSingle` in C++. pub fn cancel_pnl_single(&self, req_id: i64)
/// Request account summary. Matches `reqAccountSummary` in C++. pub fn req_account_summary(&self, req_id: i64, _group: &str, tags: &str)
/// Cancel account summary. Matches `cancelAccountSummary` in C++. pub fn cancel_account_summary(&self, req_id: i64)
/// Subscribe to account updates. Matches `reqAccountUpdates` in C++. pub fn req_account_updates(&self, subscribe: bool, _acct_code: &str)
/// Cancel positions subscription. Matches `cancelPositions` in C++. pub fn cancel_positions(&self)
/// Request managed accounts. Matches `reqManagedAccts` in C++. pub fn req_managed_accts(&self, wrapper: &mut impl Wrapper)
/// Request account updates for multiple accounts/models. Matches `reqAccountUpdatesMulti` in C++. pub fn req_account_updates_multi( &self, _req_id: i64, _account: &str, _model_code: &str, _ledger_and_nlv: bool, wrapper: &mut impl Wrapper, )
/// Cancel multi-account updates. Matches `cancelAccountUpdatesMulti` in C++. pub fn cancel_account_updates_multi(&self, _req_id: i64)
/// Request positions for multiple accounts/models. Matches `reqPositionsMulti` in C++. pub fn req_positions_multi( &self, _req_id: i64, _account: &str, _model_code: &str, wrapper: &mut impl Wrapper, )
/// Cancel multi-account positions. Matches `cancelPositionsMulti` in C++. pub fn cancel_positions_multi(&self, _req_id: i64)
/// Read account state snapshot. pub fn account(&self) -> AccountState
/// Place an order. Matches `placeOrder` in C++. pub fn place_order(&self, order_id: i64, contract: &Contract, order: &Order) -> Result<(), String>
/// Cancel an order. Matches `cancelOrder` in C++. pub fn cancel_order(&self, order_id: i64, _manual_order_cancel_time: &str) -> Result<(), String>
/// Cancel all orders. Matches `reqGlobalCancel` in C++. pub fn req_global_cancel(&self) -> Result<(), String>
/// Request next valid order ID. Matches `reqIds` in C++. pub fn req_ids(&self, wrapper: &mut impl Wrapper)
/// Get the next order ID (local counter). pub fn next_order_id(&self) -> i64
/// Request open orders for this client. Matches `reqOpenOrders` in C++. pub fn req_open_orders(&self, wrapper: &mut impl Wrapper)
/// Request all open orders. Matches `reqAllOpenOrders` in C++. pub fn req_all_open_orders(&self, wrapper: &mut impl Wrapper)
/// Request completed orders. Matches `reqCompletedOrders` in C++. /// Immediately delivers all archived completed orders, then calls `completed_orders_end`. pub fn req_completed_orders(&self, wrapper: &mut impl Wrapper)
/// Automatically bind future orders to this client. Matches `reqAutoOpenOrders` in C++. pub fn req_auto_open_orders(&self, _b_auto_bind: bool)
/// Request execution reports. Matches `reqExecutions` in C++. /// Replays stored executions (optionally filtered), firing `exec_details` + /// `commission_report` for each, then `exec_details_end`. pub fn req_executions(&self, req_id: i64, filter: &ExecutionFilter, wrapper: &mut impl Wrapper)
/// Parse algo strategy and TagValue params into internal AlgoParams. pub fn parse_algo_params(strategy: &str, params: &[TagValue]) -> Result<AlgoParams, String>
/// Subscribe to market data. Matches `reqMktData` in C++. /// When `snapshot` is true, delivers the first available quote then calls /// `tick_snapshot_end` and auto-cancels the subscription. pub fn req_mkt_data( &self, req_id: i64, contract: &Contract, generic_tick_list: &str, snapshot: bool, _regulatory_snapshot: bool, ) -> Result<(), String>
/// Cancel market data. Matches `cancelMktData` in C++. pub fn cancel_mkt_data(&self, req_id: i64) -> Result<(), String>
/// Subscribe to tick-by-tick data. Matches `reqTickByTickData` in C++. pub fn req_tick_by_tick_data( &self, req_id: i64, contract: &Contract, tick_type: &str, _number_of_ticks: i32, _ignore_size: bool, ) -> Result<(), String>
/// Cancel tick-by-tick data. Matches `cancelTickByTickData` in C++. pub fn cancel_tick_by_tick_data(&self, req_id: i64) -> Result<(), String>
/// Subscribe to market depth (L2 order book). Matches `reqMktDepth` in C++. pub fn req_mkt_depth( &self, req_id: i64, contract: &Contract, num_rows: i32, is_smart_depth: bool, ) -> Result<(), String>
/// Cancel market depth. Matches `cancelMktDepth` in C++. pub fn cancel_mkt_depth(&self, req_id: i64) -> Result<(), String>
pub fn req_real_time_bars( &self, req_id: i64, contract: &Contract, _bar_size: i32, what_to_show: &str, use_rth: bool, ) -> Result<(), String>
pub fn cancel_real_time_bars(&self, req_id: i64) -> Result<(), String>
/// Set market data type preference (1=live, 2=frozen, 3=delayed, 4=delayed-frozen). pub fn req_market_data_type(&self, market_data_type: i32)
/// Set news provider codes for per-contract news ticks. pub fn set_news_providers(&self, providers: &str)
pub fn quote(&self, req_id: i64) -> Option<Quote>
pub fn quote_by_instrument(&self, instrument: InstrumentId) -> Quote
/// Request historical data. Matches `reqHistoricalData` in C++. pub fn req_historical_data( &self, req_id: i64, contract: &Contract, end_date_time: &str, duration: &str, bar_size: &str, what_to_show: &str, use_rth: bool, _format_date: i32, _keep_up_to_date: bool, ) -> Result<(), String>
/// Cancel historical data. Matches `cancelHistoricalData` in C++. pub fn cancel_historical_data(&self, req_id: i64) -> Result<(), String>
/// Request head timestamp. Matches `reqHeadTimeStamp` in C++. pub fn req_head_time_stamp( &self, req_id: i64, contract: &Contract, what_to_show: &str, use_rth: bool, _format_date: i32, ) -> Result<(), String>
/// Request contract details. Matches `reqContractDetails` in C++. pub fn req_contract_details(&self, req_id: i64, contract: &Contract) -> Result<(), String>
/// Request available exchanges for market depth. pub fn req_mkt_depth_exchanges(&self) -> Result<(), String>
/// Request matching symbols. Matches `reqMatchingSymbols` in C++. pub fn req_matching_symbols(&self, req_id: i64, pattern: &str) -> Result<(), String>
/// Cancel head timestamp request. Matches `cancelHeadTimestamp` in C++. pub fn cancel_head_time_stamp(&self, req_id: i64) -> Result<(), String>
/// Request market rule by ID. Matches `reqMarketRule` in C++. /// Looks up cached market rules delivered during connection init. pub fn req_market_rule(&self, market_rule_id: i32, wrapper: &mut impl crate::api::wrapper::Wrapper)
/// Subscribe to news bulletins. Matches `reqNewsBulletins` in C++. pub fn req_news_bulletins(&self, _all_msgs: bool)
/// Cancel news bulletin subscription. Matches `cancelNewsBulletins` in C++. pub fn cancel_news_bulletins(&self)
pub fn req_scanner_parameters(&self) -> Result<(), String>
pub fn req_scanner_subscription( &self, req_id: i64, instrument: &str, location_code: &str, scan_code: &str, max_items: u32, ) -> Result<(), String>
pub fn cancel_scanner_subscription(&self, req_id: i64) -> Result<(), String>
pub fn req_historical_news( &self, req_id: i64, con_id: i64, provider_codes: &str, start_time: &str, end_time: &str, max_results: u32, ) -> Result<(), String>
pub fn req_news_article(&self, req_id: i64, provider_code: &str, article_id: &str) -> Result<(), String>
pub fn req_fundamental_data(&self, req_id: i64, contract: &Contract, report_type: &str) -> Result<(), String>
pub fn cancel_fundamental_data(&self, req_id: i64) -> Result<(), String>
pub fn req_histogram_data(&self, req_id: i64, contract: &Contract, use_rth: bool, period: &str) -> Result<(), String>
pub fn cancel_histogram_data(&self, req_id: i64) -> Result<(), String>
pub fn req_historical_ticks( &self, req_id: i64, contract: &Contract, start_date_time: &str, end_date_time: &str, number_of_ticks: i32, what_to_show: &str, use_rth: bool, ) -> Result<(), String>
pub fn req_historical_schedule( &self, req_id: i64, contract: &Contract, end_date_time: &str, duration: &str, use_rth: bool, ) -> Result<(), String>
/// Request smart routing components for a BBO exchange. Matches `reqSmartComponents` in C++. /// Gateway-local — returns component exchanges from init data. pub fn req_smart_components(&self, req_id: i64, _bbo_exchange: &str, wrapper: &mut impl Wrapper)
/// Request available news providers. Matches `reqNewsProviders` in C++. /// Gateway-local — returns provider list from init data. pub fn req_news_providers(&self, wrapper: &mut impl Wrapper)
/// Request current server time. Matches `reqCurrentTime` in C++. /// Returns local system time (no server round-trip). pub fn req_current_time(&self, wrapper: &mut impl Wrapper)
/// Request FA data. Not yet implemented. pub fn request_fa(&self, _fa_data_type: i32)
/// Replace FA data. Not yet implemented. pub fn replace_fa(&self, _req_id: i64, _fa_data_type: i32, _cxml: &str)
/// Query display groups. Not yet implemented. pub fn query_display_groups(&self, _req_id: i64)
/// Subscribe to display group events. Not yet implemented. pub fn subscribe_to_group_events(&self, _req_id: i64, _group_id: i32)
/// Unsubscribe from display group events. Not yet implemented. pub fn unsubscribe_from_group_events(&self, _req_id: i64)
/// Update display group. Not yet implemented. pub fn update_display_group(&self, _req_id: i64, _contract_info: &str)
/// Request soft dollar tiers. Matches `reqSoftDollarTiers` in C++. /// Gateway-local — returns tiers parsed from CCP logon tag 6560. pub fn req_soft_dollar_tiers(&self, req_id: i64, wrapper: &mut impl Wrapper)
/// Request family codes. Matches `reqFamilyCodes` in C++. /// Gateway-local — returns codes parsed from CCP logon tag 6823. pub fn req_family_codes(&self, wrapper: &mut impl Wrapper)
/// Set server log level. Matches `setServerLogLevel` in C++. pub fn set_server_log_level(&self, log_level: i32)
/// Request user info. Matches `reqUserInfo` in C++. /// Gateway-local — returns whiteBrandingId from CCP logon. pub fn req_user_info(&self, req_id: i64, wrapper: &mut impl Wrapper)
/// Request WSH metadata. Not yet implemented. pub fn req_wsh_meta_data(&self, _req_id: i64)
/// Request WSH event data. Not yet implemented. pub fn req_wsh_event_data(&self, _req_id: i64)
```
</details>

<details>
<summary>Wrapper</summary>

```rust
fn connect_ack(...)
fn connection_closed(...)
fn next_valid_id(...)
fn managed_accounts(...)
fn error(...)
fn current_time(...)
fn tick_price(...)
fn tick_size(...)
fn tick_string(...)
fn tick_generic(...)
fn tick_snapshot_end(...)
fn market_data_type(...)
fn order_status(...)
fn open_order(...)
fn open_order_end(...)
fn exec_details(...)
fn exec_details_end(...)
fn commission_report(...)
fn update_account_value(...)
fn update_portfolio(...)
fn update_account_time(...)
fn account_download_end(...)
fn account_summary(...)
fn account_summary_end(...)
fn position(...)
fn position_end(...)
fn pnl(...)
fn pnl_single(...)
fn historical_data(...)
fn historical_data_end(...)
fn historical_data_update(...)
fn head_timestamp(...)
fn contract_details(...)
fn contract_details_end(...)
fn symbol_samples(...)
fn tick_by_tick_all_last(...)
fn tick_by_tick_bid_ask(...)
fn tick_by_tick_mid_point(...)
fn scanner_data(...)
fn scanner_data_end(...)
fn scanner_parameters(...)
fn update_news_bulletin(...)
fn tick_news(...)
fn historical_news(...)
fn historical_news_end(...)
fn news_article(...)
fn real_time_bar(...)
fn historical_ticks(...)
fn histogram_data(...)
fn market_rule(...)
fn completed_order(...)
fn completed_orders_end(...)
fn historical_schedule(...)
fn fundamental_data(...)
fn update_mkt_depth(...)
fn update_mkt_depth_l2(...)
fn mkt_depth_exchanges(...)
fn tick_req_params(...)
fn smart_components(...)
fn news_providers(...)
fn soft_dollar_tiers(...)
fn family_codes(...)
fn user_info(...)
```
</details>
