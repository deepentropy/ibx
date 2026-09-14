//! ibapi-compatible EWrapper base class with no-op default callbacks.

use pyo3::prelude::*;

/// ibapi-compatible EWrapper base class.
/// Users subclass this in Python and override callbacks they care about.
/// All methods are no-ops by default.
///
/// Holds nothing and is frozen, so an instance is laid out exactly as
/// `object`'s are. The reference client's own sample program is
/// `class App(EWrapper, EClient)`, and the interpreter refuses a class whose
/// bases lay an instance out two different ways — so with a field or a borrow
/// flag here, that class cannot be defined at all. `EClient` carries the
/// state; this class must go on carrying none.
#[pyclass(frozen, subclass)]
pub struct EWrapper;

#[pymethods]
impl EWrapper {
    #[new]
    #[pyo3(signature = (*_args, **_kwargs))]
    fn new(_args: &Bound<'_, pyo3::types::PyTuple>, _kwargs: Option<&Bound<'_, pyo3::types::PyDict>>) -> Self {
        Self
    }

    /// Answer to the name the reference client gives a callback as well as the
    /// name this one gives it.
    ///
    /// Every callback here is named with underscores. Code written for the
    /// reference client names them with the words run together, and asks the
    /// base class about them — whether a subclass overrode one, or by calling
    /// the default through `super()`. Under this class those names were simply
    /// absent.
    ///
    /// Only reached when the attribute was not found, so it costs nothing on
    /// the names this class defines, and a name that names no callback is still
    /// refused rather than answered with a do-nothing.
    ///
    /// Carries the client's aliases as well: in `App(EWrapper, EClient)` the
    /// interpreter takes this hook for the whole instance and never asks the
    /// client's, so `app.eConnect` has to be answered from here. On a wrapper
    /// alone they name nothing and are refused as before.
    fn __getattr__(slf: Bound<'_, Self>, name: &str) -> PyResult<Py<PyAny>> {
        super::contract::by_reference_name(slf.as_any(), name, super::client::REFERENCE_ALIASES)
    }

    // ── Connection ──

    /// The session is open. Nothing has been asked for yet.
    fn connect_ack(&self) {}

    /// The session is over, because this client ended it. A session that
    /// went away instead is reported on `error` under 1100.
    fn connection_closed(&self) {}

    /// The first order id this session may use. Each order needs one
    /// higher than the last.
    fn next_valid_id(&self, _order_id: i64) {}

    /// Every account this login may act for, separated by commas. One
    /// for most logins; an advisor has several.
    fn managed_accounts(&self, _accounts_list: &str) {}

    #[pyo3(signature = (_req_id, _error_time, _error_code, _error_string, _advanced_order_reject_json=""))]
    /// What the venue said about a request, under the number it says it
    /// with. Codes from 2100 to 2200 are notices about a connection rather than
    /// failures. `req_id` is -1 for anything that answers no particular request.
    ///
    /// A request this client will not send is reported here too, under the same
    /// numbers the reference client uses: 321 for a request that fails
    /// validation, 200 for a contract description that matches nothing, 504
    /// for a call made with no session.
    ///
    /// `error_time` is the reference client's second parameter and is stated
    /// wherever it states one. It carries a clock reading in milliseconds for
    /// trouble this client raises before anything reached the venue, and zero
    /// for trouble the venue stated — which is what that client passes for a
    /// session speaking a protocol older than the one that added the field,
    /// and this one says it speaks an older protocol than that.
    fn error(
        &self,
        _req_id: i64,
        _error_time: i64,
        _error_code: i64,
        _error_string: &str,
        _advanced_order_reject_json: &str,
    ) {
    }

    /// The venue clock, in seconds since the epoch.
    fn current_time(&self, _time: i64) {}

