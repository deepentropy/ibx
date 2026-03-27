# API Coverage Matrix (v0.4.2)

*Auto-generated from source — do not edit.*

Canonical IB API methods vs ibx implementation status.

- **Y** = Implemented
- **STUB** = Accepts call but not wired to server (logs warning or no-op)
- **-** = Not present

## Summary

| | IB API | Rust | Python |
|---|:---:|:---:|:---:|
| **EClient methods** | 77 | 63 impl, 8 stub | 63 impl, 14 stub |
| **EWrapper callbacks** | 81 | 63 impl, 0 stub | 70 impl, 11 stub |

## EClient Methods

| Category | IB API Method | C++ Name | Rust | Python |
|----------|---------------|----------|:----:|:------:|
| Connection | `connect` | `eConnect` | Y | Y |
|  | `disconnect` | `eDisconnect` | Y | Y |
|  | `is_connected` | `isConnected` | Y | Y |
|  | `set_server_log_level` | `setServerLogLevel` | Y | Y |
|  | `req_current_time` | `reqCurrentTime` | Y | Y |
| Market Data | `req_mkt_data` | `reqMktData` | Y | Y |
|  | `cancel_mkt_data` | `cancelMktData` | Y | Y |
|  | `req_market_data_type` | `reqMarketDataType` | Y | Y |
|  | `req_tick_by_tick_data` | `reqTickByTickData` | Y | Y |
|  | `cancel_tick_by_tick_data` | `cancelTickByTickData` | Y | Y |
|  | `req_mkt_depth` | `reqMktDepth` | Y | Y |
|  | `cancel_mkt_depth` | `cancelMktDepth` | Y | Y |
|  | `req_mkt_depth_exchanges` | `reqMktDepthExchanges` | Y | Y |
|  | `req_smart_components` | `reqSmartComponents` | Y | Y |
|  | `req_real_time_bars` | `reqRealTimeBars` | Y | Y |
|  | `cancel_real_time_bars` | `cancelRealTimeBars` | Y | Y |
| Historical Data | `req_historical_data` | `reqHistoricalData` | Y | Y |
|  | `cancel_historical_data` | `cancelHistoricalData` | Y | Y |
|  | `req_head_time_stamp` | `reqHeadTimeStamp` | Y | Y |
|  | `cancel_head_time_stamp` | `cancelHeadTimestamp` | Y | Y |
|  | `req_historical_ticks` | `reqHistoricalTicks` | Y | Y |
|  | `req_histogram_data` | `reqHistogramData` | Y | Y |
|  | `cancel_histogram_data` | `cancelHistogramData` | Y | Y |
|  | `req_historical_schedule` | `reqHistoricalSchedule` | Y | Y |
| Orders | `place_order` | `placeOrder` | Y | Y |
|  | `cancel_order` | `cancelOrder` | Y | Y |
|  | `req_open_orders` | `reqOpenOrders` | Y | Y |
|  | `req_all_open_orders` | `reqAllOpenOrders` | Y | Y |
|  | `req_auto_open_orders` | `reqAutoOpenOrders` | Y | Y |
|  | `req_ids` | `reqIds` | Y | Y |
|  | `req_global_cancel` | `reqGlobalCancel` | Y | Y |
|  | `req_completed_orders` | `reqCompletedOrders` | Y | Y |
| Executions | `req_executions` | `reqExecutions` | Y | Y |
| Account | `req_account_updates` | `reqAccountUpdates` | Y | Y |
|  | `req_account_summary` | `reqAccountSummary` | Y | Y |
|  | `cancel_account_summary` | `cancelAccountSummary` | Y | Y |
|  | `req_positions` | `reqPositions` | Y | Y |
|  | `cancel_positions` | `cancelPositions` | Y | Y |
|  | `req_pnl` | `reqPnL` | Y | Y |
|  | `cancel_pnl` | `cancelPnL` | Y | Y |
|  | `req_pnl_single` | `reqPnLSingle` | Y | Y |
|  | `cancel_pnl_single` | `cancelPnLSingle` | Y | Y |
|  | `req_managed_accts` | `reqManagedAccts` | Y | Y |
|  | `req_account_updates_multi` | `reqAccountUpdatesMulti` | Y | Y |
|  | `cancel_account_updates_multi` | `cancelAccountUpdatesMulti` | Y | Y |
|  | `req_positions_multi` | `reqPositionsMulti` | Y | Y |
|  | `cancel_positions_multi` | `cancelPositionsMulti` | Y | Y |
| Contract | `req_contract_details` | `reqContractDetails` | Y | Y |
|  | `req_matching_symbols` | `reqMatchingSymbols` | Y | Y |
|  | `req_market_rule` | `reqMarketRule` | Y | Y |
| Scanner | `req_scanner_parameters` | `reqScannerParameters` | Y | Y |
|  | `req_scanner_subscription` | `reqScannerSubscription` | Y | Y |
|  | `cancel_scanner_subscription` | `cancelScannerSubscription` | Y | Y |
| News | `req_news_providers` | `reqNewsProviders` | Y | Y |
|  | `req_news_article` | `reqNewsArticle` | Y | Y |
|  | `req_historical_news` | `reqHistoricalNews` | Y | Y |
|  | `req_news_bulletins` | `reqNewsBulletins` | Y | Y |
|  | `cancel_news_bulletins` | `cancelNewsBulletins` | Y | Y |
| Fundamental | `req_fundamental_data` | `reqFundamentalData` | Y | Y |
|  | `cancel_fundamental_data` | `cancelFundamentalData` | Y | Y |
| Options | `calculate_implied_volatility` | `calculateImpliedVolatility` | - | STUB |
|  | `cancel_calculate_implied_volatility` | `cancelCalculateImpliedVolatility` | - | STUB |
|  | `calculate_option_price` | `calculateOptionPrice` | - | STUB |
|  | `cancel_calculate_option_price` | `cancelCalculateOptionPrice` | - | STUB |
|  | `exercise_options` | `exerciseOptions` | - | STUB |
|  | `req_sec_def_opt_params` | `reqSecDefOptParams` | - | STUB |
| Reference | `req_soft_dollar_tiers` | `reqSoftDollarTiers` | Y | Y |
|  | `req_family_codes` | `reqFamilyCodes` | Y | Y |
|  | `req_user_info` | `reqUserInfo` | Y | Y |
| Financial Advisor | `request_fa` | `requestFA` | STUB | STUB |
|  | `replace_fa` | `replaceFA` | STUB | STUB |
| Display Groups | `query_display_groups` | `queryDisplayGroups` | STUB | STUB |
|  | `subscribe_to_group_events` | `subscribeToGroupEvents` | STUB | STUB |
|  | `unsubscribe_from_group_events` | `unsubscribeFromGroupEvents` | STUB | STUB |
|  | `update_display_group` | `updateDisplayGroup` | STUB | STUB |
| WSH | `req_wsh_meta_data` | `reqWshMetaData` | STUB | STUB |
|  | `req_wsh_event_data` | `reqWshEventData` | STUB | STUB |

