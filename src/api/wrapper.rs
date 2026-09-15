//! ibapi-compatible Wrapper trait — Rust equivalent of C++ `EWrapper`.
//!
//! All methods have default no-op implementations. Users implement only the
//! callbacks they care about.

use crate::types::model::*;
use crate::types::HistoricalTickData;

/// ibapi-compatible callback interface. Mirrors C++ `EWrapper`.
///
/// Implement the callbacks you care about; all default to no-ops.
#[allow(unused_variables)]
pub trait Wrapper {
    // ── Connection ──

    /// The session is open. Nothing has been asked for yet.
    fn connect_ack(&mut self) {}
    /// The session is over, because this client ended it. A session that
    /// went away instead is reported on `error` under 1100.
    fn connection_closed(&mut self) {}
    /// The first order id this session may use. Each order needs one
    /// higher than the last.
    fn next_valid_id(&mut self, order_id: i64) {}
    /// Every account this login may act for, separated by commas. One
    /// for most logins; an advisor has several.
    fn managed_accounts(&mut self, accounts_list: &str) {}
    /// What the venue said about a request, under the number it says it
    /// with. Codes from 2100 to 2200 are notices about a connection rather than
    /// failures. `req_id` is -1 for anything that answers no particular request.
    ///
    /// A request this client will not send is reported here too, under the same
    /// numbers the reference client uses: 321 for a request that fails
    /// validation, 200 for a contract description that matches nothing, 504
    /// for a call made with no session.
    fn error(&mut self, req_id: i64, error_code: i64, error_string: &str, advanced_order_reject_json: &str) {}
    /// The venue's clock, in seconds since the epoch.
    fn current_time(&mut self, time: i64) {}
    /// The venue's clock, in milliseconds since the epoch.
    ///
    /// The same clock [`current_time`](Self::current_time) reports, at the
    /// precision the venue stated it in. The stamp can carry a fraction of a
    /// second and this reads it where it does — but on the sessions measured
    /// here the venue stated none, so the answer lands on a whole second and
    /// is the other call's thousandfold. Read the precision off the number
    /// rather than assuming this one has more of it.
    fn current_time_in_millis(&mut self, time_in_millis: i64) {}

    // ── Market Data ──

    /// One price of a quote, and which price it is. `tick_type` names it —
    /// 1 bid, 2 ask, 4 last, 9 close — and `attrib` says whether it can be traded
    /// against and whether it is past its limit. A size arrives on `tick_size`
    /// under the type that belongs to it.
    fn tick_price(&mut self, req_id: i64, tick_type: i32, price: f64, attrib: &TickAttrib) {}
    /// One size of a quote, and which size it is: 0 bid, 3 ask, 5 last, 8
    /// the day's volume.
    fn tick_size(&mut self, req_id: i64, tick_type: i32, size: f64) {}
    /// A quote's value that is not a number — a timestamp, an exchange
    /// map, a set of ids.
    fn tick_string(&mut self, req_id: i64, tick_type: i32, value: &str) {}
    /// A quote's value that is a number and is not a price or a size —
    /// an implied volatility, an index future's premium, a halt.
    fn tick_generic(&mut self, req_id: i64, tick_type: i32, value: f64) {}
    /// A snapshot has stated everything it is going to. Only for a
    /// subscription asked for as a snapshot; a streaming one never ends.
    fn tick_snapshot_end(&mut self, req_id: i64) {}
    /// Which feed a subscription is being served from: 1 live, 2 frozen,
    /// 3 delayed, 4 delayed and frozen.
    fn market_data_type(&mut self, req_id: i64, market_data_type: i32) {}

    // ── Orders ──

    /// Where an order stands now. Fires on every change, and again on
    /// each fill. `filled` and `remaining` are shares, `avg_fill_price` the
    /// average of what has filled so far.
    fn order_status(
        &mut self, order_id: i64, status: &str, filled: f64, remaining: f64,
        avg_fill_price: f64, perm_id: i64, parent_id: i64,
        last_fill_price: f64, client_id: i64, why_held: &str, mkt_cap_price: f64,
    ) {}

    /// An order as the venue holds it, and the state it is in. Fires
    /// beside every `order_status`, when open orders are asked for, and once for
    /// a preview — where the state carries what the order would cost and no
    /// status follows, because a preview is not an order.
    fn open_order(&mut self, order_id: i64, contract: &Contract, order: &Order, order_state: &OrderState) {}
    /// Every open order has been stated.
    fn open_order_end(&mut self) {}
    /// One fill, against the order and contract it filled. What it cost
    /// arrives separately, on `commission_and_fees_report`.
    fn exec_details(&mut self, req_id: i64, contract: &Contract, execution: &Execution) {}
    /// Every execution answering this request has been stated.
    fn exec_details_end(&mut self, req_id: i64) {}
    /// What a fill cost, matched to it by execution id.
    fn commission_and_fees_report(&mut self, report: &CommissionAndFeesReport) {}

    // ── Account ──

    /// One figure the venue states about an account, in the currency it
    /// states it in. An account is stated in several currencies at once, so the
    /// same key arrives more than once.
    fn update_account_value(&mut self, key: &str, value: &str, currency: &str, account_name: &str) {}
    /// One position, as the venue values it now.
    fn update_portfolio(
        &mut self, contract: &Contract, position: f64, market_price: f64,
        market_value: f64, average_cost: f64, unrealized_pnl: f64,
        realized_pnl: f64, account_name: &str,
    ) {}
    /// When the account figures above were last stated.
    fn update_account_time(&mut self, timestamp: &str) {}
    /// The account has been fully stated. Fires once the venue has
    /// stopped adding to it, not on the first figure.
    fn account_download_end(&mut self, account: &str) {}
    /// One figure answering `req_account_summary`, in the currency the
    /// venue states it in.
    fn account_summary(&mut self, req_id: i64, account: &str, tag: &str, value: &str, currency: &str) {}
    /// Every figure answering this request has been stated.
    fn account_summary_end(&mut self, req_id: i64) {}
    /// One position held, on any account this login may act for.
    fn position(&mut self, account: &str, contract: &Contract, pos: f64, avg_cost: f64) {}
    /// Every position has been stated.
    fn position_end(&mut self) {}
    /// A holding, answering `req_positions_multi`. Separate from `position`:
    /// a caller asks per account or model and is answered per request.
    fn position_multi(
        &mut self, req_id: i64, account: &str, model_code: &str,
        contract: &Contract, pos: f64, avg_cost: f64,
    ) {
    }
    /// Every position answering this request has been stated.
    fn position_multi_end(&mut self, req_id: i64) { let _ = req_id; }
    /// An account value, answering `req_account_updates_multi`.
    fn account_update_multi(
        &mut self, req_id: i64, account: &str, model_code: &str,
        key: &str, value: &str, currency: &str,
    ) {
    }
    /// Every figure answering this request has been stated.
    fn account_update_multi_end(&mut self, req_id: i64) { let _ = req_id; }
    /// An account's running profit: today's, what is unrealised, and what
    /// has been realised.
    fn pnl(&mut self, req_id: i64, daily_pnl: f64, unrealized_pnl: f64, realized_pnl: f64) {}
    /// The same for one position, with the size held.
    fn pnl_single(&mut self, req_id: i64, pos: f64, daily_pnl: f64, unrealized_pnl: f64, realized_pnl: f64, value: f64) {}

    // ── Historical Data ──