    /// The venue clock, in milliseconds since the epoch.
    ///
    /// The same clock `current_time` reports, at the precision the venue
    /// stated it in. The stamp can carry a fraction of a second and this reads
    /// it where it does, but the stamps measured against this venue carried
    /// none, so the answer lands on a whole second.
    fn current_time_in_millis(&self, _time_in_millis: i64) {}

    // ── Market Data ──

    /// One price of a quote, and which price it is. `tick_type` names it —
    /// 1 bid, 2 ask, 4 last, 9 close — and `attrib` says whether it can be traded
    /// against and whether it is past its limit. A size arrives on `tick_size`
    /// under the type that belongs to it.
    fn tick_price(&self, _req_id: i64, _tick_type: i32, _price: f64, _attrib: Py<PyAny>) {}

    /// One size of a quote, and which size it is: 0 bid, 3 ask, 5 last, 8
    /// the day's volume.
    fn tick_size(&self, _req_id: i64, _tick_type: i32, _size: f64) {}

    /// A quote's value that is not a number — a timestamp, an exchange
    /// map, a set of ids.
    fn tick_string(&self, _req_id: i64, _tick_type: i32, _value: &str) {}

    /// A quote's value that is a number and is not a price or a size —
    /// an implied volatility, an index future's premium, a halt.
    fn tick_generic(&self, _req_id: i64, _tick_type: i32, _value: f64) {}

    /// A snapshot has stated everything it is going to. Only for a
    /// subscription asked for as a snapshot; a streaming one never ends.
    fn tick_snapshot_end(&self, _req_id: i64) {}

    /// Which feed a subscription is being served from: 1 live, 2 frozen,
    /// 3 delayed, 4 delayed and frozen.
    fn market_data_type(&self, _req_id: i64, _market_data_type: i32) {}

    // ── Orders ──

    /// Where an order stands now. Fires on every change, and again on
    /// each fill. `filled` and `remaining` are shares, `avg_fill_price` the
    /// average of what has filled so far.
    fn order_status(
        &self, _order_id: i64, _status: &str, _filled: f64, _remaining: f64,
        _avg_fill_price: f64, _perm_id: i64, _parent_id: i64,
        _last_fill_price: f64, _client_id: i64, _why_held: &str, _mkt_cap_price: f64,
    ) {}

    /// An order as the venue holds it, and the state it is in. Fires
    /// beside every `order_status`, when open orders are asked for, and once for
    /// a preview — where the state carries what the order would cost and no
    /// status follows, because a preview is not an order.
    fn open_order(&self, _order_id: i64, _contract: Py<PyAny>, _order: Py<PyAny>, _order_state: Py<PyAny>) {}

    /// Every open order has been stated.
    fn open_order_end(&self) {}

    /// One fill, against the order and contract it filled. What it cost
    /// arrives separately, on `commission_and_fees_report`.
    fn exec_details(&self, _req_id: i64, _contract: Py<PyAny>, _execution: Py<PyAny>) {}

    /// Every execution answering this request has been stated.
    fn exec_details_end(&self, _req_id: i64) {}

    /// What a fill cost, matched to it by execution id.
    fn commission_and_fees_report(&self, _commission_and_fees_report: Py<PyAny>) {}

    // ── Account ──

    /// One figure the venue states about an account, in the currency it
    /// states it in. An account is stated in several currencies at once, so the
    /// same key arrives more than once.
    fn update_account_value(&self, _key: &str, _value: &str, _currency: &str, _account_name: &str) {}

    /// One position, as the venue values it now.
    fn update_portfolio(
        &self, _contract: Py<PyAny>, _position: f64, _market_price: f64,
        _market_value: f64, _average_cost: f64, _unrealized_pnl: f64,
        _realized_pnl: f64, _account_name: &str,
    ) {}

    /// When the account figures above were last stated.
    fn update_account_time(&self, _timestamp: &str) {}

    /// The account has been fully stated. Fires once the venue has
    /// stopped adding to it, not on the first figure.
    fn account_download_end(&self, _account: &str) {}

