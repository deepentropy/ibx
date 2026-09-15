<p align="center">
  <img src="docs/book/src/banner.png" alt="IBX" width="100%">
</p>

<p align="center">
  <strong>An Interactive Brokers client with no gateway. No JVM, no window, no process to keep alive.</strong>
</p>

<p align="center">
  <a href="https://github.com/userFRM/ibx/actions"><img src="https://github.com/userFRM/ibx/actions/workflows/tests.yml/badge.svg" alt="Build"></a>
  <img src="https://img.shields.io/badge/rust-1.89+-orange.svg" alt="Rust version">
  <img src="https://img.shields.io/badge/python-3.11+-blue.svg" alt="Python version">
  <a href="LICENSE"><img src="https://img.shields.io/badge/license-AGPL--3.0-blue.svg" alt="License"></a>
  <a href="https://userfrm.github.io/ibx/"><img src="https://img.shields.io/badge/docs-book-green.svg" alt="Docs"></a>
</p>

## Introduction

IBX implements the IBKR client protocol directly. It authenticates, maintains
the market-data, trading, historical and security-definition connections, and
exposes the same API a program would otherwise reach through IB Gateway — with
no gateway process, JVM, or local socket in between.

The API is source-compatible with the TWS API (`EClient` / `EWrapper`) and, in
Python, additionally with [ib_async](https://github.com/ib-api-reloaded/ib_async)
(`IB`). Migrating an existing program changes the connect call:

```diff
- ib.connect("127.0.0.1", 4001, clientId=1)     # requires a running gateway
+ ib.connect(username="...", password="...")    # no external process
```

### What this removes

* **The gateway process** — nothing to install, launch, log into, or restart
* **The JVM** — no heap to size, no crash on bulk data
* **The localhost socket** — ticks are delivered in-process
* **The window** — runs headless, in a container, over ssh

### Requirements

* An Interactive Brokers account, paper or live
* Rust 1.89+ for the Rust client
* Python 3.11+ for the bindings

No IB software is required. The `ibapi` package is not needed either.

<!-- capabilities:begin — written by scripts/gen_parity_matrix.py -->

## Capabilities

One row per capability, one column per client — every one of the 78 calls and 90 callbacks the documented API names, read from each client rather than recalled.

| Client | Calls carried | Callbacks carried | |
| --- | ---: | ---: | --- |
| TWS API | 78 / 78 | 90 / 90 | nothing missing |
| ibapi | 73 / 78 | 85 / 90 | 5 absent, 5 callbacks absent |
| ib_async | 77 / 78 | 78 / 90 | 1 absent, 12 callbacks absent |
| **ibx Rust** | **78 / 78** | **81 / 90** | 9 callbacks taken, not applied |
| **ibx Python** | **78 / 78** | **81 / 90** | 9 callbacks taken, not applied |

**Nothing on that list is absent here.** 9 exist and never fire — there is no terminal between this client and the venue to make a verification handshake with, and no socket layer of the reference client's own to report an error from — and each says so where it is declared, so a program that implements one still compiles and runs. Everything else is carried.

**And 75 more beyond that list.** The connection a terminal opens carries more than the documented calls describe — what the venue permits this account, which algorithms it offers, the order defaults it fills an order's blanks from, what it says about an issuer, which session holds the account — and a client that speaks that connection can answer them. Most have no call in the documented API at all; a few are one a reference client happens to name too, and the table below marks which is which, under *Beyond the canonical list*.

Every figure here is read from the client it names, on the machine that generated it. A client that is not installed is left out rather than filled in from memory.

<details>
<summary><b>The whole table — every call, every callback, and what the gateway connection carries beyond them</b></summary>


One row per capability, one column per client. This client exists to be
put where a gateway was, so the question is not what it can do but
whether anything you already use is missing — which is a question about
the columns, not about the rows.

**Every row is something the gateway connection carries.** Most of them
the documented API names too, and those are the rows a port has to
match. The last table is the rest: what that connection carries and the
documented API never named — the terminal reads it, so it is on the
wire, and a client that speaks the wire can answer it.

| Mark | Meaning |
| :---: | --- |
| ● | Carried: the call exists and does what it says |
| ◐ | Taken and not applied: the call exists and reports why it cannot be served, rather than failing to exist |
| · | Absent: no such call |

Each column is read from the client it names, on the machine that
generated this page:

- **Gateway wire** — the connection a terminal opens. Every row is something it carries: either the documented API names it, or this client was written after reading it off that connection.
- **TWS API** — the documented surface, as this repository generates it from source. Where this column is empty and the one beside it is not, the connection carries something the documented API never named.
- **ibapi** — IBKR's own Python client, version `9.81.1-1`, imported and enumerated. This is the copy published to PyPI; the version IBKR distributes directly is numbered 10.x and names calls this one predates, so a gap in this column is a gap in the copy that was read and not necessarily in the client you have.
- **ib_async** — version `2.1.0`, imported and enumerated across both the transport and the facade, because it carries some calls on one and some on the other.

A client that is not installed is left out of the table rather than
filled in from memory. A mark here is a thing that was read.

## Calls

What a program asks the venue for.

| Category | Call | Gateway wire | TWS API | ibapi | ib_async | ibx Rust | ibx Python |
| --- | --- | :---: | :---: | :---: | :---: | :---: | :---: |
| Connection | `connect` | ● | ● | ● | ● | ● | ● |
|  | `disconnect` | ● | ● | ● | ● | ● | ● |
|  | `is_connected` | ● | ● | ● | ● | ● | ● |
|  | `set_server_log_level` | ● | ● | ● | ● | ● | ● |
|  | `req_current_time` | ● | ● | ● | ● | ● | ● |
|  | `req_current_time_in_millis` | ● | ● | · | · | ● | ● |
| Market Data | `req_mkt_data` | ● | ● | ● | ● | ● | ● |
|  | `cancel_mkt_data` | ● | ● | ● | ● | ● | ● |
|  | `req_market_data_type` | ● | ● | ● | ● | ● | ● |
|  | `req_tick_by_tick_data` | ● | ● | ● | ● | ● | ● |
|  | `cancel_tick_by_tick_data` | ● | ● | ● | ● | ● | ● |
|  | `req_mkt_depth` | ● | ● | ● | ● | ● | ● |
|  | `cancel_mkt_depth` | ● | ● | ● | ● | ● | ● |
|  | `req_mkt_depth_exchanges` | ● | ● | ● | ● | ● | ● |
|  | `req_smart_components` | ● | ● | ● | ● | ● | ● |
|  | `req_real_time_bars` | ● | ● | ● | ● | ● | ● |
|  | `cancel_real_time_bars` | ● | ● | ● | ● | ● | ● |
| Historical Data | `req_historical_data` | ● | ● | ● | ● | ● | ● |
|  | `cancel_historical_data` | ● | ● | ● | ● | ● | ● |
|  | `req_head_time_stamp` | ● | ● | ● | ● | ● | ● |
|  | `cancel_head_time_stamp` | ● | ● | ● | ● | ● | ● |
|  | `req_historical_ticks` | ● | ● | ● | ● | ● | ● |
|  | `req_histogram_data` | ● | ● | ● | ● | ● | ● |
|  | `cancel_histogram_data` | ● | ● | ● | ● | ● | ● |
|  | `req_historical_schedule` | ● | ● | · | ● | ● | ● |
| Orders | `place_order` | ● | ● | ● | ● | ● | ● |
|  | `cancel_order` | ● | ● | ● | ● | ● | ● |
|  | `req_open_orders` | ● | ● | ● | ● | ● | ● |
|  | `req_all_open_orders` | ● | ● | ● | ● | ● | ● |
|  | `req_auto_open_orders` | ● | ● | ● | ● | ● | ● |
|  | `req_ids` | ● | ● | ● | ● | ● | ● |
|  | `req_global_cancel` | ● | ● | ● | ● | ● | ● |
|  | `req_completed_orders` | ● | ● | ● | ● | ● | ● |
| Executions | `req_executions` | ● | ● | ● | ● | ● | ● |
| Account | `req_account_updates` | ● | ● | ● | ● | ● | ● |
|  | `req_account_summary` | ● | ● | ● | ● | ● | ● |
|  | `cancel_account_summary` | ● | ● | ● | ● | ● | ● |
|  | `req_positions` | ● | ● | ● | ● | ● | ● |
|  | `cancel_positions` | ● | ● | ● | ● | ● | ● |
|  | `req_pnl` | ● | ● | ● | ● | ● | ● |
|  | `cancel_pnl` | ● | ● | ● | ● | ● | ● |
|  | `req_pnl_single` | ● | ● | ● | ● | ● | ● |
|  | `cancel_pnl_single` | ● | ● | ● | ● | ● | ● |
|  | `req_managed_accts` | ● | ● | ● | ● | ● | ● |
|  | `req_account_updates_multi` | ● | ● | ● | ● | ● | ● |
|  | `cancel_account_updates_multi` | ● | ● | ● | ● | ● | ● |
|  | `req_positions_multi` | ● | ● | ● | ● | ● | ● |
|  | `cancel_positions_multi` | ● | ● | ● | ● | ● | ● |
| Contract | `req_contract_details` | ● | ● | ● | ● | ● | ● |
|  | `req_matching_symbols` | ● | ● | ● | ● | ● | ● |
|  | `req_market_rule` | ● | ● | ● | ● | ● | ● |
| Scanner | `req_scanner_parameters` | ● | ● | ● | ● | ● | ● |
|  | `req_scanner_subscription` | ● | ● | ● | ● | ● | ● |
|  | `cancel_scanner_subscription` | ● | ● | ● | ● | ● | ● |
| News | `req_news_providers` | ● | ● | ● | ● | ● | ● |
|  | `req_news_article` | ● | ● | ● | ● | ● | ● |
|  | `req_historical_news` | ● | ● | ● | ● | ● | ● |
|  | `req_news_bulletins` | ● | ● | ● | ● | ● | ● |
|  | `cancel_news_bulletins` | ● | ● | ● | ● | ● | ● |
| Fundamental | `req_fundamental_data` | ● | ● | ● | ● | ● | ● |
|  | `cancel_fundamental_data` | ● | ● | ● | ● | ● | ● |
| Options | `calculate_implied_volatility` | ● | ● | ● | ● | ● | ● |
|  | `cancel_calculate_implied_volatility` | ● | ● | ● | ● | ● | ● |
|  | `calculate_option_price` | ● | ● | ● | ● | ● | ● |
|  | `cancel_calculate_option_price` | ● | ● | ● | ● | ● | ● |
|  | `exercise_options` | ● | ● | ● | ● | ● | ● |
|  | `req_sec_def_opt_params` | ● | ● | ● | ● | ● | ● |
| Reference | `req_soft_dollar_tiers` | ● | ● | ● | ● | ● | ● |
|  | `req_family_codes` | ● | ● | ● | ● | ● | ● |
|  | `req_user_info` | ● | ● | · | ● | ● | ● |
| Financial Advisor | `request_fa` | ● | ● | ● | ● | ● | ● |
|  | `replace_fa` | ● | ● | ● | ● | ● | ● |
| Display Groups | `query_display_groups` | ● | ● | ● | ● | ● | ● |
|  | `subscribe_to_group_events` | ● | ● | ● | ● | ● | ● |
|  | `unsubscribe_from_group_events` | ● | ● | ● | ● | ● | ● |
|  | `update_display_group` | ● | ● | ● | ● | ● | ● |
| WSH | `req_wsh_meta_data` | ● | ● | · | ● | ● | ● |
|  | `req_wsh_event_data` | ● | ● | · | ● | ● | ● |

## Callbacks

What the venue says back. `ib_async` delivers these as events as well as methods, so a mark here says the method exists on its wrapper, not that the information is unavailable by another route.

| Category | Call | Gateway wire | TWS API | ibapi | ib_async | ibx Rust | ibx Python |
| --- | --- | :---: | :---: | :---: | :---: | :---: | :---: |
| Connection | `connect_ack` | ● | ● | ● | ● | ● | ● |
|  | `connection_closed` | ● | ● | ● | ● | ● | ● |
|  | `next_valid_id` | ● | ● | ● | ● | ● | ● |
|  | `managed_accounts` | ● | ● | ● | ● | ● | ● |
|  | `error` | ● | ● | ● | ● | ● | ● |
|  | `current_time` | ● | ● | ● | ● | ● | ● |
|  | `current_time_in_millis` | ● | ● | · | · | ● | ● |
| Market Data | `tick_price` | ● | ● | ● | · | ● | ● |
|  | `tick_size` | ● | ● | ● | ● | ● | ● |
|  | `tick_string` | ● | ● | ● | ● | ● | ● |
|  | `tick_generic` | ● | ● | ● | ● | ● | ● |
|  | `tick_snapshot_end` | ● | ● | ● | ● | ● | ● |
|  | `market_data_type` | ● | ● | ● | ● | ● | ● |
|  | `tick_req_params` | ● | ● | ● | ● | ● | ● |
| Orders | `order_status` | ● | ● | ● | ● | ● | ● |
|  | `open_order` | ● | ● | ● | ● | ● | ● |
|  | `open_order_end` | ● | ● | ● | ● | ● | ● |
|  | `order_bound` | ● | ● | ● | ● | ● | ● |
| Executions | `exec_details` | ● | ● | ● | ● | ● | ● |
|  | `exec_details_end` | ● | ● | ● | ● | ● | ● |
|  | `commission_and_fees_report` | ● | ● | ● | ● | ● | ● |
| Account | `update_account_value` | ● | ● | ● | ● | ● | ● |
|  | `update_portfolio` | ● | ● | ● | ● | ● | ● |
|  | `update_account_time` | ● | ● | ● | ● | ● | ● |
|  | `account_download_end` | ● | ● | ● | ● | ● | ● |
|  | `account_summary` | ● | ● | ● | ● | ● | ● |
|  | `account_summary_end` | ● | ● | ● | ● | ● | ● |
|  | `position` | ● | ● | ● | ● | ● | ● |
|  | `position_end` | ● | ● | ● | ● | ● | ● |
|  | `pnl` | ● | ● | ● | ● | ● | ● |
|  | `pnl_single` | ● | ● | ● | ● | ● | ● |
|  | `position_multi` | ● | ● | ● | ● | ● | ● |
|  | `position_multi_end` | ● | ● | ● | ● | ● | ● |
|  | `account_update_multi` | ● | ● | ● | ● | ● | ● |
|  | `account_update_multi_end` | ● | ● | ● | ● | ● | ● |
| Contract | `contract_details` | ● | ● | ● | ● | ● | ● |
|  | `contract_details_end` | ● | ● | ● | ● | ● | ● |
|  | `bond_contract_details` | ● | ● | ● | ● | ● | ● |
|  | `symbol_samples` | ● | ● | ● | ● | ● | ● |
| Historical Data | `historical_data` | ● | ● | ● | ● | ● | ● |
|  | `historical_data_end` | ● | ● | ● | ● | ● | ● |
|  | `historical_data_update` | ● | ● | ● | ● | ● | ● |
|  | `head_timestamp` | ● | ● | ● | ● | ● | ● |
|  | `historical_ticks` | ● | ● | ● | ● | ● | ● |
|  | `historical_ticks_bid_ask` | ● | ● | ● | ● | ● | ● |
|  | `historical_ticks_last` | ● | ● | ● | ● | ● | ● |
|  | `histogram_data` | ● | ● | ● | ● | ● | ● |
|  | `historical_schedule` | ● | ● | · | ● | ● | ● |
| Market Depth | `update_mkt_depth` | ● | ● | ● | ● | ● | ● |
|  | `update_mkt_depth_l2` | ● | ● | ● | ● | ● | ● |
|  | `mkt_depth_exchanges` | ● | ● | ● | ● | ● | ● |
| Tick-by-Tick | `tick_by_tick_all_last` | ● | ● | ● | ● | ● | ● |
|  | `tick_by_tick_bid_ask` | ● | ● | ● | ● | ● | ● |
|  | `tick_by_tick_mid_point` | ● | ● | ● | ● | ● | ● |
| Scanner | `scanner_data` | ● | ● | ● | ● | ● | ● |
|  | `scanner_data_end` | ● | ● | ● | ● | ● | ● |
|  | `scanner_parameters` | ● | ● | ● | ● | ● | ● |
| News | `news_providers` | ● | ● | ● | ● | ● | ● |
|  | `news_article` | ● | ● | ● | ● | ● | ● |
|  | `historical_news` | ● | ● | ● | ● | ● | ● |
|  | `historical_news_end` | ● | ● | ● | ● | ● | ● |
|  | `tick_news` | ● | ● | ● | ● | ● | ● |
|  | `update_news_bulletin` | ● | ● | ● | ● | ● | ● |
| Real-Time Bars | `real_time_bar` | ● | ● | ● | ● | ● | ● |
| Fundamental | `fundamental_data` | ● | ● | ● | ● | ● | ● |
| Market Rules | `market_rule` | ● | ● | ● | ● | ● | ● |
| Completed Orders | `completed_order` | ● | ● | ● | ● | ● | ● |
|  | `completed_orders_end` | ● | ● | ● | ● | ● | ● |
| Options | `tick_option_computation` | ● | ● | ● | ● | ● | ● |
|  | `security_definition_option_parameter` | ● | ● | ● | ● | ● | ● |
|  | `security_definition_option_parameter_end` | ● | ● | ● | ● | ● | ● |
| Reference | `smart_components` | ● | ● | ● | ● | ● | ● |
|  | `soft_dollar_tiers` | ● | ● | ● | ● | ● | ● |
|  | `family_codes` | ● | ● | ● | ● | ● | ● |
|  | `user_info` | ● | ● | · | ● | ● | ● |
| FA | `receive_fa` | ● | ● | ● | ● | ● | ● |
|  | `replace_fa_end` | ● | ● | ● | · | ● | ● |
| Display Groups | `display_group_list` | ● | ● | ● | · | ● | ● |
|  | `display_group_updated` | ● | ● | ● | · | ● | ● |
| Other | `delta_neutral_validation` | ● | ● | ● | ● | ◐ | ◐ |
| WSH | `wsh_meta_data` | ● | ● | · | ● | ● | ● |
|  | `wsh_event_data` | ● | ● | · | ● | ● | ● |
| Market Data | `reroute_mkt_data_req` | ● | ● | ● | · | ◐ | ◐ |
|  | `reroute_mkt_depth_req` | ● | ● | ● | · | ◐ | ◐ |
|  | `tick_efp` | ● | ● | ● | ● | ◐ | ◐ |
| Connection | `verify_message_api` | ● | ● | ● | · | ◐ | ◐ |
|  | `verify_completed` | ● | ● | ● | · | ◐ | ◐ |
|  | `verify_and_auth_message_api` | ● | ● | ● | · | ◐ | ◐ |
|  | `verify_and_auth_completed` | ● | ● | ● | · | ◐ | ◐ |
|  | `win_error` | ● | ● | ● | · | ◐ | ◐ |

## Beyond the canonical list

The terminal's own connection carries more than the documented
surface names, and this client speaks that connection — so some of
what it answers has no call in the API at all. These fall into three
kinds, and the table does not try to sort them: things the venue
states that no documented call asks for (what it permits this
account, which algorithms it offers, the order defaults it holds,
what it says about an issuer, which session holds the account); the
same question answered rather than delivered on a callback; and this
client's own instrumentation, which is about the client and not the
venue.

A mark against a reference client here means it happens to name the
same thing, not that the documented API does.

| Call | Gateway wire | TWS API | ibapi | ib_async | ibx Rust | ibx Python |
| --- | :---: | :---: | :---: | :---: | :---: | :---: |
| `account` | ● | · | · | · | ● | · |
| `accountSnapshot` | ● | · | · | · | · | ● |
| `adjustments` | ● | · | · | · | ● | · |
| `algorithms` | ● | · | · | · | ● | · |
| `algorithmsFor` | ● | · | · | · | ● | ● |
| `await_order` | ● | · | · | · | ● | · |
| `calendar_events` | ● | · | · | · | ● | · |
| `calendar_schema` | ● | · | · | · | ● | · |
| `cancel_historical_news` | ● | · | · | · | ● | · |
| `cancelOrderByPermId` | ● | · | · | · | ● | ● |
| `cancelWshEventData` | ● | ● | · | ● | ● | ● |
| `cancelWshMetaData` | ● | ● | · | ● | ● | ● |
| `ccpSessionId` | ● | · | · | · | ● | ● |
| `checkConnected` | ● | · | · | · | · | ● |
| `closingOptionModel` | ● | · | · | · | ● | ● |
| `closingOptionModelByInstrument` | ● | · | · | · | ● | ● |
| `companyData` | ● | · | · | · | ● | ● |
| `companyDataSeries` | ● | · | · | · | ● | ● |
| `competingSession` | ● | · | · | · | · | ● |
| `connect_with_events` | ● | · | · | · | ● | · |
| `contractFigures` | ● | · | · | · | ● | ● |
| `contractFiguresByInstrument` | ● | · | · | · | ● | ● |
| `corporateActions` | ● | · | · | · | ● | ● |
| `enabledFeatures` | ● | · | · | · | ● | ● |
| `eventsLost` | ● | · | · | · | ● | ● |
| `getAccountId` | ● | · | · | · | · | ● |
| `instrument_of` | ● | · | · | · | ● | · |
| `last_rtt` | ● | · | · | · | ● | · |
| `lastRttMs` | ● | · | · | · | · | ● |
| `matchingSymbols` | ● | · | · | · | ● | ● |
| `miscUrl` | ● | · | · | · | ● | ● |
| `newsHeadlines` | ● | · | · | · | ● | ● |
| `nextOrderId` | ● | · | · | · | ● | ● |
| `nextSharedId` | ● | · | · | · | · | ● |
| `numberedFigures` | ● | · | · | · | ● | ● |
| `numberedFiguresSeries` | ● | · | · | · | ● | ● |
| `option_chain` | ● | · | · | · | ● | · |
| `optionChains` | ● | · | · | · | · | ● |
| `optionModel` | ● | · | · | · | ● | ● |
| `optionModelByInstrument` | ● | · | · | · | ● | ● |
| `orderPermissions` | ● | · | · | · | ● | ● |
| `orderPresets` | ● | · | · | · | ● | ● |
| `pairedFigures` | ● | · | · | · | ● | ● |
| `pairedFiguresSeries` | ● | · | · | · | ● | ● |
| `parse_algo_params` | ● | · | · | · | ● | · |
| `permittedOrderTypes` | ● | · | · | · | ● | ● |
| `positions` | ● | ● | · | ● | ● | · |
| `positions_elsewhere` | ● | · | · | · | ● | · |
| `qualifyContract` | ● | · | · | · | ● | ● |
| `qualifyContracts` | ● | ● | · | ● | ● | ● |
| `quote` | ● | · | · | · | ● | · |
| `quoteByInstrument` | ● | · | · | · | ● | ● |
| `reqAdjustments` | ● | · | · | · | ● | ● |
| `reqMktDataEx` | ● | · | · | · | ● | ● |
| `reqPing` | ● | · | · | · | ● | ● |
| `scan` | ● | · | · | · | ● | · |
| `schedule` | ● | ● | · | ● | ● | · |
| `serverVersion` | ● | ● | ● | ● | · | ● |
| `session` | ● | · | · | · | ● | · |
| `session_over` | ● | · | · | · | ● | · |
| `session_token_bytes` | ● | · | · | · | ● | · |
| `setConnectOptions` | ● | ● | · | ● | · | ◐ |
| `setNewsProviders` | ● | · | · | · | ● | ● |
| `shared_state` | ● | · | · | · | ● | · |
| `shortSaleRestricted` | ● | · | · | · | ● | ● |
| `shortSaleRestrictedByInstrument` | ● | · | · | · | ● | ● |
| `startApi` | ● | ● | ● | ● | · | ● |
| `statedFigures` | ● | · | · | · | ● | ● |
| `statedFiguresSeries` | ● | · | · | · | ● | ● |
| `statedRows` | ● | · | · | · | ● | ● |
| `tradingSchedule` | ● | · | · | · | · | ● |
| `twsConnectionTime` | ● | ● | ● | · | · | ● |
| `unread_wire` | ● | · | · | · | ● | · |
| `values_elsewhere` | ● | · | · | · | ● | · |
| `what_if_order` | ● | ● | · | ● | ● | · |

## Calls, counted

| Client | Carried | Taken, not applied | Absent |
| --- | ---: | ---: | ---: |
| Gateway wire | 78 | 0 | 0 |
| TWS API | 78 | 0 | 0 |
| ibapi | 73 | 0 | 5 |
| ib_async | 77 | 0 | 1 |
| ibx Rust | 78 | 0 | 0 |
| ibx Python | 78 | 0 | 0 |

</details>

The same table stands on its own in [docs/capabilities.md](docs/capabilities.md), and what each claim rests on is in [docs/evidence.md](docs/evidence.md).

<!-- capabilities:end -->

## Installation

### Python

```bash
uv venv .venv --python 3.13
source .venv/bin/activate          # .venv\Scripts\activate on Windows
pip install maturin
maturin develop --features python
```

### Rust

```toml
[dependencies]
ibx = { git = "https://github.com/userFRM/ibx" }
```

## Quick start

```python
import ibx

ib = ibx.IB()
ib.connect(username="your_user", password="your_pass", paper=True)

spy = ibx.Contract(symbol="SPY", secType="STK", exchange="SMART", currency="USD")

(ticker,) = ib.reqTickers(spy)
print(ticker.bid, ticker.ask)

order = ibx.Order(action="BUY", orderType="LMT", totalQuantity=1, lmtPrice=1.00)
trade = ib.placeOrder(spy, order)
ib.sleep(2)
print(trade.orderStatus.status)
ib.cancelOrder(order)

ib.disconnect()
```

A contract does not need to be qualified first: a request carrying a contract
rather than a contract id is resolved before transmission.

In Rust:

```rust
use ibx::types::model::{Contract, Order};
use ibx::{Client, Config};

let client = Client::connect(&Config {
    username: "your_user".into(),
    password: "your_pass".into(),
    paper: true,
    ..Default::default()
})?;

let spy = client.qualify(Contract::stock("SPY"))?;

// A quote exists because something is watching it, and updates itself after.
client.watch(&spy)?;
if let Some(quote) = client.ticker(&spy) {
    println!("bid {} ask {}", quote.bid, quote.ask);
}

// The order is the thing you hold. Its number is bookkeeping the client keeps.
let order = client.place(&spy, &Order::limit("BUY", 100.0, 42.50))?;
order.wait_done(Duration::from_secs(30));
println!("{} — {} filled", order.status(), order.fills().len());
order.cancel()?;

// What the session holds, without asking for any of it.
for position in client.positions() { /* ... */ }
for value in client.account_values() { /* ... */ }
```

One thread reads the session and keeps what arrives, so a position, an order, a
fill and a quote are things you look at rather than questions you ask. The
account, its holdings and anything already working are asked for as the session
opens, so they are there to read the moment it returns.

To be told as it happens rather than reading afterwards, take a stream. Both
are iterators, so they read the way anything else in Rust reads:

```rust
for tick in client.ticks(&spy)? {
    println!("{} at {}", tick.size, tick.price);
}

for event in client.order_events() {
    println!("order {} is {}", event.order_id, event.status);
}
```

`ticks` subscribes and hands back the stream in one, and only that contract's
ticks arrive on it — a caller watching one thing does not filter out the rest.
Dropping the stream withdraws the subscription.

**There is one client.** The calls above are the ones with a shape worth
having; every other request the protocol carries — scanners, news, corporate
events, fundamentals, option chains, histograms, market rules, P&L — is on the
same `client`, in the reference client's own shape:

```rust
client.req_scanner_parameters()?;
client.req_historical_news(9001, con_id, "BRFG", "", "", 10)?;
```

Nothing to import, nothing to choose between: `Client` reaches all 135. Where a
name appears on both, the session's own is the one you get, because it is the
better answer — `positions()` reads what the session already holds rather than
asking again.

### Inside an async runtime

The engine is a thread of its own, so a blocking call holds the thread that
made it and nothing else. Inside a runtime that thread is one of a shared pool,
so the `async` feature moves each question onto a thread that may wait. What
does not wait — reading what the session holds — is not awaited:

```toml
ibx = { git = "https://github.com/userFRM/ibx", features = ["async"] }
```

```rust
let client = AsyncClient::connect(config).await?;
let spy = client.qualify(Contract::stock("SPY")).await?;

client.watch(&spy).await?;           // may have to ask about the contract
let quote = client.ticker(&spy);     // a memory read

let order = client.place(&spy, &Order::limit("BUY", 1.0, 1.0)).await?;
client.wait_done(&order, Duration::from_secs(30)).await;
```

Every question has the same name and the same answer on both, and a test fails
if either grows a name the other does not have.

## Running an existing program

### ib_async

An unmodified program written against
[ib_async](https://github.com/ib-api-reloaded/ib_async) runs on this engine
with its connect call changed. Nothing of ib_async is copied or modified —
install it as usual, and attach:

```python
from ib_async import IB, Stock
import ibx.ib_async

ib = ibx.ib_async.attach(IB(), username="your_user", password="your_pass")
ib.connect()                      # names no host: there is no gateway

spy = Stock("SPY", "SMART", "USD")
ib.qualifyContracts(spy)
bars = ib.reqHistoricalData(spy, "", "2 D", "1 hour", "TRADES", useRTH=True)

ib.pendingTickersEvent += lambda tickers: print(len(tickers), "updates")
ib.reqMktData(spy)
ib.sleep(5)
ib.disconnect()
```

Their `IB`, their `Wrapper`, their events, their types — this engine
underneath, and no gateway process.

### TWS API (`EClient` / `EWrapper`)

```python
import threading
from ibx import EWrapper, EClient, Contract

class App(EWrapper):
    def __init__(self):
        super().__init__()
        self.ready = threading.Event()

    def next_valid_id(self, order_id):
        self.next_id = order_id
        self.ready.set()

    def tick_price(self, req_id, tick_type, price, attrib):
        print(f"tick {tick_type}: {price}")

app = App()
client = EClient(app)
client.connect(username="your_user", password="your_pass", paper=True)
threading.Thread(target=client.run, daemon=True).start()
app.ready.wait(timeout=10)

aapl = Contract(symbol="AAPL", secType="STK", exchange="SMART", currency="USD")
client.req_mkt_data(1, aapl, "", False)
```

Both naming conventions resolve on every type and method: `reqMktData` and
`req_mkt_data`, `secType` and `sec_type`, `conId` and `con_id`. Both surfaces
drive one client and one engine — `ibx.IB` is a facade over `EClient`, they
share a session, and either may be used.

## Configuration

The gateway's configuration file is replaced by settings on the client:
announced build, time zone, execution-report scope, and others — 14 in total,
readable at runtime. Ten gateway settings have no counterpart and report why (no
window geometry, no local listening socket, no JVM heap, and no message pacing:
nothing here paces outgoing messages, which the gateway ships with off).

Rust: `EClientConfig.gateway`. Python: `ibx.configure()`.

## Documentation

* [The book](https://userfrm.github.io/ibx/) — guides, recipes and the generated API reference
* [Capabilities](docs/capabilities.md) — one row per capability, one column per client
* [Evidence](docs/evidence.md) — what each claim rests on, and the session that produced it
* [Notebooks](notebooks/) — the seven ib_async subjects, in the TWS API shape and in [ib_async's own](notebooks/ib_async_nogateway/)
* [Examples](examples/) — runnable single-file programs in Rust and Python

## License

[AGPL-3.0](LICENSE)

## Disclaimer

Interactive Brokers®, IBKR®, Trader Workstation®, and IB Gateway® are
registered trademarks of Interactive Brokers Group, Inc. This project is **not
affiliated with, endorsed by, or supported by Interactive Brokers**.

IBX is an independent, open-source project provided "as is", without warranty
of any kind.

### Legal Considerations

- **No warranty.** IBX is provided "as is", without warranty of any kind. See [LICENSE](LICENSE) for full terms.
- **Use at your own risk.** Users are solely responsible for ensuring their use of IBX complies with Interactive Brokers' Terms of Service, Customer Agreement, and any applicable laws or regulations. Using IBX may carry risks including but not limited to account restriction or termination by IB.
- **Not financial software.** IBX is an experimental research project. It is not intended as a replacement for officially supported IB software in production trading environments. The authors accept no liability for financial losses, missed trades, account issues, or any other damages arising from the use of this software.
- **Protocol stability.** IBX relies on an undocumented protocol that IB may change at any time without notice. There is no guarantee of continued functionality.

### EU Interoperability

For users and contributors in the European Union: Article 6 of the EU Software
Directive (2009/24/EC) permits reverse engineering for the purpose of achieving
interoperability with independently created software, provided that specific
conditions are met. IBX was developed with this legal framework in mind,
enabling interoperability with IB's trading infrastructure on platforms where
the official Java-based Gateway cannot run (headless Linux, containers,
embedded systems).