    /// One bar answering a historical request. `bar.date` is a day for a
    /// daily bar and a moment for anything shorter, in the zone the bar carries.
    fn historical_data(&mut self, req_id: i64, bar: &BarData) {}
    /// Every bar answering this request has been stated, and the window
    /// they cover.
    fn historical_data_end(&mut self, req_id: i64, start: &str, end: &str) {}
    /// A bar that continues a `keep_up_to_date` request, after its
    /// first batch completed. The bar still forming is restated as it changes.
    fn historical_data_update(&mut self, req_id: i64, bar: &BarData) {}
    /// The earliest moment the venue holds data for a contract.
    fn head_timestamp(&mut self, req_id: i64, head_timestamp: &str) {}

    // ── Contract Details ──

    /// One contract matching a description, with everything the venue
    /// states about it. A description can match more than one.
    fn contract_details(&mut self, req_id: i64, details: &ContractDetails) {}
    /// Every contract matching this request has been stated.
    fn contract_details_end(&mut self, req_id: i64) {}
    /// Contracts whose symbol or name matches a pattern, across venues.
    fn symbol_samples(&mut self, req_id: i64, descriptions: &[ContractDescription]) {}

    // ── Tick-by-Tick ──

    /// One trade, as it happens. `tick_attrib_last` says whether it was
    /// past a limit and whether it goes unreported to the tape.
    fn tick_by_tick_all_last(
        &mut self, req_id: i64, tick_type: i32, time: i64, price: f64,
        size: f64, attrib: &TickAttribLast, exchange: &str, special_conditions: &str,
    ) {}
    /// One change to the top of the book, as it happens.
    fn tick_by_tick_bid_ask(
        &mut self, req_id: i64, time: i64, bid_price: f64, ask_price: f64,
        bid_size: f64, ask_size: f64, attrib: &TickAttribBidAsk,
    ) {}
    /// One change to the midpoint, as it happens.
    fn tick_by_tick_mid_point(&mut self, req_id: i64, time: i64, mid_point: f64) {}

    // ── Scanner ──

    /// One row of a scan, in rank order.
    fn scanner_data(
        &mut self, req_id: i64, rank: i32, details: &ContractDetails,
        distance: &str, benchmark: &str, projection: &str, legs_str: &str,
    ) {}
    /// Every row of this scan has been stated.
    fn scanner_data_end(&mut self, req_id: i64) {}
    /// Every scan the venue offers and what each can be filtered by, as
    /// the XML the venue publishes.
    fn scanner_parameters(&mut self, xml: &str) {}

    // ── News ──

    /// A notice the venue broadcasts to everyone — an exchange
    /// unavailable, a system message.
    fn update_news_bulletin(&mut self, msg_id: i64, msg_type: i32, message: &str, orig_exchange: &str) {}
    /// A headline about a contract being watched, as it is published.
    fn tick_news(
        &mut self, ticker_id: i64, timestamp: i64, provider_code: &str,
        article_id: &str, headline: &str, extra_data: &str,
    ) {}
    /// One headline from the archive.
    fn historical_news(
        &mut self, req_id: i64, time: &str, provider_code: &str,
        article_id: &str, headline: &str,
    ) {}
    /// Every headline answering this request has been stated, and
    /// whether the archive holds more.
    fn historical_news_end(&mut self, req_id: i64, has_more: bool) {}
    /// The body of one article. `article_type` is 0 for text and 1 for a
    /// binary document.
    fn news_article(&mut self, req_id: i64, article_type: i32, article_text: &str) {}

    // ── Real-Time Bars ──

    /// One five-second bar of a live stream.
    fn real_time_bar(
        &mut self, req_id: i64, date: i64, open: f64, high: f64,
        low: f64, close: f64, volume: f64, wap: f64, count: i32,
    ) {}

    // ── Historical Ticks ──

    /// Historical midpoints, in batches, until `done`.
    fn historical_ticks(&mut self, req_id: i64, ticks: &HistoricalTickData, done: bool) {}
    /// Historical quotes, in batches, until `done`.
    fn historical_ticks_bid_ask(
        &mut self, req_id: i64, ticks: &HistoricalTickData, done: bool,
    ) { let _ = (req_id, ticks, done); }
    /// Historical trades, in batches, until `done`.
    fn historical_ticks_last(
        &mut self, req_id: i64, ticks: &HistoricalTickData, done: bool,
    ) { let _ = (req_id, ticks, done); }

    // ── Option Computations / Definitions ──

    /// The venue's model for an option: the volatility its price
    /// implies, the greeks, and what the model says the option and its
    /// underlying are worth.
    fn tick_option_computation(
        &mut self, req_id: i64, tick_type: i32, tick_attrib: i32,
        implied_vol: f64, delta: f64, opt_price: f64, pv_dividend: f64,
        gamma: f64, vega: f64, theta: f64, und_price: f64,
    ) {
    }
    /// The display groups this client offers, `|`-separated.
    fn display_group_list(&mut self, req_id: i64, groups: &str) {
    }
    /// The contract a display group now holds, as `conId@exchange`, or `none`.
    fn display_group_updated(&mut self, req_id: i64, contract_info: &str) {
    }
    /// A bond's contract details, answering `req_contract_details` for fixed
    /// income: a bond, a bill, and the type the venue spells `FIXED`. Every
    /// other type answers on `contract_details`.
    fn bond_contract_details(&mut self, req_id: i64, details: &ContractDetails) {
    }
    /// The permanent id an order was given, paired with the id this client used.
    fn order_bound(&mut self, perm_id: i64, client_id: i64, order_id: i64) {
    }
    /// An advisor's allocation groups, profiles or aliases, as XML.
    fn receive_fa(&mut self, fa_data_type: i32, cxml: &str) {
    }
    /// The end of a `replace_fa` exchange.
    fn replace_fa_end(&mut self, req_id: i64, text: &str) {
    }
    /// What the event calendar can answer about, as JSON.
    fn wsh_meta_data(&mut self, req_id: i64, data_json: &str) {
    }
    /// Calendar events, as JSON.
    fn wsh_event_data(&mut self, req_id: i64, data_json: &str) {
    }
    /// One venue's option chain for an underlying: the
    /// expiries and strikes it lists.
    fn security_definition_option_parameter(
        &mut self, req_id: i64, exchange: &str, underlying_con_id: i64,
        trading_class: &str, multiplier: &str, expirations: &[String], strikes: &[f64],
    ) {
    }
    /// Every venue's chain has been stated.
    fn security_definition_option_parameter_end(&mut self, req_id: i64) { let _ = req_id; }

    // ── Delta-Neutral ──

    /// The contract the venue paired with a delta-neutral order.
    ///
    /// The venue states no such pairing on this connection — nothing it sends
    /// carries one, under any name — so nothing here fires this. A delta-neutral
    /// order is sent and answered like any other; what the reference client
    /// reports back on this callback has no message behind it here.
    fn delta_neutral_validation(
        &mut self, req_id: i64, con_id: i64, delta: f64, price: f64,
    ) {
    }

    // ── Defined by the reference client, and never fired here ──
    //
    // Each exists so a program written against that client compiles and runs
    // against this one, and each says why it stays silent. Left undeclared, a
    // port that implements one does not build; declared and undocumented, it
    // waits for a call that is never coming.

    /// The contract a market-data request should be asked for under instead.
    ///
    /// The reference client answers a request on a contract the venue reroutes
    /// — a contract for difference standing for a share — with the contract
    /// and venue to ask again under. This connection does not reroute: asked to,
    /// it says so and serves nothing, so a request that cannot be served is
    /// refused in the venue's own words instead.
    fn reroute_mkt_data_req(&mut self, req_id: i64, con_id: i64, exchange: &str) {
        let _ = (req_id, con_id, exchange);
    }