    /// One figure answering `req_account_summary`, in the currency the
    /// venue states it in.
    fn account_summary(&self, _req_id: i64, _account: &str, _tag: &str, _value: &str, _currency: &str) {}

    /// Every figure answering this request has been stated.
    fn account_summary_end(&self, _req_id: i64) {}

    /// One position held, on any account this login may act for.
    fn position(&self, _account: &str, _contract: Py<PyAny>, _pos: f64, _avg_cost: f64) {}

    /// Every position has been stated.
    fn position_end(&self) {}

    /// An account's running profit: today's, what is unrealised, and what
    /// has been realised.
    fn pnl(&self, _req_id: i64, _daily_pnl: f64, _unrealized_pnl: f64, _realized_pnl: f64) {}

    /// The same for one position, with the size held.
    fn pnl_single(
        &self, _req_id: i64, _pos: f64, _daily_pnl: f64,
        _unrealized_pnl: f64, _realized_pnl: f64, _value: f64,
    ) {}

    // ── Historical Data ──

    /// One bar answering a historical request. `bar.date` is a day for a
    /// daily bar and a moment for anything shorter, in the zone the bar carries.
    fn historical_data(&self, _req_id: i64, _bar: Py<PyAny>) {}

    /// Every bar answering this request has been stated, and the window
    /// they cover.
    fn historical_data_end(&self, _req_id: i64, _start: &str, _end: &str) {}

    /// A bar that continues a `keep_up_to_date` request, after its
    /// first batch completed. The bar still forming is restated as it changes.
    fn historical_data_update(&self, _req_id: i64, _bar: Py<PyAny>) {}

    /// The earliest moment the venue holds data for a contract.
    fn head_timestamp(&self, _req_id: i64, _head_timestamp: &str) {}

    // ── Contract Details ──

    /// One contract matching a description, with everything the venue
    /// states about it. A description can match more than one.
    fn contract_details(&self, _req_id: i64, _contract_details: Py<PyAny>) {}

    /// Every contract matching this request has been stated.
    fn contract_details_end(&self, _req_id: i64) {}

    /// Contracts whose symbol or name matches a pattern, across venues.
    fn symbol_samples(&self, _req_id: i64, _contract_descriptions: Py<PyAny>) {}

    // ── Tick-by-Tick ──

    /// One trade, as it happens. `tick_attrib_last` says whether it was
    /// past a limit and whether it goes unreported to the tape.
    fn tick_by_tick_all_last(
        &self, _req_id: i64, _tick_type: i32, _time: i64, _price: f64,
        _size: f64, _tick_attrib_last: Py<PyAny>, _exchange: &str, _special_conditions: &str,
    ) {}

    /// One change to the top of the book, as it happens.
    fn tick_by_tick_bid_ask(
        &self, _req_id: i64, _time: i64, _bid_price: f64, _ask_price: f64,
        _bid_size: f64, _ask_size: f64, _tick_attrib_bid_ask: Py<PyAny>,
    ) {}

    /// One change to the midpoint, as it happens.
    fn tick_by_tick_mid_point(&self, _req_id: i64, _time: i64, _mid_point: f64) {}

    // ── Scanner ──

    /// One row of a scan, in rank order.
    fn scanner_data(
        &self, _req_id: i64, _rank: i32, _contract_details: Py<PyAny>,
        _distance: &str, _benchmark: &str, _projection: &str, _legs_str: &str,
    ) {}

    /// Every row of this scan has been stated.
    fn scanner_data_end(&self, _req_id: i64) {}

    /// Every scan the venue offers and what each can be filtered by, as
    /// the XML the venue publishes.
    fn scanner_parameters(&self, _xml: &str) {}

    // ── News ──

    /// Every news provider this account may read.
    fn news_providers(&self, _news_providers: Py<PyAny>) {}