## EWrapper Callbacks

| Category | Callback | Rust | Python |
|----------|----------|:----:|:------:|
| Connection | `connect_ack` | Y | Y |
|  | `connection_closed` | Y | Y |
|  | `next_valid_id` | Y | Y |
|  | `managed_accounts` | Y | Y |
|  | `error` | Y | Y |
|  | `current_time` | Y | Y |
| Market Data | `tick_price` | Y | Y |
|  | `tick_size` | Y | Y |
|  | `tick_string` | Y | Y |
|  | `tick_generic` | Y | Y |
|  | `tick_snapshot_end` | Y | Y |
|  | `market_data_type` | Y | Y |
|  | `tick_req_params` | Y | Y |
| Orders | `order_status` | Y | Y |
|  | `open_order` | Y | Y |
|  | `open_order_end` | Y | Y |
|  | `order_bound` | - | STUB |
| Executions | `exec_details` | Y | Y |
|  | `exec_details_end` | Y | Y |
|  | `commission_report` | Y | Y |
| Account | `update_account_value` | Y | Y |
|  | `update_portfolio` | Y | Y |
|  | `update_account_time` | Y | Y |
|  | `account_download_end` | Y | Y |
|  | `account_summary` | Y | Y |
|  | `account_summary_end` | Y | Y |
|  | `position` | Y | Y |
|  | `position_end` | Y | Y |
|  | `pnl` | Y | Y |
|  | `pnl_single` | Y | Y |
|  | `position_multi` | - | Y |
|  | `position_multi_end` | - | Y |
|  | `account_update_multi` | - | Y |
|  | `account_update_multi_end` | - | Y |
| Contract | `contract_details` | Y | Y |
|  | `contract_details_end` | Y | Y |
|  | `bond_contract_details` | - | STUB |
|  | `symbol_samples` | Y | Y |
| Historical Data | `historical_data` | Y | Y |
|  | `historical_data_end` | Y | Y |
|  | `historical_data_update` | Y | Y |
|  | `head_timestamp` | Y | Y |
|  | `historical_ticks` | Y | Y |
|  | `historical_ticks_bid_ask` | - | Y |
|  | `historical_ticks_last` | - | Y |
|  | `histogram_data` | Y | Y |
|  | `historical_schedule` | Y | Y |
| Market Depth | `update_mkt_depth` | Y | Y |
|  | `update_mkt_depth_l2` | Y | Y |
|  | `mkt_depth_exchanges` | Y | Y |
| Tick-by-Tick | `tick_by_tick_all_last` | Y | Y |
|  | `tick_by_tick_bid_ask` | Y | Y |
|  | `tick_by_tick_mid_point` | Y | Y |
| Scanner | `scanner_data` | Y | Y |
|  | `scanner_data_end` | Y | Y |
|  | `scanner_parameters` | Y | Y |
| News | `news_providers` | Y | Y |
|  | `news_article` | Y | Y |
|  | `historical_news` | Y | Y |
|  | `historical_news_end` | Y | Y |
|  | `tick_news` | Y | Y |
|  | `update_news_bulletin` | Y | Y |
| Real-Time Bars | `real_time_bar` | Y | Y |
| Fundamental | `fundamental_data` | Y | Y |
| Market Rules | `market_rule` | Y | Y |
| Completed Orders | `completed_order` | Y | Y |
|  | `completed_orders_end` | Y | Y |
| Options | `tick_option_computation` | - | Y |
|  | `security_definition_option_parameter` | - | STUB |
|  | `security_definition_option_parameter_end` | - | STUB |
| Reference | `smart_components` | Y | Y |
|  | `soft_dollar_tiers` | Y | Y |
|  | `family_codes` | Y | Y |
|  | `user_info` | Y | Y |
| FA | `receive_fa` | - | STUB |
|  | `replace_fa_end` | - | STUB |
| Display Groups | `display_group_list` | - | STUB |
|  | `display_group_updated` | - | STUB |
| Other | `delta_neutral_validation` | - | STUB |
| WSH | `wsh_meta_data` | - | STUB |
|  | `wsh_event_data` | - | STUB |