    /// The same, for a request for the book rather than the quote.
    fn reroute_mkt_depth_req(&mut self, req_id: i64, con_id: i64, exchange: &str) {
        let _ = (req_id, con_id, exchange);
    }

    /// An exchange-for-physical quote, which the reference client reports on a
    /// callback of its own rather than as a price.
    ///
    /// The venue states none of it on this connection. The numbers such a quote
    /// would be stated under are carried here, in the list this client answers
    /// by name; what never arrives under them is the quote. Read across a
    /// share, a fund and two futures, the venue stated fifteen kinds of tick
    /// and not one of them was one of these.
    #[allow(clippy::too_many_arguments)]
    fn tick_efp(
        &mut self, req_id: i64, tick_type: i32, basis_points: f64,
        formatted_basis_points: &str, implied_future: f64, hold_days: i32,
        future_last_trade_date: &str, dividend_impact: f64, dividends_to_last_trade_date: f64,
    ) {
        let _ = (req_id, tick_type, basis_points, formatted_basis_points, implied_future,
                 hold_days, future_last_trade_date, dividend_impact,
                 dividends_to_last_trade_date);
    }

    /// A step in the handshake a third-party program makes with a terminal
    /// before that terminal will carry its requests.
    ///
    /// There is no terminal between this client and the venue, so there is no
    /// handshake to make and nothing here fires these four.
    fn verify_message_api(&mut self, api_data: &str) { let _ = api_data; }
    /// Whether that handshake was accepted.
    fn verify_completed(&mut self, is_successful: bool, error_text: &str) {
        let _ = (is_successful, error_text);
    }
    /// The same handshake, where the terminal also authenticates the program.
    fn verify_and_auth_message_api(&mut self, api_data: &str, xyz_challenge: &str) {
        let _ = (api_data, xyz_challenge);
    }
    /// Whether that one was accepted.
    fn verify_and_auth_completed(&mut self, is_successful: bool, error_text: &str) {
        let _ = (is_successful, error_text);
    }

    /// What the reference client reports when its own socket layer fails on
    /// Windows.
    ///
    /// This client has no such layer: trouble on a connection reaches a caller
    /// on the error callback, with the reason the transport gave.
    fn win_error(&mut self, text: &str, last_error: i32) { let _ = (text, last_error); }

    // ── Histogram ──

    /// How much traded at each price over a window.
    fn histogram_data(&mut self, req_id: i64, items: &[(f64, i64)]) {}

    // ── Market Rules ──

    /// The price ladder a contract trades on: each step, and what the
    /// price moves in above it.
    fn market_rule(&mut self, market_rule_id: i64, price_increments: &[PriceIncrement]) {}

    // ── Completed Orders ──

    /// An order that is done — filled, cancelled or expired — as the
    /// venue holds it.
    fn completed_order(&mut self, contract: &Contract, order: &Order, order_state: &OrderState) {}
    /// Every completed order has been stated.
    fn completed_orders_end(&mut self) {}

    // ── Historical Schedule ──

    /// When a contract's venue was open over a window, session by
    /// session, in the zone the venue keeps.
    fn historical_schedule(
        &mut self, req_id: i64, start_date_time: &str, end_date_time: &str,
        time_zone: &str, sessions: &[(String, String, String)],
    ) {}

    // ── Fundamental Data ──

    /// A fundamental report, as the XML the venue publishes.
    fn fundamental_data(&mut self, req_id: i64, data: &str) {}

    // ── Market Depth ──

    /// One level of a book that names no venue. `operation` is 0 to
    /// insert, 1 to update, 2 to delete; `side` is 0 ask, 1 bid.
    fn update_mkt_depth(
        &mut self, req_id: i64, position: i32, operation: i32,
        side: i32, price: f64, size: f64,
    ) {}
    /// One level of a book that names the venue it stands on. Every
    /// level from this client names one.
    fn update_mkt_depth_l2(
        &mut self, req_id: i64, position: i32, market_maker: &str,
        operation: i32, side: i32, price: f64, size: f64, is_smart_depth: bool,
    ) {}
    /// Every exchange the venue names, in the two sections it names
    /// them in: shares and derivatives.
    fn mkt_depth_exchanges(&mut self, _descriptions: &[crate::types::DepthMktDataDescription]) {}

    // ── Tick Req Params ──

    /// What a subscription was given: the increment its prices move in,
    /// which venues it is served from, and which feed answered.
    fn tick_req_params(&mut self, ticker_id: i64, min_tick: f64, bbo_exchange: &str, snapshot_permissions: i64) {}

    // ── Smart Components ──

    /// Which venue each bit of a quote's exchange mask refers to, and
    /// the letter that venue is named by.
    fn smart_components(&mut self, req_id: i64, components: &[crate::types::SmartComponent]) {}

    // ── News Providers ──

    /// Every news provider this account may read.
    fn news_providers(&mut self, providers: &[crate::types::NewsProvider]) {}

    // ── Soft Dollar Tiers ──

    /// The soft dollar tiers this account may direct commission to.
    fn soft_dollar_tiers(&mut self, req_id: i64, tiers: &[crate::types::SoftDollarTier]) {}

    // ── Family Codes ──

    /// The account families this login belongs to.
    fn family_codes(&mut self, codes: &[crate::types::FamilyCode]) {}

    // ── User Info ──

    /// What the login is entitled to, as the venue states it.
    fn user_info(&mut self, req_id: i64, white_branding_id: &str) {}
}


/// Deliver every callback to two places at once.
///
/// A question takes the read turn and pumps the queues into its own collector,
/// which implements the handful of callbacks it cares about and inherits an
/// empty body for the rest. The queues are drained as they are read, so
/// anything the collector ignores — a fill, a trade, an order's new status —
/// was read and thrown away, and the record a caller reads those from never
/// saw it. Leaving those queues alone while a question runs is not the answer
/// either: waiting on an order is itself a question, and the order callbacks
/// are what it waits for.
///
/// So both get everything: the collector answering the question, and whatever
/// the session keeps its running record in.
pub(crate) struct Tee<'a, A: Wrapper + ?Sized, B: Wrapper + ?Sized> {
    /// The collector answering the question being asked.
    pub asked: &'a mut A,
    /// The record kept across questions.
    pub kept: &'a mut B,
}