    /// The body of one article. `article_type` is 0 for text and 1 for a
    /// binary document.
    fn news_article(&self, _req_id: i64, _article_type: i32, _article_text: &str) {}

    /// One headline from the archive.
    fn historical_news(
        &self, _req_id: i64, _time: &str, _provider_code: &str,
        _article_id: &str, _headline: &str,
    ) {}

    /// Every headline answering this request has been stated, and
    /// whether the archive holds more.
    fn historical_news_end(&self, _req_id: i64, _has_more: bool) {}

    /// A headline about a contract being watched, as it is published.
    fn tick_news(
        &self, _ticker_id: i64, _time_stamp: i64, _provider_code: &str,
        _article_id: &str, _headline: &str, _extra_data: &str,
    ) {}

    // ── Market Depth ──

    /// One level of a book that names no venue. `operation` is 0 to
    /// insert, 1 to update, 2 to delete; `side` is 0 ask, 1 bid.
    fn update_mkt_depth(
        &self, _req_id: i64, _position: i32, _operation: i32,
        _side: i32, _price: f64, _size: f64,
    ) {}

    /// One level of a book that names the venue it stands on. Every
    /// level from this client names one.
    fn update_mkt_depth_l2(
        &self, _req_id: i64, _position: i32, _market_maker: &str,
        _operation: i32, _side: i32, _price: f64, _size: f64, _is_smart_depth: bool,
    ) {}

    // ── Market Depth (additional) ──

    /// Every exchange the venue names, in the two sections it names
    /// them in: shares and derivatives.
    fn mkt_depth_exchanges(&self, _depth_mkt_data_descriptions: Py<PyAny>) {}

    // ── Real-Time Bars ──

    /// One five-second bar of a live stream.
    fn real_time_bar(
        &self, _req_id: i64, _date: i64, _open: f64, _high: f64,
        _low: f64, _close: f64, _volume: f64, _wap: f64, _count: i32,
    ) {}

    // ── Historical Ticks ──

    /// Historical midpoints, in batches, until `done`.
    fn historical_ticks(&self, _req_id: i64, _ticks: Py<PyAny>, _done: bool) {}

    /// Historical quotes, in batches, until `done`.
    fn historical_ticks_bid_ask(&self, _req_id: i64, _ticks: Py<PyAny>, _done: bool) {}

    /// Historical trades, in batches, until `done`.
    fn historical_ticks_last(&self, _req_id: i64, _ticks: Py<PyAny>, _done: bool) {}

    // ── Options ──

    /// The venue's model for an option: the volatility its price implies, the
    /// greeks, and the modelled value of the option and its underlying.
    fn tick_option_computation(
        &self, _req_id: i64, _tick_type: i32, _tick_attrib: i32,
        _implied_vol: f64, _delta: f64, _opt_price: f64, _pv_dividend: f64,
        _gamma: f64, _vega: f64, _theta: f64, _und_price: f64,
    ) {}

    /// One venue's option chain for an underlying: the
    /// expiries and strikes it lists.
    fn security_definition_option_parameter(
        &self, _req_id: i64, _exchange: &str, _underlying_con_id: i64,
        _trading_class: &str, _multiplier: &str, _expirations: Py<PyAny>, _strikes: Py<PyAny>,
    ) {}

    /// Every venue's chain has been stated.
    fn security_definition_option_parameter_end(&self, _req_id: i64) {}

    // ── Fundamental Data ──

    /// A fundamental report, as the XML the venue publishes.
    fn fundamental_data(&self, _req_id: i64, _data: &str) {}

    // ── News Bulletins ──

    /// A notice the venue broadcasts to everyone — an exchange
    /// unavailable, a system message.
    fn update_news_bulletin(&self, _msg_id: i64, _msg_type: i32, _message: &str, _orig_exchange: &str) {}

    // ── Financial Advisor ──

    /// A partition of an advisor's configuration, as the XML the venue
    /// holds it in.
    fn receive_fa(&self, _fa_data_type: i32, _xml: &str) {}

    /// An advisor configuration has been replaced.
    fn replace_fa_end(&self, _req_id: i64, _text: &str) {}

    // ── Multi-Account / Multi-Model ──

    /// One position, for a request naming an account or a model.
    fn position_multi(&self, _req_id: i64, _account: &str, _model_code: &str, _contract: Py<PyAny>, _pos: f64, _avg_cost: f64) {}

    /// Every position answering this request has been stated.
    fn position_multi_end(&self, _req_id: i64) {}

    /// One account figure, for a request naming an account or a model.
    fn account_update_multi(&self, _req_id: i64, _account: &str, _model_code: &str, _key: &str, _value: &str, _currency: &str) {}

    /// Every figure answering this request has been stated.
    fn account_update_multi_end(&self, _req_id: i64) {}

    // ── Tier 3: Display Groups ──

    /// Which display groups exist, as the venue numbers them.
    fn display_group_list(&self, _req_id: i64, _groups: &str) {}

    /// What a display group is now showing.
    fn display_group_updated(&self, _req_id: i64, _contract_info: &str) {}

    // ── Tier 3: Market Rules ──

    /// The price ladder a contract trades on: each step, and what the
    /// price moves in above it.
    fn market_rule(&self, _market_rule_id: i64, _price_increments: Py<PyAny>) {}

    // ── Tier 3: Smart Components ──

    /// Which venue each bit of a quote's exchange mask refers to, and
    /// the letter that venue is named by.
    fn smart_components(&self, _req_id: i64, _smart_component_map: Py<PyAny>) {}

    // ── Tier 3: Soft Dollar Tiers ──

    /// The soft dollar tiers this account may direct commission to.
    fn soft_dollar_tiers(&self, _req_id: i64, _tiers: Py<PyAny>) {}

    // ── Tier 3: Family Codes ──

    /// The account families this login belongs to.
    fn family_codes(&self, _family_codes: Py<PyAny>) {}

    // ── Tier 3: Histogram Data ──

    /// How much traded at each price over a window.
    fn histogram_data(&self, _req_id: i64, _items: Py<PyAny>) {}

    // ── Tier 3: User Info ──

    /// What the login is entitled to, as the venue states it.
    fn user_info(&self, _req_id: i64, _white_branding_id: &str) {}

    // ── Tier 3: WSH ──

    /// What the corporate-events calendar carries: its event types and
    /// the fields each one has, as the JSON the venue publishes.
    fn wsh_meta_data(&self, _req_id: i64, _data_json: &str) {}

    /// Events from the corporate-events calendar, as the JSON the venue
    /// publishes. Events themselves need a Wall Street Horizon subscription; a
    /// login without one is answered with an empty set.
    fn wsh_event_data(&self, _req_id: i64, _data_json: &str) {}

    // ── Tier 3: Completed Orders ──

    /// An order that is done — filled, cancelled or expired — as the
    /// venue holds it.
    fn completed_order(&self, _contract: Py<PyAny>, _order: Py<PyAny>, _order_state: Py<PyAny>) {}

    /// Every completed order has been stated.
    fn completed_orders_end(&self) {}

    // ── Tier 3: Order Bound ──

    /// An order placed elsewhere has been bound to this session, so its
    /// changes arrive here.
    fn order_bound(&self, _order_id: i64, _api_client_id: i64, _api_order_id: i64) {}

    // ── Tier 3: Tick Req Params ──

    /// What a subscription was given: the increment its prices move in,
    /// which venues it is served from, and which feed answered.
    fn tick_req_params(&self, _ticker_id: i64, _min_tick: f64, _bbo_exchange: &str, _snapshot_permissions: i64) {}

    // ── Tier 3: Bond Contract Details ──