#[allow(unused_variables)]
impl<A: Wrapper + ?Sized, B: Wrapper + ?Sized> Wrapper for Tee<'_, A, B> {
    fn connect_ack(&mut self) {
        self.asked.connect_ack();
        self.kept.connect_ack();
    }
    fn connection_closed(&mut self) {
        self.asked.connection_closed();
        self.kept.connection_closed();
    }
    fn next_valid_id(&mut self, order_id: i64) {
        self.asked.next_valid_id(order_id);
        self.kept.next_valid_id(order_id);
    }
    fn managed_accounts(&mut self, accounts_list: &str) {
        self.asked.managed_accounts(accounts_list);
        self.kept.managed_accounts(accounts_list);
    }
    fn error(&mut self, req_id: i64, error_code: i64, error_string: &str, advanced_order_reject_json: &str) {
        self.asked.error(req_id, error_code, error_string, advanced_order_reject_json);
        self.kept.error(req_id, error_code, error_string, advanced_order_reject_json);
    }
    fn current_time(&mut self, time: i64) {
        self.asked.current_time(time);
        self.kept.current_time(time);
    }
    fn current_time_in_millis(&mut self, time_in_millis: i64) {
        self.asked.current_time_in_millis(time_in_millis);
        self.kept.current_time_in_millis(time_in_millis);
    }
    fn tick_price(&mut self, req_id: i64, tick_type: i32, price: f64, attrib: &TickAttrib) {
        self.asked.tick_price(req_id, tick_type, price, attrib);
        self.kept.tick_price(req_id, tick_type, price, attrib);
    }
    fn tick_size(&mut self, req_id: i64, tick_type: i32, size: f64) {
        self.asked.tick_size(req_id, tick_type, size);
        self.kept.tick_size(req_id, tick_type, size);
    }
    fn tick_string(&mut self, req_id: i64, tick_type: i32, value: &str) {
        self.asked.tick_string(req_id, tick_type, value);
        self.kept.tick_string(req_id, tick_type, value);
    }
    fn tick_generic(&mut self, req_id: i64, tick_type: i32, value: f64) {
        self.asked.tick_generic(req_id, tick_type, value);
        self.kept.tick_generic(req_id, tick_type, value);
    }
    fn tick_snapshot_end(&mut self, req_id: i64) {
        self.asked.tick_snapshot_end(req_id);
        self.kept.tick_snapshot_end(req_id);
    }
    fn market_data_type(&mut self, req_id: i64, market_data_type: i32) {
        self.asked.market_data_type(req_id, market_data_type);
        self.kept.market_data_type(req_id, market_data_type);
    }
    fn order_status(&mut self, order_id: i64, status: &str, filled: f64, remaining: f64, avg_fill_price: f64, perm_id: i64, parent_id: i64, last_fill_price: f64, client_id: i64, why_held: &str, mkt_cap_price: f64) {
        self.asked.order_status(order_id, status, filled, remaining, avg_fill_price, perm_id, parent_id, last_fill_price, client_id, why_held, mkt_cap_price);
        self.kept.order_status(order_id, status, filled, remaining, avg_fill_price, perm_id, parent_id, last_fill_price, client_id, why_held, mkt_cap_price);
    }
    fn open_order(&mut self, order_id: i64, contract: &Contract, order: &Order, order_state: &OrderState) {
        self.asked.open_order(order_id, contract, order, order_state);
        self.kept.open_order(order_id, contract, order, order_state);
    }
    fn open_order_end(&mut self) {
        self.asked.open_order_end();
        self.kept.open_order_end();
    }
    fn exec_details(&mut self, req_id: i64, contract: &Contract, execution: &Execution) {
        self.asked.exec_details(req_id, contract, execution);
        self.kept.exec_details(req_id, contract, execution);
    }
    fn exec_details_end(&mut self, req_id: i64) {
        self.asked.exec_details_end(req_id);
        self.kept.exec_details_end(req_id);
    }
    fn commission_and_fees_report(&mut self, report: &CommissionAndFeesReport) {
        self.asked.commission_and_fees_report(report);
        self.kept.commission_and_fees_report(report);
    }
    fn update_account_value(&mut self, key: &str, value: &str, currency: &str, account_name: &str) {
        self.asked.update_account_value(key, value, currency, account_name);
        self.kept.update_account_value(key, value, currency, account_name);
    }
    fn update_portfolio(&mut self, contract: &Contract, position: f64, market_price: f64, market_value: f64, average_cost: f64, unrealized_pnl: f64, realized_pnl: f64, account_name: &str) {
        self.asked.update_portfolio(contract, position, market_price, market_value, average_cost, unrealized_pnl, realized_pnl, account_name);
        self.kept.update_portfolio(contract, position, market_price, market_value, average_cost, unrealized_pnl, realized_pnl, account_name);
    }
    fn update_account_time(&mut self, timestamp: &str) {
        self.asked.update_account_time(timestamp);
        self.kept.update_account_time(timestamp);
    }
    fn account_download_end(&mut self, account: &str) {
        self.asked.account_download_end(account);
        self.kept.account_download_end(account);
    }
    fn account_summary(&mut self, req_id: i64, account: &str, tag: &str, value: &str, currency: &str) {
        self.asked.account_summary(req_id, account, tag, value, currency);
        self.kept.account_summary(req_id, account, tag, value, currency);
    }
    fn account_summary_end(&mut self, req_id: i64) {
        self.asked.account_summary_end(req_id);
        self.kept.account_summary_end(req_id);
    }
    fn position(&mut self, account: &str, contract: &Contract, pos: f64, avg_cost: f64) {
        self.asked.position(account, contract, pos, avg_cost);
        self.kept.position(account, contract, pos, avg_cost);
    }
    fn position_end(&mut self) {
        self.asked.position_end();
        self.kept.position_end();
    }
    fn position_multi(&mut self, req_id: i64, account: &str, model_code: &str, contract: &Contract, pos: f64, avg_cost: f64) {
        self.asked.position_multi(req_id, account, model_code, contract, pos, avg_cost);
        self.kept.position_multi(req_id, account, model_code, contract, pos, avg_cost);
    }
    fn position_multi_end(&mut self, req_id: i64) {
        self.asked.position_multi_end(req_id);
        self.kept.position_multi_end(req_id);
    }
    fn account_update_multi(&mut self, req_id: i64, account: &str, model_code: &str, key: &str, value: &str, currency: &str) {
        self.asked.account_update_multi(req_id, account, model_code, key, value, currency);
        self.kept.account_update_multi(req_id, account, model_code, key, value, currency);
    }
    fn account_update_multi_end(&mut self, req_id: i64) {
        self.asked.account_update_multi_end(req_id);
        self.kept.account_update_multi_end(req_id);
    }
    fn pnl(&mut self, req_id: i64, daily_pnl: f64, unrealized_pnl: f64, realized_pnl: f64) {
        self.asked.pnl(req_id, daily_pnl, unrealized_pnl, realized_pnl);
        self.kept.pnl(req_id, daily_pnl, unrealized_pnl, realized_pnl);
    }
    fn pnl_single(&mut self, req_id: i64, pos: f64, daily_pnl: f64, unrealized_pnl: f64, realized_pnl: f64, value: f64) {
        self.asked.pnl_single(req_id, pos, daily_pnl, unrealized_pnl, realized_pnl, value);
        self.kept.pnl_single(req_id, pos, daily_pnl, unrealized_pnl, realized_pnl, value);
    }
    fn historical_data(&mut self, req_id: i64, bar: &BarData) {
        self.asked.historical_data(req_id, bar);
        self.kept.historical_data(req_id, bar);
    }
    fn historical_data_end(&mut self, req_id: i64, start: &str, end: &str) {
        self.asked.historical_data_end(req_id, start, end);
        self.kept.historical_data_end(req_id, start, end);
    }
    fn historical_data_update(&mut self, req_id: i64, bar: &BarData) {
        self.asked.historical_data_update(req_id, bar);
        self.kept.historical_data_update(req_id, bar);
    }
    fn head_timestamp(&mut self, req_id: i64, head_timestamp: &str) {
        self.asked.head_timestamp(req_id, head_timestamp);
        self.kept.head_timestamp(req_id, head_timestamp);
    }
    fn contract_details(&mut self, req_id: i64, details: &ContractDetails) {
        self.asked.contract_details(req_id, details);
        self.kept.contract_details(req_id, details);
    }
    fn contract_details_end(&mut self, req_id: i64) {
        self.asked.contract_details_end(req_id);
        self.kept.contract_details_end(req_id);
    }
    fn symbol_samples(&mut self, req_id: i64, descriptions: &[ContractDescription]) {
        self.asked.symbol_samples(req_id, descriptions);
        self.kept.symbol_samples(req_id, descriptions);
    }
    fn tick_by_tick_all_last(&mut self, req_id: i64, tick_type: i32, time: i64, price: f64, size: f64, attrib: &TickAttribLast, exchange: &str, special_conditions: &str) {
        self.asked.tick_by_tick_all_last(req_id, tick_type, time, price, size, attrib, exchange, special_conditions);
        self.kept.tick_by_tick_all_last(req_id, tick_type, time, price, size, attrib, exchange, special_conditions);
    }
    fn tick_by_tick_bid_ask(&mut self, req_id: i64, time: i64, bid_price: f64, ask_price: f64, bid_size: f64, ask_size: f64, attrib: &TickAttribBidAsk) {
        self.asked.tick_by_tick_bid_ask(req_id, time, bid_price, ask_price, bid_size, ask_size, attrib);
        self.kept.tick_by_tick_bid_ask(req_id, time, bid_price, ask_price, bid_size, ask_size, attrib);
    }
    fn tick_by_tick_mid_point(&mut self, req_id: i64, time: i64, mid_point: f64) {
        self.asked.tick_by_tick_mid_point(req_id, time, mid_point);
        self.kept.tick_by_tick_mid_point(req_id, time, mid_point);
    }
    fn scanner_data(&mut self, req_id: i64, rank: i32, details: &ContractDetails, distance: &str, benchmark: &str, projection: &str, legs_str: &str) {
        self.asked.scanner_data(req_id, rank, details, distance, benchmark, projection, legs_str);
        self.kept.scanner_data(req_id, rank, details, distance, benchmark, projection, legs_str);
    }
    fn scanner_data_end(&mut self, req_id: i64) {
        self.asked.scanner_data_end(req_id);
        self.kept.scanner_data_end(req_id);
    }
    fn scanner_parameters(&mut self, xml: &str) {
        self.asked.scanner_parameters(xml);
        self.kept.scanner_parameters(xml);
    }
    fn update_news_bulletin(&mut self, msg_id: i64, msg_type: i32, message: &str, orig_exchange: &str) {
        self.asked.update_news_bulletin(msg_id, msg_type, message, orig_exchange);
        self.kept.update_news_bulletin(msg_id, msg_type, message, orig_exchange);
    }
    fn tick_news(&mut self, ticker_id: i64, timestamp: i64, provider_code: &str, article_id: &str, headline: &str, extra_data: &str) {
        self.asked.tick_news(ticker_id, timestamp, provider_code, article_id, headline, extra_data);
        self.kept.tick_news(ticker_id, timestamp, provider_code, article_id, headline, extra_data);
    }
    fn historical_news(&mut self, req_id: i64, time: &str, provider_code: &str, article_id: &str, headline: &str) {
        self.asked.historical_news(req_id, time, provider_code, article_id, headline);
        self.kept.historical_news(req_id, time, provider_code, article_id, headline);
    }
    fn historical_news_end(&mut self, req_id: i64, has_more: bool) {
        self.asked.historical_news_end(req_id, has_more);
        self.kept.historical_news_end(req_id, has_more);
    }
    fn news_article(&mut self, req_id: i64, article_type: i32, article_text: &str) {
        self.asked.news_article(req_id, article_type, article_text);
        self.kept.news_article(req_id, article_type, article_text);
    }
    fn real_time_bar(&mut self, req_id: i64, date: i64, open: f64, high: f64, low: f64, close: f64, volume: f64, wap: f64, count: i32) {
        self.asked.real_time_bar(req_id, date, open, high, low, close, volume, wap, count);
        self.kept.real_time_bar(req_id, date, open, high, low, close, volume, wap, count);
    }
    fn historical_ticks(&mut self, req_id: i64, ticks: &HistoricalTickData, done: bool) {
        self.asked.historical_ticks(req_id, ticks, done);
        self.kept.historical_ticks(req_id, ticks, done);
    }
    fn historical_ticks_bid_ask(&mut self, req_id: i64, ticks: &HistoricalTickData, done: bool) {
        self.asked.historical_ticks_bid_ask(req_id, ticks, done);
        self.kept.historical_ticks_bid_ask(req_id, ticks, done);
    }
    fn historical_ticks_last(&mut self, req_id: i64, ticks: &HistoricalTickData, done: bool) {
        self.asked.historical_ticks_last(req_id, ticks, done);
        self.kept.historical_ticks_last(req_id, ticks, done);
    }
    fn tick_option_computation(&mut self, req_id: i64, tick_type: i32, tick_attrib: i32, implied_vol: f64, delta: f64, opt_price: f64, pv_dividend: f64, gamma: f64, vega: f64, theta: f64, und_price: f64) {
        self.asked.tick_option_computation(req_id, tick_type, tick_attrib, implied_vol, delta, opt_price, pv_dividend, gamma, vega, theta, und_price);
        self.kept.tick_option_computation(req_id, tick_type, tick_attrib, implied_vol, delta, opt_price, pv_dividend, gamma, vega, theta, und_price);
    }
    fn display_group_list(&mut self, req_id: i64, groups: &str) {
        self.asked.display_group_list(req_id, groups);
        self.kept.display_group_list(req_id, groups);
    }
    fn display_group_updated(&mut self, req_id: i64, contract_info: &str) {
        self.asked.display_group_updated(req_id, contract_info);
        self.kept.display_group_updated(req_id, contract_info);
    }
    fn bond_contract_details(&mut self, req_id: i64, details: &ContractDetails) {
        self.asked.bond_contract_details(req_id, details);
        self.kept.bond_contract_details(req_id, details);
    }
    fn order_bound(&mut self, perm_id: i64, client_id: i64, order_id: i64) {
        self.asked.order_bound(perm_id, client_id, order_id);
        self.kept.order_bound(perm_id, client_id, order_id);
    }
    fn receive_fa(&mut self, fa_data_type: i32, cxml: &str) {
        self.asked.receive_fa(fa_data_type, cxml);
        self.kept.receive_fa(fa_data_type, cxml);
    }
    fn replace_fa_end(&mut self, req_id: i64, text: &str) {
        self.asked.replace_fa_end(req_id, text);
        self.kept.replace_fa_end(req_id, text);
    }
    fn wsh_meta_data(&mut self, req_id: i64, data_json: &str) {
        self.asked.wsh_meta_data(req_id, data_json);
        self.kept.wsh_meta_data(req_id, data_json);
    }
    fn wsh_event_data(&mut self, req_id: i64, data_json: &str) {
        self.asked.wsh_event_data(req_id, data_json);
        self.kept.wsh_event_data(req_id, data_json);
    }
    fn security_definition_option_parameter(&mut self, req_id: i64, exchange: &str, underlying_con_id: i64, trading_class: &str, multiplier: &str, expirations: &[String], strikes: &[f64]) {
        self.asked.security_definition_option_parameter(req_id, exchange, underlying_con_id, trading_class, multiplier, expirations, strikes);
        self.kept.security_definition_option_parameter(req_id, exchange, underlying_con_id, trading_class, multiplier, expirations, strikes);
    }
    fn security_definition_option_parameter_end(&mut self, req_id: i64) {
        self.asked.security_definition_option_parameter_end(req_id);
        self.kept.security_definition_option_parameter_end(req_id);
    }
    fn delta_neutral_validation(&mut self, req_id: i64, con_id: i64, delta: f64, price: f64) {
        self.asked.delta_neutral_validation(req_id, con_id, delta, price);
        self.kept.delta_neutral_validation(req_id, con_id, delta, price);
    }
    fn reroute_mkt_data_req(&mut self, req_id: i64, con_id: i64, exchange: &str) {
        self.asked.reroute_mkt_data_req(req_id, con_id, exchange);
        self.kept.reroute_mkt_data_req(req_id, con_id, exchange);
    }
    fn reroute_mkt_depth_req(&mut self, req_id: i64, con_id: i64, exchange: &str) {
        self.asked.reroute_mkt_depth_req(req_id, con_id, exchange);
        self.kept.reroute_mkt_depth_req(req_id, con_id, exchange);
    }
    fn tick_efp(
        &mut self, req_id: i64, tick_type: i32, basis_points: f64,
        formatted_basis_points: &str, implied_future: f64, hold_days: i32,
        future_last_trade_date: &str, dividend_impact: f64, dividends_to_last_trade_date: f64,
    ) {
        self.asked.tick_efp(req_id, tick_type, basis_points, formatted_basis_points, implied_future, hold_days, future_last_trade_date, dividend_impact, dividends_to_last_trade_date);
        self.kept.tick_efp(req_id, tick_type, basis_points, formatted_basis_points, implied_future, hold_days, future_last_trade_date, dividend_impact, dividends_to_last_trade_date);
    }
    fn verify_message_api(&mut self, api_data: &str) {
        self.asked.verify_message_api(api_data);
        self.kept.verify_message_api(api_data);
    }
    fn verify_completed(&mut self, is_successful: bool, error_text: &str) {
        self.asked.verify_completed(is_successful, error_text);
        self.kept.verify_completed(is_successful, error_text);
    }
    fn verify_and_auth_message_api(&mut self, api_data: &str, xyz_challenge: &str) {
        self.asked.verify_and_auth_message_api(api_data, xyz_challenge);
        self.kept.verify_and_auth_message_api(api_data, xyz_challenge);
    }
    fn verify_and_auth_completed(&mut self, is_successful: bool, error_text: &str) {
        self.asked.verify_and_auth_completed(is_successful, error_text);
        self.kept.verify_and_auth_completed(is_successful, error_text);
    }
    fn win_error(&mut self, text: &str, last_error: i32) {
        self.asked.win_error(text, last_error);
        self.kept.win_error(text, last_error);
    }
    fn histogram_data(&mut self, req_id: i64, items: &[(f64, i64)]) {
        self.asked.histogram_data(req_id, items);
        self.kept.histogram_data(req_id, items);
    }
    fn market_rule(&mut self, market_rule_id: i64, price_increments: &[PriceIncrement]) {
        self.asked.market_rule(market_rule_id, price_increments);
        self.kept.market_rule(market_rule_id, price_increments);
    }
    fn completed_order(&mut self, contract: &Contract, order: &Order, order_state: &OrderState) {
        self.asked.completed_order(contract, order, order_state);
        self.kept.completed_order(contract, order, order_state);
    }
    fn completed_orders_end(&mut self) {
        self.asked.completed_orders_end();
        self.kept.completed_orders_end();
    }
    fn historical_schedule(&mut self, req_id: i64, start_date_time: &str, end_date_time: &str, time_zone: &str, sessions: &[(String, String, String)]) {
        self.asked.historical_schedule(req_id, start_date_time, end_date_time, time_zone, sessions);
        self.kept.historical_schedule(req_id, start_date_time, end_date_time, time_zone, sessions);
    }
    fn fundamental_data(&mut self, req_id: i64, data: &str) {
        self.asked.fundamental_data(req_id, data);
        self.kept.fundamental_data(req_id, data);
    }
    fn update_mkt_depth(&mut self, req_id: i64, position: i32, operation: i32, side: i32, price: f64, size: f64) {
        self.asked.update_mkt_depth(req_id, position, operation, side, price, size);
        self.kept.update_mkt_depth(req_id, position, operation, side, price, size);
    }
    fn update_mkt_depth_l2(&mut self, req_id: i64, position: i32, market_maker: &str, operation: i32, side: i32, price: f64, size: f64, is_smart_depth: bool) {
        self.asked.update_mkt_depth_l2(req_id, position, market_maker, operation, side, price, size, is_smart_depth);
        self.kept.update_mkt_depth_l2(req_id, position, market_maker, operation, side, price, size, is_smart_depth);
    }
    fn mkt_depth_exchanges(&mut self, _descriptions: &[crate::types::DepthMktDataDescription]) {
        self.asked.mkt_depth_exchanges(_descriptions);
        self.kept.mkt_depth_exchanges(_descriptions);
    }
    fn tick_req_params(&mut self, ticker_id: i64, min_tick: f64, bbo_exchange: &str, snapshot_permissions: i64) {
        self.asked.tick_req_params(ticker_id, min_tick, bbo_exchange, snapshot_permissions);
        self.kept.tick_req_params(ticker_id, min_tick, bbo_exchange, snapshot_permissions);
    }
    fn smart_components(&mut self, req_id: i64, components: &[crate::types::SmartComponent]) {
        self.asked.smart_components(req_id, components);
        self.kept.smart_components(req_id, components);
    }
    fn news_providers(&mut self, providers: &[crate::types::NewsProvider]) {
        self.asked.news_providers(providers);
        self.kept.news_providers(providers);
    }
    fn soft_dollar_tiers(&mut self, req_id: i64, tiers: &[crate::types::SoftDollarTier]) {
        self.asked.soft_dollar_tiers(req_id, tiers);
        self.kept.soft_dollar_tiers(req_id, tiers);
    }
    fn family_codes(&mut self, codes: &[crate::types::FamilyCode]) {
        self.asked.family_codes(codes);
        self.kept.family_codes(codes);
    }
    fn user_info(&mut self, req_id: i64, white_branding_id: &str) {
        self.asked.user_info(req_id, white_branding_id);
        self.kept.user_info(req_id, white_branding_id);
    }
}