    /// One bond matching a description, with its terms: what it pays,
    /// how and when, whether it can be called or put, whether it converts, what
    /// it is rated.
    fn bond_contract_details(&self, _req_id: i64, _contract_details: Py<PyAny>) {}

    // ── Tier 3: Delta Neutral Validation ──

    /// The contract the venue paired with a delta-neutral order.
    fn delta_neutral_validation(&self, _req_id: i64, _delta_neutral_contract: Py<PyAny>) {}

    // ── Tier 3: defined by the reference client, and never fired here ──
    //
    // Each exists so a program written against that client runs against this
    // one unchanged, and each says why it stays silent.

    /// The contract a market-data request should be asked for under instead.
    ///
    /// The reference client answers a request on a contract the venue reroutes
    /// — a contract for difference standing for a share — with the contract and
    /// venue to ask again under. Nothing on this connection has been seen to
    /// state one, so nothing here fires this.
    fn reroute_mkt_data_req(&self, _req_id: i64, _con_id: i64, _exchange: &str) {}

    /// The same, for a request for the book rather than the quote.
    fn reroute_mkt_depth_req(&self, _req_id: i64, _con_id: i64, _exchange: &str) {}

    /// An exchange-for-physical quote, which the reference client reports on a
    /// callback of its own rather than as a price. This client carries the tick
    /// types such a quote is numbered under and does not assemble them into
    /// this record.
    #[allow(clippy::too_many_arguments)]
    fn tick_efp(
        &self, _req_id: i64, _tick_type: i32, _basis_points: f64,
        _formatted_basis_points: &str, _implied_future: f64, _hold_days: i32,
        _future_last_trade_date: &str, _dividend_impact: f64,
        _dividends_to_last_trade_date: f64,
    ) {}

    /// A step in the handshake a third-party program makes with a terminal
    /// before that terminal will carry its requests. There is no terminal
    /// between this client and the venue, so nothing here fires these four.
    fn verify_message_api(&self, _api_data: &str) {}
    /// Whether that handshake was accepted.
    fn verify_completed(&self, _is_successful: bool, _error_text: &str) {}
    /// The same handshake, where the terminal also authenticates the program.
    fn verify_and_auth_message_api(&self, _api_data: &str, _xyz_challenge: &str) {}
    /// Whether that one was accepted.
    fn verify_and_auth_completed(&self, _is_successful: bool, _error_text: &str) {}

    /// What the reference client reports when its own socket layer fails on
    /// Windows. This client has no such layer: trouble on a connection reaches
    /// a caller on the error callback.
    fn win_error(&self, _text: &str, _last_error: i32) {}

    // ── Tier 3: Historical Schedule ──

    /// When a contract's venue was open over a window, session by
    /// session, in the zone the venue keeps.
    fn historical_schedule(&self, _req_id: i64, _start_date_time: &str, _end_date_time: &str, _time_zone: &str, _sessions: Py<PyAny>) {}
}

/// Register EWrapper on the module.
pub fn register(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<EWrapper>()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The constructor takes the arguments PyO3 passes a `#[new]`, so the only
    /// way to build one is from Python. That the type is constructible at all
    /// is what the compiler already checks; this asserts that the type reaches
    /// Python carrying callbacks on it.
    ///
    /// Which callbacks, and all of them, is asserted where the list can be
    /// written out without repeating it in two languages:
    /// `tests/python/test_the_callback_surface_is_complete.py`. Five names
    /// here would pass while the other seventy-six were missing.
    #[test]
    fn ewrapper_reaches_python_carrying_callbacks() {
        Python::initialize();
        Python::attach(|py| {
            let cls = py.get_type::<EWrapper>();
            for method in ["error", "tick_price", "tick_size", "next_valid_id", "connection_closed"] {
                assert!(
                    cls.hasattr(method).unwrap(),
                    "EWrapper must expose {method}() for a subclass to override",
                );
            }
        });
    }
}