#[cfg(test)]
mod tee_tests {
    use super::*;

    #[derive(Default)]
    struct Counts { fills: Vec<String>, errors: Vec<i64> }
    impl Wrapper for Counts {
        fn exec_details(
            &mut self, _req_id: i64, _contract: &crate::types::model::Contract,
            execution: &crate::types::model::Execution,
        ) {
            self.fills.push(execution.exec_id.clone());
        }
        fn error(&mut self, _req_id: i64, code: i64, _message: &str, _adv: &str) {
            self.errors.push(code);
        }
    }

    /// A collector that does not implement a callback must not swallow it.
    ///
    /// This is the whole point: a question's collector implements the few
    /// callbacks it needs and inherits an empty body for the rest, and the
    /// queues empty as they are read — so anything it ignores is gone unless
    /// something else is fed at the same time.
    #[test]
    fn what_one_side_ignores_still_reaches_the_other() {
        struct AsksAboutErrorsOnly { seen: Vec<i64> }
        impl Wrapper for AsksAboutErrorsOnly {
            fn error(&mut self, _req_id: i64, code: i64, _message: &str, _adv: &str) {
                self.seen.push(code);
            }
        }

        let mut asked = AsksAboutErrorsOnly { seen: Vec::new() };
        let mut kept = Counts::default();
        {
            let mut both = Tee { asked: &mut asked, kept: &mut kept };
            let contract = crate::types::model::Contract::default();
            let execution = crate::types::model::Execution {
                exec_id: "0001".to_string(),
                ..Default::default()
            };
            both.exec_details(7, &contract, &execution);
            both.error(7, 321, "a reason", "");
        }
        assert_eq!(kept.fills, ["0001"], "the fill reached the record");
        assert_eq!(kept.errors, [321], "and so did the error");
        assert_eq!(asked.seen, [321], "the question heard what it asked about");
    }
}

/// Test helpers for Wrapper-based testing. Hidden from docs.
#[doc(hidden)]
pub mod tests {
    use super::*;

    /// A Wrapper impl that records all callback invocations for testing.
    #[derive(Default)]
    pub struct RecordingWrapper {
        /// Parent ids seen on `order_status`, in order.
        pub parent_ids: Vec<i64>,
        pub events: Vec<String>,
    }

    impl Wrapper for RecordingWrapper {
        fn connect_ack(&mut self) {
            self.events.push("connect_ack".into());
        }
        fn connection_closed(&mut self) {
            self.events.push("connection_closed".into());
        }
        fn next_valid_id(&mut self, order_id: i64) {
            self.events.push(format!("next_valid_id:{order_id}"));
        }
        fn error(&mut self, req_id: i64, error_code: i64, error_string: &str, _: &str) {
            self.events.push(format!("error:{req_id}:{error_code}:{error_string}"));
        }
        fn position_multi(
            &mut self, req_id: i64, account: &str, _model_code: &str,
            contract: &Contract, pos: f64, _avg_cost: f64,
        ) {
            self.events.push(format!("position_multi:{req_id}:{account}:{}:{pos}", contract.symbol));
        }
        fn position_multi_end(&mut self, req_id: i64) {
            self.events.push(format!("position_multi_end:{req_id}"));
        }
        fn account_update_multi(
            &mut self, req_id: i64, _account: &str, _model_code: &str,
            key: &str, value: &str, _currency: &str,
        ) {
            self.events.push(format!("account_update_multi:{req_id}:{key}:{value}"));
        }
        fn account_update_multi_end(&mut self, req_id: i64) {
            self.events.push(format!("account_update_multi_end:{req_id}"));
        }
        fn display_group_list(&mut self, req_id: i64, groups: &str) {
            self.events.push(format!("display_group_list:{req_id}:{groups}"));
        }
        fn display_group_updated(&mut self, req_id: i64, contract_info: &str) {
            self.events.push(format!("display_group_updated:{req_id}:{contract_info}"));
        }
        fn tick_price(&mut self, req_id: i64, tick_type: i32, price: f64, _: &TickAttrib) {
            self.events.push(format!("tick_price:{req_id}:{tick_type}:{price}"));
        }
        fn tick_size(&mut self, req_id: i64, tick_type: i32, size: f64) {
            self.events.push(format!("tick_size:{req_id}:{tick_type}:{size}"));
        }
        fn tick_req_params(&mut self, ticker_id: i64, min_tick: f64, bbo_exchange: &str, snapshot_permissions: i64) {
            self.events.push(format!("tick_req_params:{ticker_id}:{min_tick}:{bbo_exchange}:{snapshot_permissions}"));
        }
        fn historical_data_update(&mut self, req_id: i64, bar: &BarData) {
            self.events.push(format!("historical_data_update:{req_id}:{}", bar.date));
        }
        fn order_status(
            &mut self, order_id: i64, status: &str, filled: f64, remaining: f64,
            avg_fill_price: f64, _: i64, parent_id: i64, _: f64, _: i64, _: &str, _: f64,
        ) {
            self.events.push(format!("order_status:{order_id}:{status}:{filled}:{remaining}:{avg_fill_price}"));
            self.parent_ids.push(parent_id);
        }
        fn open_order(&mut self, order_id: i64, _contract: &Contract, order: &Order, state: &OrderState) {
            self.parent_ids.push(order.parent_id);
            self.events.push(format!(
                "open_order:{order_id}:{}:initB={}:initC={}:initA={}:maintB={}:maintC={}:maintA={}:eqlB={}:eqlC={}:eqlA={}:comm={}",
                state.status,
                state.init_margin_before, state.init_margin_change, state.init_margin_after,
                state.maint_margin_before, state.maint_margin_change, state.maint_margin_after,
                state.equity_with_loan_before, state.equity_with_loan_change, state.equity_with_loan_after,
                state.commission_and_fees,
            ));
        }
        fn open_order_end(&mut self) {
            self.events.push("open_order_end".into());
        }
        fn exec_details(&mut self, req_id: i64, _contract: &Contract, execution: &Execution) {
            self.events.push(format!("exec_details:{req_id}:{}:{}", execution.side, execution.shares));
        }
        fn historical_data(&mut self, req_id: i64, bar: &BarData) {
            self.events.push(format!("historical_data:{req_id}:{}", bar.date));
        }
        fn historical_data_end(&mut self, req_id: i64, _: &str, _: &str) {
            self.events.push(format!("historical_data_end:{req_id}"));
        }
        fn contract_details(&mut self, req_id: i64, details: &ContractDetails) {
            self.events.push(format!("contract_details:{req_id}:{}", details.contract.symbol));
        }
        fn contract_details_end(&mut self, req_id: i64) {
            self.events.push(format!("contract_details_end:{req_id}"));
        }
        fn head_timestamp(&mut self, req_id: i64, ts: &str) {
            self.events.push(format!("head_timestamp:{req_id}:{ts}"));
        }
        fn tick_by_tick_all_last(
            &mut self, req_id: i64, tick_type: i32, time: i64, price: f64,
            size: f64, _: &TickAttribLast, exchange: &str, _: &str,
        ) {
            self.events.push(format!("tbt_last:{req_id}:{tick_type}:{time}:{price}:{size}:{exchange}"));
        }
        fn tick_by_tick_bid_ask(
            &mut self, req_id: i64, time: i64, bid_price: f64, ask_price: f64,
            bid_size: f64, ask_size: f64, _: &TickAttribBidAsk,
        ) {
            self.events.push(format!("tbt_bidask:{req_id}:{time}:{bid_price}:{ask_price}:{bid_size}:{ask_size}"));
        }
        fn position(&mut self, account: &str, contract: &Contract, pos: f64, avg_cost: f64) {
            self.events.push(format!("position:{account}:{}:{pos}:{avg_cost}", contract.con_id));
        }
        fn real_time_bar(
            &mut self, req_id: i64, date: i64, open: f64, high: f64,
            low: f64, close: f64, _volume: f64, _wap: f64, _count: i32,
        ) {
            self.events.push(format!("real_time_bar:{req_id}:{date}:{open}:{high}:{low}:{close}"));
        }
        fn scanner_parameters(&mut self, _xml: &str) {
            self.events.push("scanner_parameters".into());
        }
        fn update_news_bulletin(&mut self, msg_id: i64, msg_type: i32, message: &str, orig_exchange: &str) {
            self.events.push(format!("news_bulletin:{msg_id}:{msg_type}:{message}:{orig_exchange}"));
        }
        fn tick_news(
            &mut self, _: i64, _: i64, provider_code: &str, article_id: &str, headline: &str, _: &str,
        ) {
            self.events.push(format!("tick_news:{provider_code}:{article_id}:{headline}"));
        }
        fn histogram_data(&mut self, req_id: i64, items: &[(f64, i64)]) {
            self.events.push(format!("histogram_data:{req_id}:{}", items.len()));
        }
        fn market_rule(&mut self, id: i64, increments: &[PriceIncrement]) {
            self.events.push(format!("market_rule:{id}:{}", increments.len()));
        }
        fn fundamental_data(&mut self, req_id: i64, _data: &str) {
            self.events.push(format!("fundamental_data:{req_id}"));
        }
        fn symbol_samples(&mut self, req_id: i64, descriptions: &[ContractDescription]) {
            self.events.push(format!("symbol_samples:{req_id}:{}", descriptions.len()));
        }
        fn security_definition_option_parameter(
            &mut self, req_id: i64, exchange: &str, underlying_con_id: i64,
            trading_class: &str, multiplier: &str, expirations: &[String], strikes: &[f64],
        ) {
            let strikes: Vec<String> = strikes.iter().map(|s| s.to_string()).collect();
            self.events.push(format!(
                "sec_def_opt_param:{req_id}:{exchange}:{underlying_con_id}:{trading_class}:{multiplier}:{}:{}",
                expirations.join(","), strikes.join(","),
            ));
        }
        fn security_definition_option_parameter_end(&mut self, req_id: i64) {
            self.events.push(format!("sec_def_opt_param_end:{req_id}"));
        }
        fn scanner_data(
            &mut self, req_id: i64, rank: i32, _details: &ContractDetails,
            _: &str, _: &str, _: &str, _: &str,
        ) {
            self.events.push(format!("scanner_data:{req_id}:{rank}"));
        }
        fn scanner_data_end(&mut self, req_id: i64) {
            self.events.push(format!("scanner_data_end:{req_id}"));
        }
        fn historical_news(
            &mut self, req_id: i64, _time: &str, provider_code: &str,
            article_id: &str, headline: &str,
        ) {
            self.events.push(format!("historical_news:{req_id}:{provider_code}:{article_id}:{headline}"));
        }
        fn historical_news_end(&mut self, req_id: i64, has_more: bool) {
            self.events.push(format!("historical_news_end:{req_id}:{has_more}"));
        }
        fn news_article(&mut self, req_id: i64, article_type: i32, article_text: &str) {
            self.events.push(format!("news_article:{req_id}:{article_type}:{article_text}"));
        }
        fn historical_ticks(&mut self, req_id: i64, _ticks: &HistoricalTickData, done: bool) {
            self.events.push(format!("historical_ticks:{req_id}:{done}"));
        }
        fn historical_ticks_bid_ask(&mut self, req_id: i64, _ticks: &HistoricalTickData, done: bool) {
            self.events.push(format!("historical_ticks_bid_ask:{req_id}:{done}"));
        }
        fn historical_ticks_last(&mut self, req_id: i64, _ticks: &HistoricalTickData, done: bool) {
            self.events.push(format!("historical_ticks_last:{req_id}:{done}"));
        }
        fn historical_schedule(
            &mut self, req_id: i64, _start: &str, _end: &str,
            tz: &str, sessions: &[(String, String, String)],
        ) {
            self.events.push(format!("historical_schedule:{req_id}:{tz}:{}", sessions.len()));
        }
        fn position_end(&mut self) {
            self.events.push("position_end".into());
        }
        fn completed_order(&mut self, _contract: &Contract, _order: &Order, _state: &OrderState) {
            self.events.push("completed_order".into());
        }
        fn completed_orders_end(&mut self) {
            self.events.push("completed_orders_end".into());
        }
        fn pnl(&mut self, req_id: i64, daily_pnl: f64, unrealized_pnl: f64, realized_pnl: f64) {
            self.events.push(format!("pnl:{req_id}:{daily_pnl}:{unrealized_pnl}:{realized_pnl}"));
        }
        fn pnl_single(&mut self, req_id: i64, pos: f64, daily_pnl: f64, unrealized_pnl: f64, realized_pnl: f64, value: f64) {
            self.events.push(format!("pnl_single:{req_id}:{pos}:{daily_pnl}:{unrealized_pnl}:{realized_pnl}:{value}"));
        }
        fn account_summary(&mut self, req_id: i64, account: &str, tag: &str, value: &str, currency: &str) {
            self.events.push(format!("account_summary:{req_id}:{account}:{tag}:{value}:{currency}"));
        }
        fn account_summary_end(&mut self, req_id: i64) {
            self.events.push(format!("account_summary_end:{req_id}"));
        }
        fn smart_components(&mut self, req_id: i64, components: &[crate::types::SmartComponent]) {
            self.events.push(format!("smart_components:{req_id}:{}", components.len()));
        }
        fn news_providers(&mut self, providers: &[crate::types::NewsProvider]) {
            self.events.push(format!("news_providers:{}", providers.len()));
        }
        fn soft_dollar_tiers(&mut self, req_id: i64, tiers: &[crate::types::SoftDollarTier]) {
            self.events.push(format!("soft_dollar_tiers:{req_id}:{}", tiers.len()));
        }
        fn family_codes(&mut self, codes: &[crate::types::FamilyCode]) {
            self.events.push(format!("family_codes:{}", codes.len()));
        }
        fn user_info(&mut self, req_id: i64, white_branding_id: &str) {
            self.events.push(format!("user_info:{req_id}:{white_branding_id}"));
        }
    }

    #[test]
    fn recording_wrapper_starts_empty() {
        let w = RecordingWrapper::default();
        assert!(w.events.is_empty());
    }

    #[test]
    fn recording_wrapper_records_connect_ack() {
        let mut w = RecordingWrapper::default();
        w.connect_ack();
        assert_eq!(w.events, vec!["connect_ack"]);
    }

    #[test]
    fn recording_wrapper_records_tick_price() {
        let mut w = RecordingWrapper::default();
        let attrib = TickAttrib::default();
        w.tick_price(1, 1, 150.25, &attrib);
        assert_eq!(w.events, vec!["tick_price:1:1:150.25"]);
    }

    #[test]
    fn recording_wrapper_records_order_status() {
        let mut w = RecordingWrapper::default();
        w.order_status(42, "Filled", 100.0, 0.0, 150.0, 0, 0, 150.0, 0, "", 0.0);
        assert_eq!(w.events, vec!["order_status:42:Filled:100:0:150"]);
    }

    #[test]
    fn recording_wrapper_records_exec_details() {
        let mut w = RecordingWrapper::default();
        let c = Contract::default();
        let e = Execution { side: "BOT".into(), shares: 100.0, ..Default::default() };
        w.exec_details(-1, &c, &e);
        assert_eq!(w.events, vec!["exec_details:-1:BOT:100"]);
    }

    #[test]
    fn recording_wrapper_records_historical_data() {
        let mut w = RecordingWrapper::default();
        let bar = BarData { date: "20260101".into(), ..Default::default() };
        w.historical_data(5, &bar);
        w.historical_data_end(5, "", "");
        assert_eq!(w.events, vec!["historical_data:5:20260101", "historical_data_end:5"]);
    }

    #[test]
    fn recording_wrapper_records_position() {
        let mut w = RecordingWrapper::default();
        let c = Contract { con_id: 265598, ..Default::default() };
        w.position("DU1234567", &c, 100.0, 150.25);
        assert_eq!(w.events, vec!["position:DU1234567:265598:100:150.25"]);
    }

    #[test]
    fn recording_wrapper_multiple_events() {
        let mut w = RecordingWrapper::default();
        w.connect_ack();
        w.next_valid_id(1);
        w.connection_closed();
        assert_eq!(w.events.len(), 3);
        assert_eq!(w.events[0], "connect_ack");
        assert_eq!(w.events[1], "next_valid_id:1");
        assert_eq!(w.events[2], "connection_closed");
    }

    /// Verify a bare no-op impl compiles — ensures all defaults work.
    #[test]
    fn noop_wrapper_compiles() {
        struct NoOpWrapper;
        impl Wrapper for NoOpWrapper {}

        let mut w = NoOpWrapper;
        w.connect_ack();
        w.tick_price(0, 0, 0.0, &TickAttrib::default());
        w.order_status(0, "", 0.0, 0.0, 0.0, 0, 0, 0.0, 0, "", 0.0);
        // If this compiles, all defaults are valid.
    }
}
