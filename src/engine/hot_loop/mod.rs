pub mod farm;
pub mod ccp;
pub mod hmds;
pub(crate) mod pool;
pub mod order_builder;
pub mod liveness;
pub(crate) mod optcalc;
pub(crate) mod optparams;

use std::sync::Arc;
use std::time::Instant;
use std::io;

use crate::bridge::{Event, SharedState};
use crate::engine::context::Context;
use crate::config::chrono_free_timestamp;
use crate::gateway::{ccp_reconnect_host, reconnect_ccp_via, CcpReconnect, ReconnectAuth};
use crate::protocol::connection::Connection;
use crate::protocol::fix;
use crate::types::{ControlCommand, InstrumentId, ReqId, Price, Qty, PRICE_SCALE, QTY_SCALE};
#[cfg(any(test, feature = "test-support"))]
use crate::types::{Fill, TbtQuote, TbtTrade};
use crossbeam_channel::{bounded, Receiver, Sender};

use farm::FarmState;
use ccp::CcpState;
use hmds::HmdsState;
pub use liveness::HeartbeatState;
use liveness::FarmCheck;
use pool::{FarmKind, FarmPool, FixSink, PRIMARY_HMDS, PRIMARY_MD};

/// The sink of a farm: the primary market data or historical connection,
/// or a farm opened on demand (#445). A macro so that the borrow stays on
/// the connection fields and the subsystems can be borrowed beside it.
macro_rules! farm_sink {
    ($self:ident, $id:expr) => {
        match $id {
            PRIMARY_MD => Some(&mut $self.farm_conn as &mut dyn FixSink),
            PRIMARY_HMDS => Some(&mut $self.hmds_conn as &mut dyn FixSink),
            id => $self.pool.get_mut(id).map(|f| f as &mut dyn FixSink),
        }
    };
}

/// The pinned-core hot loop. Pushes events to SharedState + optional event channel.
pub struct HotLoop {
    shared: Arc<SharedState>,
    event_tx: Option<Sender<Event>>,
    context: Context,
    /// Core ID to pin the hot loop thread to. None = no pinning.
    core_id: Option<usize>,
    /// Next scheduled CCP/farm reconnect attempt (jittered backoff, ibx#218).
    ccp_next_attempt_at: Option<Instant>,
    farm_next_attempt_at: Option<Instant>,
    /// Farm connection for market data (market data farm).
    pub farm_conn: Option<Connection>,
    /// Auth connection for order management.
    pub ccp_conn: Option<Connection>,
    /// Historical farm connection for historical data (optional).
    pub hmds_conn: Option<Connection>,
    /// SPSC channel receiver for control plane commands.
    control_rx: Option<Receiver<ControlCommand>>,
    /// Whether the hot loop should keep running.
    running: bool,
    /// Account ID for order submission.
    account_id: String,
    /// Heartbeat state.
    hb: HeartbeatState,
    /// Reusable buffer for control commands (avoids per-iteration allocation).
    cmd_buf: Vec<ControlCommand>,
    /// Connection states last reported to the clients; `None` until the
    /// first observation (ibx#399).
    links: Option<Links>,
    /// Market-data farm name, for the farm status messages.
    farm_name: String,
    // ── Subsystems ──
    pub(crate) farm: FarmState,
    pub(crate) ccp: CcpState,
    pub(crate) hmds: HmdsState,
    /// Data farms opened on demand by the routing table (#445).
    pub(crate) pool: FarmPool,
    /// Instruments whose market data record nobody uses, since when, and
    /// whether their quote tags were freed (#292).
    tag_marks: Vec<(InstrumentId, Instant, bool)>,
    /// Next run of the server tag cleaner (#292).
    next_tag_clean: Instant,
    /// Most contracts with API depth at once, from the logon (#452).
    depth_limit: usize,
    // ── Auto-reconnect ──
    reconnect_auth: Option<ReconnectAuth>,
    pending_farm_reconnect: Option<Receiver<io::Result<Connection>>>,
    farm_reconnect_attempt: u32,
    pending_ccp_reconnect: Option<Receiver<io::Result<CcpReconnect>>>,
    ccp_reconnect_attempt: u32,
    /// HMDS reconnect state (ibx#187). Drives a background reconnect loop with
    /// exponential backoff when the historical-data farm is down — initial
    /// connect failed, or a future runtime disconnect detector trips it.
    pending_hmds_reconnect: Option<Receiver<io::Result<Connection>>>,
    hmds_reconnect_attempt: u32,
    /// Earliest instant the next HMDS reconnect attempt may spawn. `None` once
    /// retries are exhausted or HMDS is healthy.
    hmds_next_attempt_at: Option<Instant>,
    /// The timing of the market data resubscription after a data
    /// permission change (ibx#421).
    permission_change: crate::control::logon::PermissionChange,
}

/// Maximum HMDS reconnect attempts before giving up (ibx#187).
/// Total wait at cap: 3+6+12+24+48 = 93s before final attempt fires.
const HMDS_MAX_RECONNECT_ATTEMPTS: u32 = 6;

impl HotLoop {
    pub fn new(shared: Arc<SharedState>, event_tx: Option<Sender<Event>>, core_id: Option<usize>) -> Self {
        Self {
            shared,
            event_tx,
            context: Context::new(),
            core_id,
            farm_conn: None,
            ccp_conn: None,
            hmds_conn: None,
            control_rx: None,
            running: true,
            account_id: String::new(),
            hb: HeartbeatState::new(),
            cmd_buf: Vec::with_capacity(16),
            farm: FarmState::new(),
            ccp: CcpState::new(),
            hmds: HmdsState::new(),
            pool: FarmPool::new(Instant::now()),
            tag_marks: Vec::new(),
            next_tag_clean: Instant::now() + TAG_CLEAN_PERIOD,
            depth_limit: 3,
            reconnect_auth: None,
            pending_farm_reconnect: None,
            ccp_next_attempt_at: None,
            farm_next_attempt_at: None,
            farm_reconnect_attempt: 0,
            pending_ccp_reconnect: None,
            ccp_reconnect_attempt: 0,
            pending_hmds_reconnect: None,
            hmds_reconnect_attempt: 0,
            hmds_next_attempt_at: None,
            links: None,
            farm_name: "usfarm".to_string(),
            permission_change: Default::default(),
        }
    }

    /// Set the control channel receiver. The caller keeps the sender.
    pub fn set_control_rx(&mut self, rx: Receiver<ControlCommand>) {
        self.control_rx = Some(rx);
    }

    /// Set the account ID for order submission.
    pub fn set_account_id(&mut self, account_id: String) {
        self.account_id = account_id;
    }

    /// The session counts US stock sizes in round lots (ibx#287): market
    /// data sizes of those stocks are multiplied by the contract's lot.
    pub fn set_scale_us_lots(&mut self, on: bool) {
        self.context.scale_us_lots = on;
    }

    /// The logon's price management feature and exclusion list (ibx#492).
    pub fn set_price_mgmt(&mut self, on: bool, exclusions: Option<&str>) {
        self.context.price_mgmt_feature = on;
        self.context.price_mgmt_exclusions = exclusions.map(crate::engine::price_mgmt::parse_exclusions);
    }

    /// The routing table of a primary farm (#445): the market data table
    /// routes market data, the historical table historical requests.
    pub fn set_routing_table(&mut self, kind: crate::engine::routing::TableKind, text: &str) {
        let table = crate::engine::routing::RoutingTable::parse(text, kind);
        log::info!("Routing table ({:?}): {} farms", kind, table.routes().len());
        match kind {
            crate::engine::routing::TableKind::MarketData => self.farm.routing = Some(table),
            crate::engine::routing::TableKind::Historical => self.hmds.routing = Some(table),
        }
    }

    /// Most contracts with API depth at once, from the logon (#452).
    pub fn set_depth_limit(&mut self, limit: usize) {
        self.depth_limit = limit;
    }

    /// The logon turns the user book on (`6247=demo`, as on paper): depth
    /// books are given as an index diff of the book shown (#451).
    pub fn set_user_book(&mut self, on: bool) {
        self.farm.user_book = on;
    }

    /// Most real-time bar requests at once, from the logon (ibx#454).
    pub fn set_max_real_time_requests(&mut self, max: u32) {
        self.hmds.max_real_time_requests = max;
    }

    /// Access the context (for pre-start configuration like registering instruments).
    pub fn context_mut(&mut self) -> &mut Context {
        &mut self.context
    }

    /// Process pending control commands once. For testing.
    pub fn poll_once(&mut self) {
        self.poll_control_commands();
    }

    /// Whether the hot loop is still running. For testing.
    #[doc(hidden)]
    pub fn is_running(&self) -> bool {
        self.running
    }

    /// Build a HotLoop with connections and control channel, without requiring a Gateway.
    pub fn with_connections(
        shared: Arc<SharedState>,
        event_tx: Option<Sender<Event>>,
        account_id: String,
        farm_conn: Connection,
        ccp_conn: Connection,
        hmds_conn: Option<Connection>,
        core_id: Option<usize>,
    ) -> (Self, Sender<ControlCommand>) {
        let (tx, rx) = bounded(64);
        let mut hl = Self::new(shared, event_tx, core_id);
        hl.set_control_rx(rx);
        hl.set_account_id(account_id);
        hl.farm_conn = Some(farm_conn);
        hl.ccp_conn = Some(ccp_conn);
        hl.hmds_conn = hmds_conn;
        (hl, tx)
    }

    /// Run the hot loop under `catch_unwind`. On panic, log the payload and
    /// emit `Event::Disconnected` so consumers see the dead engine without
    /// having to wait for the next outbound call to fail. Use this from the
    /// engine-spawn site instead of `run()` directly (ibx#182).
    /// try_register + full-table rejection (ibx#233). On a full table the
    /// reply channel gets an Err — the caller's request fails loudly and the
    /// hot loop keeps running. Previously this was an assert! that killed
    /// the engine for the rest of the process.
    fn register_or_reject(
        &mut self,
        con_id: i64,
        symbol: String,
        sec_type: &str,
        exchange: &str,
        reply_tx: &Option<crossbeam_channel::Sender<Result<InstrumentId, String>>>,
    ) -> Option<InstrumentId> {
        self.register_slot_or_reject(Some(con_id), symbol, sec_type, exchange, reply_tx)
    }

    /// `register_or_reject` for a slot keyed by its conId, or (None) a slot
    /// of its own for a contract whose conId is not known yet (ibx#278).
    fn register_slot_or_reject(
        &mut self,
        con_id: Option<i64>,
        symbol: String,
        sec_type: &str,
        exchange: &str,
        reply_tx: &Option<crossbeam_channel::Sender<Result<InstrumentId, String>>>,
    ) -> Option<InstrumentId> {
        let con_id_log = con_id.unwrap_or(0);
        let registered = match con_id {
            Some(con_id) => self.context.market.try_register(con_id),
            None => self.context.market.try_register_unresolved(),
        };
        match registered {
            Some(id) => {
                self.context.market.set_symbol(id, symbol);
                self.context.market.set_routing(id, sec_type, exchange);
                self.shared.market.set_instrument_count(self.context.market.count());
                if let Some(tx) = reply_tx { let _ = tx.send(Ok(id)); }
                Some(id)
            }
            None => {
                log::error!("Instrument table full: rejecting registration for con_id={}", con_id_log);
                if let Some(tx) = reply_tx {
                    let _ = tx.send(Err(format!(
                        "instrument table full: {} contracts are live concurrently; \
                         cancel unused market-data subscriptions to free slots",
                        crate::types::MAX_INSTRUMENTS
                    )));
                }
                None
            }
        }
    }

    /// Whether the farm of a subscription depends on the contract's
    /// aggregate group and it is not known yet: a SMART route with a
    /// routing table (#445). The reference knows the contract before it
    /// subscribes.
    fn needs_agg_group(&self, sub: &farm::MdSubscribe) -> bool {
        let sec_type = if sub.sec_type.is_empty() { "STK" } else { sub.sec_type.as_str() };
        self.farm.routing.is_some()
            && sub.con_id > 0
            && farm::routing_exchange(&sub.exchange, sec_type) == "BEST"
            && !self.context.agg_groups.contains_key(&sub.con_id)
    }

    /// Round lot of a new market data subscription (ibx#287), and the
    /// aggregate group its route needs (#445). When the session counts US
    /// stock sizes in round lots and the contract may be one, or when its
    /// SMART route needs its aggregate group, its definition is asked for
    /// and the subscription waits for it, as the reference knows the
    /// contract before it subscribes: true when parked. Otherwise the known
    /// lot (or 1) is set now.
    fn park_for_round_lot(&mut self, sub: &farm::MdSubscribe) -> bool {
        let id = sub.instrument;
        let maybe_stock = matches!(sub.sec_type.to_ascii_uppercase().as_str(), "" | "STK" | "WAR");
        let lot_needed = self.context.scale_us_lots && maybe_stock && sub.con_id > 0
            && !self.context.round_lots.contains_key(&sub.con_id);
        if !lot_needed {
            let lot = if self.context.scale_us_lots {
                self.context.round_lots.get(&sub.con_id).copied().unwrap_or(1)
            } else {
                1
            };
            self.context.market.set_round_lot(id, lot);
        }
        if !lot_needed && !self.needs_agg_group(sub) {
            return false;
        }
        if !self.ask_definition(sub.con_id, &sub.exchange) {
            log::warn!("No auth connection to read the definition of con_id {}: subscribing with a round lot of 1", sub.con_id);
            self.context.market.set_round_lot(id, 1);
            return false;
        }
        self.context.lot_parked.push(sub.clone());
        true
    }

    /// The farm of a top-of-book subscription (#445): the farm of its
    /// routing row for (routing exchange or aggregate group, security type,
    /// Top), opened on demand when it is not a primary farm. The primary
    /// farm when no table came. None when no row serves it (the reference
    /// drops the entry with a log line only) or for a NEWS contract, which
    /// has no top of book.
    fn md_target(&mut self, sub: &farm::MdSubscribe) -> Option<pool::FarmId> {
        if sub.sec_type.eq_ignore_ascii_case("NEWS") {
            log::info!("No top-of-book entry for the NEWS contract {}", sub.symbol);
            return None;
        }
        self.md_route(sub)
    }

    /// The farm of the route of a contract's market data (#445), for its
    /// top of book and its news (ibx#458).
    fn md_route(&mut self, sub: &farm::MdSubscribe) -> Option<pool::FarmId> {
        let Some(table) = self.farm.routing.as_ref() else { return Some(PRIMARY_MD) };
        let sec_type = if sub.sec_type.is_empty() { "STK" } else { sub.sec_type.as_str() };
        let exchange = farm::routing_exchange(&sub.exchange, sec_type);
        let agg_group = if exchange == "BEST" {
            self.context.agg_groups.get(&sub.con_id).copied().unwrap_or(-1)
        } else {
            -1
        };
        let Some(route) = table.lookup(exchange, agg_group, sec_type, crate::engine::routing::DataType::Top, "*") else {
            log::error!("Error: no route data for {} ({}) type=Top. Looked for exchange={} aggGroup={} secType={}",
                sub.con_id, sub.symbol, exchange, agg_group, sec_type);
            return None;
        };
        if route.farm == self.farm_name {
            return Some(PRIMARY_MD);
        }
        let (name, host) = (route.farm.clone(), route.host.clone());
        Some(self.pool.ensure(&name, &host, FarmKind::MarketData, Instant::now()))
    }

    /// Start a tick-by-tick request (ibx#404, ibx#455), in the reference's
    /// order: no `TickByTick` route for the contract is 10189; past the
    /// tick-by-tick limit, 10190; a stream that exists for the contract,
    /// type and size filter takes the request; else the query goes to the
    /// farm of the contract's day chart route.
    #[allow(clippy::too_many_arguments)]
    fn start_tbt(&mut self, req_id: ReqId, con_id: i64, id: InstrumentId, symbol: &str, exchange: &str, sec_type: &str,
        tbt_type: crate::types::TbtType, number_of_ticks: i32, ignore_size: bool) {
        use crate::engine::routing::DataType;
        let st = if sec_type.is_empty() { "STK" } else { sec_type };
        let route_exchange = farm::routing_exchange(exchange, st).to_string();
        let agg_group = self.agg_group_for(con_id, &route_exchange);
        let not_supported = || format!("{}{} tick-by-tick requests are not supported for {}",
            hmds::TBT_REQUEST_ERROR, tbt_type.as_str(), symbol);
        let tbt_route = self.hmds.routing.as_ref()
            .is_none_or(|t| t.lookup(&route_exchange, agg_group, st, DataType::TickByTick, "*").is_some());
        if !tbt_route {
            log::error!("No tick-by-tick route for exchange={} aggGroup={} secType={}", route_exchange, agg_group, st);
            self.shared.market.push_tbt_error(req_id, 10189, not_supported());
            return;
        }
        if let (Some(limit), _) = self.shared.reference.tick_by_tick_limits() {
            // The farm of the query, when it is open: queries out are
            // counted on it.
            let farm_id = match self.hmds.routing.as_ref() {
                None => Some(PRIMARY_HMDS),
                Some(t) => t.lookup(&route_exchange, agg_group, st, DataType::DayChart, "*").and_then(|r|
                    if r.farm == self.primary_hmds_name() { Some(PRIMARY_HMDS) } else { self.pool.find(&r.farm) }),
            };
            if self.hmds.tbt_over_limit(con_id, farm_id, limit) {
                self.shared.market.push_tbt_error(req_id, 10190, "Max number of tick-by-tick requests has been reached".to_string());
                return;
            }
        }
        if self.hmds.tbt_join(req_id, con_id, id, tbt_type, ignore_size) {
            return;
        }
        let Some(farm_id) = self.hmds_target(&route_exchange, agg_group, st, DataType::DayChart) else {
            self.shared.market.push_tbt_error(req_id, 10189, not_supported());
            return;
        };
        if let Some(sink) = farm_sink!(self, farm_id) {
            self.hmds.send_tbt_subscribe(req_id, con_id, id, symbol, exchange, sec_type, tbt_type,
                number_of_ticks, ignore_size, farm_id, sink, &mut self.hb);
        }
    }

    /// Send the tick-by-tick cancels that are due, each to its stream's
    /// farm (ibx#404).
    fn send_due_tbt_cancels(&mut self, now: Instant) {
        for (id, xml) in self.hmds.take_due_tbt_cancels(now) {
            let ts = chrono_free_timestamp();
            if let Some(sink) = farm_sink!(self, id) {
                sink.send_plain(&[(fix::TAG_MSG_TYPE, "Z"), (fix::TAG_SENDING_TIME, &ts), (6118, &xml)]);
            }
        }
    }

    /// The aggregate group of a contract for a route on `route_exchange`:
    /// its own for a SMART route, else none.
    fn agg_group_for(&self, con_id: i64, route_exchange: &str) -> i32 {
        if route_exchange == "BEST" {
            self.context.agg_groups.get(&con_id).copied().unwrap_or(-1)
        } else {
            -1
        }
    }

    /// A historical request whose route needs the contract's aggregate
    /// group (a SMART route, with a historical routing table) waits for the
    /// contract's definition when the group is not known; it comes back
    /// to the command loop once the definition is read (or after the
    /// lookup's deadline). True when parked.
    fn park_for_hmds_definition(&mut self, con_id: i64, exchange: &str, sec_type: &str, cmd: ControlCommand) -> bool {
        let st = if sec_type.is_empty() { "STK" } else { sec_type };
        let needs = self.hmds.routing.is_some()
            && con_id > 0
            && farm::routing_exchange(exchange, st) == "BEST"
            && !self.context.agg_groups.contains_key(&con_id);
        if !needs || !self.ask_definition(con_id, exchange) {
            return false;
        }
        self.context.def_parked.push((con_id, cmd));
        true
    }

    /// Ask the auth connection for a contract's definition, unless a lookup
    /// for it is in flight. False when it cannot be asked.
    fn ask_definition(&mut self, con_id: i64, exchange: &str) -> bool {
        if self.context.lot_lookups.iter().any(|(_, c, _)| *c == con_id) {
            return true;
        }
        let Some(conn) = self.ccp_conn.as_mut().filter(|_| !self.ccp.disconnected) else {
            return false;
        };
        let req_id = format!("ibxlot{}", self.context.next_lot_lookup);
        self.context.next_lot_lookup = self.context.next_lot_lookup.wrapping_add(1);
        let ts = chrono_free_timestamp();
        let con_id_str = con_id.to_string();
        let exchange = match exchange.to_ascii_uppercase().as_str() {
            "" | "SMART" => "BEST".to_string(),
            other => other.to_string(),
        };
        let _ = conn.send_fix(&[
            (fix::TAG_MSG_TYPE, "c"),
            (fix::TAG_SENDING_TIME, &ts),
            (320, &req_id),
            (321, "2"),
            (146, "1"),
            (6008, &con_id_str),
            (6004, &exchange),
        ]);
        self.hb.last_ccp_sent = Instant::now();
        log::info!("Definition of con_id {} on {} asked ({})", con_id, exchange, req_id);
        self.context.lot_lookups.push((req_id, con_id, Instant::now() + farm::LOT_LOOKUP_TIMEOUT));
        true
    }

    /// Symbol of the chart name of a historical query: the contract's
    /// local symbol when known (`EUR.USD`), else the given one.
    fn chart_symbol(&self, con_id: i64, symbol: &str) -> String {
        match self.shared.reference.get_contract(con_id) {
            Some(c) if !c.local_symbol.is_empty() => c.local_symbol,
            _ => symbol.to_string(),
        }
    }

    /// Name of the primary historical farm.
    fn primary_hmds_name(&self) -> String {
        self.hmds_farm_name().to_string()
    }

    /// The farm of a historical request (#445): the farm of its routing
    /// row (exchange, aggregate group for a SMART request, security type,
    /// data kind), opened on demand when it is not the primary historical
    /// farm. The primary farm when no table came; None when no row
    /// serves it.
    fn hmds_target(&mut self, exchange: &str, agg_group: i32, sec_type: &str, data_type: crate::engine::routing::DataType) -> Option<pool::FarmId> {
        let Some(table) = self.hmds.routing.as_ref() else { return Some(PRIMARY_HMDS) };
        let Some(route) = table.lookup(exchange, agg_group, sec_type, data_type, "*") else {
            log::error!("No historical route for exchange={} aggGroup={} secType={} type={}",
                exchange, agg_group, sec_type, data_type.name());
            return None;
        };
        if route.farm == self.primary_hmds_name() {
            return Some(PRIMARY_HMDS);
        }
        let (name, host) = (route.farm.clone(), route.host.clone());
        Some(self.pool.ensure(&name, &host, FarmKind::Historical, Instant::now()))
    }

    /// Send a top-of-book subscription to the farm of its route (#445),
    /// then the news entry of its request on the same farm (ibx#458). A
    /// request whose news tick is refused ends with 10094 instead, nothing
    /// sent, as the reference.
    fn route_md_subscribe(&mut self, sub: &farm::MdSubscribe) {
        let news = self.take_waiting_news(sub.instrument);
        if let Some(text) = news.iter().find_map(|(_, refusal)| refusal.clone()) {
            self.shared.market.push_md_reject(crate::bridge::MdReject::NewsRefused { instrument: sub.instrument, text });
            return;
        }
        // A NEWS contract has no top of book.
        let news_contract = sub.sec_type.eq_ignore_ascii_case("NEWS");
        if news_contract && news.is_empty() {
            log::info!("No top-of-book entry for the NEWS contract {}", sub.symbol);
            return;
        }
        let Some(id) = self.md_route(sub) else { return };
        if let Some(f) = self.pool.get_mut(id) {
            f.note_request(Instant::now());
        }
        if !news_contract {
            let Some(sink) = farm_sink!(self, id) else { return };
            self.farm.subscribe_top(sub, id, sink, &mut self.hb);
        }
        for (providers, _) in news {
            let msgs = self.farm.start_news(sub.instrument, sub.con_id, &sub.sec_type, &providers, id);
            self.send_farm_messages(msgs);
        }
    }

    /// The news ticks waiting for the top of book of an instrument, in the
    /// order they came: (provider key, 10094 text of a refused one).
    fn take_waiting_news(&mut self, instrument: InstrumentId) -> Vec<(String, Option<String>)> {
        let mut out = Vec::new();
        self.farm.news_waiting.retain(|(id, providers, refusal)| {
            if *id != instrument { return true; }
            out.push((providers.clone(), refusal.clone()));
            false
        });
        out
    }

    /// The news tick of a request (ibx#458): with its top of book when that
    /// waits (a lookup, a round lot, an aggregate group); else at once, on
    /// the farm of the contract's route.
    fn subscribe_news(&mut self, instrument: InstrumentId, con_id: i64, exchange: String, sec_type: String,
                      providers: String, refusal: Option<String>) {
        let waits = self.context.lot_parked.iter().chain(&self.context.lot_ready).chain(&self.context.md_resolved)
            .chain(self.context.md_lookups.iter().map(|(_, s, _)| s))
            .any(|s| s.instrument == instrument);
        if waits {
            self.farm.news_waiting.push((instrument, providers, refusal));
            return;
        }
        if let Some(text) = refusal {
            self.shared.market.push_md_reject(crate::bridge::MdReject::NewsRefused { instrument, text });
            return;
        }
        let sub = farm::MdSubscribe {
            con_id, symbol: String::new(), exchange, sec_type, last_trade_date: String::new(), strike: 0.0,
            right: String::new(), multiplier: String::new(), instrument, mode_9887: 0, snapshot: false,
        };
        let Some(id) = self.md_route(&sub) else { return };
        if let Some(f) = self.pool.get_mut(id) {
            f.note_request(Instant::now());
        }
        let msgs = self.farm.start_news(instrument, con_id, &sub.sec_type, &providers, id);
        self.send_farm_messages(msgs);
    }

    /// Cancel the top of book of an instrument on each farm it went to
    /// (#445).
    fn route_md_cancel(&mut self, instrument: InstrumentId) {
        let now = Instant::now();
        for (id, msg) in self.farm.unsubscribe_top(instrument) {
            let fields: Vec<(u32, &str)> = msg.iter().map(|(t, v)| (*t, v.as_str())).collect();
            if let Some(f) = self.pool.get_mut(id) {
                f.note_request(now);
            }
            if let Some(sink) = farm_sink!(self, id) {
                if sink.send_comp(&fields) && id == PRIMARY_MD {
                    self.hb.last_farm_sent = now;
                }
            }
        }
        // The news entry of the request goes with it, after its top of
        // book, as the reference's cancels (ibx#458).
        let msgs = self.farm.stop_news(instrument);
        self.send_farm_messages(msgs);
    }

    /// The server tag cleaner (#292), as the reference runs it every 60 s:
    /// the market data record of an instrument with no live top-of-book
    /// request is marked with the time (the mark goes when it is used
    /// again); a record marked for 300 s or more has its quote tags freed,
    /// at most 100 per run, with no age test when more than 1000 wait. The
    /// trade stream tags stay with the contract until its last slot goes.
    fn clean_server_tags(&mut self, now: Instant) {
        if now < self.next_tag_clean {
            return;
        }
        self.next_tag_clean = now + TAG_CLEAN_PERIOD;
        let active: Vec<InstrumentId> = self.context.market.active_instruments().map(|(id, _)| id).collect();
        self.tag_marks.retain(|(id, ..)| active.contains(id));
        for id in active {
            let used = self.farm.has_md_subscription(id);
            let pos = self.tag_marks.iter().position(|(m, ..)| *m == id);
            match (used, pos) {
                (true, Some(p)) => { self.tag_marks.remove(p); }
                (false, None) => self.tag_marks.push((id, now, false)),
                _ => {}
            }
        }
        let waiting: Vec<usize> = (0..self.tag_marks.len()).filter(|&i| !self.tag_marks[i].2).collect();
        let skip_age = waiting.len() > TAG_CLEAN_SKIP_AGE_ABOVE;
        let mut due: Vec<usize> = waiting.into_iter()
            .filter(|&i| skip_age || now.duration_since(self.tag_marks[i].1) >= TAG_UNUSED_FOR)
            .collect();
        due.sort_by_key(|&i| self.tag_marks[i].1);
        for i in due.into_iter().take(TAG_CLEAN_MAX_PER_RUN) {
            let id = self.tag_marks[i].0;
            let freed = self.context.market.drop_quote_tags(id);
            self.tag_marks[i].2 = true;
            log::debug!("Market data record of instrument {} unused: {} quote tags freed", id, freed);
        }
    }

    /// Send market data messages, each to its farm.
    fn send_farm_messages(&mut self, msgs: Vec<(pool::FarmId, Vec<(u32, String)>)>) {
        let now = Instant::now();
        for (id, msg) in msgs {
            let fields: Vec<(u32, &str)> = msg.iter().map(|(t, v)| (*t, v.as_str())).collect();
            if let Some(f) = self.pool.get_mut(id) {
                f.note_request(now);
            }
            if let Some(sink) = farm_sink!(self, id) {
                if sink.send_comp(&fields) && id == PRIMARY_MD {
                    self.hb.last_farm_sent = now;
                }
            }
        }
    }

    /// The farm of a market data route: the primary farm, or one opened on
    /// demand.
    fn md_farm_of(&mut self, route: &crate::engine::routing::Route) -> pool::FarmId {
        if route.farm == self.farm_name {
            PRIMARY_MD
        } else {
            let (name, host) = (route.farm.clone(), route.host.clone());
            self.pool.ensure(&name, &host, FarmKind::MarketData, Instant::now())
        }
    }

    /// The farm of the book of a contract on an exchange (#452): the first
    /// of the aggregate, level-two, extended and plain depth routes, as the
    /// reference tries them. The aggregate rows are taken only with a known
    /// listing exchange.
    fn depth_route(&mut self, con_id: i64, exchange: &str, sec_type: &str) -> Option<pool::FarmId> {
        use crate::engine::routing::DataType;
        let route_exchange = farm::routing_exchange(exchange, sec_type);
        let agg_group = self.agg_group_for(con_id, route_exchange);
        let listing = self.context.listing_exchanges.get(&con_id).cloned();
        let table = self.farm.routing.as_ref()?;
        let route = [DataType::AggDeep, DataType::Deep2, DataType::DeepX, DataType::Deep].into_iter()
            .filter(|dt| *dt != DataType::AggDeep || listing.is_some())
            .find_map(|dt| table.lookup(route_exchange, agg_group, sec_type, dt, listing.as_deref().unwrap_or("*")))?
            .clone();
        Some(self.md_farm_of(&route))
    }

    /// The routing table has the market-maker depth service (Deep2) for
    /// the exchange (#451).
    fn has_deep2(&mut self, con_id: i64, exchange: &str, sec_type: &str) -> bool {
        use crate::engine::routing::DataType;
        let group = self.agg_group_for(con_id, exchange);
        let listing = self.context.listing_exchanges.get(&con_id).cloned();
        self.farm.routing.as_ref()
            .is_some_and(|t| t.lookup(exchange, group, sec_type, DataType::Deep2, listing.as_deref().unwrap_or("*")).is_some())
    }

    /// A depth request (#452), as the reference handles it: the local
    /// refusals (321 for no exchange, a combo or no rows, 322 for a live
    /// request id of the same kind, 309 past the logon's limit of
    /// contracts), then SmartDepth from the contract's SMART component
    /// exchanges (a book where a depth route exists, else the top of book
    /// where a top route exists, else nothing), or the book of its own
    /// exchange (10092 when no depth route serves it); each entry goes to
    /// the farm of its route.
    fn route_depth_subscribe(&mut self, req_id: ReqId, con_id: i64, exchange: String, sec_type: String, num_rows: i32, is_smart_depth: bool) {
        let refuse = |shared: &SharedState, code: i64, text: String| shared.orders.push_order_error(i64::from(req_id), code, text);
        let invalid = |cause: &str| format!("Error validating request.-'bR' : cause - {}", cause);
        if exchange.trim().is_empty() {
            return refuse(&self.shared, 321, invalid("Please enter exchange."));
        }
        if sec_type == "BAG" {
            return refuse(&self.shared, 321, invalid("Market depth does not support combos."));
        }
        if num_rows <= 0 {
            return refuse(&self.shared, 321, invalid("Market depth rows requested must be greater than zero."));
        }
        if self.farm.has_depth_req(req_id, is_smart_depth)
            || (is_smart_depth && self.context.depth_gathers.iter().any(|g| g.req_id == req_id))
        {
            return refuse(&self.shared, 322, "Error processing request.-'bR' : cause - Duplicate ticker id".into());
        }
        let contracts = self.farm.depth_contracts();
        if !contracts.contains(&con_id) && contracts.len() >= self.depth_limit {
            return refuse(&self.shared, 309, format!("Max number ({}) of market depth requests has been reached", self.depth_limit));
        }
        let st = if sec_type.is_empty() { "STK".to_string() } else { sec_type.clone() };
        // The reference knows the contract (its SMART components, listing
        // exchange, aggregate group) first: wait for its definition.
        if self.farm.routing.is_some() && con_id > 0 && !self.context.agg_groups.contains_key(&con_id) {
            let again = ControlCommand::SubscribeDepth {
                req_id, con_id, exchange: exchange.clone(), sec_type: sec_type.clone(), num_rows, is_smart_depth,
            };
            if self.ask_definition(con_id, &exchange) {
                self.context.def_parked.push((con_id, again));
                return;
            }
        }
        if self.farm.routing.is_none() {
            // No routing table: the book of the exchange on the primary farm.
            let spec = farm::DepthSpec::Book { farm: PRIMARY_MD, exchange: farm::routing_exchange(&exchange, &st).to_string() };
            let lot = self.depth_lot(con_id);
            let msgs = self.farm.start_depth(req_id, con_id, &st, is_smart_depth, num_rows, lot, false, vec![spec], None, &self.shared);
            return self.send_farm_messages(msgs);
        }
        let valid = self.context.valid_exchanges.get(&con_id).cloned().unwrap_or_default();
        if is_smart_depth && !valid.is_empty() && st == "STK" {
            return self.gather_depth_components(req_id, con_id, st, num_rows, valid);
        }
        let Some(farm_id) = self.depth_route(con_id, &exchange, &st) else {
            return refuse(&self.shared, 10092, "Deep market data is not supported for this combination of security type/exchange".into());
        };
        if is_smart_depth {
            // SmartDepth that the contract does not allow: the reference
            // hands the book to its single-exchange requests, where this
            // request is not (`jextend.dA.c(dy)` -> `jextend.dK.a(int,dy,false)`
            // finds no entry in its single map): the request stays, with
            // nothing on the wire and no error.
            log::warn!("SmartDepth req {}: not allowed for this contract; nothing is sent, as the reference", req_id);
            let msgs = self.farm.start_depth(req_id, con_id, &st, true, num_rows, 1, false, Vec::new(), None, &self.shared);
            return self.send_farm_messages(msgs);
        }
        let book_exchange = farm::routing_exchange(&exchange, &st).to_string();
        // A book with the market-maker service is written with
        // updateMktDepthL2, as the reference (#451).
        let l2 = self.has_deep2(con_id, &book_exchange, &st);
        let lot = self.depth_lot(con_id);
        let description = self.context.depth_descriptions.get(&con_id).cloned();
        let spec = farm::DepthSpec::Book { farm: farm_id, exchange: book_exchange };
        let msgs = self.farm.start_depth(req_id, con_id, &st, is_smart_depth, num_rows, lot, l2, vec![spec],
            description.as_deref(), &self.shared);
        self.send_farm_messages(msgs);
    }

    /// The round lot of a contract's depth sizes: its round lot when the
    /// session counts in round lots, as its top of book (#451).
    fn depth_lot(&self, con_id: i64) -> i64 {
        if self.context.scale_us_lots { self.context.round_lots.get(&con_id).copied().unwrap_or(1) } else { 1 }
    }

    /// The farm of the top-of-book route of a contract on an exchange
    /// (#452).
    fn top_route_farm(&mut self, con_id: i64, exchange: &str, sec_type: &str) -> Option<pool::FarmId> {
        let route_exchange = farm::routing_exchange(exchange, sec_type);
        let group = self.agg_group_for(con_id, route_exchange);
        let route = self.farm.routing.as_ref()?
            .lookup(route_exchange, group, sec_type, crate::engine::routing::DataType::Top, "*")?
            .clone();
        Some(self.md_farm_of(&route))
    }

    /// SmartDepth (#452): the contract is looked up on each of its valid
    /// exchanges first, as the reference does (`jextend.d0.c(dy)`); its
    /// books and tops go out once every lookup answered, or after 10 s
    /// (`start_gathered_depth`).
    fn gather_depth_components(&mut self, req_id: ReqId, con_id: i64, sec_type: String, num_rows: i32, exchanges: Vec<String>) {
        let ts = chrono_free_timestamp();
        let con = con_id.to_string();
        let mut components = Vec::with_capacity(exchanges.len());
        for exchange in exchanges {
            let lookup = self.context.combos.next_req("SecDefReqMsgReqByConid");
            let wire = farm::routing_exchange(&exchange, &sec_type).to_string();
            if let Some(conn) = self.ccp_conn.as_mut().filter(|_| !self.ccp.disconnected) {
                let _ = conn.send_fix(&[
                    (fix::TAG_MSG_TYPE, "c"),
                    (fix::TAG_SENDING_TIME, &ts),
                    (320, &lookup),
                    (321, "2"),
                    (146, "1"),
                    (6008, &con),
                    (6004, &wire),
                ]);
                self.hb.last_ccp_sent = Instant::now();
            }
            components.push(farm::DepthComponent { exchange, lookup, definition: farm::ComponentDefinition::Waiting });
        }
        log::info!("SmartDepth req {}: {} component definitions asked", req_id, components.len());
        self.context.depth_gathers.push(farm::DepthGather {
            req_id, con_id, sec_type, num_rows, components, deadline: Instant::now() + farm::DEPTH_GATHER_WAIT,
        });
    }

    /// SmartDepth requests whose component lookups all answered, or whose
    /// wait is over (#452), go out.
    fn start_gathered_depth(&mut self) {
        if self.context.depth_gathers.is_empty() {
            return;
        }
        let now = Instant::now();
        let (ready, waiting): (Vec<_>, Vec<_>) = std::mem::take(&mut self.context.depth_gathers).into_iter()
            .partition(|g| g.deadline <= now || g.components.iter().all(|c| c.definition != farm::ComponentDefinition::Waiting));
        self.context.depth_gathers = waiting;
        for g in ready {
            self.start_smart_depth(g);
        }
    }

    /// The books and tops of a SmartDepth request, as the reference sorts
    /// its components (`jextend.d1.process`, `jextend.d0.b(dy)`): a
    /// component found with a depth route gets a book; else one with a top
    /// route gets a top of book, unless it is the smart-routing name or its
    /// order types hold NOT2D; the others nothing. The LAST entry of a top
    /// of book goes to BEST when SMART is a valid exchange (else to the
    /// listing exchange), except on an overnight venue, which keeps its own
    /// (`jclient.is.a(pa,List,List,..)@531-738`, `jmdclient.aP.a(dy,TopQuote,..)`;
    /// captured 28/09/2026: `207=ISE|264=442` with `207=BEST|264=443`,
    /// `207=IBEOS|264=442` with `207=IBEOS|264=443`).
    fn start_smart_depth(&mut self, g: farm::DepthGather) {
        let st = g.sec_type.as_str();
        let has_smart = g.components.iter().any(|c| c.exchange == "SMART");
        let listing = self.context.listing_exchanges.get(&g.con_id).cloned();
        let last_default = if has_smart {
            self.top_route_farm(g.con_id, "SMART", st).map(|f| (f, "BEST".to_string()))
        } else {
            listing.and_then(|l| self.top_route_farm(g.con_id, &l, st).map(|f| (f, l)))
        };
        let mut specs = Vec::new();
        let mut tops = Vec::new();
        for c in &g.components {
            let farm::ComponentDefinition::Found { not2d } = c.definition else { continue };
            if let Some(farm_id) = self.depth_route(g.con_id, &c.exchange, st) {
                specs.push(farm::DepthSpec::Book { farm: farm_id, exchange: farm::routing_exchange(&c.exchange, st).to_string() });
                continue;
            }
            if not2d || c.exchange == "SMART" {
                continue;
            }
            let Some(farm_id) = self.top_route_farm(g.con_id, &c.exchange, st) else { continue };
            let last = if farm::overnight_venue(&c.exchange) {
                Some((farm_id, c.exchange.clone()))
            } else {
                last_default.clone()
            };
            tops.push(farm::DepthSpec::Top { farm: farm_id, exchange: c.exchange.clone(), last });
        }
        specs.extend(tops);
        let lot = self.depth_lot(g.con_id);
        let msgs = self.farm.start_depth(g.req_id, g.con_id, st, true, g.num_rows, lot, false, specs, None, &self.shared);
        self.send_farm_messages(msgs);
    }

    /// Send again, each to the farm of its route, the subscriptions that
    /// lost their entries with their farm (#445, ibx#288).
    fn resend_unsent_subscriptions(&mut self) {
        let unsent = self.farm.unsent_subscriptions(&self.context);
        if !unsent.is_empty() {
            log::info!("Sending {} market data subscriptions again", unsent.len());
        }
        for sub in unsent {
            self.route_md_subscribe(&sub);
        }
    }

    /// Poll the auth socket once, then the timeouts of what waits on it,
    /// and send the market data subscriptions it released (ibx#287).
    #[inline]
    fn poll_auth(&mut self) {
        self.ccp.poll_executions(
            &mut self.ccp_conn, &mut self.context, &self.shared,
            &self.event_tx, &mut self.hb, &self.account_id,
        );
        self.ccp.sweep_pending_schedule_pairs(&self.shared, &self.event_tx);
        self.ccp.sweep_scanner_enrichments(&self.shared);
        self.ccp.sweep_contract_details(&self.shared, &self.event_tx, &mut self.ccp_conn, &mut self.hb);
        self.ccp.optcalc.progress(&mut self.ccp_conn, &mut self.hb, &self.shared);
        self.ccp.pump_matching_symbols(Instant::now(), &mut self.ccp_conn, &mut self.hb, &self.shared);
        order_builder::sweep_rth_lookups(&mut self.context);
        farm::sweep_md_lookups(&mut self.context, &self.shared);
        self.send_md_resolved();
        farm::sweep_round_lot_lookups(&mut self.context);
        self.send_lot_ready();
        // SmartDepth requests whose components answered (#452).
        self.start_gathered_depth();
        // Depth books asked again after a reset (#451).
        let msgs = self.farm.sweep_depth(&self.shared);
        if !msgs.is_empty() {
            self.send_farm_messages(msgs);
        }
        self.hmds.sweep_head_timestamps(&self.shared);
        self.service_logon_updates(Instant::now());
    }

    /// What a logon update or a relogin left for the hot loop (ibx#421,
    /// ibx#276): the data permission change and its market data
    /// resubscription, and the SSL farm list.
    pub(crate) fn service_logon_updates(&mut self, now: Instant) {
        if std::mem::take(&mut self.ccp.permissions_changed) && self.permission_change.change(now) {
            // The reference's API access manager: a warning to the API
            // clients when a request was refused for an API subscription
            // since the last change (`jextend.F.onPermissionsChanged()`).
            if self.farm.take_refused_api_subscriptions() {
                use crate::control::logon::{MARKET_DATA_SUBSCRIPTION_CHANGED, MARKET_DATA_SUBSCRIPTION_CHANGED_CODE};
                log::info!("Market data subscription has been changed: warning {}", MARKET_DATA_SUBSCRIPTION_CHANGED_CODE);
                self.shared.push_connection_notice(MARKET_DATA_SUBSCRIPTION_CHANGED_CODE, MARKET_DATA_SUBSCRIPTION_CHANGED.to_string());
            }
        }
        if self.permission_change.take_due(now) {
            self.resubscribe_market_data();
        }
        if let Some(list) = self.ccp.ssl_farms_update.take() {
            let old = self.reconnect_auth.as_ref().map(|a| a.ssl_farms.clone()).unwrap_or_default();
            log::info!("Unsolicited logon [oldUseSslFarmList={},useSslFarmList={}].", old, list);
            if let Some(auth) = self.reconnect_auth.as_mut() {
                auth.ssl_farms = list.clone();
            }
            if !old.is_empty() && list.is_empty() {
                log::info!("Disconnecting/reconnecting farms without SSL...");
                self.reconnect_all_farms();
            }
        }
    }

    /// The reference's market data resubscription after a data permission
    /// change (ibx#421, `jclient.ij.r()`): "Desubscribing all farm mkt
    /// data", then every streaming subscription and news entry asked again
    /// on new ids.
    fn resubscribe_market_data(&mut self) {
        log::info!("Resubscribing market data. desubscribe=true");
        log::info!("Desubscribing all farm mkt data");
        let cancels = self.farm.desubscribe_all();
        self.send_farm_messages(cancels);
        self.resend_unsent_subscriptions();
        let mut msgs = Vec::new();
        for farm in self.farm.news_farms_to_resend() {
            msgs.extend(self.farm.resend_news(farm));
        }
        self.send_farm_messages(msgs);
    }

    /// Every farm connection is closed and opened again (ibx#276,
    /// `jmdclient.aP.j()`): the market data farm, the historical data farm
    /// and the farms opened on demand; their reconnects follow.
    fn reconnect_all_farms(&mut self) {
        if let Some(conn) = self.farm_conn.as_mut() {
            conn.shutdown();
        }
        if !self.farm.disconnected {
            self.farm.handle_disconnect(&mut self.context, &self.event_tx);
        }
        if let Some(conn) = self.hmds_conn.as_mut() {
            conn.shutdown();
        }
        self.hmds.disconnected = true;
        self.hmds_conn = None;
        let ids: Vec<pool::FarmId> = self.pool.farms.iter().filter(|f| f.conn.is_some()).map(|f| f.id).collect();
        for id in ids {
            if let Some(conn) = self.pool.get_mut(id).and_then(|f| f.conn.as_mut()) {
                conn.shutdown();
            }
            self.pool_farm_lost(id);
        }
    }

    /// A market data subscription without a conId (ibx#278): the contract
    /// is looked up by symbol first, as the reference does, and the
    /// subscription waits for the reply.
    fn lookup_md_contract(&mut self, sub: farm::MdSubscribe, currency: String, filters: crate::types::SecDefFilters) {
        // Asked in the reference's named form, as a contract lookup, with a
        // number from a range of its own so its reply is never taken for
        // a caller's lookup.
        let req_id = farm::MD_LOOKUP_FIRST_ID + self.context.next_md_lookup % farm::MD_LOOKUP_IDS;
        self.context.next_md_lookup = self.context.next_md_lookup.wrapping_add(1);
        let lookup = ccp::SymbolLookup {
            symbol: sub.symbol.clone(),
            sec_type: sub.sec_type.clone(),
            exchange: sub.exchange.clone(),
            currency,
            filters,
            continuous: false,
        };
        let strike = lookup.filters.strike_text();
        match self.ccp_conn.as_mut().filter(|_| !self.ccp.disconnected) {
            Some(conn) => {
                ccp::send_symbol_lookup_on(conn, ReqId::from(req_id), &lookup, &strike);
                self.hb.last_ccp_sent = Instant::now();
                log::info!("Market data for {} {}: contract lookup ({})", sub.symbol, sub.sec_type, req_id);
            }
            // The deadline still ends it with error 200.
            None => log::warn!("No auth connection to look up {} {} for market data ({})", sub.symbol, sub.sec_type, req_id),
        }
        self.context.md_lookups.push((req_id, sub, Instant::now() + farm::MD_LOOKUP_TIMEOUT));
    }

    /// Send the subscriptions whose conId was resolved (ibx#278), through
    /// the round-lot step as any other. A contract another request already
    /// has a top of book for is not asked again: the request joins that
    /// subscription, as the reference attaches it to the contract's record
    /// (ibx#444, captured 02/10/2026), and its own slot goes.
    fn send_md_resolved(&mut self) {
        if self.context.md_resolved.is_empty() { return; }
        for sub in std::mem::take(&mut self.context.md_resolved) {
            let into = self.context.market.instrument_by_con_id(sub.con_id)
                .filter(|&into| into != sub.instrument && sub.mode_9887 == 0 && self.joins_top(into, sub.snapshot));
            if let Some(into) = into {
                let news = self.take_waiting_news(sub.instrument);
                if let Some(text) = news.iter().find_map(|(_, refusal)| refusal.clone()) {
                    self.shared.market.push_md_reject(crate::bridge::MdReject::NewsRefused { instrument: sub.instrument, text });
                    continue;
                }
                log::info!("Market data for {} {}: conId {} joins the subscription of instrument {}",
                    sub.symbol, sub.sec_type, sub.con_id, into);
                for (providers, _) in news {
                    self.subscribe_news(into, sub.con_id, sub.exchange.clone(), sub.sec_type.clone(), providers, None);
                }
                self.shared.market.push_md_merge(sub.instrument, into);
                continue;
            }
            if !self.park_for_round_lot(&sub) {
                self.route_md_subscribe(&sub);
            }
        }
    }

    /// A new top-of-book request of an instrument shares the one already
    /// running or waiting (ibx#444): a stream joins a stream, a snapshot
    /// joins either; a stream asked where only a snapshot runs goes out.
    fn joins_top(&self, instrument: InstrumentId, snapshot: bool) -> bool {
        let running = self.farm.top_snapshot(instrument).or_else(|| {
            self.context.lot_parked.iter().chain(&self.context.lot_ready).chain(&self.context.md_resolved)
                .chain(self.context.md_lookups.iter().map(|(_, s, _)| s))
                .find(|s| s.instrument == instrument)
                .map(|s| s.snapshot)
        });
        running.is_some_and(|running_snapshot| !running_snapshot || snapshot)
    }

    /// Send the subscriptions whose round lot came in (ibx#287), and give
    /// the requests that waited for a definition back to the command loop.
    fn send_lot_ready(&mut self) {
        if !self.context.def_ready.is_empty() {
            let ready = std::mem::take(&mut self.context.def_ready);
            self.ccp.resolved_requests.extend(ready);
        }
        if self.context.lot_ready.is_empty() { return; }
        for sub in std::mem::take(&mut self.context.lot_ready) {
            self.route_md_subscribe(&sub);
        }
    }

    /// Reclaim an instrument slot if nothing references it any more
    /// (ibx#233): no open orders, no market data subscription, no
    /// tick-by-tick subscription, no news subscription. A reused id would
    /// repoint those references at the wrong contract, so referenced slots
    /// stay resident until released. As the reference keeps a contract's
    /// market data while any observer still needs it, dropping one consumer
    /// leaves the others' data running (ibx#291).
    fn try_reclaim_instrument(&mut self, instrument: InstrumentId) {
        if !self.context.open_orders_for(instrument).is_empty() {
            return;
        }
        if self.farm.has_md_subscription(instrument)
            || self.context.lot_parked.iter().chain(&self.context.lot_ready).chain(&self.context.md_resolved)
                .chain(self.context.md_lookups.iter().map(|(_, s, _)| s))
                .any(|s| s.instrument == instrument)
        {
            return;
        }
        // A stream with no request left does not hold the slot.
        if self.hmds.tbt_subscriptions.iter().any(|s| s.instrument == instrument && s.is_live()) {
            return;
        }
        if self.context.market.unregister(instrument).is_some() {
            // Zero the shared-side quote so a reused slot cannot serve the
            // previous contract's prices before its first tick.
            self.shared.market.push_quote(instrument, &crate::types::Quote::default());
            log::info!("Reclaimed instrument slot {}", instrument);
        }
    }

    pub fn run_with_panic_recovery(mut self) {
        let event_tx = self.event_tx.clone();
        let shared = self.shared.clone();
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            self.run();
        }));
        if let Err(payload) = result {
            let msg: &str = payload
                .downcast_ref::<String>()
                .map(|s| s.as_str())
                .or_else(|| payload.downcast_ref::<&'static str>().copied())
                .unwrap_or("<non-string panic payload>");
            log::error!("Engine hot loop panicked, emitting Disconnected: {}", msg);
            shared.set_connection_lost();
            emit(&event_tx, Event::Disconnected);
        }
    }

    /// Run the hot loop. Blocks until Shutdown command received.
    pub fn run(&mut self) {
        if let Some(core) = self.core_id {
            Self::pin_to_core(core);
        }

        self.running = true;
        for conn in [&mut self.farm_conn, &mut self.ccp_conn, &mut self.hmds_conn].into_iter().flatten() {
            conn.set_queued_writes(true);
        }
        // The links are known from the start: one lost in the first pass
        // is reported too (ibx#488: the first look took it as the start).
        if self.links.is_none() {
            self.links = Some(self.current_links());
        }

        while self.running {
            self.context.loop_iterations += 1;

            // 1. Busy-poll market data farm socket (non-blocking recv)
            let farm_was_ok = !self.farm.disconnected;
            self.farm.poll_market_data(
                &mut self.farm_conn, &mut self.context, &self.shared,
                &self.event_tx, &mut self.hb,
            );
            let _ = farm_was_ok; // reconnects are scheduled below (ibx#218)

            // 1b. Busy-poll historical socket for tick-by-tick data
            self.hmds.poll(
                &mut self.hmds_conn, &self.shared,
                &self.event_tx, &mut self.hb,
            );

            // 1b''. The server tag cleaner (#292), once a minute, and the
            //       tick-by-tick cancels that are due (ibx#404).
            let now = Instant::now();
            self.clean_server_tags(now);
            self.send_due_tbt_cancels(now);

            // 1b'. Farms opened on demand (#445): read, then connect,
            //      heartbeat and close them by their rules.
            if !self.pool.is_empty() {
                self.poll_pool();
                self.service_pool();
            }

            // 1c. Hand off any scanner results with cache-miss con_ids to CCP for
            //     contract-detail fan-out (ibx#156). Mirrors what the gateway does
            //     internally for binary-API scanner clients — see ib-agent#142.
            for (req_id, result) in self.hmds.cold_scanner_results.drain(..).collect::<Vec<_>>() {
                self.ccp.start_scanner_enrichment(
                    req_id, result, &mut self.ccp_conn, &self.shared, &mut self.hb,
                );
            }

            // 2. Drain pending orders → build → sign → send to auth
            //    Skip if CCP is disconnected — orders stay in buffer for retry after reconnect.
            order_builder::drain_and_send_orders(
                &mut self.ccp_conn, &mut self.context, &self.account_id, &mut self.hb,
                self.ccp.disconnected, &self.shared,
            );

            // 3. Busy-poll auth socket for execution reports
            let ccp_was_ok = !self.ccp.disconnected;
            self.poll_auth();
            let _ = ccp_was_ok; // reconnects are scheduled below (ibx#218)

            // 4. Check control_plane_rx (SPSC) for commands
            self.poll_control_commands();

            // 4b. Write what waits for a slow peer; a failed write drops
            //     that link (ibx#254)
            self.check_writes();

            // 5. Liveness of the links (ibx#419)
            self.check_heartbeats();

            // 5b. Poll pending reconnects and schedule the next attempts
            //     (jittered backoff instead of immediate re-dials, ibx#218)
            self.poll_farm_reconnect();
            self.poll_ccp_reconnect();
            self.poll_hmds_reconnect();
            self.maybe_spawn_farm_reconnect();
            self.maybe_spawn_ccp_reconnect();
            self.maybe_spawn_hmds_reconnect();

            // 5c. Tell the clients about lost and restored links (ibx#399)
            self.report_link_changes();
            self.maybe_report_restored();

            // 6. Wake any waiting consumers (e.g. Python event loop)
            self.shared.notify();

            // 7. With every transport down there is nothing to poll, and the
            //    spin pinned a core for the whole outage (ibx#399). Park 1ms in
            //    that state only; reconnects run on a seconds-scale backoff and
            //    the connected path is unchanged.
            if self.all_transports_down() {
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
        }
    }

    /// True when the farm and auth connections are down and no historical
    /// connection is up.
    fn all_transports_down(&self) -> bool {
        self.farm.disconnected
            && self.ccp.disconnected
            && (self.hmds_conn.is_none() || self.hmds.disconnected)
    }

    fn current_links(&self) -> Links {
        Links {
            ccp: !self.ccp.disconnected && self.ccp_conn.is_some(),
            farm: !self.farm.disconnected && self.farm_conn.is_some(),
            hmds: !self.hmds.disconnected && self.hmds_conn.is_some(),
        }
    }

    fn hmds_farm_name(&self) -> &str {
        self.reconnect_auth.as_ref()
            .map(|a| a.hmds_farm.as_str())
            .filter(|f| !f.is_empty())
            .unwrap_or("ushmds")
    }

    /// A lost link is reported to every client at once, as the reference
    /// does (ibx#399): 1100 for the auth connection, 2103 / 2105 for the
    /// market-data and historical farms. The clients stay connected. A farm
    /// that is up again after its logon gives 2104 / 2106 with its name, as
    /// in the reference; the auth link gives 1102 after its replay instead
    /// (`maybe_report_restored`).
    fn report_link_changes(&mut self) {
        let now = self.current_links();
        let Some(before) = self.links.replace(now) else { return };
        if before == now {
            return;
        }
        if before.ccp && !now.ccp {
            // Open-order requests wait for the order replay of the new
            // logon, as in the reference (ibx#251).
            self.shared.orders.set_open_orders_held(true);
            self.shared.push_connection_notice(1100, LINK_LOST.to_string());
        }
        if before.farm && !now.farm {
            self.shared.push_connection_notice(2103, format!("Market data farm connection is broken:{}", self.farm_name));
        }
        if !before.farm && now.farm {
            self.shared.push_connection_notice(2104, format!("Market data farm connection is OK:{}", self.farm_name));
        }
        if before.hmds && !now.hmds {
            let name = self.hmds_farm_name().to_string();
            self.shared.push_connection_notice(2105, format!("HMDS data farm connection is broken:{}", name));
            self.hmds.scanner_link_lost(&self.shared);
        }
        if !before.hmds && now.hmds {
            self.hmds.scanner_link_restored(&mut self.hmds_conn, &mut self.hb, &self.shared);
        }
        if !before.hmds && now.hmds {
            let name = self.hmds_farm_name().to_string();
            self.shared.push_connection_notice(2106, format!("HMDS data farm connection is OK:{}", name));
        }
    }

    /// 1102 after a reconnect of the auth connection, once its order status
    /// replay has ended: at once when the data farms are up, else when they
    /// are up or after `RESTORE_FARM_WAIT`, with the farms that are not
    /// (ibx#399).
    ///
    /// The reference gives 1101 (data lost) instead only when the contract
    /// requests it makes for its own saved settings were not all answered
    /// at the drop. This client makes no such requests, so after a relogin
    /// the answer is always 1102 (ibx#251).
    fn maybe_report_restored(&mut self) {
        let Some(end_at) = self.ccp.status_replay_end_at else { return };
        let links = self.current_links();
        let hmds_expected = links.hmds
            || self.reconnect_auth.as_ref().is_some_and(|a| !a.hmds_host.is_empty());
        let all_up = links.farm && (links.hmds || !hmds_expected);
        if !all_up && end_at.elapsed() < RESTORE_FARM_WAIT {
            return;
        }
        self.ccp.status_replay_end_at = None;
        let mut farms = vec![(self.farm_name.clone(), links.farm)];
        if hmds_expected {
            farms.push((self.hmds_farm_name().to_string(), links.hmds));
        }
        let names = |up: bool| farms.iter().filter(|f| f.1 == up).map(|f| f.0.as_str()).collect::<Vec<_>>().join("; ");
        let message = if all_up {
            format!("{} All data farms are connected: {}.", LINK_RESTORED, names(true))
        } else {
            format!("{} The following farms are connected: {}. The following farms are not connected: {}.",
                LINK_RESTORED, names(true), names(false))
        };
        log::info!("Link restored: {}", message);
        self.shared.push_connection_notice(1102, message);
    }

    fn emit_hmds_unavailable(&self, req_id: ReqId, from_historical: bool) {
        push_hmds_unavailable(&self.shared, req_id, from_historical);
    }

    fn poll_control_commands(&mut self) {
        let rx = match self.control_rx.as_ref() {
            Some(rx) => rx,
            None => return,
        };

        self.cmd_buf.clear();
        self.cmd_buf.extend(rx.try_iter());

        // try_iter() stops on both Empty and Disconnected — do one extra
        // try_recv() to distinguish.  If a straggler command arrived between
        // try_iter() finishing and this call, push it into the batch.
        let sender_dropped = match rx.try_recv() {
            Ok(cmd)  => { self.cmd_buf.push(cmd); false }
            Err(crossbeam_channel::TryRecvError::Empty)        => false,
            Err(crossbeam_channel::TryRecvError::Disconnected) => true,
        };

        // Drain the buffer so we can mutably borrow self in the loop body.
        // Requests whose contract was looked up (ibx#427) go first.
        let cmds: Vec<ControlCommand> = self.ccp.resolved_requests.drain(..)
            .chain(self.cmd_buf.drain(..))
            .collect();
        for cmd in cmds {
            match cmd {
                ControlCommand::Subscribe { con_id, symbol, exchange, sec_type, last_trade_date, strike, right, multiplier, mode_9887, snapshot, reply_tx } => {
                    // No conId: resolved first, as the reference (ibx#278).
                    let key = (con_id != 0).then_some(con_id);
                    if let Some(id) = self.register_slot_or_reject(key, symbol.clone(), &sec_type, &exchange, &reply_tx) {
                        let filters = crate::types::SecDefFilters {
                            last_trade_date_or_contract_month: last_trade_date.clone(), strike,
                            right: right.clone(), multiplier: multiplier.clone(), ..Default::default()
                        };
                        let sub = farm::MdSubscribe {
                            con_id, symbol, exchange, sec_type, last_trade_date, strike, right, multiplier,
                            instrument: id, mode_9887, snapshot,
                        };
                        if con_id == 0 {
                            self.lookup_md_contract(sub, String::new(), filters);
                        } else if mode_9887 == 0 && self.joins_top(id, snapshot) {
                            // Another request of the contract has its top of
                            // book running: this one shares it, nothing is
                            // sent (ibx#444).
                            log::info!("Market data for con_id {} joins the subscription of instrument {}", con_id, id);
                        } else if !self.park_for_round_lot(&sub) {
                            self.route_md_subscribe(&sub);
                        }
                    }
                }
                ControlCommand::SubscribeSnapshot { con_id, symbol, exchange, sec_type, reply_tx } => {
                    if let Some(id) = self.register_slot_or_reject(Some(con_id), symbol.clone(), &sec_type, &exchange, &reply_tx) {
                        let sub = farm::MdSubscribe {
                            con_id, symbol, exchange, sec_type, last_trade_date: String::new(), strike: 0.0,
                            right: String::new(), multiplier: String::new(), instrument: id, mode_9887: 0, snapshot: false,
                        };
                        if let Some(farm_id) = self.md_target(&sub) {
                            if let Some(f) = self.pool.get_mut(farm_id) {
                                f.note_request(Instant::now());
                            }
                            if let Some(sink) = farm_sink!(self, farm_id) {
                                self.farm.subscribe_snapshot(&sub, farm_id, sink, &mut self.hb);
                            }
                        }
                    }
                }
                ControlCommand::DropSnapshot { instrument } => {
                    self.farm.drop_snapshot(instrument);
                    self.try_reclaim_instrument(instrument);
                }
                ControlCommand::SubscribeBySymbol { symbol, sec_type, exchange, currency, filters, mode_9887, snapshot, reply_tx } => {
                    if let Some(id) = self.register_slot_or_reject(None, symbol.clone(), &sec_type, &exchange, &reply_tx) {
                        let sub = farm::MdSubscribe {
                            con_id: 0, symbol, exchange, sec_type,
                            last_trade_date: filters.last_trade_date_or_contract_month.clone(),
                            strike: filters.strike, right: filters.right.clone(), multiplier: filters.multiplier.clone(),
                            instrument: id, mode_9887, snapshot,
                        };
                        self.lookup_md_contract(sub, currency, filters);
                    }
                }
                ControlCommand::SetMarketDataType { market_data_type } => {
                    self.farm.md_modes.apply(market_data_type);
                }
                ControlCommand::Unsubscribe { instrument } => {
                    // Not sent yet: nothing to cancel on the farm.
                    self.context.lot_parked.retain(|s| s.instrument != instrument);
                    self.context.lot_ready.retain(|s| s.instrument != instrument);
                    self.context.md_lookups.retain(|(_, s, _)| s.instrument != instrument);
                    self.context.md_resolved.retain(|s| s.instrument != instrument);
                    self.route_md_cancel(instrument);
                    self.try_reclaim_instrument(instrument);
                }
                ControlCommand::SubscribeTbt { req_id, con_id, symbol, exchange, sec_type, tbt_type, number_of_ticks, ignore_size, reply_tx } => {
                    if let Some(id) = self.register_or_reject(con_id, symbol.clone(), &sec_type, &exchange, &reply_tx) {
                        // The farm of a SMART contract's route needs its
                        // aggregate group: the request waits for the
                        // definition when it is not known (ibx#404).
                        let again = ControlCommand::SubscribeTbt {
                            req_id, con_id, symbol: symbol.clone(), exchange: exchange.clone(), sec_type: sec_type.clone(),
                            tbt_type, number_of_ticks, ignore_size, reply_tx: None,
                        };
                        if self.park_for_hmds_definition(con_id, &exchange, &sec_type, again) {
                            continue;
                        }
                        self.start_tbt(req_id, con_id, id, &symbol, &exchange, &sec_type, tbt_type, number_of_ticks, ignore_size);
                    }
                }
                ControlCommand::UnsubscribeTbt { req_id } => {
                    // A request waiting for its contract's definition is
                    // never sent.
                    self.context.def_parked.retain(|(_, c)| !matches!(c, ControlCommand::SubscribeTbt { req_id: r, .. } if *r == req_id));
                    if let Some(instrument) = self.hmds.send_tbt_unsubscribe(req_id, Instant::now()) {
                        self.try_reclaim_instrument(instrument);
                    }
                }
                ControlCommand::SubscribeNews { instrument, con_id, exchange, sec_type, providers, refusal } => {
                    self.subscribe_news(instrument, con_id, exchange, sec_type, providers, refusal);
                }
                ControlCommand::UnsubscribeNews { instrument, providers } => {
                    let msgs = self.farm.release_news(instrument, &providers);
                    self.send_farm_messages(msgs);
                }
                ControlCommand::UpdateParam { key, value } => {
                    let _ = (key, value);
                }
                ControlCommand::Ping => {
                    // On-demand RTT sample (ibx#158). Reuses the liveness
                    // test-request machinery; a pending liveness test is
                    // already a measurement in flight, so don't stomp it.
                    if self.hb.pending_ccp_test.is_none() {
                        if let Some(conn) = self.ccp_conn.as_mut() {
                            let ts = chrono_free_timestamp();
                            let test_id = self.hb.next_test_id();
                            let _ = conn.send_fix(&[
                                (fix::TAG_MSG_TYPE, fix::MSG_TEST_REQUEST),
                                (fix::TAG_SENDING_TIME, &ts),
                                (fix::TAG_TEST_REQ_ID, &test_id),
                            ]);
                            self.hb.pending_ccp_test = Some((test_id, Instant::now()));
                            self.hb.last_ccp_sent = Instant::now();
                        }
                    }
                }
                ControlCommand::Order(req) => {
                    self.context.pending_orders.push(req);
                }
                ControlCommand::RegisterInstrument { con_id, symbol, sec_type, exchange, reply_tx } => {
                    self.register_or_reject(con_id, symbol, &sec_type, &exchange, &reply_tx);
                }
                ControlCommand::ResolveContract { req_id, lookup, request } => {
                    // Look the contract up first (ibx#427); the request comes
                    // back through `resolved_requests` with its conId.
                    self.ccp.start_contract_resolve(req_id, lookup, *request, &mut self.ccp_conn, &mut self.hb);
                }
                ControlCommand::FetchHistorical { req_id, con_id, symbol, sec_type, exchange, end_date_time, duration, bar_size, what_to_show, use_rth, keep_up_to_date, include_expired, format_date } => {
                    // A SMART route needs the contract's aggregate group
                    // (#445): the request waits for the definition. A
                    // keepUpToDate request goes the same way, to the farm of
                    // its route, as the reference (ibx#429).
                    let again = ControlCommand::FetchHistorical {
                        req_id, con_id, symbol: symbol.clone(), sec_type: sec_type.clone(), exchange: exchange.clone(),
                        end_date_time: end_date_time.clone(), duration: duration.clone(), bar_size: bar_size.clone(),
                        what_to_show: what_to_show.clone(), use_rth, keep_up_to_date, include_expired, format_date,
                    };
                    if self.park_for_hmds_definition(con_id, &exchange, &sec_type, again) {
                        continue;
                    }
                    let st = if sec_type.is_empty() { "STK" } else { sec_type.as_str() };
                    let route_exchange = farm::routing_exchange(&exchange, st).to_string();
                    let agg_group = self.agg_group_for(con_id, &route_exchange);
                    let data_type = bar_data_kind(&bar_size);
                    match self.hmds_target(&route_exchange, agg_group, st, data_type) {
                        None => self.shared.reference.push_historical_error(req_id, 162, format!(
                            "Historical Market Data Service error message:No data of type {} is available for the exchange '{}' and the security type '{}'",
                            data_type.name(), route_exchange, st)),
                        Some(PRIMARY_HMDS) if self.hmds_conn.is_none() => self.emit_hmds_unavailable(req_id, true),
                        Some(farm_id) => {
                            let zone = self.shared.reference.time_zone_id(con_id);
                            let chart_symbol = self.chart_symbol(con_id, &symbol);
                            if let Some(sink) = farm_sink!(self, farm_id) {
                                self.hmds.send_historical_request_ex(req_id, con_id, &sec_type, &exchange, &end_date_time, &duration, &bar_size, &what_to_show, use_rth, keep_up_to_date, &chart_symbol, sink, &mut self.hb, &self.shared, include_expired, format_date, zone, farm_id);
                            }
                            if farm_id != PRIMARY_HMDS {
                                let sent: Vec<String> = self.hmds.pending_historical.iter()
                                    .filter(|(_, r)| *r == req_id).map(|(q, _)| q.clone()).collect();
                                for q in sent {
                                    self.hmds.query_farms.insert(q, farm_id);
                                }
                            }
                        }
                    }
                }
                ControlCommand::CancelHistorical { req_id } => {
                    // 162 or 366 to the client, then the cancels (ibx#431, ibx#429).
                    for (farm_id, xml) in self.hmds.cancel_bar_request(req_id, &self.shared) {
                        if let Some(sink) = farm_sink!(self, farm_id) {
                            hmds::HmdsState::send_cancel_xml(&xml, sink, &mut self.hb);
                        }
                    }
                }
                ControlCommand::FetchHeadTimestamp { req_id, con_id, sec_type, exchange, what_to_show, use_rth, format_date } => {
                    if self.hmds_conn.is_none() {
                        self.emit_hmds_unavailable(req_id, false);
                    } else {
                        self.hmds.send_head_timestamp_request(req_id, con_id, &sec_type, &exchange, &what_to_show, use_rth, format_date, &mut self.hmds_conn, &mut self.hb, &self.shared);
                    }
                }
                ControlCommand::FetchContractDetails { req_id, con_id, symbol, sec_type, exchange, currency, filters } => {
                    if con_id > 0 {
                        self.ccp.send_contract_details_by_con_id(req_id, con_id, &exchange, &mut self.ccp_conn, &mut self.hb);
                    } else {
                        self.ccp.send_secdef_request_by_symbol(req_id, &symbol, &sec_type, &exchange, &currency, &filters, &mut self.ccp_conn, &mut self.hb);
                    }
                }
                ControlCommand::CancelHeadTimestamp { req_id } => {
                    if let Some(pos) = self.hmds.pending_head_ts.iter().position(|(_, rid, _)| *rid == req_id) {
                        self.hmds.pending_head_ts.remove(pos);
                    }
                }
                ControlCommand::FetchMatchingSymbols { req_id, pattern } => {
                    self.ccp.send_matching_symbols_request(req_id, &pattern, &mut self.ccp_conn, &mut self.hb, &self.shared);
                }
                ControlCommand::FetchSecDefOptParams { req_id, underlying_symbol, fut_fop_exchange, underlying_sec_type, underlying_con_id } => {
                    let connected = !self.ccp.disconnected;
                    self.ccp.optparams.request(req_id, &underlying_symbol, &fut_fop_exchange, &underlying_sec_type,
                        underlying_con_id, &mut self.ccp_conn, connected, &mut self.hb, &self.shared);
                }
                ControlCommand::FetchMktDepthExchanges => {
                    // Answered locally from the depth routes of the market
                    // data routing table, as the reference (#453).
                    let rows = self.farm.routing.as_ref().map(|t| t.depth_exchanges()).unwrap_or_default();
                    self.shared.reference.set_depth_exchanges(rows.into_iter().map(|r| crate::types::DepthMktDataDescription {
                        exchange: r.exchange, sec_type: r.sec_type, listing_exch: r.listing_exch,
                        service_data_type: r.service_data_type, agg_group: r.agg_group,
                    }).collect());
                    self.shared.reference.notify_depth_exchanges();
                }
                ControlCommand::FetchScannerParams => {
                    self.hmds.req_scanner_params(&mut self.hmds_conn, &mut self.hb, &self.shared);
                }
                ControlCommand::SubscribeScanner { req_id, client_id, subscription } => {
                    if let Some(text) = scanner_refusal(&self.hmds, req_id) {
                        self.shared.reference.push_historical_error(req_id, 322, text);
                    } else if self.hmds_conn.is_none() {
                        self.emit_hmds_unavailable(req_id, false);
                    } else {
                        self.hmds.send_scanner_subscribe(req_id, client_id, subscription, &mut self.hmds_conn, &mut self.hb);
                    }
                }
                ControlCommand::CancelScanner { req_id } => {
                    if self.hmds.cancel_scanner(req_id, &mut self.hmds_conn, &mut self.hb, &self.shared) {
                        self.ccp.pending_scanner_enrichment.retain(|p| p.api_req_id != req_id);
                    }
                }
                ControlCommand::FetchHistoricalNews { req_id, con_id, provider_codes, start_time, end_time, max_results } => {
                    if self.hmds_conn.is_none() {
                        self.emit_hmds_unavailable(req_id, false);
                    } else {
                        self.hmds.send_historical_news_request(req_id, con_id, &provider_codes, &start_time, &end_time, max_results, &mut self.hmds_conn, &mut self.hb, &self.shared);
                    }
                }
                ControlCommand::FetchNewsArticle { req_id, provider_code, article_id } => {
                    if self.hmds_conn.is_none() {
                        self.emit_hmds_unavailable(req_id, false);
                    } else {
                        self.hmds.send_news_article_request(req_id, &provider_code, &article_id, &mut self.hmds_conn, &mut self.hb, &self.shared);
                    }
                }
                ControlCommand::FetchFundamentalData { req_id, con_id, report_type } => {
                    // The farm of the fundamentals route, opened on demand
                    // (#434); with no route, 430 as the reference.
                    match self.hmds_target("RTRSFND", -1, "STK", crate::engine::routing::DataType::DayChart) {
                        None => self.shared.reference.push_historical_error(req_id, 430,
                            format!("{}No Route Found", crate::control::fundamental::FUNDAMENTALS_NOT_AVAILABLE)),
                        Some(PRIMARY_HMDS) if self.hmds_conn.is_none() => self.emit_hmds_unavailable(req_id, false),
                        Some(id) => {
                            if let Some(sink) = farm_sink!(self, id) {
                                self.hmds.send_fundamental_data_request(req_id, con_id, &report_type, id, sink, &mut self.hb, &self.shared);
                            }
                        }
                    }
                }
                ControlCommand::CalcOption { req_id, con_id, kind, under_price } => {
                    self.ccp.optcalc.start(req_id, con_id, kind, under_price, &mut self.ccp_conn, &mut self.hb, &self.shared);
                }
                ControlCommand::CancelFundamentalData { req_id } => {
                    if let Some((id, xml)) = self.hmds.cancel_fundamental(req_id) {
                        let ts = chrono_free_timestamp();
                        if let Some(sink) = farm_sink!(self, id) {
                            sink.send_plain(&[(fix::TAG_MSG_TYPE, "U"), (fix::TAG_SENDING_TIME, &ts), (6040, "10011"), (6118, &xml)]);
                        }
                    }
                }
                ControlCommand::FetchHistogramData { req_id, con_id, sec_type, exchange, use_rth, period } => {
                    if self.hmds_conn.is_none() {
                        self.emit_hmds_unavailable(req_id, false);
                    } else {
                        self.hmds.send_histogram_request(req_id, con_id, &sec_type, &exchange, use_rth, &period, &mut self.hmds_conn, &mut self.hb, &self.shared);
                    }
                }
                ControlCommand::CancelHistogramData { req_id } => {
                    if let Some(pos) = self.hmds.pending_histogram.iter().position(|h| h.req_id == req_id) {
                        self.hmds.pending_histogram.remove(pos);
                    }
                }
                ControlCommand::FetchHistoricalTicks { req_id, con_id, symbol, sec_type, exchange, start_date_time, end_date_time, number_of_ticks, what_to_show, use_rth, ignore_size } => {
                    // To the farm of the route, as bars (ibx#432).
                    let again = ControlCommand::FetchHistoricalTicks {
                        req_id, con_id, symbol: symbol.clone(), sec_type: sec_type.clone(), exchange: exchange.clone(),
                        start_date_time: start_date_time.clone(), end_date_time: end_date_time.clone(), number_of_ticks,
                        what_to_show: what_to_show.clone(), use_rth, ignore_size,
                    };
                    if self.park_for_hmds_definition(con_id, &exchange, &sec_type, again) {
                        continue;
                    }
                    let st = if sec_type.is_empty() { "STK" } else { sec_type.as_str() };
                    let route_exchange = farm::routing_exchange(&exchange, st).to_string();
                    let agg_group = self.agg_group_for(con_id, &route_exchange);
                    let data_type = crate::engine::routing::DataType::DayChart;
                    match self.hmds_target(&route_exchange, agg_group, st, data_type) {
                        None => self.shared.reference.push_historical_error(req_id, 10187, format!(
                            "Failed to request historical ticks:No data of type {} is available for the exchange '{}' and the security type '{}'",
                            data_type.name(), route_exchange, st)),
                        Some(PRIMARY_HMDS) if self.hmds_conn.is_none() => self.emit_hmds_unavailable(req_id, false),
                        Some(farm_id) => {
                            let zone = self.shared.reference.time_zone_id(con_id);
                            let chart_symbol = self.chart_symbol(con_id, &symbol);
                            if let Some(sink) = farm_sink!(self, farm_id) {
                                self.hmds.send_historical_ticks_request(req_id, con_id, &chart_symbol, &sec_type, &exchange,
                                    &start_date_time, &end_date_time, number_of_ticks, &what_to_show, use_rth, ignore_size,
                                    zone.as_deref(), farm_id, sink, &mut self.hb, &self.shared);
                            }
                        }
                    }
                }
                ControlCommand::SubscribeRealTimeBar { req_id, con_id, symbol, sec_type, exchange, what_to_show, use_rth } => {
                    if self.hmds_conn.is_none() {
                        self.emit_hmds_unavailable(req_id, false);
                    } else {
                        self.hmds.send_realtime_bar_subscribe(req_id, con_id, &sec_type, &exchange, &symbol, &what_to_show, use_rth, &mut self.hmds_conn, &mut self.hb, &self.shared);
                    }
                }
                ControlCommand::CancelRealTimeBar { req_id } => {
                    // The router's cancel once no request is left on it
                    // (ibx#454).
                    if let Some(tid) = self.hmds.end_rtbar(req_id) {
                        self.hmds.send_historical_cancel(&tid.to_string(), &mut self.hmds_conn, &mut self.hb);
                    }
                }
                ControlCommand::FetchHistoricalSchedule { req_id, con_id, sec_type, exchange, end_date_time, duration, use_rth } => {
                    if self.hmds_conn.is_none() {
                        self.emit_hmds_unavailable(req_id, false);
                    } else {
                        self.hmds.send_schedule_request(req_id, con_id, &sec_type, &exchange, &end_date_time, &duration, use_rth, &mut self.hmds_conn, &mut self.hb);
                    }
                }
                ControlCommand::SubscribeDepth { req_id, con_id, exchange, sec_type, num_rows, is_smart_depth } => {
                    self.route_depth_subscribe(req_id, con_id, exchange, sec_type, num_rows, is_smart_depth);
                }
                ControlCommand::UnsubscribeDepth { req_id } => {
                    // A SmartDepth request still waiting for its
                    // components' definitions has nothing on the wire.
                    if let Some(pos) = self.context.depth_gathers.iter().position(|g| g.req_id == req_id) {
                        self.context.depth_gathers.remove(pos);
                        self.shared.market.purge_depth_updates(req_id);
                        continue;
                    }
                    match self.farm.stop_depth(req_id) {
                        // An unknown request id: 310, as the reference.
                        None => self.shared.orders.push_order_error(i64::from(req_id), 310,
                            format!("Can't find the subscribed market depth with tickerId:{}", req_id)),
                        Some(msgs) => self.send_farm_messages(msgs),
                    }
                    // Purge any already-buffered depth updates so callers never see stale data
                    self.shared.market.purge_depth_updates(req_id);
                }
                ControlCommand::SubscribePnl { req_id, account } => {
                    self.ccp.send_pnl_subscribe(req_id, &account, &mut self.ccp_conn, &mut self.hb);
                }
                ControlCommand::SetInstrumentCurrency { con_id, currency } => {
                    if let Some(id) = self.context.market.instrument_by_con_id(con_id) {
                        self.context.market.set_currency(id, &currency);
                    }
                }
                ControlCommand::SubscribeAccountSummary { sr_id, tags, group } => {
                    self.ccp.send_account_summary(&sr_id, Some((&tags, &group)), &mut self.ccp_conn, &mut self.hb);
                }
                ControlCommand::CancelAccountSummary { sr_id } => {
                    self.ccp.send_account_summary(&sr_id, None, &mut self.ccp_conn, &mut self.hb);
                }
                ControlCommand::CancelPnl { req_id } => {
                    let _ = req_id; // Server auto-cancels on disconnect; no explicit cancel message needed
                }
                ControlCommand::FetchNewsProviders { .. }
                | ControlCommand::FetchSmartComponents { .. }
                | ControlCommand::FetchSoftDollarTiers { .. }
                | ControlCommand::FetchUserInfo { .. } => {
                    // Gateway-local data — handled synchronously in Python EClient.
                    // These variants exist for future CCP round-trip support.
                }
                ControlCommand::Shutdown => {
                    // Unsubscribe all active market data before stopping
                    let instruments: Vec<InstrumentId> = self.farm.instrument_md_reqs
                        .iter().map(|(id, _)| *id).collect();
                    for instrument in instruments {
                        self.route_md_cancel(instrument);
                    }
                    // Cancel all tick-by-tick streams at once before stopping
                    let now = Instant::now();
                    for sub in self.hmds.tbt_subscriptions.iter_mut() {
                        sub.cancel_at = Some(now);
                    }
                    self.send_due_tbt_cancels(now);
                    // Cancel the news entries left (NEWS contracts) before stopping
                    let msgs = self.farm.stop_all_news();
                    self.send_farm_messages(msgs);
                    self.running = false;
                    self.shared.set_connection_lost();
                    emit(&self.event_tx, Event::Disconnected);
                }
            }
        }

        // All senders dropped — treat as implicit shutdown.
        if sender_dropped && self.running {
            log::warn!("Control channel disconnected — shutting down hot loop");
            self.running = false;
            self.shared.set_connection_lost();
            emit(&self.event_tx, Event::Disconnected);
        }
    }

    /// Each link writes on its own (ibx#254): output a slow peer has not
    /// taken waits on its connection and goes out here, so it stalls only
    /// that link. A write error drops the link and the reconnect follows,
    /// as in the reference; the frame is never sent again.
    #[inline]
    fn check_writes(&mut self) {
        if !self.ccp.disconnected {
            if let Some(conn) = self.ccp_conn.as_mut() {
                if conn.has_queued_output() {
                    let _ = conn.flush_queued();
                }
                if let Some(error) = conn.write_error() {
                    log::error!("Auth connection: send: {}", error);
                    self.drop_ccp_link();
                }
            }
        }
        if !self.farm.disconnected {
            if let Some(conn) = self.farm_conn.as_mut() {
                if conn.has_queued_output() {
                    let _ = conn.flush_queued();
                }
                if let Some(error) = conn.write_error() {
                    log::error!("Market data farm: send failed: {}", error);
                    conn.shutdown();
                    self.farm.handle_disconnect(&mut self.context, &self.event_tx);
                }
            }
        }
        if !self.hmds.disconnected {
            if let Some(conn) = self.hmds_conn.as_mut() {
                if conn.has_queued_output() {
                    let _ = conn.flush_queued();
                }
                if let Some(error) = conn.write_error() {
                    log::error!("Historical farm: send failed: {}", error);
                    conn.shutdown();
                    self.hmds.disconnected = true;
                    // With no socket held the reconnect loop re-dials it.
                    self.hmds_conn = None;
                }
            }
        }
    }

    fn check_heartbeats(&mut self) {
        self.check_heartbeats_at(Instant::now());
    }

    /// Liveness of the three links at `now`, as the reference checks it
    /// (ibx#419, see `liveness`). A dead link is closed and reported to the
    /// clients by `report_link_changes` (1100, 2103, 2105).
    pub(crate) fn check_heartbeats_at(&mut self, now: Instant) {
        // --- Auth link monitor ---
        if !self.ccp.disconnected && self.ccp_conn.is_some() {
            let check = self.hb.poll_ccp(now);
            if check.dead {
                log::error!("Auth connection: nothing received for {:?}, connection reset",
                    now.saturating_duration_since(self.hb.last_ccp_recv));
                self.drop_ccp_link();
            } else if check.heartbeat || check.test_request {
                let ts = chrono_free_timestamp();
                let conn = self.ccp_conn.as_mut().expect("checked above");
                if check.heartbeat {
                    let _ = conn.send_fix(&[
                        (fix::TAG_MSG_TYPE, fix::MSG_HEARTBEAT),
                        (fix::TAG_SENDING_TIME, &ts),
                    ]);
                    self.hb.last_ccp_sent = now;
                }
                if check.test_request {
                    let test_id = self.hb.next_test_id();
                    log::warn!("Auth connection: nothing received for {:?}, test request {}",
                        now.saturating_duration_since(self.hb.last_ccp_recv), test_id);
                    let _ = conn.send_fix(&[
                        (fix::TAG_MSG_TYPE, fix::MSG_TEST_REQUEST),
                        (fix::TAG_SENDING_TIME, &ts),
                        (fix::TAG_TEST_REQ_ID, &test_id),
                    ]);
                    self.hb.pending_ccp_test = Some((test_id, now));
                    self.hb.last_ccp_sent = now;
                }
            }
        }

        // --- Market data farm: periodic test request ---
        if !self.farm.disconnected && self.farm_conn.is_some() {
            match self.hb.poll_farm(now) {
                FarmCheck::Nothing => {}
                FarmCheck::Ping => {
                    let test_id = self.hb.next_test_id();
                    if let Some(conn) = self.farm_conn.as_mut() {
                        let _ = send_farm_ping(conn, &test_id);
                    }
                    self.hb.pending_farm_test = Some((test_id, now));
                    self.hb.last_farm_sent = now;
                }
                FarmCheck::Dead => {
                    log::error!("Market data farm: no answer to the test request, connection reset");
                    if let Some(conn) = self.farm_conn.as_mut() {
                        conn.shutdown();
                    }
                    self.farm.handle_disconnect(&mut self.context, &self.event_tx);
                }
            }
        }

        // --- Historical farm: periodic test request ---
        if !self.hmds.disconnected && self.hmds_conn.is_some() {
            match self.hb.poll_hmds(now) {
                FarmCheck::Nothing => {}
                FarmCheck::Ping => {
                    let test_id = self.hb.next_test_id();
                    if let Some(conn) = self.hmds_conn.as_mut() {
                        let _ = send_farm_ping(conn, &test_id);
                    }
                    self.hb.pending_hmds_test = Some((test_id, now));
                    self.hb.last_hmds_sent = now;
                }
                FarmCheck::Dead => {
                    log::error!("Historical farm: no answer to the test request, connection reset");
                    if let Some(conn) = self.hmds_conn.as_mut() {
                        conn.shutdown();
                    }
                    self.hmds.disconnected = true;
                    // Drop the dead socket so the HMDS reconnect loop, which
                    // only runs with no connection held, re-dials it (ibx#399).
                    self.hmds_conn = None;
                }
            }
        }
    }

    /// Close the auth link and mark it lost; the reconnect and the 1100
    /// report follow.
    fn drop_ccp_link(&mut self) {
        if let Some(conn) = self.ccp_conn.as_mut() {
            conn.shutdown();
        }
        self.ccp.handle_disconnect(&mut self.context, &self.event_tx);
    }

    fn pin_to_core(core: usize) {
        let core_ids = core_affinity::get_core_ids().unwrap_or_default();
        if let Some(id) = core_ids.get(core) {
            core_affinity::set_for_current(*id);
        }
    }

    /// Whether the farm connection has been lost.
    pub fn is_farm_disconnected(&self) -> bool {
        self.farm.disconnected
    }

    /// Whether the auth connection has been lost.
    pub fn is_ccp_disconnected(&self) -> bool {
        self.ccp.disconnected
    }

    /// Replace the farm connection (after reconnection) and re-subscribe to all instruments.
    pub fn reconnect_farm(&mut self, mut conn: Connection) {
        conn.set_queued_writes(true);
        // Entries made while the farm was down never reached it.
        self.farm.farm_lost(PRIMARY_MD, &mut self.context);
        self.farm.reconnect(
            conn,
            &mut self.farm_conn,
            &mut self.context, &mut self.hb,
        );
        self.resend_unsent_subscriptions();
        let mut msgs = self.farm.resend_depth(PRIMARY_MD);
        msgs.extend(self.farm.resend_news(PRIMARY_MD));
        self.send_farm_messages(msgs);
    }

    /// Read the farms opened on demand and hand their messages to the
    /// market data or historical handlers (#445).
    fn poll_pool(&mut self) {
        let now = Instant::now();
        for i in 0..self.pool.farms.len() {
            let (id, kind) = (self.pool.farms[i].id, self.pool.farms[i].kind);
            let Some(conn) = self.pool.farms[i].conn.as_mut() else { continue };
            let (msgs, bad_signature) = match pool::read_messages(conn) {
                Ok(read) => read,
                Err(e) => {
                    log::error!("{} connection lost: {}", self.pool.farms[i].name, e);
                    self.pool_farm_lost(id);
                    continue;
                }
            };
            for msg in &msgs {
                let heartbeat = matches!(fast_extract_msg_type(msg), Some(b"0") | Some(b"1"));
                self.pool.note_received(id, !heartbeat, now);
                match kind {
                    FarmKind::MarketData => {
                        self.farm.rx_farm = id;
                        self.farm.process_farm_message(msg, &mut self.pool.farms[i].conn, &mut self.context,
                            &self.shared, &self.event_tx, &mut self.hb);
                        self.farm.rx_farm = PRIMARY_MD;
                    }
                    FarmKind::Historical => {
                        self.hmds.rx_farm = id;
                        self.hmds.process_hmds_message(msg, &mut self.pool.farms[i].conn, &self.shared,
                            &self.event_tx, &mut self.hb);
                        self.hmds.rx_farm = PRIMARY_HMDS;
                    }
                }
            }
            if bad_signature {
                log::error!("{} frame signature mismatch: connection dropped", self.pool.farms[i].name);
                self.pool_farm_lost(id);
            }
        }
    }

    /// A farm opened on demand was lost: its market data entries go, the
    /// clients hear of it, and it is opened again when requests still use
    /// it (#445).
    fn pool_farm_lost(&mut self, id: pool::FarmId) {
        let Some(kind) = self.pool.get(id).map(|f| f.kind) else { return };
        let wanted = kind == FarmKind::MarketData && (self.farm.uses_farm(id) || self.farm.depth_uses_farm(id));
        if kind == FarmKind::MarketData {
            self.farm.farm_lost(id, &mut self.context);
            self.farm.depth_farm_lost(id);
            self.farm.news_farm_lost(id);
        }
        let event = self.pool.on_lost(id, wanted, Instant::now());
        self.pool_notice(&event);
    }

    /// The client notice of an event of a farm opened on demand.
    fn pool_notice(&self, event: &pool::FarmEvent) {
        let id = match event {
            pool::FarmEvent::Connecting(id) | pool::FarmEvent::Connected(id)
            | pool::FarmEvent::Lost(id) | pool::FarmEvent::IdleClosed(id) => *id,
        };
        let Some(farm) = self.pool.get(id) else { return };
        let (code, text) = pool::farm_notice(farm.kind, event, &farm.name);
        log::info!("Farm notice {}: {}", code, text);
        self.shared.push_connection_notice(code, text);
    }

    /// Connect, heartbeat and close the farms opened on demand (#445).
    fn service_pool(&mut self) {
        let now = Instant::now();
        for id in self.pool.due_connects(now) {
            self.spawn_pool_connect(id, now);
        }
        let (done, events) = self.pool.poll_connects(now);
        for event in &events {
            // The reference reports a connecting market data farm.
            if self.pool.get(event_farm(event)).is_some_and(|f| f.kind == FarmKind::MarketData) {
                self.pool_notice(event);
            }
        }
        for (id, result) in done {
            match result {
                Ok(conn) => {
                    let event = self.pool.on_connected(id, conn, now);
                    self.pool_notice(&event);
                    if self.pool.get(id).is_some_and(|f| f.kind == FarmKind::MarketData) {
                        self.resend_unsent_subscriptions();
                        let mut msgs = self.farm.resend_depth(id);
                        msgs.extend(self.farm.resend_news(id));
                        self.send_farm_messages(msgs);
                    }
                }
                Err(e) => {
                    log::warn!("Farm {} connect failed: {}", self.pool.get(id).map(|f| f.name.as_str()).unwrap_or("?"), e);
                    self.pool.on_connect_failed(id, now);
                }
            }
        }
        let hb = &mut self.hb;
        let mut dead = self.pool.liveness(now, &mut || hb.next_test_id());
        dead.extend(self.pool.flush_writes());
        for id in dead {
            self.pool_farm_lost(id);
        }
        let others_up = usize::from(!self.farm.disconnected && self.farm_conn.is_some());
        let farm = &self.farm;
        for id in self.pool.md_activity_check(now, others_up, |id| farm.uses_farm(id) || farm.depth_uses_farm(id)) {
            let event = self.pool.close_idle(id);
            self.pool_notice(&event);
        }
        let listener = self.hmds.has_live_listener();
        for id in self.pool.hmds_dormant_check(now, listener) {
            let event = self.pool.close_idle(id);
            self.pool_notice(&event);
        }
    }

    /// Open a farm on demand in the background, with the session's
    /// credentials; no routing table is asked there, as in the reference.
    fn spawn_pool_connect(&mut self, id: pool::FarmId, now: Instant) {
        let Some(farm) = self.pool.get(id) else { return };
        let auth = match self.reconnect_auth.clone() {
            Some(a) if !a.username.is_empty() => a,
            _ => {
                log::warn!("Farm {} cannot be opened: no session credentials", farm.name);
                self.pool.on_connect_failed(id, now);
                return;
            }
        };
        let (host, name) = (farm.host.clone(), farm.name.clone());
        let slot = if farm.kind == FarmKind::MarketData { 18 } else { 17 };
        log::info!("Opening farm {} ({}) on demand", name, host);
        let (tx, rx) = crossbeam_channel::bounded(1);
        let spawned = std::thread::Builder::new()
            .name(format!("{}-connect", name))
            .spawn(move || {
                let link = auth.farm_link(&name, crate::gateway::FarmService::of_slot(slot));
                let result = crate::gateway::connect_farm_opts(
                    &host, &name, &auth.username, &auth.password, auth.paper,
                    &auth.server_session_id, &auth.session_key, &auth.hw_info, &auth.encoded, slot, false,
                    link,
                ).map(|(conn, _)| conn);
                let _ = tx.send(result);
            });
        if spawned.is_ok() {
            self.pool.set_connecting(id, rx, now);
        } else {
            self.pool.on_connect_failed(id, now);
        }
    }

    /// Replace the auth connection (after reconnection) and reconcile order state.
    pub fn reconnect_ccp(&mut self, mut conn: Connection) {
        conn.set_queued_writes(true);
        self.ccp.reconnect(conn, &mut self.ccp_conn, &mut self.hb, &self.account_id);
    }

    /// The logon sent its order status replay request: open-order requests
    /// wait for the end of that replay, as the reference answers them only
    /// once the first order recovery of the session is complete (ibx#251).
    pub fn await_login_replay(&mut self) {
        self.ccp.awaiting_login_replay = true;
        self.shared.orders.set_open_orders_held(true);
    }

    /// Set the market-data farm name used in the farm status messages.
    pub fn set_farm_name(&mut self, name: String) {
        if !name.is_empty() {
            self.farm_name = name;
        }
    }

    /// Set cached auth credentials for farm auto-reconnect.
    pub fn set_reconnect_auth(&mut self, auth: ReconnectAuth) {
        self.reconnect_auth = Some(auth);
    }

    /// Whether auto-reconnect has a host to dial (ibx#399).
    pub fn has_reconnect_host(&self) -> bool {
        self.reconnect_auth.as_ref().is_some_and(|a| !a.host.is_empty())
    }

    /// Update caller-specific fields on the reconnect auth (host, username, password, paper).
    pub fn update_reconnect_auth(
        &mut self,
        host: String,
        username: String,
        password: zeroize::Zeroizing<String>,
        paper: bool,
    ) {
        if let Some(auth) = self.reconnect_auth.as_mut() {
            auth.host = host;
            auth.username = username;
            auth.password = password;
            auth.paper = paper;
        }
    }

    /// Schedule-then-spawn farm reconnects on the jittered backoff ladder
    /// (ibx#218). Called every loop iteration; no-op while connected or an
    /// attempt is in flight.
    fn maybe_spawn_farm_reconnect(&mut self) {
        if !self.farm.disconnected || self.pending_farm_reconnect.is_some() {
            return;
        }
        match self.farm_next_attempt_at {
            None => {
                let delay = reconnect_backoff();
                log::info!("Farm reconnect attempt {} scheduled in {:?} (ibx#218)",
                    self.farm_reconnect_attempt + 1, delay);
                self.farm_next_attempt_at = Some(Instant::now() + delay);
            }
            Some(due) if Instant::now() >= due => {
                self.farm_next_attempt_at = None;
                self.spawn_farm_reconnect();
                if self.pending_farm_reconnect.is_none() {
                    // Could not spawn (no cached credentials): re-check in a
                    // minute instead of warn-spamming every iteration.
                    self.farm_next_attempt_at =
                        Some(Instant::now() + std::time::Duration::from_secs(60));
                }
            }
            _ => {}
        }
    }

    /// See `maybe_spawn_farm_reconnect`.
    fn maybe_spawn_ccp_reconnect(&mut self) {
        if !self.ccp.disconnected || self.pending_ccp_reconnect.is_some() {
            return;
        }
        match self.ccp_next_attempt_at {
            None => {
                let delay = reconnect_backoff();
                log::info!("CCP reconnect attempt {} scheduled in {:?} (ibx#218)",
                    self.ccp_reconnect_attempt + 1, delay);
                self.ccp_next_attempt_at = Some(Instant::now() + delay);
            }
            Some(due) if Instant::now() >= due => {
                self.ccp_next_attempt_at = None;
                self.spawn_ccp_reconnect();
                if self.pending_ccp_reconnect.is_none() {
                    self.ccp_next_attempt_at =
                        Some(Instant::now() + std::time::Duration::from_secs(60));
                }
            }
            _ => {}
        }
    }

    /// Spawn a background thread to reconnect the farm using cached credentials.
    fn spawn_farm_reconnect(&mut self) {
        if self.pending_farm_reconnect.is_some() { return; } // already in progress
        let auth = match self.reconnect_auth.clone() {
            Some(a) if !a.host.is_empty() => a,
            _ => {
                log::warn!("Farm auto-reconnect skipped: no credentials (host empty or auth missing)");
                return;
            }
        };
        self.farm_reconnect_attempt += 1;
        let attempt = self.farm_reconnect_attempt;
        // The farm of the session, not a fixed name: a regional account logs
        // on again to its own farm, as in the reference (ibx#295).
        let (farm_host, farm_name) = farm_reconnect_target(&auth, &self.farm_name);
        log::info!("Farm auto-reconnect attempt {} starting (farm={}/{}, user={})",
            attempt, farm_host, farm_name, auth.username);

        let (tx, rx) = crossbeam_channel::bounded(1);
        std::thread::Builder::new()
            .name(format!("farm-reconnect-{}", attempt))
            .spawn(move || {
                let link = auth.farm_link(&farm_name, crate::gateway::FarmService::MarketData);
                let result = crate::gateway::connect_farm_opts(
                    &farm_host, &farm_name,
                    &auth.username, &auth.password, auth.paper,
                    &auth.server_session_id, &auth.session_key,
                    &auth.hw_info, &auth.encoded, 18, true, link,
                ).map(|(conn, _)| conn);
                let _ = tx.send(result);
            })
            .ok();
        self.pending_farm_reconnect = Some(rx);
    }

    /// Poll for a completed farm reconnect. Non-blocking.
    fn poll_farm_reconnect(&mut self) {
        let rx = match self.pending_farm_reconnect.as_ref() {
            Some(rx) => rx,
            None => return,
        };
        match rx.try_recv() {
            Ok(Ok(conn)) => {
                log::info!("Farm auto-reconnect succeeded (attempt {})", self.farm_reconnect_attempt);
                self.reconnect_farm(conn);
                self.farm_reconnect_attempt = 0;
                self.farm_next_attempt_at = None;
                self.pending_farm_reconnect = None;
            }
            Ok(Err(e)) => {
                log::error!("Farm auto-reconnect failed (attempt {}): {}", self.farm_reconnect_attempt, e);
                self.pending_farm_reconnect = None;
                // Retries continue on the backoff; the session stays open,
                // as with the reference, whose clients learn of the loss
                // from the farm status message (ibx#218, ibx#399).
                if self.farm_reconnect_attempt == 3 {
                    log::error!("Farm auto-reconnect failed 3 times (retries continue)");
                }
            }
            Err(crossbeam_channel::TryRecvError::Empty) => {}
            Err(crossbeam_channel::TryRecvError::Disconnected) => {
                log::error!("Farm reconnect thread dropped without result");
                self.pending_farm_reconnect = None;
            }
        }
    }

    /// Spawn a background thread to reconnect CCP using cached credentials.
    fn spawn_ccp_reconnect(&mut self) {
        if self.pending_ccp_reconnect.is_some() { return; }
        let auth = match self.reconnect_auth.clone() {
            Some(a) if !a.host.is_empty() => a,
            _ => {
                log::warn!("CCP auto-reconnect skipped: no credentials");
                return;
            }
        };
        self.ccp_reconnect_attempt += 1;
        let attempt = self.ccp_reconnect_attempt;
        // The primary host and its backups in turn (ibx#399).
        let host = ccp_reconnect_host(&auth.host, attempt);
        log::info!("CCP auto-reconnect attempt {} starting (host={})", attempt, host);

        let (tx, rx) = crossbeam_channel::bounded(1);
        std::thread::Builder::new()
            .name(format!("ccp-reconnect-{}", attempt))
            .spawn(move || {
                let _ = tx.send(reconnect_ccp_via(&auth, &host));
            })
            .ok();
        self.pending_ccp_reconnect = Some(rx);
    }

    /// Poll for a completed CCP reconnect. Non-blocking.
    fn poll_ccp_reconnect(&mut self) {
        let rx = match self.pending_ccp_reconnect.as_ref() {
            Some(rx) => rx,
            None => return,
        };
        match rx.try_recv() {
            Ok(Ok(CcpReconnect { conn, session_epoch, ns_secure_refused, logon })) => {
                log::info!("CCP auto-reconnect succeeded (attempt {})", self.ccp_reconnect_attempt);
                // Every logon reply sets the clock offset, the feature
                // tokens and the pending accounts; a changed data
                // permission stamp runs the permission change (ibx#421).
                crate::gateway::apply_logon_values(&logon, &self.shared);
                if self.ccp.relogin_data_permissions(logon.data_permissions.as_deref()) {
                    self.ccp.permissions_changed = true;
                }
                // The next reconnect resumes this server session (ibx#422).
                if let (Some(epoch), Some(auth)) = (session_epoch, self.reconnect_auth.as_mut()) {
                    auth.session_epoch = epoch;
                }
                // Farms opened from now on follow this login: in clear when
                // the server refused its encryption (ibx#423).
                if let Some(auth) = self.reconnect_auth.as_mut() {
                    auth.ns_secure_refused = ns_secure_refused;
                }
                self.reconnect_ccp(conn);
                self.ccp_reconnect_attempt = 0;
                self.ccp_next_attempt_at = None;
                self.pending_ccp_reconnect = None;
            }
            Ok(Err(e)) => {
                log::error!("CCP auto-reconnect failed (attempt {}): {}", self.ccp_reconnect_attempt, e);
                self.pending_ccp_reconnect = None;
                // See the farm path: the clients had the lost-link message
                // at once and stay connected (ibx#399).
                if self.ccp_reconnect_attempt == 3 {
                    log::error!("CCP auto-reconnect failed 3 times (retries continue)");
                }
            }
            Err(crossbeam_channel::TryRecvError::Empty) => {}
            Err(crossbeam_channel::TryRecvError::Disconnected) => {
                log::error!("CCP reconnect thread dropped without result");
                self.pending_ccp_reconnect = None;
            }
        }
    }

    /// If HMDS is down and a backoff window has elapsed, spawn the next attempt.
    /// Auto-schedules the first attempt when the engine starts with no HMDS
    /// connection — covers the ibx#187 case where initial soft-token returned
    /// FAILED and the gateway dropped the socket.
    fn maybe_spawn_hmds_reconnect(&mut self) {
        if self.hmds_conn.is_some() { return; }
        if self.pending_hmds_reconnect.is_some() { return; }
        let auth = match self.reconnect_auth.as_ref() {
            Some(a) if !a.host.is_empty() && !a.hmds_host.is_empty() => a,
            _ => return,
        };
        if self.hmds_reconnect_attempt >= HMDS_MAX_RECONNECT_ATTEMPTS {
            return;
        }
        // Schedule the first attempt if not already scheduled.
        if self.hmds_next_attempt_at.is_none() {
            self.hmds_next_attempt_at = Some(Instant::now() + hmds_reconnect_backoff(self.hmds_reconnect_attempt + 1));
            return;
        }
        let due = self.hmds_next_attempt_at.unwrap();
        if Instant::now() < due { return; }
        let auth = auth.clone();
        self.hmds_reconnect_attempt += 1;
        let attempt = self.hmds_reconnect_attempt;
        log::info!(
            "HMDS reconnect attempt {} starting (host={}/{})",
            attempt, auth.hmds_host, auth.hmds_farm,
        );
        let (tx, rx) = crossbeam_channel::bounded(1);
        std::thread::Builder::new()
            .name(format!("hmds-reconnect-{}", attempt))
            .spawn(move || {
                let link = auth.farm_link(&auth.hmds_farm, crate::gateway::FarmService::Historical);
                let result = crate::gateway::connect_farm_opts(
                    &auth.hmds_host, &auth.hmds_farm,
                    &auth.username, &auth.password, auth.paper,
                    &auth.server_session_id, &auth.session_key,
                    &auth.hw_info, &auth.encoded, 17, true, link,
                ).map(|(conn, _)| conn);
                let _ = tx.send(result);
            })
            .ok();
        self.pending_hmds_reconnect = Some(rx);
    }

    /// Poll for a completed HMDS reconnect. Non-blocking.
    fn poll_hmds_reconnect(&mut self) {
        let rx = match self.pending_hmds_reconnect.as_ref() {
            Some(rx) => rx,
            None => return,
        };
        match rx.try_recv() {
            Ok(Ok(mut conn)) => {
                log::info!("HMDS reconnect succeeded (attempt {})", self.hmds_reconnect_attempt);
                conn.set_queued_writes(true);
                self.hmds_conn = Some(conn);
                self.hmds.disconnected = false;
                self.hb.hmds_connected(Instant::now());
                self.hmds_reconnect_attempt = 0;
                self.hmds_next_attempt_at = None;
                self.pending_hmds_reconnect = None;
            }
            Ok(Err(e)) => {
                log::warn!(
                    "HMDS reconnect failed (attempt {}/{}): {}",
                    self.hmds_reconnect_attempt, HMDS_MAX_RECONNECT_ATTEMPTS, e,
                );
                self.pending_hmds_reconnect = None;
                self.hmds.scanner_connect_failed(&self.shared);
                if self.hmds_reconnect_attempt >= HMDS_MAX_RECONNECT_ATTEMPTS {
                    log::error!(
                        "HMDS reconnect exhausted {} attempts — historical data unavailable for this session",
                        HMDS_MAX_RECONNECT_ATTEMPTS,
                    );
                    self.hmds_next_attempt_at = None;
                } else {
                    self.hmds_next_attempt_at = Some(Instant::now() + hmds_reconnect_backoff(self.hmds_reconnect_attempt + 1));
                }
            }
            Err(crossbeam_channel::TryRecvError::Empty) => {}
            Err(crossbeam_channel::TryRecvError::Disconnected) => {
                log::error!("HMDS reconnect thread dropped without result");
                self.pending_hmds_reconnect = None;
            }
        }
    }

    /// Access heartbeat state for testing.
    pub fn heartbeat_state(&self) -> &HeartbeatState {
        &self.hb
    }

    /// Test-only: force farm into disconnected state.
    #[cfg(any(test, feature = "test-support"))]
    pub fn force_farm_disconnect(&mut self) {
        self.farm.handle_disconnect_for_test();
    }

    /// Test-only: lose the farm connection through the same path as a real
    /// loss (subscription state cleared, socket dropped).
    #[cfg(any(test, feature = "test-support"))]
    pub fn lose_farm_for_test(&mut self) {
        self.farm.handle_disconnect(&mut self.context, &self.event_tx);
        self.farm_conn = None;
    }

    /// Test-only: the engine's instrument table.
    #[cfg(any(test, feature = "test-support"))]
    pub fn market_for_test(&mut self) -> &mut crate::engine::market_state::MarketState {
        &mut self.context.market
    }

    /// Test-only: poll the farm socket once.
    #[cfg(any(test, feature = "test-support"))]
    pub fn poll_farm_for_test(&mut self) {
        self.farm.poll_market_data(
            &mut self.farm_conn, &mut self.context, &self.shared,
            &self.event_tx, &mut self.hb,
        );
    }

    /// Test-only: poll the auth socket once, as step 3 of the loop does.
    #[cfg(any(test, feature = "test-support"))]
    pub fn poll_auth_for_test(&mut self) {
        self.poll_auth();
    }

    /// Test-only: trigger farm reconnect spawn.
    #[cfg(any(test, feature = "test-support"))]
    pub fn spawn_farm_reconnect_for_test(&mut self) {
        self.spawn_farm_reconnect();
    }

    /// Test-only: poll pending farm reconnect.
    #[cfg(any(test, feature = "test-support"))]
    pub fn poll_farm_reconnect_for_test(&mut self) {
        self.poll_farm_reconnect();
    }

    /// Mutably access heartbeat state for testing (e.g., setting timestamps).
    pub fn heartbeat_state_mut(&mut self) -> &mut HeartbeatState {
        &mut self.hb
    }

    /// One turn of the loop, for the replay tests (ibx#486): read the farm,
    /// historical and auth links, send the pending orders, take the control
    /// commands and write what waits. No liveness checks, no reconnects, no
    /// farms opened on demand: a test drives the links itself.
    #[cfg(any(test, feature = "test-support"))]
    pub fn step_for_test(&mut self) {
        self.farm.poll_market_data(&mut self.farm_conn, &mut self.context, &self.shared, &self.event_tx, &mut self.hb);
        self.hmds.poll(&mut self.hmds_conn, &self.shared, &self.event_tx, &mut self.hb);
        order_builder::drain_and_send_orders(
            &mut self.ccp_conn, &mut self.context, &self.account_id, &mut self.hb,
            self.ccp.disconnected, &self.shared,
        );
        self.poll_auth();
        self.poll_control_commands();
        self.check_writes();
    }

    /// Inject a raw farm message for testing. Processes it through the full decode pipeline.
    #[cfg(any(test, feature = "test-support"))]
    pub fn inject_farm_message(&mut self, msg: &[u8]) {
        self.farm.process_farm_message(msg, &mut self.farm_conn, &mut self.context, &self.shared, &self.event_tx, &mut self.hb);
    }

    /// Inject a raw auth message for testing. Processes execution reports, etc.
    #[cfg(any(test, feature = "test-support"))]
    pub fn inject_ccp_message(&mut self, msg: &[u8]) {
        self.ccp.process_ccp_message(msg, &mut self.ccp_conn, &mut self.context, &self.shared, &self.event_tx, &mut self.hb, &self.account_id);
    }

    /// Inject a raw HMDS message for testing. Processes historical data, news, etc.
    #[cfg(any(test, feature = "test-support"))]
    pub fn inject_hmds_message(&mut self, msg: &[u8]) {
        self.hmds.process_hmds_message(msg, &mut self.hmds_conn, &self.shared, &self.event_tx, &mut self.hb);
    }

    /// Inject a TBT trade for testing. Pushes to SharedState and emits event.
    #[cfg(any(test, feature = "test-support"))]
    pub fn inject_tbt_trade(&mut self, trade: &TbtTrade) {
        self.shared.market.push_tbt_trade(trade.clone());
        emit(&self.event_tx, Event::TbtTrade(trade.clone()));
    }

    /// Inject a TBT quote for testing. Pushes to SharedState.
    #[cfg(any(test, feature = "test-support"))]
    pub fn inject_tbt_quote(&mut self, quote: &TbtQuote) {
        self.shared.market.push_tbt_quote(quote.clone());
    }

    /// Inject a simulated tick for testing: the instrument's quote, as one
    /// farm message for the API client (`md_events::TestMessage`).
    #[cfg(any(test, feature = "test-support"))]
    pub fn inject_tick(&mut self, instrument: InstrumentId) {
        self.shared.market.push_test_message(instrument, self.context.quote(instrument), &Default::default());
        emit(&self.event_tx, Event::Tick(instrument));
    }

    /// Simulate a fill for testing. Updates position and notifies.
    #[cfg(any(test, feature = "test-support"))]
    pub fn inject_fill(&mut self, fill: &Fill) {
        let delta = match fill.side {
            crate::types::Side::Buy => fill.qty_fixed,
            crate::types::Side::Sell | crate::types::Side::ShortSell => -fill.qty_fixed,
        };
        self.context.update_position_fixed(fill.instrument, delta);
        self.shared.orders.push_fill(*fill);
        self.shared.portfolio.set_position_fixed(fill.instrument, self.context.position_fixed(fill.instrument));
        emit(&self.event_tx, Event::Fill(*fill));
    }
}

/// The periodic farm test request, outside the sequence count, as the
/// reference sends it (ibx#419).
fn send_farm_ping(conn: &mut Connection, test_id: &str) -> io::Result<()> {
    let ts = chrono_free_timestamp();
    conn.send_fix_unsequenced(&[
        (fix::TAG_MSG_TYPE, fix::MSG_TEST_REQUEST),
        (fix::TAG_SENDING_TIME, &ts),
        (fix::TAG_TEST_REQ_ID, test_id),
    ])
}

// ── Helper functions used by subsystems ──

/// Stack-allocated string (up to 24 bytes). Zero heap allocations.
pub(crate) struct StackStr {
    buf: [u8; 24],
    len: u8,
}

impl StackStr {
    #[inline]
    fn new() -> Self {
        Self { buf: [0; 24], len: 0 }
    }

    #[inline]
    fn push(&mut self, b: u8) {
        self.buf[self.len as usize] = b;
        self.len += 1;
    }

    /// Write an i64 in decimal. Returns number of bytes written.
    fn write_i64(&mut self, val: i64) {
        if val < 0 {
            self.push(b'-');
            self.write_u64((-val) as u64);
        } else {
            self.write_u64(val as u64);
        }
    }

    fn write_u64(&mut self, val: u64) {
        if val == 0 {
            self.push(b'0');
            return;
        }
        // Write digits in reverse, then reverse them in-place.
        let start = self.len as usize;
        let mut v = val;
        while v > 0 {
            self.push(b'0' + (v % 10) as u8);
            v /= 10;
        }
        self.buf[start..self.len as usize].reverse();
    }
}

impl std::ops::Deref for StackStr {
    type Target = str;
    #[inline]
    fn deref(&self) -> &str {
        // SAFETY: We only write ASCII digits, '.', '-', and ':'
        unsafe { std::str::from_utf8_unchecked(&self.buf[..self.len as usize]) }
    }
}

impl std::fmt::Display for StackStr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self)
    }
}

impl std::fmt::Debug for StackStr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self)
    }
}

/// Format an unsigned integer to a stack string. Zero alloc.
#[inline]
pub(crate) fn format_uint(val: u64) -> StackStr {
    let mut s = StackStr::new();
    s.write_u64(val);
    s
}

/// Emit an event to the channel (if connected). Non-blocking — drops event if full.
#[inline]
pub(crate) fn emit(event_tx: &Option<Sender<Event>>, event: Event) {
    if let Some(tx) = event_tx {
        let _ = tx.try_send(event);
    }
}

/// Clone a payload for the event channel, but only when one is attached.
///
/// Use this wherever the payload is a deep copy (bar batches, contract
/// definitions): the value goes to `SharedState` by move and the clone is paid
/// for only when someone is listening. With no channel — the default for the
/// Rust client — nothing is copied at all (ibx#242).
///
/// Clone first, push second, emit last, so the event never becomes visible
/// before the same data is readable from `SharedState`.
#[inline]
pub(crate) fn clone_for_event<T: Clone>(event_tx: &Option<Sender<Event>>, value: &T) -> Option<T> {
    event_tx.as_ref().map(|_| value.clone())
}

/// Jittered reconnect delay for CCP/farm: 5 s plus up to 10 s, the same
/// for every attempt, as the reference retries a lost link every 5 to 15 s
/// (ibx#399; ibx#218 for the jitter). Immediate rapid-fire re-dials risk
/// server-side rate limiting. (HMDS keeps its own schedule below.)
pub(crate) fn reconnect_backoff() -> std::time::Duration {
    const FLOOR_MS: u64 = 5_000;
    const JITTER_MS: u64 = 10_000;
    std::time::Duration::from_millis(FLOOR_MS + rand::random::<u64>() % JITTER_MS)
}

/// Host and name for a market-data farm reconnect: the session's farm, else
/// the auth host and `fallback_name` (the name used at login).
fn farm_reconnect_target(auth: &ReconnectAuth, fallback_name: &str) -> (String, String) {
    let host = if auth.farm_host.is_empty() { auth.host.clone() } else { auth.farm_host.clone() };
    let name = if auth.farm_name.is_empty() { fallback_name.to_string() } else { auth.farm_name.clone() };
    (host, name)
}

/// The data kind of a bar request's route: daily or longer bars are end of
/// day charts, others day charts, as the reference picks it (#445).
fn bar_data_kind(bar_size: &str) -> crate::engine::routing::DataType {
    let unit = bar_size.trim().to_ascii_lowercase();
    if ["day", "week", "month"].iter().any(|u| unit.contains(u)) {
        crate::engine::routing::DataType::EODChart
    } else {
        crate::engine::routing::DataType::DayChart
    }
}

/// The farm of a pool event.
fn event_farm(event: &pool::FarmEvent) -> pool::FarmId {
    match event {
        pool::FarmEvent::Connecting(id) | pool::FarmEvent::Connected(id)
        | pool::FarmEvent::Lost(id) | pool::FarmEvent::IdleClosed(id) => *id,
    }
}

/// Up/down state of each connection, as reported to the clients.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Links {
    ccp: bool,
    farm: bool,
    hmds: bool,
}

/// Period of the server tag cleaner, how long a market data record stays
/// unused before its quote tags go, most records torn down per run, and the
/// number of waiting records above which the age is not checked (#292).
const TAG_CLEAN_PERIOD: std::time::Duration = std::time::Duration::from_secs(60);
const TAG_UNUSED_FOR: std::time::Duration = std::time::Duration::from_secs(300);
const TAG_CLEAN_MAX_PER_RUN: usize = 100;
const TAG_CLEAN_SKIP_AGE_ABOVE: usize = 1000;

/// Longest wait for the data farms before the restored-link message.
const RESTORE_FARM_WAIT: std::time::Duration = std::time::Duration::from_secs(30);

const LINK_LOST: &str = "Connectivity between client and server has been lost.";
const LINK_RESTORED: &str = "Connectivity between client and server has been restored - data maintained.";

/// Backoff schedule for HMDS reconnect attempts (ibx#187, ib-agent#153).
/// `min(64, 3 * 2^(attempt-1))` seconds — approximates the captured cadence
/// of 3.2 / 11.4 / 18.5 / 42.7 / 63.7 s the official client uses.
#[inline]
pub(crate) fn hmds_reconnect_backoff(attempt: u32) -> std::time::Duration {
    let n = attempt.saturating_sub(1).min(31);
    let secs = (3u64.saturating_mul(1u64 << n)).min(64);
    std::time::Duration::from_secs(secs)
}

/// The reference's local refusals of a scanner subscription (ibx#457), in
/// its order: the concurrent limit (a tenth of the ticker limit), then a
/// request id that is already live. Both are 322.
fn scanner_refusal(hmds: &HmdsState, req_id: ReqId) -> Option<String> {
    let cause = |c: &str| format!("Error processing request.-'co' : cause - {}", c);
    // The ticker limit of the logon, the one real-time bars use too.
    let max = hmds.max_real_time_requests / 10;
    if hmds.scanner_sessions() >= max as usize {
        return Some(cause(&format!("Only {} simultaneous API scanner subscriptions are allowed.", max)));
    }
    if hmds.pending_scanner.iter().any(|s| s.req_id == req_id) {
        return Some(cause("Duplicate ticker ID for API scanner subscription"));
    }
    None
}

/// Surface an "HMDS unavailable" error for `req_id` when the historical-data
/// socket isn't connected: code 162
/// via `push_historical_error` for the consumer's `error()` callback, plus —
/// for historical-bar requests only — a terminal empty-bars response so
/// `historical_data_end` fires. Without this, requests issued while HMDS is
/// down hang silently (ibx#187).
pub(crate) fn push_hmds_unavailable(shared: &SharedState, req_id: ReqId, from_historical: bool) {
    push_hmds_error(
        shared, req_id,
        "Historical data service connection is not available".to_string(),
        from_historical,
    );
}

/// Surface an HMDS-side request failure: error 162 plus, for bar requests,
/// the terminal completion sentinel so a blocked wait unblocks.
pub(crate) fn push_hmds_error(shared: &SharedState, req_id: ReqId, message: String, from_historical: bool) {
    const HMDS_ERROR_CODE: i32 = 162;
    shared.reference.push_historical_error(
        req_id,
        HMDS_ERROR_CODE,
        message,
    );
    if from_historical {
        shared.reference.push_historical_data(
            req_id,
            crate::control::historical::HistoricalResponse {
                query_id: String::new(),
                timezone: String::new(),
                is_complete: true,
                bars: Vec::new(),
                ..Default::default()
            },
        );
    }
}

/// Format a fixed-point Price as a decimal string for FIX tags. Zero alloc.
pub(crate) fn format_price(price: Price) -> StackStr {
    let whole = price / PRICE_SCALE;
    let frac = (price % PRICE_SCALE).unsigned_abs();
    let mut s = StackStr::new();
    // Between -1 and 0 the whole part truncates to 0 and cannot carry the sign.
    if price < 0 && whole == 0 {
        s.push(b'-');
    }
    s.write_i64(whole);
    if frac != 0 {
        s.push(b'.');
        // Write 8-digit zero-padded fraction, then trim trailing zeros.
        let frac_start = s.len as usize;
        let digits = [
            b'0' + (frac / 10_000_000 % 10) as u8,
            b'0' + (frac / 1_000_000 % 10) as u8,
            b'0' + (frac / 100_000 % 10) as u8,
            b'0' + (frac / 10_000 % 10) as u8,
            b'0' + (frac / 1_000 % 10) as u8,
            b'0' + (frac / 100 % 10) as u8,
            b'0' + (frac / 10 % 10) as u8,
            b'0' + (frac % 10) as u8,
        ];
        // Find last non-zero digit.
        let mut end = 8;
        while end > 0 && digits[end - 1] == b'0' { end -= 1; }
        for i in 0..end {
            s.buf[frac_start + i] = digits[i];
        }
        s.len = (frac_start + end) as u8;
    }
    s
}

/// A price in the reference's number form for its price fields: at least
/// two decimals, at most eight (`0.00`, `0.05`, `272.885`, `-0.10`), the
/// pattern `#0.00######` of its price formatter (`jutils.dO.F`, US
/// symbols, no grouping), which writes every price tag of its order
/// messages (ibx#263). Zero alloc.
pub(crate) fn format_price_ref(price: Price) -> StackStr {
    let mut s = format_price(price);
    let len = s.len as usize;
    match s.buf[..len].iter().position(|&b| b == b'.') {
        None => { s.push(b'.'); s.push(b'0'); s.push(b'0'); }
        Some(dot) if len - dot == 2 => s.push(b'0'),
        Some(_) => {}
    }
    s
}

/// Parse a FIX tag value as a Price (fixed-point). Returns 0 if absent,
/// unparseable, or non-finite. Rust's f64 parser accepts "nan"/"inf", but on
/// the wire those are not-available sentinels, not values: the gateway's own
/// field parser maps nan/unparseable to unset (ibx#214). Without the finite
/// filter, "nan" saturated to 0 and "inf" to i64::MAX.
pub(crate) fn parse_price_tag(val: Option<&String>) -> Price {
    val.and_then(|s| s.parse::<f64>().ok())
        .filter(|f| f.is_finite())
        .map(|f| (f * PRICE_SCALE as f64) as Price)
        .unwrap_or(0)
}

/// The time in force of a code, as the reference reports it (ibx#307):
/// every code it knows by its text, "???" for any other. The inverse of
/// `api::types::Order::tif_byte` for the values ibx sends (ibx#220).
pub(crate) fn decode_tif(tif: u8) -> &'static str {
    match tif {
        b'0' => "DAY", b'1' => "GTC", b'2' => "OPG", b'3' => "IOC",
        b'4' => "FOK", b'5' => "GTX", b'6' => "GTD", b'8' => "AUC",
        b'?' => "[INVALID]", b'b' => "OVERNIGHT + DAY", b'j' => "OVERNIGHT",
        b'p' => "Minutes", crate::types::TIF_DTC => "DTC", _ => "???",
    }
}

/// The time-in-force code of an order report, as the reference reads it
/// (ibx#307): DAY when 59 is absent; else the first character of 59, DTC
/// for 59=1 with the DTC flag (6436=1), OVERNIGHT + DAY with 8534=1, else
/// OVERNIGHT when the exchange (6004) is OVERNIGHT.
pub(crate) fn report_tif(parsed: &std::collections::HashMap<u32, String>) -> u8 {
    let Some(mut code) = parsed.get(&59).and_then(|s| s.bytes().next()) else { return b'0' };
    let flag = |tag: u32| parsed.get(&tag).map(|s| s.as_str()) == Some("1");
    if code == b'1' && flag(6436) { code = crate::types::TIF_DTC; }
    if flag(8534) {
        code = b'b';
    } else if parsed.get(&6004).map(|s| s.as_str()) == Some("OVERNIGHT") {
        code = b'j';
    }
    code
}

/// Parse a decimal quantity ("1", "0.5") into a fixed-point Qty
/// (QTY_SCALE = 10^4). A fraction such as a partial share is kept: reading
/// it as a whole number dropped the fill (ibx#313).
pub(crate) fn parse_qty(s: &str) -> Option<Qty> {
    s.parse::<f64>().ok().filter(|v| v.is_finite()).map(|v| (v * QTY_SCALE as f64).round() as Qty)
}

/// Format a fixed-point Qty (QTY_SCALE = 10^4) to a decimal string. Zero alloc.
pub(crate) fn format_qty(qty: Qty) -> StackStr {
    let whole = qty / QTY_SCALE;
    let frac = (qty % QTY_SCALE).unsigned_abs();
    let mut s = StackStr::new();
    s.write_i64(whole);
    if frac != 0 {
        s.push(b'.');
        let frac_start = s.len as usize;
        let digits = [
            b'0' + (frac / 1_000 % 10) as u8,
            b'0' + (frac / 100 % 10) as u8,
            b'0' + (frac / 10 % 10) as u8,
            b'0' + (frac % 10) as u8,
        ];
        let mut end = 4;
        while end > 0 && digits[end - 1] == b'0' { end -= 1; }
        for i in 0..end {
            s.buf[frac_start + i] = digits[i];
        }
        s.len = (frac_start + end) as u8;
    }
    s
}

/// Fast extraction of FIX tag 35 (MsgType) value via byte scan.
pub(crate) fn fast_extract_msg_type(msg: &[u8]) -> Option<&[u8]> {
    let limit = msg.len().min(48);
    let mut i = 0;
    while i + 3 < limit {
        if msg[i] == b'3' && msg[i + 1] == b'5' && msg[i + 2] == b'=' {
            if i == 0 || msg[i - 1] == 0x01 {
                let val_start = i + 3;
                let mut j = val_start;
                while j < msg.len() && msg[j] != 0x01 {
                    j += 1;
                }
                if j > val_start {
                    return Some(&msg[val_start..j]);
                }
            }
        }
        i += 1;
    }
    None
}

pub(crate) fn find_body_after_tag<'a>(msg: &'a [u8], tag_marker: &[u8]) -> Option<&'a [u8]> {
    msg.windows(tag_marker.len())
        .position(|w| w == tag_marker)
        .map(|pos| &msg[pos + tag_marker.len()..])
}

/// Extract the raw bytes of a binary FIX tag value using a length tag.
pub(crate) fn extract_raw_tag(msg: &[u8], tag: u32) -> Option<Vec<u8>> {
    let len_tag = tag - 1;
    if let Some(len_val) = extract_text_tag(msg, len_tag) {
        if let Ok(data_len) = len_val.parse::<usize>() {
            let needle = format!("{}=", tag);
            let needle_bytes = needle.as_bytes();
            if let Some(idx) = msg.windows(needle_bytes.len()).position(|w| w == needle_bytes) {
                let val_start = idx + needle_bytes.len();
                // A length past the message takes the rest of it; the sum
                // overflowed on a length near the largest number (ibx#488).
                let val_end = val_start.saturating_add(data_len).min(msg.len());
                return Some(msg[val_start..val_end].to_vec());
            }
        }
    }
    let needle = format!("{}=", tag);
    let needle_bytes = needle.as_bytes();
    let mut pos = 0;
    while pos < msg.len() {
        let remaining = &msg[pos..];
        if let Some(idx) = remaining.windows(needle_bytes.len()).position(|w| w == needle_bytes) {
            let abs_idx = pos + idx;
            if abs_idx == 0 || msg[abs_idx - 1] == 0x01 {
                let val_start = abs_idx + needle_bytes.len();
                let val_end = msg[val_start..].iter().position(|&b| b == 0x01)
                    .map(|p| val_start + p)
                    .unwrap_or(msg.len());
                return Some(msg[val_start..val_end].to_vec());
            }
            pos = abs_idx + 1;
        } else {
            break;
        }
    }
    None
}

/// Extract a text FIX tag value (SOH-delimited) from raw message bytes.
fn extract_text_tag(msg: &[u8], tag: u32) -> Option<String> {
    let needle = format!("{}=", tag);
    let needle_bytes = needle.as_bytes();
    let mut pos = 0;
    while pos < msg.len() {
        let remaining = &msg[pos..];
        if let Some(idx) = remaining.windows(needle_bytes.len()).position(|w| w == needle_bytes) {
            let abs_idx = pos + idx;
            if abs_idx == 0 || msg[abs_idx - 1] == 0x01 {
                let val_start = abs_idx + needle_bytes.len();
                let val_end = msg[val_start..].iter().position(|&b| b == 0x01)
                    .map(|p| val_start + p)
                    .unwrap_or(msg.len());
                return Some(String::from_utf8_lossy(&msg[val_start..val_end]).into_owned());
            }
            pos = abs_idx + 1;
        } else {
            break;
        }
    }
    None
}

#[cfg(test)]
mod combo_tests;
#[cfg(test)]
mod robustness_tests;

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use crate::bridge::{Event, SharedState};
    use crate::types::*;
    use std::time::Duration;

    // ibx#214: f64::from_str accepts "nan"/"inf", so a not-available sentinel
    // of "nan" collapsed to price 0 (and "inf" saturated to i64::MAX) instead
    // of being treated as unset.
    #[test]
    fn parse_price_tag_rejects_non_finite_sentinels() {
        let s = |v: &str| v.to_string();
        assert_eq!(parse_price_tag(Some(&s("nan"))), 0);
        assert_eq!(parse_price_tag(Some(&s("NaN"))), 0);
        assert_eq!(parse_price_tag(Some(&s("inf"))), 0);
        assert_eq!(parse_price_tag(Some(&s("-inf"))), 0);
        assert_eq!(parse_price_tag(Some(&s("n/a"))), 0);
        assert_eq!(parse_price_tag(None), 0);
        // Genuine numbers still parse, including a true zero.
        assert_eq!(parse_price_tag(Some(&s("0"))), 0);
        assert_eq!(parse_price_tag(Some(&s("434.71"))), (434.71 * PRICE_SCALE as f64) as Price);
        assert_eq!(parse_price_tag(Some(&s("-1.5"))), (-1.5 * PRICE_SCALE as f64) as Price);
    }

    // ibx#332: a price between -1 and 0 lost its minus sign.
    #[test]
    fn format_price_keeps_sign_between_minus_one_and_zero() {
        let p = |v: f64| format_price((v * PRICE_SCALE as f64).round() as Price);
        assert_eq!(&*p(-0.30), "-0.3");
        assert_eq!(&*p(-0.05), "-0.05");
        assert_eq!(&*p(-0.01), "-0.01");
        assert_eq!(&*p(-1.25), "-1.25");
        assert_eq!(&*p(-1.0), "-1");
        assert_eq!(&*p(0.0), "0");
        assert_eq!(&*p(0.35), "0.35");
        assert_eq!(&*p(1.5), "1.5");
        assert_eq!(&*format_price(-1), "-0.00000001");
    }

    // The reference writes its price fields with two decimals at least.
    #[test]
    fn format_price_ref_has_two_decimals_at_least() {
        let p = |v: f64| format_price_ref((v * PRICE_SCALE as f64).round() as Price);
        assert_eq!(&*p(0.0), "0.00");
        assert_eq!(&*p(0.05), "0.05");
        assert_eq!(&*p(0.5), "0.50");
        assert_eq!(&*p(-0.5), "-0.50");
        assert_eq!(&*p(272.88), "272.88");
        assert_eq!(&*p(721.0), "721.00");
        assert_eq!(&*p(272.885), "272.885");
    }

    #[test]
    fn inject_tick_emits_events() {
        let shared = Arc::new(SharedState::new());
        let (event_tx, event_rx) = crossbeam_channel::unbounded();
        let mut engine = HotLoop::new(shared.clone(), Some(event_tx), None);
        engine.context_mut().market.register(265598);

        engine.inject_tick(0);
        engine.inject_tick(0);

        let events: Vec<Event> = event_rx.try_iter().collect();
        let tick_count = events.iter().filter(|e| matches!(e, Event::Tick(_))).count();
        assert_eq!(tick_count, 2);
    }

    #[test]
    fn inject_tick_multiple_instruments() {
        let shared = Arc::new(SharedState::new());
        let (event_tx, event_rx) = crossbeam_channel::unbounded();
        let mut engine = HotLoop::new(shared.clone(), Some(event_tx), None);
        engine.context_mut().market.register(265598); // 0: AAPL
        engine.context_mut().market.register(272093); // 1: MSFT

        engine.inject_tick(0);
        engine.inject_tick(1);

        let events: Vec<Event> = event_rx.try_iter().collect();
        let tick_events: Vec<_> = events.iter().filter_map(|e| match e {
            Event::Tick(id) => Some(*id),
            _ => None,
        }).collect();
        assert_eq!(tick_events, vec![0, 1]);
    }

    #[test]
    fn inject_fill_updates_position() {
        let shared = Arc::new(SharedState::new());
        let (event_tx, event_rx) = crossbeam_channel::unbounded();
        let mut engine = HotLoop::new(shared.clone(), Some(event_tx), None);
        engine.context_mut().market.register(265598);

        let fill = Fill {
            cum_qty_fixed: (0) as i64 * crate::types::QTY_SCALE, avg_price: 0,
            instrument: 0,
            order_id: 1001,
            side: Side::Buy,
            price: 150_00000000,
            qty_fixed: (100) as i64 * crate::types::QTY_SCALE,
            remaining_fixed: (0) as i64 * crate::types::QTY_SCALE,
            commission: 1_00000000,
            timestamp_ns: 0,
        };
        engine.inject_fill(&fill);
        assert_eq!(engine.context_mut().position_fixed(0) / crate::types::QTY_SCALE, 100);
    }

    #[test]
    fn heartbeat_state_accessible() {
        let shared = Arc::new(SharedState::new());
        let mut engine = HotLoop::new(shared, None, None);
        let hb = engine.heartbeat_state_mut();
        hb.last_farm_sent = Instant::now() - Duration::from_secs(60);
        assert!(engine.heartbeat_state().last_farm_sent.elapsed().as_secs() >= 59);
    }

    #[test]
    fn shutdown_sets_running_false() {
        let shared = Arc::new(SharedState::new());
        let (tx, rx) = crossbeam_channel::bounded(1);
        let mut engine = HotLoop::new(shared, None, None);
        engine.set_control_rx(rx);
        engine.running = true;
        tx.send(ControlCommand::Shutdown).unwrap();
        engine.poll_once();
        assert!(!engine.is_running());
    }

    #[test]
    fn channel_disconnect_stops_loop() {
        let shared = Arc::new(SharedState::new());
        let (event_tx, event_rx) = crossbeam_channel::unbounded();
        let (tx, rx) = crossbeam_channel::bounded(1);
        let mut engine = HotLoop::new(shared, Some(event_tx), None);
        engine.set_control_rx(rx);
        engine.running = true;

        // Drop sender — simulates EClient being dropped without disconnect().
        drop(tx);

        engine.poll_once();
        assert!(!engine.is_running(), "hot loop should stop when control channel disconnects");

        // Should emit Disconnected event.
        let events: Vec<Event> = event_rx.try_iter().collect();
        assert!(events.iter().any(|e| matches!(e, Event::Disconnected)));
    }

    #[test]
    fn shutdown_sets_connection_lost_flag_without_event_channel() {
        // ibx#242: the flag path must work with no event channel attached,
        // which is the default for the Rust client.
        let shared = Arc::new(SharedState::new());
        let (tx, rx) = crossbeam_channel::bounded(1);
        let mut engine = HotLoop::new(shared.clone(), None, None);
        engine.set_control_rx(rx);
        engine.running = true;
        tx.send(ControlCommand::Shutdown).unwrap();
        engine.poll_once();

        assert!(shared.take_connection_lost(), "shutdown must signal connection lost");
        assert!(!shared.take_connection_lost(), "flag must clear after being read");
    }

    // ibx#427: a request without conId waits for its contract lookup, then
    // goes through the normal command path with the conId found.
    #[test]
    fn a_resolved_request_is_dispatched_with_its_con_id() {
        let shared = Arc::new(SharedState::new());
        let (tx, rx) = crossbeam_channel::unbounded();
        let mut engine = HotLoop::new(shared.clone(), None, None);
        engine.set_control_rx(rx);
        engine.running = true;
        let request = ControlCommand::FetchHeadTimestamp {
            req_id: 3, con_id: 0, sec_type: "STK".into(), exchange: "SMART".into(),
            what_to_show: "TRADES".into(), use_rth: true, format_date: 1,
        };
        tx.send(ControlCommand::ResolveContract {
            req_id: 3,
            lookup: crate::types::ContractLookup { symbol: "AAPL".into(), sec_type: "STK".into(), exchange: "SMART".into(), currency: "USD".into(), ..Default::default() },
            request: Box::new(request),
        }).unwrap();
        engine.poll_once();
        assert_eq!(engine.ccp.pending_resolves.len(), 1);
        assert!(shared.reference.drain_historical_errors().is_empty(), "nothing is sent before the lookup answers");

        engine.inject_ccp_message(&crate::control::contracts::tests::pipe_msg(
            "35=d|43=N|320=FixSecDefReqBySymbol3489660928|322=*|323=4|55=AAPL|167=STK|207=BEST|6008=265598|15=USD"));
        assert!(matches!(engine.ccp.resolved_requests.as_slice(),
            [ControlCommand::FetchHeadTimestamp { con_id: 265598, .. }]));
        engine.poll_once();
        assert!(engine.ccp.resolved_requests.is_empty());
        // No historical connection in this test: the dispatched request
        // reports it, which shows it went through the command path.
        let errors = shared.reference.drain_historical_errors();
        assert_eq!(errors.len(), 1);
        assert_eq!((errors[0].0, errors[0].1), (3, 162));
    }

    #[test]
    fn channel_disconnect_sets_connection_lost_flag() {
        let shared = Arc::new(SharedState::new());
        let (tx, rx) = crossbeam_channel::bounded(1);
        let mut engine = HotLoop::new(shared.clone(), None, None);
        engine.set_control_rx(rx);
        engine.running = true;
        drop(tx);
        engine.poll_once();

        assert!(shared.take_connection_lost());
    }

    #[test]
    fn clone_for_event_skips_the_copy_when_no_channel() {
        // ibx#242: with no listener the deep copy must not happen at all.
        let payload = vec![1u8, 2, 3];
        assert!(clone_for_event(&None, &payload).is_none());

        let (tx, _rx) = crossbeam_channel::bounded::<Event>(1);
        assert_eq!(clone_for_event(&Some(tx), &payload), Some(payload));
    }

    #[test]
    fn run_exits_on_shutdown() {
        let shared = Arc::new(SharedState::new());
        let (tx, rx) = crossbeam_channel::bounded(1);
        let mut engine = HotLoop::new(shared, None, None);
        engine.set_control_rx(rx);

        // Send Shutdown before run() starts — run() should drain it and exit.
        tx.send(ControlCommand::Shutdown).unwrap();

        // run() should return (not hang).
        engine.run();
        assert!(!engine.is_running());
    }

    #[test]
    fn run_exits_on_channel_disconnect() {
        let shared = Arc::new(SharedState::new());
        let (tx, rx) = crossbeam_channel::bounded(1);
        let mut engine = HotLoop::new(shared, None, None);
        engine.set_control_rx(rx);

        // Drop sender — run() should detect disconnect and exit.
        drop(tx);

        engine.run();
        assert!(!engine.is_running());
    }

    #[test]
    fn push_hmds_unavailable_historical_emits_error_and_terminal_sentinel() {
        let shared = SharedState::new();
        push_hmds_unavailable(&shared, 7, true);

        let errors = shared.reference.drain_historical_errors();
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].0, 7);
        assert_eq!(errors[0].1, 162);
        assert!(errors[0].2.contains("not available"));

        let hist = shared.reference.drain_historical_data();
        assert_eq!(hist.len(), 1, "terminal sentinel required so historical_data_end fires");
        assert_eq!(hist[0].0, 7);
        assert!(hist[0].1.is_complete);
        assert!(hist[0].1.bars.is_empty());
    }

    // ibx#399: every CCP/farm reconnect waits 5 to 15 s, as the reference.
    #[test]
    fn reconnect_backoff_is_five_to_fifteen_seconds() {
        use std::time::Duration;
        let (mut lo, mut hi) = (Duration::MAX, Duration::ZERO);
        for _ in 0..2_000 {
            let d = reconnect_backoff();
            assert!(d >= Duration::from_secs(5) && d < Duration::from_secs(15), "got {:?}", d);
            lo = lo.min(d);
            hi = hi.max(d);
        }
        assert!(lo < Duration::from_secs(7) && hi > Duration::from_secs(13), "jittered over the range: {:?}..{:?}", lo, hi);
    }

    /// A link and its peer end; the link's output holds 256 KB, as a
    /// socket buffer, so a peer that does not read stalls it.
    fn loopback_conn() -> (Connection, crate::protocol::connection::MemTransport) {
        let (client, server) = crate::protocol::connection::mem_pair();
        client.set_write_capacity(Some(256 * 1024));
        (Connection::new_mem(client), server)
    }

    fn engine_with_links() -> (HotLoop, Arc<SharedState>, Vec<crate::protocol::connection::MemTransport>) {
        let shared = Arc::new(SharedState::new());
        let mut engine = HotLoop::new(shared.clone(), None, None);
        engine.set_reconnect_auth(reconnect_auth_with_host("gw.example"));
        engine.set_farm_name("usfarm".into());
        let (ccp, s1) = loopback_conn();
        let (farm, s2) = loopback_conn();
        let (hmds, s3) = loopback_conn();
        engine.ccp_conn = Some(ccp);
        engine.farm_conn = Some(farm);
        engine.hmds_conn = Some(hmds);
        (engine, shared, vec![s1, s2, s3])
    }

    // ibx#399: a lost link is told to the clients at once, once, and the
    // session stays open.
    #[test]
    fn lost_links_are_reported_at_once() {
        let (mut engine, shared, _servers) = engine_with_links();
        engine.report_link_changes();
        assert!(shared.drain_connection_notices().is_empty(), "first look: nothing to report");

        engine.ccp.disconnected = true;
        engine.farm.disconnected = true;
        engine.hmds.disconnected = true;
        engine.report_link_changes();
        assert_eq!(shared.drain_connection_notices(), vec![
            (1100, "Connectivity between client and server has been lost.".to_string()),
            (2103, "Market data farm connection is broken:usfarm".to_string()),
            (2105, "HMDS data farm connection is broken:ushmds".to_string()),
        ]);
        engine.report_link_changes();
        assert!(shared.drain_connection_notices().is_empty(), "reported once");
        assert!(!shared.take_connection_lost(), "the session is not closed");
    }

    /// Bytes the peer of a loopback link has received so far.
    fn received(server: &mut crate::protocol::connection::MemTransport) -> Vec<u8> {
        use std::io::Read;
        server.set_nonblocking(true).unwrap();
        let mut out = Vec::new();
        let mut buf = [0u8; 4096];
        loop {
            match server.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => out.extend_from_slice(&buf[..n]),
                Err(_) => break,
            }
        }
        out
    }

    fn contains(hay: &[u8], needle: &str) -> bool {
        hay.windows(needle.len()).any(|w| w == needle.as_bytes())
    }

    // ibx#419: a silent auth link is reset at the first check after the
    // pause of its last send (62 s), with nothing sent before; 1100 follows.
    #[test]
    fn silent_auth_link_is_reset_after_the_send_pause() {
        let (mut engine, shared, mut servers) = engine_with_links();
        let t0 = Instant::now();
        engine.hb = HeartbeatState::new_at(t0);
        engine.report_link_changes();
        for s in 1..=61 {
            engine.hb.last_farm_recv = t0 + Duration::from_secs(s);
            engine.hb.pending_farm_test = None;
            engine.hb.pending_hmds_test = None;
            engine.check_heartbeats_at(t0 + Duration::from_secs(s));
            assert!(!engine.ccp.disconnected, "alive at {} s", s);
        }
        assert!(received(&mut servers[0]).is_empty(), "nothing sent on the auth link during the pause");
        engine.check_heartbeats_at(t0 + Duration::from_secs(62));
        assert!(engine.ccp.disconnected, "reset at 62 s");
        engine.report_link_changes();
        let notices = shared.drain_connection_notices();
        assert_eq!(notices.iter().map(|n| n.0).collect::<Vec<_>>(), vec![1100]);
        assert!(!shared.take_connection_lost(), "the clients stay connected");
    }

    // ibx#419: a send on the auth link restarts the pause, so a link the
    // engine keeps using is not reset by the monitor.
    #[test]
    fn auth_link_in_use_is_not_checked() {
        let (mut engine, _shared, _servers) = engine_with_links();
        let t0 = Instant::now();
        engine.hb = HeartbeatState::new_at(t0);
        for s in 1..=200 {
            let now = t0 + Duration::from_secs(s);
            if s % 30 == 0 {
                engine.hb.last_ccp_sent = now;
            }
            engine.hb.pending_farm_test = None;
            engine.hb.pending_hmds_test = None;
            engine.check_heartbeats_at(now);
        }
        assert!(!engine.ccp.disconnected);
    }

    // ibx#419: farms get a test request every 60 s, outside the sequence count; one
    // with no answer within 10 s is reset: 2103 and 2105.
    #[test]
    fn farm_test_request_every_minute_and_reset_without_answer() {
        let (mut engine, shared, mut servers) = engine_with_links();
        let t0 = Instant::now();
        engine.hb = HeartbeatState::new_at(t0);
        engine.report_link_changes();
        // The auth link is in use all along: its monitor stays paused.
        let at = |engine: &mut HotLoop, s: u64| {
            engine.hb.last_ccp_sent = t0 + Duration::from_secs(s);
            engine.check_heartbeats_at(t0 + Duration::from_secs(s));
        };
        for s in 1..=59 {
            at(&mut engine, s);
        }
        assert!(received(&mut servers[1]).is_empty(), "no own heartbeat on the farm");
        at(&mut engine, 60);
        let sent = received(&mut servers[1]);
        assert!(contains(&sent, "35=1\x0134=000000\x01"), "{:?}", String::from_utf8_lossy(&sent));
        assert!(contains(&sent, "112=FixTestRequest"));
        assert!(contains(&received(&mut servers[2]), "112=FixTestRequest"), "historical farm too");
        // The market data farm answers, the historical farm does not.
        engine.hb.pending_farm_test = None;
        at(&mut engine, 69);
        assert!(!engine.hmds.disconnected);
        at(&mut engine, 70);
        assert!(engine.hmds.disconnected && engine.hmds_conn.is_none());
        assert!(!engine.farm.disconnected);
        engine.report_link_changes();
        assert_eq!(shared.drain_connection_notices().iter().map(|n| n.0).collect::<Vec<_>>(), vec![2105]);
        // Next minute: no answer from the market data farm either.
        at(&mut engine, 120);
        at(&mut engine, 130);
        assert!(engine.farm.disconnected);
        engine.report_link_changes();
        assert_eq!(shared.drain_connection_notices().iter().map(|n| n.0).collect::<Vec<_>>(), vec![2103]);
    }

    // ibx#254: a farm whose peer stops reading stalls only the farm: the
    // loop goes on and sends on the auth link. A write error drops the link
    // it happened on (1100 for the auth link), without a retry.
    #[test]
    fn a_stalled_farm_does_not_block_the_other_links() {
        let (mut engine, shared, mut servers) = engine_with_links();
        for conn in [&mut engine.farm_conn, &mut engine.ccp_conn, &mut engine.hmds_conn].into_iter().flatten() {
            conn.set_queued_writes(true);
        }
        engine.report_link_changes();
        let big = vec![b'x'; 64 * 1024];
        let farm = engine.farm_conn.as_mut().unwrap();
        let mut n = 0;
        while !farm.has_queued_output() {
            farm.send_raw(&big).unwrap();
            n += 1;
            assert!(n < 10_000, "the socket buffers never filled");
        }
        engine.check_writes();
        assert!(!engine.farm.disconnected, "a slow peer is not an error");
        assert!(engine.farm_conn.as_ref().unwrap().has_queued_output());

        engine.ccp_conn.as_mut().unwrap().send_raw(b"order").unwrap();
        assert!(contains(&received(&mut servers[0]), "order"), "the auth link is not blocked");

        let ccp = engine.ccp_conn.as_mut().unwrap();
        ccp.shutdown();
        assert!(ccp.send_raw(b"cancel").is_err());
        engine.check_writes();
        assert!(engine.ccp.disconnected);
        assert!(!engine.farm.disconnected);
        assert!(!shared.orders.open_orders_held());
        engine.report_link_changes();
        assert_eq!(shared.drain_connection_notices().iter().map(|n| n.0).collect::<Vec<_>>(), vec![1100]);
        assert!(shared.orders.open_orders_held(), "open-order requests wait from the 1100 (ibx#251)");
    }

    // ibx#251: from the logon, open-order requests wait for the end of its
    // order status replay, as the reference's start-up.
    #[test]
    fn logon_holds_open_order_requests_until_its_replay() {
        let shared = Arc::new(SharedState::new());
        let mut engine = HotLoop::new(shared.clone(), None, None);
        assert!(!shared.orders.open_orders_held());
        engine.await_login_replay();
        assert!(shared.orders.open_orders_held());
        assert!(engine.ccp.awaiting_login_replay);
    }

    // ibx#399: 1102 after the status replay end, at once with the farms up.
    #[test]
    fn restored_link_is_reported_after_the_status_replay() {
        let (mut engine, shared, _servers) = engine_with_links();
        engine.maybe_report_restored();
        assert!(shared.drain_connection_notices().is_empty(), "no reconnect, no message");

        engine.ccp.status_replay_end_at = Some(Instant::now());
        engine.maybe_report_restored();
        assert_eq!(shared.drain_connection_notices(), vec![(1102,
            "Connectivity between client and server has been restored - data maintained. All data farms are connected: usfarm; ushmds.".to_string())]);
        engine.maybe_report_restored();
        assert!(shared.drain_connection_notices().is_empty(), "reported once");
    }

    // With a farm still down the message waits for it, 30 s at most.
    #[test]
    fn restored_link_waits_for_the_farms() {
        let (mut engine, shared, _servers) = engine_with_links();
        engine.farm.disconnected = true;
        engine.ccp.status_replay_end_at = Some(Instant::now());
        engine.maybe_report_restored();
        assert!(shared.drain_connection_notices().is_empty(), "farm down: wait");

        engine.ccp.status_replay_end_at = Instant::now().checked_sub(RESTORE_FARM_WAIT);
        engine.maybe_report_restored();
        assert_eq!(shared.drain_connection_notices(), vec![(1102,
            "Connectivity between client and server has been restored - data maintained. The following farms are connected: ushmds. The following farms are not connected: usfarm.".to_string())]);
    }

    #[test]
    fn hmds_reconnect_backoff_matches_captured_cadence() {
        use std::time::Duration;
        // Captured cadence (ib-agent#153): 3.2 / 11.4 / 18.5 / 42.7 / 63.7 s.
        // Our schedule: 3 / 6 / 12 / 24 / 48 / 64 s — captures the doubling
        // shape and caps at the 64 s ceiling.
        assert_eq!(hmds_reconnect_backoff(1), Duration::from_secs(3));
        assert_eq!(hmds_reconnect_backoff(2), Duration::from_secs(6));
        assert_eq!(hmds_reconnect_backoff(3), Duration::from_secs(12));
        assert_eq!(hmds_reconnect_backoff(4), Duration::from_secs(24));
        assert_eq!(hmds_reconnect_backoff(5), Duration::from_secs(48));
        assert_eq!(hmds_reconnect_backoff(6), Duration::from_secs(64));
        // Cap holds for any further attempts.
        assert_eq!(hmds_reconnect_backoff(7), Duration::from_secs(64));
        assert_eq!(hmds_reconnect_backoff(100), Duration::from_secs(64));
        // Saturating math survives degenerate inputs.
        assert_eq!(hmds_reconnect_backoff(0), Duration::from_secs(3));
        assert_eq!(hmds_reconnect_backoff(u32::MAX), Duration::from_secs(64));
    }

    fn reconnect_auth_with_host(host: &str) -> ReconnectAuth {
        ReconnectAuth {
            host: host.into(),
            username: "user".into(),
            password: zeroize::Zeroizing::new("pass".into()),
            paper: true,
            session_key: num_bigint::BigUint::default(),
            session_token: num_bigint::BigUint::default(),
            server_session_id: String::new(),
            hw_info: String::new(),
            encoded: String::new(),
            hmds_host: "hmds.example".into(),
            hmds_farm: "ushmds".into(),
            farm_host: String::new(),
            farm_name: String::new(),
            session_epoch: String::new(),
            ns_secure_refused: false,
            use_ssl: true,
            ssl_farms: String::new(),
        }
    }

    // ibx#295: the farm reconnect goes to the session's farm (host and name
    // from the logon), not to the auth host under a fixed name.
    #[test]
    fn farm_reconnect_uses_the_session_farm() {
        let mut auth = reconnect_auth_with_host("gw.example");
        auth.farm_host = "zdc1.example".into();
        auth.farm_name = "eufarm".into();
        assert_eq!(farm_reconnect_target(&auth, "usfarm"), ("zdc1.example".to_string(), "eufarm".to_string()));
        // No farm parsed: the auth host and the login name, as at login.
        let auth = reconnect_auth_with_host("gw.example");
        assert_eq!(farm_reconnect_target(&auth, "usfarm.nj"), ("gw.example".to_string(), "usfarm.nj".to_string()));
    }

    // ibx#399: a farm that is up again gives the farm-OK notice with its
    // name; the first look reports nothing.
    #[test]
    fn restored_farms_are_reported_with_their_names() {
        let (mut engine, shared, _servers) = engine_with_links();
        engine.set_farm_name("eufarm".into());
        engine.report_link_changes();
        assert!(shared.drain_connection_notices().is_empty(), "first look: nothing to report");

        engine.farm.disconnected = true;
        engine.hmds.disconnected = true;
        engine.report_link_changes();
        let _ = shared.drain_connection_notices();

        engine.farm.disconnected = false;
        engine.hmds.disconnected = false;
        engine.report_link_changes();
        assert_eq!(shared.drain_connection_notices(), vec![
            (2104, "Market data farm connection is OK:eufarm".to_string()),
            (2106, "HMDS data farm connection is OK:ushmds".to_string()),
        ]);
        engine.report_link_changes();
        assert!(shared.drain_connection_notices().is_empty(), "reported once");

        // The auth link coming back gives no farm notice.
        engine.ccp.disconnected = true;
        engine.report_link_changes();
        let _ = shared.drain_connection_notices();
        engine.ccp.disconnected = false;
        engine.report_link_changes();
        assert!(shared.drain_connection_notices().is_empty(), "1102 comes after the replay, not here");
    }

    // ibx#399: with every transport down the loop spun at ~1M passes/s and
    // pinned a core for the whole outage. Parked, 60ms is ~60 passes.
    #[test]
    fn a_loop_with_every_transport_down_does_not_spin() {
        let shared = Arc::new(SharedState::new());
        let (tx, rx) = crossbeam_channel::bounded(1);
        let mut engine = HotLoop::new(shared, None, None);
        engine.set_control_rx(rx);
        engine.force_farm_disconnect();
        engine.ccp.disconnected = true;
        assert!(engine.all_transports_down());

        let handle = std::thread::spawn(move || { engine.run(); engine });
        std::thread::sleep(Duration::from_millis(60));
        tx.send(ControlCommand::Shutdown).unwrap();
        let engine = handle.join().unwrap();
        let passes = engine.context.loop_iterations;
        assert!(passes < 1_000, "loop spun {} times in 60ms while every transport was down", passes);
    }

    // ibx#399: the park applies only when nothing is up.
    #[test]
    fn a_loop_with_any_transport_up_is_not_parked() {
        let shared = Arc::new(SharedState::new());
        let mut engine = HotLoop::new(shared, None, None);
        assert!(!engine.all_transports_down(), "all up");
        engine.force_farm_disconnect();
        assert!(!engine.all_transports_down(), "auth still up");
        engine.farm.disconnected = false;
        engine.ccp.disconnected = true;
        assert!(!engine.all_transports_down(), "farm still up");
    }

    // ibx#399: a mid-session historical loss set the flag but kept the dead
    // socket, and the reconnect loop only runs with no socket held, so the
    // historical connection never came back.
    #[test]
    fn a_lost_hmds_socket_is_dropped_and_reconnect_is_scheduled() {
        let (client, server) = crate::protocol::connection::mem_pair();
        drop(server);

        let shared = Arc::new(SharedState::new());
        let mut engine = HotLoop::new(shared.clone(), None, None);
        engine.set_reconnect_auth(reconnect_auth_with_host("gw.example"));
        engine.hmds_conn = Some(Connection::new_mem(client));

        let deadline = Instant::now() + Duration::from_secs(2);
        while !engine.hmds.disconnected && Instant::now() < deadline {
            engine.hmds.poll(&mut engine.hmds_conn, &shared, &None, &mut engine.hb);
        }
        assert!(engine.hmds.disconnected, "peer close must be detected");
        assert!(engine.hmds_conn.is_none(), "dead socket must be dropped");

        engine.maybe_spawn_hmds_reconnect();
        assert!(engine.hmds_next_attempt_at.is_some(), "reconnect must be scheduled");
    }

    /// A keyed connection and its server side, plus a frame the server
    /// signed whose signature value was then changed.
    fn conn_with_bad_signed_frame() -> (Connection, crate::protocol::connection::MemTransport, Vec<u8>) {
        let (client, server) = socket_pair();
        let mac_key: Vec<u8> = (0..20).collect();
        let iv: Vec<u8> = (0..16).collect();
        let mut conn = Connection::new_mem(client);
        conn.set_keys(Vec::new(), Vec::new(), mac_key.clone(), iv.clone());
        let (mut signed, _) = fix::fix_sign(&fix::fix_build(&[(35, "0")], 1), &mac_key, &iv);
        let pos = signed.windows(5).position(|w| w == b"8349=").unwrap() + 5;
        signed[pos] = if signed[pos] == b'0' { b'1' } else { b'0' };
        (conn, server, signed)
    }

    // ibx#275: a signature mismatch on the market-data farm drops the
    // connection and schedules the normal reconnect.
    #[test]
    fn a_farm_signature_mismatch_drops_the_connection() {
        use std::io::{Read, Write};
        let shared = Arc::new(SharedState::new());
        let mut engine = HotLoop::new(shared, None, None);
        engine.set_reconnect_auth(reconnect_auth_with_host("gw.example"));
        let (conn, mut server, bad) = conn_with_bad_signed_frame();
        engine.farm_conn = Some(conn);
        server.write_all(&bad).unwrap();

        let deadline = Instant::now() + Duration::from_secs(2);
        while !engine.farm.disconnected && Instant::now() < deadline {
            engine.poll_farm_for_test();
        }
        assert!(engine.farm.disconnected, "mismatch must drop the farm");
        // The socket is closed: the server side reads end of stream.
        server.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        let mut buf = [0u8; 16];
        assert_eq!(server.read(&mut buf).unwrap_or(0), 0, "socket closed");
        engine.maybe_spawn_farm_reconnect();
        assert!(engine.farm_next_attempt_at.is_some(), "reconnect scheduled");
    }

    // ibx#275: same on the historical connection: the socket is dropped and
    // the reconnect loop re-dials it.
    #[test]
    fn an_hmds_signature_mismatch_drops_the_connection() {
        use std::io::Write;
        let shared = Arc::new(SharedState::new());
        let mut engine = HotLoop::new(shared.clone(), None, None);
        engine.set_reconnect_auth(reconnect_auth_with_host("gw.example"));
        let (conn, mut server, bad) = conn_with_bad_signed_frame();
        engine.hmds_conn = Some(conn);
        server.write_all(&bad).unwrap();

        let deadline = Instant::now() + Duration::from_secs(2);
        while !engine.hmds.disconnected && Instant::now() < deadline {
            engine.hmds.poll(&mut engine.hmds_conn, &shared, &None, &mut engine.hb);
        }
        assert!(engine.hmds.disconnected, "mismatch must drop the historical connection");
        assert!(engine.hmds_conn.is_none(), "socket dropped");
        engine.maybe_spawn_hmds_reconnect();
        assert!(engine.hmds_next_attempt_at.is_some(), "reconnect scheduled");
    }

    fn socket_pair() -> (crate::protocol::connection::MemTransport, crate::protocol::connection::MemTransport) {
        let (client, server) = crate::protocol::connection::mem_pair();
        (client, server)
    }

    /// Every compressed message the engine wrote to `server`, as inner text.
    fn farm_messages_sent(server: &mut crate::protocol::connection::MemTransport) -> Vec<String> {
        use std::io::Read;
        server.set_read_timeout(Some(Duration::from_millis(300))).unwrap();
        let mut buf = Vec::new();
        let mut chunk = [0u8; 8192];
        while let Ok(n) = server.read(&mut chunk) {
            if n == 0 { break; }
            buf.extend_from_slice(&chunk[..n]);
        }
        let mut out = Vec::new();
        let mut rest = &buf[..];
        while let Some(len) = crate::protocol::fixcomp::fixcomp_length(rest) {
            for m in crate::protocol::fixcomp::fixcomp_decompress(&rest[..len]).unwrap() {
                out.push(String::from_utf8_lossy(&m).replace('\x01', "|"));
            }
            rest = &rest[len..];
        }
        out
    }

    /// Every plain message the engine wrote to `server`, `|` separated.
    fn plain_messages_sent(server: &mut crate::protocol::connection::MemTransport) -> Vec<String> {
        use std::io::Read;
        server.set_read_timeout(Some(Duration::from_millis(300))).unwrap();
        let mut buf = Vec::new();
        let mut chunk = [0u8; 8192];
        while let Ok(n) = server.read(&mut chunk) {
            if n == 0 { break; }
            buf.extend_from_slice(&chunk[..n]);
        }
        String::from_utf8_lossy(&buf).replace('\x01', "|")
            .split("8=FIX").filter(|m| !m.is_empty()).map(|m| format!("8=FIX{m}")).collect()
    }

    /// Inbound historical-data messages of a recorded reference scenario,
    /// decompressed, in recorded order.
    fn fixture_hmds_messages(scenario: &str) -> Vec<Vec<u8>> {
        use base64::Engine as _;
        let path = format!("{}/tests/fixtures/gw1040/scenarios/{}", env!("CARGO_MANIFEST_DIR"), scenario);
        let text = std::fs::read_to_string(&path).unwrap();
        let mut out = Vec::new();
        for line in text.lines().skip(1) {
            let rec: serde_json::Value = serde_json::from_str(line).unwrap();
            if rec["leg"] != "fix_in" || rec["conn"] != "ushmds" { continue; }
            let raw = base64::engine::general_purpose::STANDARD.decode(rec["raw_b64"].as_str().unwrap()).unwrap();
            if raw.starts_with(b"8=FIXCOMP") {
                out.extend(crate::protocol::fixcomp::fixcomp_decompress(&raw).unwrap());
            }
        }
        out
    }

    fn scanner_engine(shared: &Arc<SharedState>) -> (HotLoop, crate::protocol::connection::MemTransport, Sender<ControlCommand>) {
        let mut engine = HotLoop::new(shared.clone(), None, None);
        let (c, server) = socket_pair();
        engine.hmds_conn = Some(Connection::new_mem(c));
        let (tx, rx) = crossbeam_channel::unbounded();
        engine.set_control_rx(rx);
        (engine, server, tx)
    }

    fn scanner_cmd(req_id: ReqId, client_id: i64, scan_code: &str) -> ControlCommand {
        ControlCommand::SubscribeScanner {
            req_id, client_id,
            subscription: crate::control::scanner::ScannerSubscription {
                instrument: "STK".into(), location_code: "STK.US.MAJOR".into(),
                scan_code: scan_code.into(), number_of_rows: 10, filters: Vec::new(),
            },
        }
    }

    fn load_scanner_params(engine: &mut HotLoop, server: &mut crate::protocol::connection::MemTransport, xml: &str) {
        load_scanner_params_keep(engine, xml);
        let _ = plain_messages_sent(server);
    }

    fn load_scanner_params_keep(engine: &mut HotLoop, xml: &str) {
        let reply = crate::protocol::fix::fix_build(&[(35, "U"), (6040, "10002"), (6118, xml)], 1);
        engine.inject_hmds_message(&reply);
    }

    fn errors(shared: &SharedState) -> Vec<String> {
        shared.reference.drain_historical_errors().into_iter()
            .map(|(r, c, m)| format!("{r}:{c}:{m}")).collect()
    }

    // ibx#457 / ibx#456 (reference scenario scanner_two, 26/09/2026): two
    // scanners open at once; the subscriptions wait for the scanner
    // parameters, then go out with the asked rows; each result goes to
    // the subscription its id names, the ids are client id and request
    // id; each cancel gives the local 162 then its desubscribe; an
    // unknown cancel gives 365 only.
    #[test]
    fn two_scanners_get_their_own_results() {
        let shared = Arc::new(SharedState::new());
        let (mut engine, mut server, tx) = scanner_engine(&shared);
        tx.send(scanner_cmd(9005, 198, "TOP_PERC_GAIN")).unwrap();
        tx.send(scanner_cmd(9006, 198, "MOST_ACTIVE")).unwrap();
        engine.poll_control_commands();
        let sent = plain_messages_sent(&mut server);
        assert_eq!(sent.len(), 1, "{sent:?}");
        assert!(sent[0].contains("6040=10001|"), "parameters first: {sent:?}");

        for msg in fixture_hmds_messages("20260926b/scanner_two.jsonl") {
            engine.inject_hmds_message(&msg);
        }
        let sent = plain_messages_sent(&mut server);
        assert_eq!(sent.len(), 2, "{sent:?}");
        for (m, id, code) in [(&sent[0], "APISCAN198:9005", "TOP_PERC_GAIN"), (&sent[1], "APISCAN198:9006", "MOST_ACTIVE")] {
            assert!(m.contains(&format!("<id>{id}</id>")) && m.contains(&format!("<scanCode>{code}</scanCode>")), "{m}");
            assert!(m.contains("<maxItems>10</maxItems><suspend>no</suspend>"), "as the reference: {m}");
        }
        let got: Vec<(ReqId, i64, usize)> = engine.hmds.cold_scanner_results.iter()
            .map(|(r, res)| (*r, res.con_ids[0], res.con_ids.len())).collect();
        assert_eq!(got, [(9006, 911617323, 10), (9005, 909360667, 10)]);

        for req_id in [9005, 9006, 99] {
            tx.send(ControlCommand::CancelScanner { req_id }).unwrap();
        }
        engine.poll_control_commands();
        assert_eq!(errors(&shared), [
            "9005:162:Historical Market Data Service error message:API scanner subscription cancelled: 9005",
            "9006:162:Historical Market Data Service error message:API scanner subscription cancelled: 9006",
            "99:365:No scanner subscription found for ticker id:99",
        ]);
        let sent = plain_messages_sent(&mut server);
        assert_eq!(sent.len(), 2, "{sent:?}");
        assert!(sent[0].contains("6040=10004|") && sent[0].contains("<id>APISCAN198:9005</id>"));
        assert!(sent[1].contains("<id>APISCAN198:9006</id>"));
        assert!(engine.hmds.cold_scanner_results.is_empty());
        assert!(engine.hmds.pending_scanner.is_empty());
    }

    // ibx#457: a result whose id is not live is dropped; a refusal text
    // ends the subscription with 162; a warning text gives 165 and the rows.
    #[test]
    fn scanner_result_texts() {
        let shared = Arc::new(SharedState::new());
        let (mut engine, _server, tx) = scanner_engine(&shared);
        tx.send(scanner_cmd(1, 7, "TOP_PERC_GAIN")).unwrap();
        tx.send(scanner_cmd(2, 7, "MOST_ACTIVE")).unwrap();
        engine.poll_control_commands();
        let result = |xml: &str| crate::protocol::fix::fix_build(&[(35, "U"), (6040, "10005"), (6118, xml)], 1);
        engine.inject_hmds_message(&result("<ScanResponse><id>APISCAN3:1</id><Contracts>\
            <Contract><contractID>1</contractID></Contract></Contracts></ScanResponse>"));
        engine.inject_hmds_message(&result("<ScanResponse><id>APISCAN7:1</id>\
            <errorText>Scanner type with code X is disabled</errorText></ScanResponse>"));
        engine.inject_hmds_message(&result("<ScanResponse><id>APISCAN7:2</id><warningText>delayed</warningText>\
            <Contracts><Contract><contractID>2</contractID></Contract></Contracts></ScanResponse>"));
        assert_eq!(errors(&shared), [
            "1:162:Historical Market Data Service error message:Scanner type with code X is disabled",
            "2:165:Historical Market Data Service query message:delayed",
        ]);
        let rows: Vec<ReqId> = engine.hmds.cold_scanner_results.iter().map(|(r, _)| *r).collect();
        assert_eq!(rows, [2]);
        assert_eq!(engine.hmds.pending_scanner.iter().map(|s| s.req_id).collect::<Vec<_>>(), [2]);
    }

    // ibx#457: a tenth of the ticker limit runs at once, and a live
    // request id is refused, both with 322 and nothing sent.
    #[test]
    fn scanner_limit_and_duplicate_id() {
        let shared = Arc::new(SharedState::new());
        let (mut engine, mut server, tx) = scanner_engine(&shared);
        engine.hmds.max_real_time_requests = 20;
        load_scanner_params(&mut engine, &mut server, "<ScanParameterResponse/>");
        tx.send(scanner_cmd(1, 0, "A")).unwrap();
        tx.send(scanner_cmd(1, 0, "A")).unwrap();
        tx.send(scanner_cmd(2, 0, "B")).unwrap();
        tx.send(scanner_cmd(3, 0, "C")).unwrap();
        engine.poll_control_commands();
        assert_eq!(errors(&shared), [
            "1:322:Error processing request.-'co' : cause - Duplicate ticker ID for API scanner subscription",
            "3:322:Error processing request.-'co' : cause - Only 2 simultaneous API scanner subscriptions are allowed.",
        ]);
        assert_eq!(plain_messages_sent(&mut server).len(), 2);
    }

    // ibx#457: the scanner parameters are asked once per connection; the
    // requests waiting for them all get the answer, later ones are served
    // from the cache; a lost link clears it.
    #[test]
    fn scanner_parameters_are_cached() {
        let shared = Arc::new(SharedState::new());
        let (mut engine, mut server, tx) = scanner_engine(&shared);
        tx.send(ControlCommand::FetchScannerParams).unwrap();
        tx.send(ControlCommand::FetchScannerParams).unwrap();
        engine.poll_control_commands();
        let sent = plain_messages_sent(&mut server);
        assert_eq!(sent.iter().filter(|m| m.contains("6040=10001|")).count(), 1, "{sent:?}");
        let reply = crate::protocol::fix::fix_build(&[(35, "U"), (6040, "10002"), (6118, "<ScanParameterResponse/>")], 1);
        engine.inject_hmds_message(&reply);
        assert_eq!(shared.reference.drain_scanner_params().len(), 2);
        tx.send(ControlCommand::FetchScannerParams).unwrap();
        engine.poll_control_commands();
        assert_eq!(shared.reference.drain_scanner_params(), ["<ScanParameterResponse/>"]);
        assert!(plain_messages_sent(&mut server).is_empty(), "answered from the cache");
        engine.hmds.scanner_link_lost(&shared);
        tx.send(ControlCommand::FetchScannerParams).unwrap();
        engine.poll_control_commands();
        assert!(shared.reference.drain_scanner_params().is_empty());
        assert_eq!(plain_messages_sent(&mut server).len(), 1, "asked again");
    }

    // ibx#456: the row count follows the scan type's limit in the
    // parameters, the rows are cut to the asked count, and a cancel
    // before the parameters arrive sends nothing.
    #[test]
    fn scanner_rows_follow_the_scan_type_limit() {
        let shared = Arc::new(SharedState::new());
        let (mut engine, mut server, tx) = scanner_engine(&shared);
        engine.hmds.max_real_time_requests = 100;
        let cmd = |req_id: ReqId, code: &str, rows: i32| ControlCommand::SubscribeScanner {
            req_id, client_id: 5,
            subscription: crate::control::scanner::ScannerSubscription {
                instrument: "STK".into(), location_code: "STK.US.MAJOR".into(),
                scan_code: code.into(), number_of_rows: rows, filters: Vec::new(),
            },
        };
        tx.send(cmd(1, "HIGH_DIVIDEND_YIELD_IB", 100)).unwrap();
        tx.send(cmd(2, "TOP_PERC_GAIN", 100)).unwrap();
        tx.send(cmd(3, "TOP_PERC_GAIN", -1)).unwrap();
        tx.send(cmd(4, "TOP_PERC_GAIN", 3)).unwrap();
        tx.send(cmd(5, "TOP_PERC_GAIN", 3)).unwrap();
        tx.send(ControlCommand::CancelScanner { req_id: 5 }).unwrap();
        engine.poll_control_commands();
        assert_eq!(plain_messages_sent(&mut server).len(), 1, "parameters request only");
        assert_eq!(errors(&shared), ["5:162:Historical Market Data Service error message:API scanner subscription cancelled: 5"]);
        load_scanner_params_keep(&mut engine, "<ScanParameterResponse><ScanTypeList><ScanType>\
            <scanCode>HIGH_DIVIDEND_YIELD_IB</scanCode><respSizeLimit>750</respSizeLimit></ScanType>\
            </ScanTypeList></ScanParameterResponse>");
        let sent = plain_messages_sent(&mut server);
        let items: Vec<&str> = sent.iter().map(|m| {
            let a = m.find("<maxItems>").unwrap() + 10;
            &m[a..a + m[a..].find('<').unwrap()]
        }).collect();
        assert_eq!(items, ["100", "50", "50", "3"]);

        let rows: String = (1..=5).map(|c| format!("<Contract><contractID>{c}</contractID></Contract>")).collect();
        let xml = format!("<ScanResponse><id>APISCAN5:4</id><Contracts>{rows}</Contracts></ScanResponse>");
        engine.inject_hmds_message(&crate::protocol::fix::fix_build(&[(35, "U"), (6040, "10005"), (6118, &xml)], 1));
        let (_, result) = &engine.hmds.cold_scanner_results[0];
        assert_eq!(result.con_ids, [1, 2, 3]);
        assert_eq!(result.entries.len(), 3);
    }

    // ibx#457: a lost and restored link is told to every live scanner
    // with 165, and the subscription is sent again.
    #[test]
    fn scanner_link_notices_and_resubscribe() {
        let shared = Arc::new(SharedState::new());
        let (mut engine, mut server, tx) = scanner_engine(&shared);
        load_scanner_params(&mut engine, &mut server, "<ScanParameterResponse/>");
        tx.send(scanner_cmd(4, 1, "TOP_PERC_GAIN")).unwrap();
        engine.poll_control_commands();
        assert_eq!(plain_messages_sent(&mut server).len(), 1);
        engine.hmds.scanner_link_lost(&shared);
        engine.hmds.scanner_connect_failed(&shared);
        engine.hmds.scanner_link_restored(&mut engine.hmds_conn, &mut engine.hb, &shared);
        assert_eq!(errors(&shared), [
            "4:165:Historical Market Data Service query message:HMDS server disconnect occurred.  Attempting reconnection...",
            "4:165:Historical Market Data Service query message:HMDS connection attempt failed.  Connection will be re-attempted...",
            "4:165:Historical Market Data Service query message:HMDS server connection was successful.",
        ]);
        let sent = plain_messages_sent(&mut server);
        assert_eq!(sent.len(), 1);
        assert!(sent[0].contains("<id>APISCAN1:4</id>"));
    }

    // ibx#461: a bulletin keeps the server's message id and gets the
    // reference's client type; an empty message (the logon one), a type
    // the client never gets and a repeated id are dropped.
    #[test]
    fn news_bulletin_ids_and_types() {
        let shared = Arc::new(SharedState::new());
        let mut engine = HotLoop::new(shared.clone(), None, None);
        let bulletin = |t: &str, text: &str, id: &str| {
            let mut f = vec![(35u32, "B"), (61, t), (148, text), (207, "NYSE")];
            if !id.is_empty() { f.push((6143, id)); }
            crate::protocol::fix::fix_build(&f, 1)
        };
        engine.inject_ccp_message(&bulletin("-2", "", "0"));
        engine.inject_ccp_message(&bulletin("3", "Exchange back", "123"));
        engine.inject_ccp_message(&bulletin("2", "Exchange down", "124"));
        engine.inject_ccp_message(&bulletin("2", "Exchange down", "124"));
        engine.inject_ccp_message(&bulletin("8", "<b>html</b>", "125"));
        engine.inject_ccp_message(&bulletin("9", "popup", "126"));
        engine.inject_ccp_message(&bulletin("10", "popup html", "127"));
        engine.inject_ccp_message(&bulletin("1", "regular", "128"));
        engine.inject_ccp_message(&bulletin("0", "ad", "129"));
        engine.inject_ccp_message(&bulletin("99", "unknown", "130"));
        engine.inject_ccp_message(&bulletin("1", "no id", ""));
        let got: Vec<(i32, i32)> = shared.market.drain_news_bulletins().iter().map(|b| (b.msg_id, b.msg_type)).collect();
        assert_eq!(got, [(123, 2), (124, 3), (125, 4), (126, 5), (127, 6), (128, 1), (0, 1)]);
    }

    /// A news reply frame with `id`, status line and properties.
    fn news_reply(id: &str, status: &str, props: &str) -> Vec<u8> {
        let mut zip = Vec::new();
        zip.extend_from_slice(b"PK\x03\x04");
        zip.extend_from_slice(&[20, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
        zip.extend_from_slice(&(props.len() as u32).to_le_bytes());
        zip.extend_from_slice(&(props.len() as u32).to_le_bytes());
        zip.extend_from_slice(&5u16.to_le_bytes());
        zip.extend_from_slice(&0u16.to_le_bytes());
        zip.extend_from_slice(b"ENTRY");
        zip.extend_from_slice(props.as_bytes());
        // Codec header with no replaced byte.
        let mut payload = format!("{status}\n").into_bytes();
        payload.extend_from_slice(&[0u8; 8]);
        payload.extend_from_slice(&zip);
        let xml = format!("<NewsResponse><id>{id}</id></NewsResponse>");
        let mut msg = crate::protocol::fix::fix_build(&[(35, "U"), (6040, "10032"), (6118, &xml)], 1);
        msg.truncate(msg.len() - 7); // drop the checksum field
        msg.extend_from_slice(format!("95={}\x0196=", payload.len()).as_bytes());
        msg.extend_from_slice(&payload);
        msg.extend_from_slice(b"\x0110=000\x01");
        msg
    }

    // ibx#459: news replies are matched by query id, not in send order;
    // equal requests share one query and both get the reply; a failed
    // reply is 10173 / 10172 with no end.
    #[test]
    fn news_replies_matched_by_query_id() {
        let shared = Arc::new(SharedState::new());
        shared.reference.set_news_sources(vec!["BRFG".into(), "DJ-N".into()]);
        let (mut engine, mut server, tx) = scanner_engine(&shared);
        for (req_id, providers) in [(1, "BRFG"), (2, "DJ-N"), (3, "BRFG")] {
            tx.send(ControlCommand::FetchHistoricalNews {
                req_id, con_id: 265598, provider_codes: providers.into(),
                start_time: String::new(), end_time: String::new(), max_results: 5,
            }).unwrap();
        }
        tx.send(ControlCommand::FetchNewsArticle { req_id: 4, provider_code: "BRFG".into(), article_id: "A1".into() }).unwrap();
        engine.poll_control_commands();
        let sent = plain_messages_sent(&mut server);
        assert_eq!(sent.len(), 3, "the equal request shares the query: {sent:?}");
        assert!(sent[0].contains("<id>1-history;;NewsQuery;;0;;true;;0;;U</id>"), "{}", sent[0]);
        assert!(sent[1].contains("<id>2-history;"), "{}", sent[1]);
        assert!(sent[2].contains("<id>4-article_file;"), "{}", sent[2]);

        engine.inject_hmds_message(&news_reply("4-article_file;;NewsQuery;;0;;true;;0;;U", "200", "error_code=Not available\n"));
        engine.inject_hmds_message(&news_reply("2-history;;NewsQuery;;0;;true;;0;;U", "200",
            "h\\:0=Dow|2026-03-03 14:02:00.0|DJ-N$1|0|1|DJ-N|265598\nhas_more=1\n"));
        engine.inject_hmds_message(&news_reply("1-history;;NewsQuery;;0;;true;;0;;U", "500", ""));
        let news: Vec<(ReqId, usize, bool)> = shared.reference.drain_historical_news().into_iter()
            .map(|(r, h, more)| (r, h.len(), more)).collect();
        assert_eq!(news, [(2, 1, true)]);
        assert_eq!(errors(&shared), [
            "4:10172:Failed to request news article:Not available",
            "1:10173:Failed to request historical news:Request ignored",
            "3:10173:Failed to request historical news:Request ignored",
        ]);
        assert!(engine.hmds.pending_news.is_empty() && engine.hmds.pending_articles.is_empty());
    }

    // A scanner and a fundamentals request in flight at once, replies
    // interleaved: each reply goes to its own request, and the scanner
    // cancel acknowledgement carries the scanner's request id only.
    #[test]
    fn scanner_and_fundamentals_replies_interleaved() {
        let shared = Arc::new(SharedState::new());
        let (mut engine, mut server, tx) = scanner_engine(&shared);
        load_scanner_params(&mut engine, &mut server, "<ScanParameterResponse/>");
        tx.send(scanner_cmd(460, 0, "TOP_PERC_GAIN")).unwrap();
        tx.send(ControlCommand::FetchFundamentalData { req_id: 470, con_id: 265598, report_type: "ReportSnapshot".into() }).unwrap();
        engine.poll_control_commands();
        let sent = plain_messages_sent(&mut server);
        let fund = sent.iter().find(|m| m.contains("6040=10010|")).expect("fundamentals request sent");
        let a = fund.find("<id>").unwrap() + 4;
        let fund_id = &fund[a..a + fund[a..].find("</id>").unwrap()];

        let scan = |rows: &str| crate::protocol::fix::fix_build(&[(35, "U"), (6040, "10005"),
            (6118, &format!("<ScanResponse><id>APISCAN0:460</id><Contracts>{rows}</Contracts></ScanResponse>"))], 1);
        let fund_window = fund_id.split(";;").next().unwrap();
        let fund_reply = super::hmds::tests::fund_reply(fund_window, "", b"<Snapshot/>");
        engine.inject_hmds_message(&scan("<Contract><contractID>1</contractID></Contract>"));
        engine.inject_hmds_message(&fund_reply);
        engine.inject_hmds_message(&scan("<Contract><contractID>2</contractID></Contract>"));
        let rows: Vec<(ReqId, i64)> = engine.hmds.cold_scanner_results.iter().map(|(r, res)| (*r, res.con_ids[0])).collect();
        assert_eq!(rows, [(460, 1), (460, 2)]);
        assert!(errors(&shared).is_empty());
        tx.send(ControlCommand::CancelScanner { req_id: 460 }).unwrap();
        engine.poll_control_commands();

        let fund: Vec<ReqId> = shared.reference.drain_fundamental_data().into_iter().map(|(r, _)| r).collect();
        assert_eq!(fund, [470]);
        assert!(engine.hmds.pending_fundamental.is_empty());
        let rows: Vec<ReqId> = engine.hmds.cold_scanner_results.iter().map(|(r, _)| *r).collect();
        assert!(rows.is_empty(), "the cancel drops the parked rows: {rows:?}");
        assert_eq!(errors(&shared), ["460:162:Historical Market Data Service error message:API scanner subscription cancelled: 460"]);
    }

    fn subscribe_cmd(con_id: i64, sec_type: &str) -> ControlCommand {
        ControlCommand::Subscribe {
            con_id, symbol: String::new(), exchange: "SMART".into(), sec_type: sec_type.into(),
            last_trade_date: String::new(), strike: 0.0, right: String::new(), multiplier: String::new(),
            mode_9887: 0, snapshot: false, reply_tx: None,
        }
    }

    // ibx#287: with the lots scaling on, a stock subscription waits for the
    // contract's definition and goes out once the round lot is known; a
    // second subscription of the contract uses the kept lot at once.
    #[test]
    fn stock_subscription_waits_for_its_round_lot() {
        let shared = Arc::new(SharedState::new());
        let mut engine = HotLoop::new(shared.clone(), None, None);
        let (c1, mut farm_side) = socket_pair();
        let (c2, mut ccp_side) = socket_pair();
        engine.farm_conn = Some(Connection::new_mem(c1));
        engine.ccp_conn = Some(Connection::new_mem(c2));
        engine.set_scale_us_lots(true);
        let (tx, rx) = crossbeam_channel::bounded(8);
        engine.set_control_rx(rx);

        tx.send(subscribe_cmd(265598, "STK")).unwrap();
        engine.poll_once();
        let asked = plain_messages_sent(&mut ccp_side);
        assert_eq!(asked.len(), 1, "{asked:?}");
        assert!(asked[0].contains("|35=c|") && asked[0].contains("|6008=265598|") && asked[0].contains("|6004=BEST|"), "{}", asked[0]);
        let req_id = asked[0].split('|').find_map(|p| p.strip_prefix("320=")).unwrap().to_string();
        assert!(farm_messages_sent(&mut farm_side).is_empty(), "nothing subscribed before the lot is known");

        let reply = fix::fix_build(&[(35, "d"), (320, &req_id), (6008, "265598"), (167, "CS"),
            (6523, "USSTK"), (6030, "1"), (6023, "40"), (6027, "40")], 1);
        assert!(farm::round_lot_reply(&mut engine.context, &req_id, &reply));
        engine.send_lot_ready();
        let sent = farm_messages_sent(&mut farm_side);
        assert_eq!(sent.len(), 1, "{sent:?}");
        assert!(sent[0].contains("6008=265598|"), "{}", sent[0]);
        let id = engine.context.market.instrument_by_con_id(265598).unwrap();
        assert_eq!(engine.context.market.round_lot(id), 40);

        // Cancelled and asked again: the lot is kept, no second lookup.
        tx.send(ControlCommand::Unsubscribe { instrument: id }).unwrap();
        tx.send(subscribe_cmd(265598, "STK")).unwrap();
        engine.poll_once();
        assert!(plain_messages_sent(&mut ccp_side).is_empty());
        let id = engine.context.market.instrument_by_con_id(265598).unwrap();
        assert_eq!(engine.context.market.round_lot(id), 40);
        let sent = farm_messages_sent(&mut farm_side);
        assert_eq!(sent.len(), 2, "one cancel with both entries, then the new subscription: {sent:?}");
        assert!(sent[0].contains("|263=2|146=2|"), "{}", sent[0]);
        assert!(sent[1].contains("6008=265598|"), "{}", sent[1]);
    }

    // ibx#278 (captured E6): two requests with no conId get their own
    // slots, a lookup each, then a subscription by the conId found; both
    // run side by side. A lookup that finds several contracts ends its
    // request with error 200 and sends nothing.
    #[test]
    fn a_request_without_con_id_is_resolved_first() {
        use crate::control::contracts::tests::pipe_msg;
        let shared = Arc::new(SharedState::new());
        let mut engine = HotLoop::new(shared.clone(), None, None);
        let (c1, mut farm_side) = socket_pair();
        let (c2, mut ccp_side) = socket_pair();
        engine.farm_conn = Some(Connection::new_mem(c1));
        engine.ccp_conn = Some(Connection::new_mem(c2));
        let (tx, rx) = crossbeam_channel::bounded(8);
        engine.set_control_rx(rx);
        let by_symbol = |symbol: &str, sec_type: &str, reply: crossbeam_channel::Sender<Result<InstrumentId, String>>| {
            ControlCommand::SubscribeBySymbol {
                symbol: symbol.into(), sec_type: sec_type.into(), exchange: "SMART".into(), currency: "USD".into(),
                filters: Default::default(), mode_9887: 0, snapshot: false, reply_tx: Some(reply),
            }
        };
        let (r1, a1) = crossbeam_channel::bounded(1);
        let (r2, a2) = crossbeam_channel::bounded(1);
        let (r3, a3) = crossbeam_channel::bounded(1);
        tx.send(by_symbol("QQQ", "STK", r1)).unwrap();
        tx.send(by_symbol("SPY", "STK", r2)).unwrap();
        tx.send(by_symbol("MNQ", "FUT", r3)).unwrap();
        engine.poll_once();
        let (qqq, spy, mnq) = (a1.recv().unwrap().unwrap(), a2.recv().unwrap().unwrap(), a3.recv().unwrap().unwrap());
        assert!(qqq != spy && spy != mnq && qqq != mnq, "a slot each");
        let asked = plain_messages_sent(&mut ccp_side);
        assert_eq!(asked.len(), 3, "{asked:?}");
        assert!(asked[0].contains("|35=c|") && asked[0].contains("|321=2|6088=Socket|55=QQQ|167=CS|100=BEST|15=USD|"), "{}", asked[0]);
        assert!(farm_messages_sent(&mut farm_side).is_empty(), "nothing subscribed before the conId is known");
        let ids: Vec<String> = asked.iter()
            .map(|m| m.split('|').find_map(|p| p.strip_prefix("320=")).unwrap().to_string()).collect();

        let reply = |id: &str, body: &str| pipe_msg(&format!("35=d|43=N|320={id}|322=*|323=4|{body}"));
        let mut context = std::mem::replace(&mut engine.context, Context::new());
        assert!(farm::md_contract_reply(&mut context, &shared, &ids[0],
            &reply(&ids[0], "55=QQQ|167=STK|207=BEST|6008=320227571|15=USD|55=QQQ|167=STK|207=NASDAQ|6008=320227571|15=USD|")));
        assert!(farm::md_contract_reply(&mut context, &shared, &ids[1],
            &reply(&ids[1], "55=SPY|167=STK|207=BEST|6008=756733|15=USD|")));
        assert!(farm::md_contract_reply(&mut context, &shared, &ids[2],
            &reply(&ids[2], "55=MNQ|167=FUT|207=CME|6008=815824267|55=MNQ|167=FUT|207=CME|6008=840227399|")));
        engine.context = context;
        engine.send_md_resolved();

        let sent = farm_messages_sent(&mut farm_side);
        assert_eq!(sent.len(), 2, "{sent:?}");
        assert!(sent[0].contains("6008=320227571|") && sent[1].contains("6008=756733|"), "{sent:?}");
        assert!(!sent.iter().any(|m| m.contains("|55=")), "never a descriptive request: {sent:?}");
        assert_eq!(engine.context.market.instrument_by_con_id(320227571), Some(qqq));
        assert_eq!(engine.context.market.instrument_by_con_id(756733), Some(spy));
        assert_eq!(shared.market.drain_md_rejects(), [crate::bridge::MdReject::NoSecurityDefinition { instrument: mnq }]);

        // The client cancels the refused request: its slot is freed.
        tx.send(ControlCommand::Unsubscribe { instrument: mnq }).unwrap();
        engine.poll_once();
        assert_eq!(engine.context.market.con_id(mnq), None);
        assert_eq!(engine.context.market.con_id(qqq), Some(320227571));
    }

    // ibx#455: the query names the type as the API does, asks for past
    // ticks only when some are wanted, and a server refusal of it reaches
    // the client with the server's text.
    #[test]
    fn tick_by_tick_query_and_server_refusal() {
        let shared = Arc::new(SharedState::new());
        let mut engine = HotLoop::new(shared.clone(), None, None);
        let (c1, mut hmds_side) = socket_pair();
        engine.hmds_conn = Some(Connection::new_mem(c1));
        let (tx, rx) = crossbeam_channel::bounded(8);
        engine.set_control_rx(rx);
        let tbt = |req_id, tbt_type, number_of_ticks| ControlCommand::SubscribeTbt {
            req_id, con_id: 265598, symbol: "AAPL".into(), exchange: "SMART".into(), sec_type: "STK".into(), tbt_type, number_of_ticks, ignore_size: false, reply_tx: None,
        };
        tx.send(tbt(1, crate::types::TbtType::Last, 10)).unwrap();
        tx.send(tbt(2, crate::types::TbtType::MidPoint, 0)).unwrap();
        engine.poll_once();
        let sent = plain_messages_sent(&mut hmds_side);
        assert_eq!(sent.len(), 2, "{sent:?}");
        assert!(sent[0].contains("<data>Last</data><refresh>ticks</refresh><timeLength>10 t</timeLength><source>API</source>"), "{}", sent[0]);
        assert!(sent[1].contains("<data>MidPoint</data><refresh>ticks</refresh><source>API</source>"), "{}", sent[1]);
        let qid = sent[0].split("<id>").nth(1).and_then(|s| s.split("</id>").next()).unwrap().to_string();

        let xml = format!("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<QueryError>\n\t<id>{qid}</id>\n\t<error>No historical market data for AAPL@BEST Last 10</error>\n</QueryError>\n");
        let msg = fix::fix_build(&[(35, "W"), (6118, &xml)], 1);
        engine.hmds.process_hmds_message(&msg, &mut engine.hmds_conn, &shared, &None, &mut engine.hb);
        assert_eq!(shared.market.drain_tbt_errors(),
            [(1, 10189, "Failed to request tick-by-tick data.No historical market data for AAPL@BEST Last 10".to_string())]);
        assert_eq!(engine.hmds.tbt_subscriptions.len(), 1, "the refused query is gone, the other stays");
    }

    // ibx#291: dropping the tick-by-tick consumer of a contract
    // keeps the slot while its market data runs, also while the farm is
    // down; the market data cancel then frees it.
    #[test]
    fn a_live_market_data_subscription_keeps_its_slot() {
        let shared = Arc::new(SharedState::new());
        let mut engine = HotLoop::new(shared.clone(), None, None);
        let (tx, rx) = crossbeam_channel::bounded(8);
        engine.set_control_rx(rx);
        tx.send(subscribe_cmd(265598, "STK")).unwrap();
        tx.send(ControlCommand::SubscribeTbt { req_id: 1, con_id: 265598, symbol: String::new(), exchange: "SMART".into(), sec_type: "STK".into(), tbt_type: crate::types::TbtType::BidAsk, number_of_ticks: 0, ignore_size: false, reply_tx: None }).unwrap();
        engine.poll_once();
        let id = engine.context.market.instrument_by_con_id(265598).unwrap();

        tx.send(ControlCommand::UnsubscribeTbt { req_id: 1 }).unwrap();
        engine.poll_once();
        assert_eq!(engine.context.market.con_id(id), Some(265598));

        // Farm down: the subscription waits for the reconnect, the slot stays.
        engine.farm.handle_disconnect(&mut engine.context, &None);
        tx.send(ControlCommand::SubscribeTbt { req_id: 2, con_id: 265598, symbol: String::new(), exchange: "SMART".into(), sec_type: "STK".into(), tbt_type: crate::types::TbtType::BidAsk, number_of_ticks: 0, ignore_size: false, reply_tx: None }).unwrap();
        tx.send(ControlCommand::UnsubscribeTbt { req_id: 2 }).unwrap();
        engine.poll_once();
        assert_eq!(engine.context.market.con_id(id), Some(265598));

        tx.send(ControlCommand::Unsubscribe { instrument: id }).unwrap();
        engine.poll_once();
        assert_eq!(engine.context.market.con_id(id), None, "freed once nothing uses it");
    }

    // ibx#287: no lookup for a contract that is not a stock, or when the
    // session does not scale lots; a subscription cancelled while it
    // waits is never sent.
    #[test]
    fn round_lot_lookup_only_where_it_can_apply() {
        let shared = Arc::new(SharedState::new());
        let mut engine = HotLoop::new(shared.clone(), None, None);
        let (c1, mut farm_side) = socket_pair();
        let (c2, mut ccp_side) = socket_pair();
        engine.farm_conn = Some(Connection::new_mem(c1));
        engine.ccp_conn = Some(Connection::new_mem(c2));
        let (tx, rx) = crossbeam_channel::bounded(8);
        engine.set_control_rx(rx);

        // Scaling off: sent at once, lot 1.
        tx.send(subscribe_cmd(265598, "STK")).unwrap();
        engine.poll_once();
        assert!(plain_messages_sent(&mut ccp_side).is_empty());
        assert_eq!(farm_messages_sent(&mut farm_side).len(), 1);

        // Scaling on, a future: sent at once, lot 1.
        engine.set_scale_us_lots(true);
        tx.send(subscribe_cmd(551601503, "FUT")).unwrap();
        engine.poll_once();
        assert!(plain_messages_sent(&mut ccp_side).is_empty());
        assert_eq!(farm_messages_sent(&mut farm_side).len(), 1);
        let fut = engine.context.market.instrument_by_con_id(551601503).unwrap();
        assert_eq!(engine.context.market.round_lot(fut), 1);

        // A stock cancelled while it waits: nothing reaches the farm.
        tx.send(subscribe_cmd(272093, "STK")).unwrap();
        engine.poll_once();
        assert_eq!(plain_messages_sent(&mut ccp_side).len(), 1);
        let msft = engine.context.market.instrument_by_con_id(272093).unwrap();
        tx.send(ControlCommand::Unsubscribe { instrument: msft }).unwrap();
        engine.poll_once();
        assert!(engine.context.lot_parked.is_empty());
        engine.context.lot_lookups[0].2 = Instant::now();
        farm::sweep_round_lot_lookups(&mut engine.context);
        engine.send_lot_ready();
        assert!(farm_messages_sent(&mut farm_side).is_empty());
    }

    // ibx#288: handle_disconnect cleared the request-id maps and reconnect
    // rebuilt its list from them, so no subscription came back after a farm
    // reconnect. A subscription cancelled while the farm was down must still
    // stay cancelled.
    #[test]
    fn farm_reconnect_reissues_every_live_subscription() {
        let shared = Arc::new(SharedState::new());
        let mut engine = HotLoop::new(shared, None, None);
        let (c1, _s1) = socket_pair();
        engine.farm_conn = Some(Connection::new_mem(c1));
        let aapl = engine.context.market.register(265598);
        let msft = engine.context.market.register(272093);
        let spy = engine.context.market.register(756733);
        for (con_id, sym, inst, mode) in [(265598, "AAPL", aapl, 0), (272093, "MSFT", msft, 0), (756733, "SPY", spy, 3)] {
            engine.farm.send_mktdata_subscribe(
                con_id, sym, "SMART", "STK", "", 0.0, "", "", inst, mode,
                &mut engine.farm_conn, &mut engine.hb,
            );
        }

        engine.farm.handle_disconnect(&mut engine.context, &None);
        engine.farm.send_mktdata_unsubscribe(msft, &mut engine.farm_conn, &mut engine.hb);

        let (c2, mut s2) = socket_pair();
        engine.reconnect_farm(Connection::new_mem(c2));

        let sent = farm_messages_sent(&mut s2);
        assert_eq!(sent.len(), 2, "one subscribe per live instrument: {:?}", sent);
        let aapl_sub = sent.iter().find(|m| m.contains("6008=265598")).expect("AAPL re-subscribed");
        assert!(aapl_sub.contains("264=442|") && aapl_sub.contains("264=443|"), "realtime keeps both entries");
        let spy_sub = sent.iter().find(|m| m.contains("6008=756733")).expect("SPY re-subscribed");
        assert!(spy_sub.contains("9887=3|"), "delayed mode kept: {}", spy_sub);
        assert!(!sent.iter().any(|m| m.contains("6008=272093")), "MSFT was cancelled while down");
        assert_eq!(engine.farm.instrument_md_reqs.len(), 2);
        // Every subscription is the 442 / 443 pair, delayed too (ibx#447).
        assert_eq!(engine.farm.md_req_to_instrument.len(), 4);

        // A second drop and reconnect re-issues them again.
        engine.farm.handle_disconnect(&mut engine.context, &None);
        let (c3, mut s3) = socket_pair();
        engine.reconnect_farm(Connection::new_mem(c3));
        assert_eq!(farm_messages_sent(&mut s3).len(), 2);
    }

    // ibx#421: a logon update with a new data permission stamp makes the
    // reference resubscribe its market data (`jclient.ij`): every
    // streaming top of book and news entry cancelled, then asked again on
    // new ids, at once the first time, then at most every 30 s; a change
    // within 1 s of the last one is ignored. A refusal for an API
    // subscription since the last change gives warning 2134 with id -1.
    #[test]
    fn data_permission_change_resubscribes_market_data() {
        let shared = Arc::new(SharedState::new());
        let mut engine = HotLoop::new(shared.clone(), None, None);
        let (c1, mut s1) = socket_pair();
        engine.farm_conn = Some(Connection::new_mem(c1));
        let aapl = engine.context.market.register(265598);
        engine.farm.send_mktdata_subscribe(265598, "AAPL", "SMART", "STK", "", 0.0, "", "", aapl, 0,
            &mut engine.farm_conn, &mut engine.hb);
        let first = farm_messages_sent(&mut s1);
        assert_eq!(first.len(), 1);
        engine.ccp.data_permissions = Some("1788356313".into());
        engine.farm.note_refused_api_subscription_for_test("12,0,none");
        let update = |engine: &mut HotLoop, stamp: &str| {
            let frame = format!("35=A\x0152=20261002-06:20:00\x016764={}\x01", stamp);
            let mut hb = HeartbeatState::new();
            engine.ccp.process_ccp_message(frame.as_bytes(), &mut None, &mut engine.context, &engine.shared,
                &None, &mut hb, "DU1");
        };
        let t = Instant::now();
        update(&mut engine, "1788999999");
        engine.service_logon_updates(t);
        let sent = farm_messages_sent(&mut s1);
        assert_eq!(sent.len(), 2, "{:?}", sent);
        assert!(sent[0].contains("263=2|") && sent[0].contains("262=1|") && sent[0].contains("262=2|"), "cancel: {}", sent[0]);
        assert!(sent[1].contains("263=1|") && sent[1].contains("262=3|") && sent[1].contains("262=4|"), "new ids: {}", sent[1]);
        assert_eq!(shared.drain_connection_notices(), [(2134, "Market Data subscription has been changed.".to_string())]);

        // The same stamp: no change. A new one within 1 s: ignored.
        update(&mut engine, "1788999999");
        engine.service_logon_updates(t + Duration::from_millis(500));
        update(&mut engine, "1789000000");
        engine.service_logon_updates(t + Duration::from_millis(900));
        assert!(farm_messages_sent(&mut s1).is_empty());
        // A change 10 s later runs 30 s after the last resubscription,
        // with no warning (no refusal since the last change).
        update(&mut engine, "1789000001");
        engine.service_logon_updates(t + Duration::from_secs(10));
        engine.service_logon_updates(t + Duration::from_secs(29));
        assert!(farm_messages_sent(&mut s1).is_empty());
        engine.service_logon_updates(t + Duration::from_secs(30));
        assert_eq!(farm_messages_sent(&mut s1).len(), 2);
        assert!(shared.drain_connection_notices().is_empty());
    }

    // ibx#421: a relogin's stamp is a change only when the old and the
    // new one are both set and differ (`jclient.gi.a(jfix.dk, jfix.bb,
    // LogonType)@297-388`).
    #[test]
    fn relogin_data_permission_rule() {
        let mut ccp = CcpState::new();
        assert!(!ccp.relogin_data_permissions(Some("1")), "no old stamp");
        assert!(!ccp.relogin_data_permissions(Some("1")));
        assert!(ccp.relogin_data_permissions(Some("2")));
        assert!(!ccp.relogin_data_permissions(None), "no new stamp");
        assert_eq!(ccp.data_permissions, None, "kept even unset");
        assert!(!ccp.relogin_data_permissions(Some("3")));
    }

    // ibx#276: a logon update whose SSL farm list goes from a list to
    // nothing closes every farm, to open them again without TLS.
    #[test]
    fn ssl_farm_list_emptied_reconnects_the_farms() {
        let shared = Arc::new(SharedState::new());
        let mut engine = HotLoop::new(shared, None, None);
        let (c1, _s1) = socket_pair();
        engine.farm_conn = Some(Connection::new_mem(c1));
        let mut auth = reconnect_auth_with_host("gw.example");
        auth.ssl_farms = "usfarm".into();
        engine.set_reconnect_auth(auth);
        engine.ccp.ssl_farms_update = Some("usfarm;ushmds".into());
        engine.service_logon_updates(Instant::now());
        assert!(!engine.farm.disconnected, "a list replaced by a list");
        assert_eq!(engine.reconnect_auth.as_ref().unwrap().ssl_farms, "usfarm;ushmds");
        engine.ccp.ssl_farms_update = Some(String::new());
        engine.service_logon_updates(Instant::now());
        assert!(engine.farm.disconnected, "farms closed to reconnect without TLS");
        assert_eq!(engine.reconnect_auth.as_ref().unwrap().ssl_farms, "");
    }

    #[test]
    fn push_hmds_unavailable_non_historical_emits_error_without_sentinel() {
        let shared = SharedState::new();
        push_hmds_unavailable(&shared, 42, false);

        let errors = shared.reference.drain_historical_errors();
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].0, 42);
        assert_eq!(errors[0].1, 162);
        // Head-ts / histogram / ticks / schedule / scanner / news / fundamental:
        // no bar-stream consumer waiting for historical_data_end.
        assert!(shared.reference.drain_historical_data().is_empty());
    }

    // ibx#447 (captured 28/09/2026): the subscribe is the 442 / 443 pair
    // with no 9839; with delayed data enabled, a reject with delayed data
    // available asks again on new ids with 9887=1 on each entry, and the
    // client learns it (marketDataType 3, 10167). Without it, the
    // subscription stops (354 with the "delayed available" text).
    #[test]
    fn a_rejected_subscription_goes_delayed_or_stops() {
        for market_data_type in [3, 1] {
            let shared = Arc::new(SharedState::new());
            let mut engine = HotLoop::new(shared.clone(), None, None);
            let (c1, mut s1) = socket_pair();
            engine.farm_conn = Some(Connection::new_mem(c1));
            engine.farm.md_modes.apply(market_data_type);
            let jp = engine.context.market.register(13905804);
            engine.farm.send_mktdata_subscribe(13905804, "7203", "SMART", "STK", "", 0.0, "", "", jp, 0,
                &mut engine.farm_conn, &mut engine.hb);
            let first = farm_messages_sent(&mut s1);
            assert_eq!(first.len(), 1);
            assert!(first[0].contains("264=442|") && first[0].contains("264=443|"), "{}", first[0]);
            assert!(!first[0].contains("9839=") && !first[0].contains("9887="), "{}", first[0]);
            let ids: Vec<String> = engine.farm.md_req_to_instrument.iter().map(|(r, _)| r.to_string()).collect();

            let reject = crate::protocol::fix::fix_build(&[(35, "3"), (45, "0"),
                (58, "Error&BEST/STK/Top&BEST/STK/Top"), (262, &ids.join(";")), (9887, "1;1"),
                (6756, "133,134;133,134"), (9888, "1;1")], 1);
            engine.farm.process_farm_message(&reject, &mut engine.farm_conn, &mut engine.context,
                &shared, &None, &mut engine.hb);

            let rejects = shared.market.drain_md_rejects();
            if market_data_type == 3 {
                let again = farm_messages_sent(&mut s1);
                assert_eq!(again.len(), 1, "{again:?}");
                assert_eq!(again[0].matches("9887=1|").count(), 2, "{}", again[0]);
                assert!(!ids.iter().any(|id| again[0].contains(&format!("262={}|", id))), "new ids: {}", again[0]);
                assert_eq!(rejects, [crate::bridge::MdReject::Delayed { instrument: jp }]);
                assert_eq!(engine.farm.instrument_md_reqs[0].1.len(), 4, "a cancel covers the rejected ids too");
            } else {
                assert!(farm_messages_sent(&mut s1).is_empty());
                assert_eq!(rejects, [crate::bridge::MdReject::NotSubscribed {
                    instrument: jp, delayed_available: true, needs_api_subscription: false }]);
                assert!(engine.farm.instrument_md_reqs.is_empty());
                assert!(engine.farm.md_req_to_instrument.is_empty());
            }
        }
    }
}

#[cfg(test)]
mod routing_tests {
    use super::*;
    use std::sync::Arc;
    use std::time::Duration;
    use crate::bridge::SharedState;
    use crate::engine::routing::TableKind;

    const MD_TABLE: &str = "BEST,STK,Top,1,*,cdc1.example,4000,usfarm;\
        BEST,STK,Top,3,*,zdc1.example,4000,eufarm;\
        CME,FUT,Top|Deep,-1,*,cdc1.example,4000,usfuture;\
        IDEALPRO,CASH,Top|Deep,4,*,ndc1.example,4000,cashfarm;\
        NASDAQ,STK,Top|Deep2|Deep,-1,*,cdc1.example,4000,usfarm";

    fn loopback() -> (Connection, crate::protocol::connection::MemTransport) {
        let (client, server) = crate::protocol::connection::mem_pair();
        (Connection::new_mem(client), server)
    }

    fn sent(server: &mut crate::protocol::connection::MemTransport) -> Vec<String> {
        use std::io::Read;
        server.set_read_timeout(Some(Duration::from_millis(300))).unwrap();
        let mut buf = Vec::new();
        let mut chunk = [0u8; 8192];
        while let Ok(n) = server.read(&mut chunk) {
            if n == 0 { break; }
            buf.extend_from_slice(&chunk[..n]);
        }
        let mut out = Vec::new();
        let mut rest = &buf[..];
        while let Some(len) = crate::protocol::fixcomp::fixcomp_length(rest) {
            for m in crate::protocol::fixcomp::fixcomp_decompress(&rest[..len]).unwrap() {
                out.push(String::from_utf8_lossy(&m).replace('\x01', "|"));
            }
            rest = &rest[len..];
        }
        out
    }

    fn sub(con_id: i64, symbol: &str, exchange: &str, sec_type: &str) -> farm::MdSubscribe {
        farm::MdSubscribe {
            con_id, symbol: symbol.into(), exchange: exchange.into(), sec_type: sec_type.into(),
            last_trade_date: String::new(), strike: 0.0, right: String::new(), multiplier: String::new(),
            instrument: 0, mode_9887: 0, snapshot: false,
        }
    }

    fn engine() -> (HotLoop, crate::protocol::connection::MemTransport) {
        let mut engine = HotLoop::new(Arc::new(SharedState::new()), None, None);
        engine.set_farm_name("usfarm".into());
        let (farm, farm_side) = loopback();
        engine.farm_conn = Some(farm);
        (engine, farm_side)
    }

    fn register(engine: &mut HotLoop, s: &mut farm::MdSubscribe) {
        s.instrument = engine.context.market.register(s.con_id);
        engine.context.market.set_routing(s.instrument, &s.sec_type, &s.exchange);
    }

    // #445: each entry writes the contract's own exchange and security
    // type (captured 23/09/2026: a future, a currency pair, a SMART stock).
    #[test]
    fn entries_carry_the_contract_exchange_and_security_type() {
        let (mut engine, mut farm_side) = engine();
        for (con_id, sym, exch, st) in [(815824267, "MNQ", "CME", "FUT"), (12087792, "EUR", "IDEALPRO", "CASH"), (265598, "AAPL", "SMART", "STK")] {
            let mut s = sub(con_id, sym, exch, st);
            register(&mut engine, &mut s);
            engine.route_md_subscribe(&s);
        }
        let msgs = sent(&mut farm_side);
        assert_eq!(msgs.len(), 3, "{msgs:?}");
        assert!(msgs[0].contains("|6008=815824267|207=CME|167=FUT|264=442|6088=Socket|9830=1|"), "{}", msgs[0]);
        assert!(msgs[0].contains("|207=CME|167=FUT|264=443|"), "{}", msgs[0]);
        assert!(msgs[1].contains("|207=FXSUBPIP|167=CASH|264=442|"), "{}", msgs[1]);
        assert!(msgs[1].contains("|207=IDEALPRO|167=CASH|264=443|"), "{}", msgs[1]);
        assert!(msgs[2].contains("|207=BEST|167=CS|264=442|"), "{}", msgs[2]);
        assert!(msgs.iter().all(|m| !m.contains("|9839=")));
    }

    // #445: the cancel repeats the entries of the subscribe in one message
    // (captured 18/06/2026).
    #[test]
    fn cancel_repeats_the_entries() {
        let (mut engine, mut farm_side) = engine();
        let mut s = sub(815824267, "MNQ", "CME", "FUT");
        register(&mut engine, &mut s);
        engine.route_md_subscribe(&s);
        let _ = sent(&mut farm_side);
        let first = engine.farm.next_md_req_id - 2;
        engine.route_md_cancel(s.instrument);
        let msgs = sent(&mut farm_side);
        assert_eq!(msgs.len(), 1, "{msgs:?}");
        let want = format!("|263=2|146=2|262={}|6008=815824267|207=CME|167=FUT|264=442|9830=1|262={}|6008=815824267|207=CME|167=FUT|264=443|9830=1|",
            first, first + 1);
        assert!(msgs[0].contains(&want), "{}\nwant {}", msgs[0], want);
        assert!(!msgs[0].contains("6088"), "{}", msgs[0]);
    }

    // #445: with a routing table, a contract whose row names another farm
    // goes to that farm, opened on demand: its subscription waits in the
    // farm's queue and nothing is sent to the primary farm. A SMART stock
    // of group 1 stays on the primary farm; one with no row is dropped.
    #[test]
    fn routed_to_the_farm_of_its_row() {
        let (mut engine, mut farm_side) = engine();
        engine.set_routing_table(TableKind::MarketData, MD_TABLE);

        let mut fut = sub(815824267, "MNQ", "CME", "FUT");
        register(&mut engine, &mut fut);
        engine.route_md_subscribe(&fut);
        let id = engine.pool.find("usfuture").expect("usfuture opened on demand");
        let farm = engine.pool.get(id).unwrap();
        assert_eq!(farm.host, "cdc1.example");
        assert_eq!(farm.queued(), 1, "the subscription waits for the logon");
        assert!(sent(&mut farm_side).is_empty());
        assert!(engine.farm.uses_farm(id));

        let mut fx = sub(12087792, "EUR", "IDEALPRO", "CASH");
        register(&mut engine, &mut fx);
        engine.route_md_subscribe(&fx);
        assert!(engine.pool.find("cashfarm").is_some());

        let mut aapl = sub(265598, "AAPL", "SMART", "STK");
        register(&mut engine, &mut aapl);
        engine.context.agg_groups.insert(265598, 1);
        engine.route_md_subscribe(&aapl);
        assert_eq!(sent(&mut farm_side).len(), 1, "group 1 is the primary farm");

        let mut opt = sub(1234, "X", "CBOE", "OPT");
        register(&mut engine, &mut opt);
        engine.route_md_subscribe(&opt);
        assert!(!engine.farm.has_md_subscription(opt.instrument), "no row: nothing sent");
        assert!(sent(&mut farm_side).is_empty());

        let mut news = sub(999, "BRF", "BRF", "NEWS");
        register(&mut engine, &mut news);
        engine.route_md_subscribe(&news);
        assert!(!engine.farm.has_md_subscription(news.instrument), "no top of book for NEWS");
    }

    // #445: a SMART subscription waits for the contract's definition when
    // its group is not known, then goes to the farm of its group.
    #[test]
    fn smart_route_waits_for_the_aggregate_group() {
        let (mut engine, mut farm_side) = engine();
        let (ccp, mut ccp_side) = loopback();
        engine.ccp_conn = Some(ccp);
        engine.set_routing_table(TableKind::MarketData, MD_TABLE);
        let (tx, rx) = crossbeam_channel::bounded(8);
        engine.set_control_rx(rx);
        tx.send(ControlCommand::Subscribe {
            con_id: 14094, symbol: "BMW".into(), exchange: "SMART".into(), sec_type: "STK".into(),
            last_trade_date: String::new(), strike: 0.0, right: String::new(), multiplier: String::new(),
            mode_9887: 0, snapshot: false, reply_tx: None,
        }).unwrap();
        engine.poll_once();
        use std::io::Read;
        ccp_side.set_read_timeout(Some(Duration::from_millis(300))).unwrap();
        let mut buf = [0u8; 4096];
        let n = ccp_side.read(&mut buf).unwrap();
        let asked = String::from_utf8_lossy(&buf[..n]).replace('\x01', "|");
        assert!(asked.contains("|35=c|") && asked.contains("|6008=14094|"), "{asked}");
        let req_id = asked.split('|').find_map(|p| p.strip_prefix("320=")).unwrap().to_string();
        assert!(sent(&mut farm_side).is_empty());

        let reply = fix::fix_build(&[(35, "d"), (320, &req_id), (6008, "14094"), (167, "CS"), (6178, "3")], 1);
        assert!(farm::round_lot_reply(&mut engine.context, &req_id, &reply));
        engine.send_lot_ready();
        let id = engine.pool.find("eufarm").expect("group 3 is eufarm");
        assert_eq!(engine.pool.get(id).unwrap().queued(), 1);
        let inst = engine.context.market.instrument_by_con_id(14094).unwrap();
        assert_eq!(engine.context.market.round_lot(inst), 1, "sizes as on the wire with the lots scaling off");
    }

    // #445: server tags are numbered by each farm: a tag acked on a farm
    // opened on demand maps its ticks, the same tag on the primary farm
    // does not; the farm's loss drops only its own tags, and the
    // subscription goes out again when the farm is back.
    #[test]
    fn server_tags_are_kept_per_farm() {
        let (mut engine, _farm_side) = engine();
        engine.set_routing_table(TableKind::MarketData, MD_TABLE);
        let mut fut = sub(815824267, "MNQ", "CME", "FUT");
        register(&mut engine, &mut fut);
        engine.route_md_subscribe(&fut);
        let id = engine.pool.find("usfuture").unwrap();
        let (conn, mut side) = loopback();
        engine.pool.on_connected(id, conn, std::time::Instant::now());
        let flushed = sent(&mut side);
        assert_eq!(flushed.len(), 1);
        let first = engine.farm.next_md_req_id - 2;

        engine.farm.rx_farm = id;
        let ack = format!("8=O\x0135=Q\x01228,{},0.25,0,3,5,,1,1", first);
        engine.inject_farm_message(ack.as_bytes());
        engine.farm.rx_farm = pool::PRIMARY_MD;
        assert_eq!(engine.context.market.instrument_by_farm_tag(id, 228), Some(fut.instrument));
        assert_eq!(engine.context.market.instrument_by_server_tag(228), None);

        engine.pool_farm_lost(id);
        assert_eq!(engine.context.market.instrument_by_farm_tag(id, 228), None);
        assert!(!engine.farm.uses_farm(id));
        assert!(engine.farm.has_md_subscription(fut.instrument), "kept for the reconnect");
        let (conn, mut side) = loopback();
        engine.pool.on_connected(id, conn, std::time::Instant::now());
        engine.resend_unsent_subscriptions();
        let again = sent(&mut side);
        assert_eq!(again.len(), 1, "{again:?}");
        assert!(again[0].contains("|207=CME|167=FUT|264=442|"));
    }
}

#[cfg(test)]
mod tag_cleaner_tests {
    use super::*;
    use std::sync::Arc;
    use std::time::Duration;
    use crate::bridge::SharedState;

    fn subscribe(engine: &mut HotLoop, con_id: i64) -> (InstrumentId, u32) {
        let instrument = engine.context.market.register(con_id);
        let sub = farm::MdSubscribe {
            con_id, symbol: String::new(), exchange: "SMART".into(), sec_type: "STK".into(),
            last_trade_date: String::new(), strike: 0.0, right: String::new(), multiplier: String::new(),
            instrument, mode_9887: 0, snapshot: false,
        };
        engine.route_md_subscribe(&sub);
        (instrument, engine.farm.next_md_req_id - 2)
    }

    // #292: quote and trade tags are kept apart. The cleaner runs every
    // 60 s; the quote tags of an instrument with no live top-of-book
    // request go once it was unused for 300 s (a request in between clears
    // the mark); its trade stream tags stay while the slot stays, and go
    // with the contract's last slot.
    #[test]
    fn quote_tags_freed_after_five_unused_minutes_trade_tags_with_the_slot() {
        let mut engine = HotLoop::new(Arc::new(SharedState::new()), None, None);
        let t0 = Instant::now();
        let (id, first) = subscribe(&mut engine, 265598);
        engine.inject_farm_message(format!("8=O\x0135=Q\x011101,{},0.01,0,3,9c,,1,1", first).as_bytes());
        engine.inject_farm_message(b"8=O\x0135=L\x01265598,0.01,1098,,1");
        assert_eq!(engine.context.market.tag_counts(), (1, 1));
        assert_eq!(engine.context.market.instrument_by_server_tag(1098), Some(id), "trade tag found");

        // The slot stays (pinned, as by an open order) after the cancel.
        engine.route_md_cancel(id);
        assert_eq!(engine.context.market.instrument_by_server_tag(1101), Some(id), "kept after the cancel");

        engine.clean_server_tags(t0 + Duration::from_secs(30));
        assert_eq!(engine.context.market.tag_counts(), (1, 1), "first run at 60 s");
        engine.clean_server_tags(t0 + Duration::from_secs(60)); // marked
        engine.clean_server_tags(t0 + Duration::from_secs(300));
        assert_eq!(engine.context.market.tag_counts(), (1, 1), "unused for 240 s only");
        engine.clean_server_tags(t0 + Duration::from_secs(360));
        assert_eq!(engine.context.market.tag_counts(), (0, 1), "quote tags freed, trade tags kept");
        assert_eq!(engine.context.market.instrument_by_server_tag(1101), None);

        // Used again: the mark is cleared, nothing more is freed.
        let (_, second) = subscribe(&mut engine, 265598);
        engine.inject_farm_message(format!("8=O\x0135=Q\x011102,{},0.01,0,3,9c,,1,1", second).as_bytes());
        engine.clean_server_tags(t0 + Duration::from_secs(420));
        engine.clean_server_tags(t0 + Duration::from_secs(900));
        assert_eq!(engine.context.market.instrument_by_server_tag(1102), Some(id));

        // The last slot of the contract goes: its trade tags go too.
        engine.route_md_cancel(id);
        engine.context.market.unregister(id);
        assert_eq!(engine.context.market.tag_counts(), (0, 0));
    }

    // #292: the trade tags of a contract with two slots stay while one
    // remains.
    #[test]
    fn trade_tags_follow_the_last_slot_of_the_contract() {
        let mut m = crate::engine::market_state::MarketState::new();
        let a = m.register(265598);
        let b = m.try_register_unresolved().unwrap();
        m.resolve_con_id(b, 265598);
        m.register_trade_tag(0, 1098, a, 0.01);
        m.unregister(a);
        assert_eq!(m.instrument_by_server_tag(1098), Some(b));
        m.unregister(b);
        assert_eq!(m.instrument_by_server_tag(1098), None);
    }
}

#[cfg(test)]
mod news_tests {
    use super::*;
    use std::sync::Arc;
    use std::time::Duration;
    use crate::bridge::SharedState;

    const ALL: &str = "BRFG,BRFUPDN,DJ-N,DJ-RTA,DJ-RTE,DJ-RTG,DJ-RTPRO,DJNL";

    pub(super) fn socket_pair() -> (crate::protocol::connection::MemTransport, crate::protocol::connection::MemTransport) {
        let (client, server) = crate::protocol::connection::mem_pair();
        (client, server)
    }

    /// The compressed messages the engine wrote, without the header,
    /// sequence and time tags.
    pub(super) fn sent(server: &mut crate::protocol::connection::MemTransport) -> Vec<String> {
        use std::io::Read;
        server.set_read_timeout(Some(Duration::from_millis(300))).unwrap();
        let mut buf = Vec::new();
        let mut chunk = [0u8; 8192];
        while let Ok(n) = server.read(&mut chunk) {
            if n == 0 { break; }
            buf.extend_from_slice(&chunk[..n]);
        }
        let mut out = Vec::new();
        let mut rest = &buf[..];
        while let Some(len) = crate::protocol::fixcomp::fixcomp_length(rest) {
            for m in crate::protocol::fixcomp::fixcomp_decompress(&rest[..len]).unwrap() {
                let text = String::from_utf8_lossy(&m).replace('\x01', "|");
                out.push(text.split('|').filter(|f| !f.is_empty())
                    .filter(|f| !["8=", "9=", "34=", "52=", "10="].iter().any(|p| f.starts_with(p)))
                    .map(|f| format!("{f}|")).collect());
            }
            rest = &rest[len..];
        }
        out
    }

    pub(super) fn engine() -> (HotLoop, Arc<SharedState>, crate::protocol::connection::MemTransport, Sender<ControlCommand>) {
        let shared = Arc::new(SharedState::new());
        let mut engine = HotLoop::new(shared.clone(), None, None);
        let (c, server) = socket_pair();
        engine.farm_conn = Some(Connection::new_mem(c));
        let (tx, rx) = crossbeam_channel::unbounded();
        engine.set_control_rx(rx);
        (engine, shared, server, tx)
    }

    pub(super) fn aapl(tx: &Sender<ControlCommand>, engine: &mut HotLoop, providers: &str) -> InstrumentId {
        tx.send(ControlCommand::Subscribe {
            con_id: 265598, symbol: "AAPL".into(), exchange: "SMART".into(), sec_type: "STK".into(),
            last_trade_date: String::new(), strike: 0.0, right: String::new(), multiplier: String::new(),
            mode_9887: 0, snapshot: false, reply_tx: None,
        }).unwrap();
        engine.poll_once();
        let id = engine.context.market.instrument_by_con_id(265598).unwrap();
        tx.send(ControlCommand::SubscribeNews {
            instrument: id, con_id: 265598, exchange: "SMART".into(), sec_type: "STK".into(),
            providers: providers.into(), refusal: None,
        }).unwrap();
        engine.poll_once();
        id
    }

    /// The inbound farm frames of a recorded reference scenario, from `from_seq` on.
    fn fixture_farm_frames(scenario: &str, from_seq: u64) -> Vec<Vec<u8>> {
        use base64::Engine as _;
        let path = format!("{}/tests/fixtures/gw1040/scenarios/{}", env!("CARGO_MANIFEST_DIR"), scenario);
        let text = std::fs::read_to_string(&path).unwrap();
        let mut out = Vec::new();
        for line in text.lines().skip(1) {
            let rec: serde_json::Value = serde_json::from_str(line).unwrap();
            if rec["leg"] != "fix_in" || rec["conn"] != "usfarm" || rec["seq"].as_u64().unwrap() < from_seq { continue; }
            out.push(base64::engine::general_purpose::STANDARD.decode(rec["raw_b64"].as_str().unwrap()).unwrap());
        }
        out
    }

    // ibx#458 (captured 02/10/2026): the news entry follows the top of book
    // on the contract's farm, in its own message, with the provider key,
    // the streaming-client mark and the API flag; its cancel follows the
    // top-of-book cancel, without the mark.
    #[test]
    fn news_entry_on_the_farm_and_its_cancel() {
        let (mut engine, _shared, mut farm_side, tx) = engine();
        let id = aapl(&tx, &mut engine, ALL);
        let out = sent(&mut farm_side);
        assert_eq!(out.len(), 2, "{out:?}");
        assert!(out[0].contains("|264=442|") && out[0].contains("|262=1|") && out[0].contains("|262=2|"), "{}", out[0]);
        // Captured: 35=V|263=1|146=1|262=35|6008=265598|207=NEWS|167=CS|264=292|6472=...|6088=Socket|9830=1
        assert_eq!(out[1], format!("35=V|263=1|146=1|262=3|6008=265598|207=NEWS|167=CS|264=292|6472={ALL}|6088=Socket|9830=1|"));

        tx.send(ControlCommand::Unsubscribe { instrument: id }).unwrap();
        engine.poll_once();
        let out = sent(&mut farm_side);
        assert_eq!(out.len(), 2, "{out:?}");
        assert!(out[0].starts_with("35=V|263=2|146=2|"), "{}", out[0]);
        // Captured: 35=V|263=2|146=2|262=38|...|9830=1|262=35|6008=265598|207=NEWS|167=CS|264=292|6472=...|9830=1
        assert_eq!(out[1], format!("35=V|263=2|146=1|262=3|6008=265598|207=NEWS|167=CS|264=292|6472={ALL}|9830=1|"));
        assert!(engine.farm.news.is_empty());
        assert_eq!(engine.context.market.con_id(id), None, "the slot is freed");
    }

    // ibx#458: no provider key (no news source), no 6472.
    #[test]
    fn empty_provider_key_is_not_written() {
        let (mut engine, _shared, mut farm_side, tx) = engine();
        aapl(&tx, &mut engine, "");
        let out = sent(&mut farm_side);
        assert_eq!(out[1], "35=V|263=1|146=1|262=3|6008=265598|207=NEWS|167=CS|264=292|6088=Socket|9830=1|");
    }

    // ibx#458, golden frames of b1_458_news_dup (02/10/2026): the ack of the
    // news entry binds its server tag; the empty frames give nothing; the
    // headlines reach tickNews with the time in milliseconds, the headline
    // without its {...} part and that part as extra data. An article already
    // given is not given again; a tag of no news entry is dropped (#292).
    #[test]
    fn captured_news_frames_reach_tick_news() {
        let (mut engine, shared, _farm_side, tx) = engine();
        let id = aapl(&tx, &mut engine, ALL);
        // A headline before the ack: its tag is not known.
        let frames = fixture_farm_frames("20261002/b1_458_news_dup.jsonl", 15490);
        let g = |seq_frame: &[u8]| seq_frame.windows(6).any(|w| w == b"\x0135=G\x01");
        let g_frames: Vec<&Vec<u8>> = frames.iter().filter(|f| g(f)).collect();
        // 15490, 15497 (empty), 15503 (AAPL, 5 entries), then the MSFT ones.
        engine.inject_farm_message(g_frames[2]);
        assert!(shared.market.drain_tick_news().is_empty(), "no news entry has this tag yet");

        // Captured ack of the news entry: 25064,35,0.01,0,0,9c,,0,1 (our id is 3).
        engine.inject_farm_message(b"8=O\x019=0045\x0135=Q\x0125064,3,0.01,0,0,9c,,0,1\x018349=7E2367C2\x01");
        assert!(shared.market.drain_tick_req_params().is_empty(), "a news ack gives no request parameters");
        engine.inject_farm_message(g_frames[0]);
        engine.inject_farm_message(g_frames[1]);
        assert!(shared.market.drain_tick_news().is_empty(), "empty frames");
        engine.inject_farm_message(g_frames[2]);
        let news: Vec<(InstrumentId, i64, String, String, String, String)> = shared.market.drain_tick_news().into_iter()
            .map(|n| (n.instrument, n.timestamp, n.provider_code, n.article_id, n.headline, n.extra_data)).collect();
        let row = |t: i64, p: &str, a: &str, h: &str, x: &str| (id, t, p.to_string(), a.to_string(), h.to_string(), x.to_string());
        let math = "The New Math of AI: Are Those Trillion-Dollar Numbers for Real? -- Barrons.com";
        // The client got the first four (events.jsonl, req 9580).
        assert_eq!(news[..4], [
            row(1790893800000, "DJ-RTPRO", "DJ-RTPRO$1f790db1", "VP Newstead Sells 2,399 Of Apple Inc >AAPL", "A:800015:L:en"),
            row(1790920800000, "DJ-N", "DJ-N$1f798bd2", math, "L:en:A:800015"),
            row(1790920800000, "DJ-RTG", "DJ-RTG$1f798bd2", math, "L:en:A:800015"),
            row(1790920800000, "DJ-RTPRO", "DJ-RTPRO$1f798bd2", math, "L:en:A:800015"),
        ]);
        // The reference held back the fifth (Chinese and English story);
        // its rule is not known (ibx#458).
        assert_eq!(news.len(), 5);
        assert_eq!(news[4].3, "DJ-N$1f79997f");
        assert_eq!(news[4].4, "Apple Bids to Put China in Its Fold -- WSJ");

        // The same articles again (the reference sent AMZN repeats with
        // the first number 3, never given again): nothing.
        engine.inject_farm_message(g_frames[2]);
        assert!(shared.market.drain_tick_news().is_empty());

        // The MSFT frame's tag (25065) has no news entry here.
        engine.inject_farm_message(g_frames[5]);
        assert!(shared.market.drain_tick_news().is_empty());
    }

    // ibx#458: a request without a conId has its news tick checked once the
    // lookup found the contract; a refused one ends with 10094 and nothing
    // goes to the farm. An accepted one goes with its top of book.
    #[test]
    fn news_tick_waits_for_the_lookup() {
        use crate::control::contracts::tests::pipe_msg;
        let (mut engine, shared, mut farm_side, tx) = engine();
        let (c2, mut ccp_side) = socket_pair();
        engine.ccp_conn = Some(Connection::new_mem(c2));
        let mut ids = Vec::new();
        for (symbol, refusal) in [("NVDA", Some("API News error:Source code unchecked in API news Settings: XYZ")), ("MSFT", None)] {
            let (reply, answer) = crossbeam_channel::bounded(1);
            tx.send(ControlCommand::SubscribeBySymbol {
                symbol: symbol.into(), sec_type: "STK".into(), exchange: "SMART".into(), currency: "USD".into(),
                filters: Default::default(), mode_9887: 0, snapshot: false, reply_tx: Some(reply),
            }).unwrap();
            engine.poll_once();
            let id = answer.recv().unwrap().unwrap();
            tx.send(ControlCommand::SubscribeNews {
                instrument: id, con_id: 0, exchange: "SMART".into(), sec_type: "STK".into(),
                providers: "DJ-N".into(), refusal: refusal.map(String::from),
            }).unwrap();
            engine.poll_once();
            ids.push(id);
        }
        assert!(shared.market.drain_md_rejects().is_empty(), "not before the contract is known");
        let asked: Vec<String> = {
            use std::io::Read;
            ccp_side.set_read_timeout(Some(Duration::from_millis(300))).unwrap();
            let mut buf = Vec::new();
            let mut chunk = [0u8; 8192];
            while let Ok(n) = ccp_side.read(&mut chunk) { if n == 0 { break; } buf.extend_from_slice(&chunk[..n]); }
            String::from_utf8_lossy(&buf).replace('\x01', "|").split("|320=").skip(1)
                .map(|m| m.split('|').next().unwrap().to_string()).collect()
        };
        assert_eq!(asked.len(), 2, "{asked:?}");
        let reply = |id: &str, body: &str| pipe_msg(&format!("35=d|43=N|320={id}|322=*|323=4|{body}"));
        let mut context = std::mem::replace(&mut engine.context, Context::new());
        assert!(farm::md_contract_reply(&mut context, &shared, &asked[0], &reply(&asked[0], "55=NVDA|167=STK|207=BEST|6008=4815747|15=USD|")));
        assert!(farm::md_contract_reply(&mut context, &shared, &asked[1], &reply(&asked[1], "55=MSFT|167=STK|207=BEST|6008=272093|15=USD|")));
        engine.context = context;
        engine.send_md_resolved();
        assert_eq!(shared.market.drain_md_rejects(), [crate::bridge::MdReject::NewsRefused {
            instrument: ids[0], text: "API News error:Source code unchecked in API news Settings: XYZ".into() }]);
        let out = sent(&mut farm_side);
        assert_eq!(out.len(), 2, "only MSFT: {out:?}");
        assert!(out[0].contains("|6008=272093|") && out[0].contains("|264=442|"), "{}", out[0]);
        assert!(out[1].contains("|6008=272093|207=NEWS|167=CS|264=292|6472=DJ-N|6088=Socket|9830=1|"), "{}", out[1]);
    }

    // ibx#458: the news entry of a lost farm goes out again, with a new id,
    // when the farm is back; its old tag no longer binds.
    #[test]
    fn news_entry_is_sent_again_after_a_farm_loss() {
        let (mut engine, shared, mut farm_side, tx) = engine();
        aapl(&tx, &mut engine, "BRFG");
        let _ = sent(&mut farm_side);
        engine.inject_farm_message(b"8=O\x0135=Q\x0125064,3,0.01,0,0,9c,,0,1\x01");
        assert_eq!(engine.farm.news[0].tag, Some(25064));
        engine.farm.handle_disconnect(&mut engine.context, &None);
        assert!(!engine.farm.news[0].live && engine.farm.news[0].tag.is_none());
        let (c, mut farm_side) = socket_pair();
        engine.reconnect_farm(Connection::new_mem(c));
        let out = sent(&mut farm_side);
        assert!(out.iter().any(|m| m == "35=V|263=1|146=1|262=6|6008=265598|207=NEWS|167=CS|264=292|6472=BRFG|6088=Socket|9830=1|"), "{out:?}");
        assert!(shared.market.drain_tick_news().is_empty());
    }
}

#[cfg(test)]
mod sharing_tests {
    use super::*;
    use super::news_tests::{engine, sent, aapl, socket_pair};

    const ALL: &str = "BRFG,BRFUPDN,DJ-N,DJ-RTA,DJ-RTE,DJ-RTG,DJ-RTPRO,DJNL";

    // ibx#444, captured 02/10/2026 (b1_458_news_dup): a second request on
    // a contract with the same news key sends nothing to the farm; the news
    // entry is cancelled with its last request. Another key gets its own
    // entry, as the reference's news tick keeps one per key.
    #[test]
    fn requests_on_one_contract_share_its_entries() {
        let (mut engine, _shared, mut farm_side, tx) = engine();
        let id = aapl(&tx, &mut engine, ALL);
        assert_eq!(sent(&mut farm_side).len(), 2);
        assert_eq!(aapl(&tx, &mut engine, ALL), id);
        assert!(sent(&mut farm_side).is_empty(), "the second request shares the top of book and the news entry");
        tx.send(ControlCommand::SubscribeNews {
            instrument: id, con_id: 265598, exchange: "SMART".into(), sec_type: "STK".into(),
            providers: "DJ-N".into(), refusal: None,
        }).unwrap();
        engine.poll_once();
        let out = sent(&mut farm_side);
        assert_eq!(out, ["35=V|263=1|146=1|262=4|6008=265598|207=NEWS|167=CS|264=292|6472=DJ-N|6088=Socket|9830=1|"]);

        tx.send(ControlCommand::UnsubscribeNews { instrument: id, providers: ALL.into() }).unwrap();
        engine.poll_once();
        assert!(sent(&mut farm_side).is_empty(), "one request still uses the entry");
        tx.send(ControlCommand::UnsubscribeNews { instrument: id, providers: "DJ-N".into() }).unwrap();
        engine.poll_once();
        assert_eq!(sent(&mut farm_side), ["35=V|263=2|146=1|262=4|6008=265598|207=NEWS|167=CS|264=292|6472=DJ-N|9830=1|"]);
        tx.send(ControlCommand::Unsubscribe { instrument: id }).unwrap();
        engine.poll_once();
        let out = sent(&mut farm_side);
        assert_eq!(out.len(), 2, "{out:?}");
        assert!(out[0].starts_with("35=V|263=2|146=2|") && out[1].contains("|262=3|"), "{out:?}");
    }

    // ibx#444: a snapshot joins a stream; a stream asked where a snapshot
    // runs goes out.
    #[test]
    fn a_stream_is_sent_where_only_a_snapshot_runs() {
        let (mut engine, _shared, mut farm_side, tx) = engine();
        let subscribe = |snapshot: bool| ControlCommand::Subscribe {
            con_id: 756733, symbol: "SPY".into(), exchange: "SMART".into(), sec_type: "STK".into(),
            last_trade_date: String::new(), strike: 0.0, right: String::new(), multiplier: String::new(),
            mode_9887: 0, snapshot, reply_tx: None,
        };
        tx.send(subscribe(true)).unwrap();
        engine.poll_once();
        assert!(sent(&mut farm_side)[0].starts_with("35=V|263=3|"));
        tx.send(subscribe(true)).unwrap();
        engine.poll_once();
        assert!(sent(&mut farm_side).is_empty());
        tx.send(subscribe(false)).unwrap();
        engine.poll_once();
        let out = sent(&mut farm_side);
        assert!(out.len() == 1 && out[0].starts_with("35=V|263=1|"), "{out:?}");
    }

    // ibx#444, captured 02/10/2026: the second symbol-only AAPL request is
    // looked up, then joins the first one's subscription: no second top of
    // book or news entry; the client is told, and its news share moves.
    #[test]
    fn a_looked_up_contract_already_subscribed_is_joined() {
        use crate::control::contracts::tests::pipe_msg;
        let (mut engine, shared, mut farm_side, tx) = engine();
        let (c2, _ccp_side) = socket_pair();
        engine.ccp_conn = Some(Connection::new_mem(c2));
        let first = aapl(&tx, &mut engine, ALL);
        let _ = sent(&mut farm_side);
        let (reply, answer) = crossbeam_channel::bounded(1);
        tx.send(ControlCommand::SubscribeBySymbol {
            symbol: "AAPL".into(), sec_type: "STK".into(), exchange: "SMART".into(), currency: "USD".into(),
            filters: Default::default(), mode_9887: 0, snapshot: false, reply_tx: Some(reply),
        }).unwrap();
        engine.poll_once();
        let second = answer.recv().unwrap().unwrap();
        assert_ne!(second, first);
        tx.send(ControlCommand::SubscribeNews {
            instrument: second, con_id: 0, exchange: "SMART".into(), sec_type: "STK".into(),
            providers: ALL.into(), refusal: None,
        }).unwrap();
        engine.poll_once();
        let lookup = format!("{}", engine.context.md_lookups[0].0);
        let mut context = std::mem::replace(&mut engine.context, Context::new());
        let body = format!("35=d|43=N|320={lookup}|322=*|323=4|55=AAPL|167=STK|207=BEST|6008=265598|15=USD|");
        assert!(farm::md_contract_reply(&mut context, &shared, &lookup, &pipe_msg(&body)));
        engine.context = context;
        engine.send_md_resolved();
        assert!(sent(&mut farm_side).is_empty(), "nothing new on the farm");
        assert_eq!(shared.market.drain_md_merges(), [(second, first, 0)]);
        assert_eq!(engine.farm.news.len(), 1);
        assert_eq!(engine.farm.news[0].refs, 2);
        // The client frees the slot it no longer uses.
        tx.send(ControlCommand::Unsubscribe { instrument: second }).unwrap();
        engine.poll_once();
        assert!(sent(&mut farm_side).is_empty());
        assert_eq!(engine.context.market.con_id(second), None, "the slot is freed");
        assert_eq!(engine.context.market.instrument_by_con_id(265598), Some(first));
    }
}

#[cfg(test)]
mod tbt_tests {
    use super::*;
    use std::sync::Arc;
    use std::time::Duration;
    use crate::bridge::SharedState;
    use crate::engine::routing::TableKind;

    fn loopback() -> (Connection, crate::protocol::connection::MemTransport) {
        let (client, server) = crate::protocol::connection::mem_pair();
        (Connection::new_mem(client), server)
    }

    fn plain_sent(server: &mut crate::protocol::connection::MemTransport) -> Vec<String> {
        use std::io::Read;
        server.set_read_timeout(Some(Duration::from_millis(300))).unwrap();
        let mut buf = Vec::new();
        let mut chunk = [0u8; 8192];
        while let Ok(n) = server.read(&mut chunk) {
            if n == 0 { break; }
            buf.extend_from_slice(&chunk[..n]);
        }
        String::from_utf8_lossy(&buf).replace('\x01', "|")
            .split("8=FIX").filter(|m| !m.is_empty()).map(|m| format!("8=FIX{m}")).collect()
    }

    fn vlq(val: u64) -> Vec<u8> {
        let mut v = val;
        let mut groups = vec![(v & 0x7F) as u8];
        v >>= 7;
        while v > 0 {
            groups.push((v & 0x7F) as u8);
            v >>= 7;
        }
        groups.reverse();
        let last = groups.len() - 1;
        groups[last] |= 0x80;
        groups
    }

    fn text(s: &str) -> Vec<u8> {
        if s.is_empty() { return vec![0x80]; }
        let mut out = s.as_bytes().to_vec();
        let last = out.len() - 1;
        out[last] |= 0x80;
        out
    }

    fn tbt_frame(entries: &[u8]) -> Vec<u8> {
        let mut msg = b"8=O\x019=0\x0135=E\x01".to_vec();
        msg.extend_from_slice(&((entries.len() * 8) as u16).to_be_bytes());
        msg.extend_from_slice(entries);
        msg
    }

    fn ack(window_query_id: &str, rt: u32, min_tick: &str, size_min_tick: &str) -> Vec<u8> {
        let xml = format!("<ResultSetTickerId><id>{window_query_id}</id><rtTickerId>{rt}</rtTickerId><minTick>{min_tick}</minTick><sizeMinTick>{size_min_tick}</sizeMinTick><eoq>false</eoq></ResultSetTickerId>");
        fix::fix_build(&[(35, "W"), (6118, &xml)], 1)
    }

    #[allow(clippy::too_many_arguments)]
    fn subscribe_with(engine: &mut HotLoop, tx: &crossbeam_channel::Sender<ControlCommand>, req_id: ReqId, con_id: i64, symbol: &str, exchange: &str, sec_type: &str,
        tbt_type: crate::types::TbtType, number_of_ticks: i32, ignore_size: bool) {
        tx.send(ControlCommand::SubscribeTbt {
            req_id, con_id, symbol: symbol.into(), exchange: exchange.into(), sec_type: sec_type.into(),
            tbt_type, number_of_ticks, ignore_size, reply_tx: None,
        }).unwrap();
        engine.poll_once();
    }

    /// Subscribe with the next request id of the test; the request id.
    fn subscribe(engine: &mut HotLoop, tx: &crossbeam_channel::Sender<ControlCommand>, con_id: i64, symbol: &str, exchange: &str, sec_type: &str, tbt_type: crate::types::TbtType) -> ReqId {
        static NEXT: std::sync::atomic::AtomicI64 = std::sync::atomic::AtomicI64::new(100);
        let req_id = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        subscribe_with(engine, tx, req_id, con_id, symbol, exchange, sec_type, tbt_type, 0, false);
        req_id
    }

    fn trade_entry(rt: u64, time: u64, delta: u64, attribs: u64, size: u64, exchange: &str) -> Vec<u8> {
        [vlq(rt), vlq(time), vlq(delta), vlq(attribs), vlq(size), text(exchange), text("")].concat()
    }

    // ibx#404 (captured, issue comment): the query id is the stream prefix
    // with a counter and the chart name; the contract's own routing exchange (the
    // high-precision book for a currency pair) and security type.
    #[test]
    fn query_names_the_stream_and_the_contract() {
        let mut engine = HotLoop::new(Arc::new(SharedState::new()), None, None);
        let (conn, mut side) = loopback();
        engine.hmds_conn = Some(conn);
        let (tx, rx) = crossbeam_channel::bounded(8);
        engine.set_control_rx(rx);
        subscribe(&mut engine, &tx, 12087792, "EUR", "IDEALPRO", "CASH", crate::types::TbtType::BidAsk);
        subscribe(&mut engine, &tx, 815824267, "MNQ", "CME", "FUT", crate::types::TbtType::AllLast);
        subscribe(&mut engine, &tx, 265598, "AAPL", "SMART", "STK", crate::types::TbtType::Last);
        let sent = plain_sent(&mut side);
        assert_eq!(sent.len(), 3, "{sent:?}");
        assert!(sent[0].contains("<id>rtTicker1;;EUR@IDEALPROBidAsk;;1;;true;;0;;U</id><contractID>12087792</contractID><exchange>FXSUBPIP</exchange><secType>CASH</secType>"), "{}", sent[0]);
        assert!(sent[1].contains("<id>rtTicker2;;MNQ@CMEAllLast;;1;;true;;0;;U</id>") && sent[1].contains("<exchange>CME</exchange><secType>FUT</secType>"), "{}", sent[1]);
        assert!(sent[2].contains("<exchange>BEST</exchange><secType>STK</secType>"), "{}", sent[2]);
    }

    // ibx#404: ticks go to the stream of their id on their farm, read by
    // the stream's type, prices as running sums times the price increment,
    // sizes times the size increment; two streams on one frame each get
    // their own ticks.
    #[test]
    fn ticks_go_to_the_stream_of_their_id() {
        let shared = Arc::new(SharedState::new());
        let mut engine = HotLoop::new(shared.clone(), None, None);
        let (conn, _side) = loopback();
        engine.hmds_conn = Some(conn);
        let (tx, rx) = crossbeam_channel::bounded(8);
        engine.set_control_rx(rx);
        let quotes_req = subscribe(&mut engine, &tx, 815824267, "MNQ", "CME", "FUT", crate::types::TbtType::BidAsk);
        let trades_req = subscribe(&mut engine, &tx, 815824267, "MNQ", "CME", "FUT", crate::types::TbtType::AllLast);
        engine.inject_hmds_message(&ack("rtTicker1;;MNQ@CMEBidAsk;;1;;true;;0;;U", 1, "0.25", "1"));
        engine.inject_hmds_message(&ack("rtTicker2;;MNQ@CMEAllLast;;1;;true;;0;;U", 2, "0.25", "1"));
        let mnq = engine.context.market.instrument_by_con_id(815824267).unwrap();

        let mut e = Vec::new();
        // Stream 2 (trades): 100000 ticks, attributes 12, size 3, CME.
        e.extend(trade_entry(2, 1_790_000_000, 100_000, 12, 3, "CME"));
        // Stream 1 (bid/ask): 99999 / 100001 ticks, sizes 4 / 5.
        e.extend(vlq(1)); e.extend(vlq(1_790_000_001)); e.extend(vlq(99_999)); e.extend(vlq(100_001)); e.extend(vlq(0));
        e.extend(vlq(4)); e.extend(vlq(5));
        // Stream 2 again: one tick down (signed one-byte -1).
        e.extend(vlq(2)); e.extend(vlq(1_790_000_002)); e.push(0xFF); e.extend(vlq(12)); e.extend(vlq(1));
        e.extend(text("CME")); e.extend(text(""));
        engine.inject_hmds_message(&tbt_frame(&e));

        let trades = shared.market.drain_tbt_trades();
        assert_eq!(trades.len(), 2);
        assert_eq!((trades[0].instrument, trades[0].req_id, trades[0].tbt_type), (mnq, trades_req, crate::types::TbtType::AllLast));
        assert_eq!(trades[0].price, 25_000 * PRICE_SCALE);
        assert_eq!(trades[0].size, 3);
        assert_eq!(trades[0].exchange, "CME");
        assert_eq!(trades[1].price, 25_000 * PRICE_SCALE - PRICE_SCALE / 4);
        let quotes = shared.market.drain_tbt_quotes();
        assert_eq!(quotes.len(), 1);
        assert_eq!(quotes[0].req_id, quotes_req);
        assert_eq!((quotes[0].bid, quotes[0].ask), (24_999 * PRICE_SCALE + 3 * PRICE_SCALE / 4, 25_000 * PRICE_SCALE + PRICE_SCALE / 4));
        assert_eq!((quotes[0].bid_size, quotes[0].ask_size), (4, 5));
        assert_eq!(quotes[0].timestamp, 1_790_000_001);

        // The same stream id on another farm is another stream.
        engine.hmds.rx_farm = 5;
        engine.inject_hmds_message(&tbt_frame(&[vlq(1), vlq(1), vlq(1), vlq(1), vlq(0), vlq(1), vlq(1)].concat()));
        engine.hmds.rx_farm = pool::PRIMARY_HMDS;
        assert!(shared.market.drain_tbt_quotes().is_empty());
    }

    // ibx#404 (captured 18/06/2026, seq 11453, ALAB AllLast, minTick 0.01,
    // sizeMinTick 1): attribute bits 0 and 1 are the past limit and
    // unreported flags of the trade (14: unreported), bits 0 and 1 of a
    // quote its bid past low and ask past high; a midpoint stream gives
    // midpoints; no tick before the stream holds a price or a size.
    #[test]
    fn attribute_masks_midpoints_and_the_first_value() {
        let shared = Arc::new(SharedState::new());
        let mut engine = HotLoop::new(shared.clone(), None, None);
        let (conn, _side) = loopback();
        engine.hmds_conn = Some(conn);
        let (tx, rx) = crossbeam_channel::bounded(8);
        engine.set_control_rx(rx);
        let last = subscribe(&mut engine, &tx, 692196414, "ALAB", "SMART", "STK", crate::types::TbtType::AllLast);
        let quotes = subscribe(&mut engine, &tx, 12087792, "EUR", "IDEALPRO", "CASH", crate::types::TbtType::BidAsk);
        let mids = subscribe(&mut engine, &tx, 12087792, "EUR", "IDEALPRO", "CASH", crate::types::TbtType::MidPoint);
        engine.inject_hmds_message(&ack("rtTicker1;;ALAB@SMARTAllLast;;1;;true;;0;;U", 2, "0.01", "1"));
        engine.inject_hmds_message(&ack("rtTicker2;;EUR@IDEALPROBidAsk;;1;;true;;0;;U", 3, "0.00005", "1"));
        engine.inject_hmds_message(&ack("rtTicker3;;EUR@IDEALPROMidPoint;;1;;true;;0;;U", 4, "0.00005", "1"));

        // A first entry with price 0 and size 0 gives nothing; then 61900
        // ticks; then the captured entry: no price change, attributes 14,
        // size 26, FINRA, "  TI".
        let captured: [u8; 20] = [0x00, 0x90, 0x82, 0x06, 0x51, 0x4f, 0x35, 0x86, 0x80, 0x8e, 0x9a, 0x46, 0x49, 0x4e, 0x52, 0xc1, 0x20, 0x20, 0x54, 0xc9];
        let mut e = trade_entry(2, 1_781_783_170, 0, 12, 0, "ARCA");
        e.extend(trade_entry(2, 1_781_783_173, 61_900, 13, 100, "ARCA"));
        e.extend_from_slice(&captured[2..]);
        // Bid/ask with bit 1 (ask past high), then a midpoint.
        e.extend([vlq(3), vlq(1_781_783_175), vlq(23_000), vlq(23_002), vlq(2), vlq(10), vlq(20)].concat());
        e.extend([vlq(4), vlq(1_781_783_176), vlq(23_001), vlq(0), vlq(0)].concat());
        engine.inject_hmds_message(&tbt_frame(&e));

        let trades = shared.market.drain_tbt_trades();
        assert_eq!(trades.len(), 2, "{trades:?}");
        assert_eq!((trades[0].req_id, trades[0].price, trades[0].past_limit, trades[0].unreported), (last, 619 * PRICE_SCALE, true, false));
        assert_eq!((trades[1].price, trades[1].size, trades[1].timestamp), (619 * PRICE_SCALE, 26, 1_781_783_174));
        assert_eq!((trades[1].exchange.as_str(), trades[1].conditions.as_str()), ("FINRA", "TI"));
        assert_eq!((trades[1].past_limit, trades[1].unreported), (false, true));
        let q = shared.market.drain_tbt_quotes();
        assert_eq!(q.len(), 1);
        assert_eq!((q[0].req_id, q[0].bid_past_low, q[0].ask_past_high), (quotes, false, true));
        let m = shared.market.drain_tbt_mid_points();
        assert_eq!(m.len(), 1);
        assert_eq!((m[0].req_id, m[0].timestamp), (mids, 1_781_783_176));
        assert_eq!(m[0].mid_point, (23_001.0 * 0.00005 * PRICE_SCALE as f64).round() as i64);
    }

    // ibx#455: requests for one contract, type and size filter share one
    // stream, as the reference's router: one query, every tick to each; a
    // request joining while the query is out waits for its answer; the
    // size filter makes another stream, its query with the filter element.
    // The stream's cancel waits for its last request, and a request
    // joining in the cancel delay calls the cancel off.
    #[test]
    fn requests_share_a_stream() {
        let shared = Arc::new(SharedState::new());
        let mut engine = HotLoop::new(shared.clone(), None, None);
        let (conn, mut side) = loopback();
        engine.hmds_conn = Some(conn);
        let (tx, rx) = crossbeam_channel::bounded(8);
        engine.set_control_rx(rx);
        use crate::types::TbtType::Last;
        subscribe_with(&mut engine, &tx, 1, 265598, "AAPL", "SMART", "STK", Last, 0, false);
        subscribe_with(&mut engine, &tx, 2, 265598, "AAPL", "SMART", "STK", Last, 0, false);
        subscribe_with(&mut engine, &tx, 3, 265598, "AAPL", "SMART", "STK", Last, 0, true);
        let sent = plain_sent(&mut side);
        assert_eq!(sent.len(), 2, "{sent:?}");
        assert!(!sent[0].contains("<Filter"), "{}", sent[0]);
        assert!(sent[1].contains("<wholeDays>false</wholeDays><Filter varName=\"filter\"><ignoreSize>true</ignoreSize></Filter></Query>"), "{}", sent[1]);

        engine.inject_hmds_message(&ack("rtTicker1;;AAPL@SMARTLast;;1;;true;;0;;U", 1, "0.01", "1"));
        subscribe_with(&mut engine, &tx, 4, 265598, "AAPL", "SMART", "STK", Last, 0, false);
        assert!(plain_sent(&mut side).is_empty(), "an answered stream takes the request");
        engine.inject_hmds_message(&tbt_frame(&trade_entry(1, 1_790_000_000, 25_000, 12, 7, "ISLAND")));
        let reqs: Vec<ReqId> = shared.market.drain_tbt_trades().iter().map(|t| t.req_id).collect();
        assert_eq!(reqs, [1, 2, 4]);

        let t0 = Instant::now();
        for r in [1, 2] {
            tx.send(ControlCommand::UnsubscribeTbt { req_id: r }).unwrap();
        }
        engine.poll_once();
        engine.send_due_tbt_cancels(t0 + Duration::from_secs(16));
        assert!(plain_sent(&mut side).is_empty(), "request 4 is left");
        tx.send(ControlCommand::UnsubscribeTbt { req_id: 4 }).unwrap();
        engine.poll_once();
        subscribe_with(&mut engine, &tx, 5, 265598, "AAPL", "SMART", "STK", Last, 0, false);
        engine.send_due_tbt_cancels(Instant::now() + Duration::from_secs(16));
        assert!(plain_sent(&mut side).is_empty(), "the join called the cancel off, no new query");
        engine.inject_hmds_message(&tbt_frame(&trade_entry(1, 1_790_000_001, 1, 12, 7, "ISLAND")));
        let reqs: Vec<ReqId> = shared.market.drain_tbt_trades().iter().map(|t| t.req_id).collect();
        assert_eq!(reqs, [5]);
    }

    // ibx#455: a refused query ends its own request with 10189; a request
    // that joined while it was out waits, and the next request sends a new
    // query whose answer feeds both.
    #[test]
    fn a_refused_query_ends_only_its_request() {
        let shared = Arc::new(SharedState::new());
        let mut engine = HotLoop::new(shared.clone(), None, None);
        let (conn, mut side) = loopback();
        engine.hmds_conn = Some(conn);
        let (tx, rx) = crossbeam_channel::bounded(8);
        engine.set_control_rx(rx);
        use crate::types::TbtType::BidAsk;
        subscribe_with(&mut engine, &tx, 1, 265598, "AAPL", "SMART", "STK", BidAsk, 0, false);
        subscribe_with(&mut engine, &tx, 2, 265598, "AAPL", "SMART", "STK", BidAsk, 0, false);
        assert_eq!(plain_sent(&mut side).len(), 1);
        let xml = "<QueryError><id>rtTicker1;;AAPL@SMARTBidAsk;;1;;true;;0;;U</id><error>No market data permissions</error></QueryError>";
        engine.inject_hmds_message(&fix::fix_build(&[(35, "W"), (6118, xml)], 1));
        assert_eq!(shared.market.drain_tbt_errors(), [(1, 10189, "Failed to request tick-by-tick data.No market data permissions".to_string())]);

        subscribe_with(&mut engine, &tx, 3, 265598, "AAPL", "SMART", "STK", BidAsk, 0, false);
        let sent = plain_sent(&mut side);
        assert!(sent.len() == 1 && sent[0].contains("<id>rtTicker2;;AAPL@SMARTBidAsk;;1;;true;;0;;U</id>"), "{sent:?}");
        engine.inject_hmds_message(&ack("rtTicker2;;AAPL@SMARTBidAsk;;1;;true;;0;;U", 1, "0.01", "1"));
        engine.inject_hmds_message(&tbt_frame(&[vlq(1), vlq(1_790_000_000), vlq(100), vlq(101), vlq(0), vlq(1), vlq(1)].concat()));
        let reqs: Vec<ReqId> = shared.market.drain_tbt_quotes().iter().map(|q| q.req_id).collect();
        assert_eq!(reqs, [2, 3]);

        // A stream id not above 0 refuses the query's request the same way.
        subscribe_with(&mut engine, &tx, 4, 272093, "MSFT", "SMART", "STK", BidAsk, 0, false);
        engine.inject_hmds_message(&ack("rtTicker3;;MSFT@SMARTBidAsk;;1;;true;;0;;U", 0, "0.01", "1"));
        assert_eq!(shared.market.drain_tbt_errors(), [(4, 10189, "Failed to request tick-by-tick data.Invalid Real-time Query".to_string())]);
    }

    // ibx#455: with a tick count, the past ticks come first as historical
    // ticks to the query's request, read by the stream's type; live ticks
    // that come meanwhile are held and given after the last frame of the
    // past ticks, to every request of the stream.
    #[test]
    fn past_ticks_first_then_the_held_live_ticks() {
        let shared = Arc::new(SharedState::new());
        let mut engine = HotLoop::new(shared.clone(), None, None);
        let (conn, mut side) = loopback();
        engine.hmds_conn = Some(conn);
        let (tx, rx) = crossbeam_channel::bounded(8);
        engine.set_control_rx(rx);
        use crate::types::TbtType::Last;
        subscribe_with(&mut engine, &tx, 1, 265598, "AAPL", "SMART", "STK", Last, 2, false);
        assert!(plain_sent(&mut side)[0].contains("<timeLength>2 t</timeLength>"));
        engine.inject_hmds_message(&ack("rtTicker1;;AAPL@SMARTLast;;1;;true;;0;;U", 1, "0.01", "1"));
        subscribe_with(&mut engine, &tx, 2, 265598, "AAPL", "SMART", "STK", Last, 0, false);
        engine.inject_hmds_message(&tbt_frame(&trade_entry(1, 1_790_000_005, 25_000, 12, 7, "ISLAND")));
        assert!(shared.market.drain_tbt_trades().is_empty(), "held until the past ticks end");

        let xml = "<ResultSetTick><id>rtTicker1;;AAPL@SMARTLast;;1;;true;;0;;U</id><eoq>true</eoq><sizeMinTick>1</sizeMinTick><Events>\
            <Tick><time>20260918-13:30:00</time><price>249.99</price><size>5</size><exch>NASDAQ</exch><cond></cond><flags></flags></Tick>\
            <Tick><time>20260918-13:30:01</time><price>250.00</price><size>3</size><exch>ARCA</exch><cond></cond><flags>U</flags></Tick>\
            </Events></ResultSetTick>";
        engine.inject_hmds_message(&fix::fix_build(&[(35, "W"), (6118, xml)], 1));
        let hist = shared.reference.drain_historical_ticks();
        assert_eq!(hist.len(), 1);
        assert_eq!((hist[0].0, hist[0].3), (1, true));
        match &hist[0].1 {
            crate::types::HistoricalTickData::Last(ticks) => {
                assert_eq!(ticks.len(), 2);
                assert!(ticks[1].tick_attrib_last.unreported);
            }
            other => panic!("trades expected: {other:?}"),
        }
        let reqs: Vec<ReqId> = shared.market.drain_tbt_trades().iter().map(|t| t.req_id).collect();
        assert_eq!(reqs, [1, 2], "the held tick, to both requests");
    }

    // ibx#404: the entry of a stream id with no stream yet is skipped by
    // the reference's guess, the one of a stream that is gone by its
    // type's field count, and the rest of the frame is read.
    #[test]
    fn entries_of_no_live_stream_are_skipped() {
        let shared = Arc::new(SharedState::new());
        let mut engine = HotLoop::new(shared.clone(), None, None);
        let (conn, _side) = loopback();
        engine.hmds_conn = Some(conn);
        let (tx, rx) = crossbeam_channel::bounded(8);
        engine.set_control_rx(rx);
        let gone = subscribe(&mut engine, &tx, 272093, "MSFT", "SMART", "STK", crate::types::TbtType::Last);
        let live = subscribe(&mut engine, &tx, 265598, "AAPL", "SMART", "STK", crate::types::TbtType::Last);
        engine.inject_hmds_message(&ack("rtTicker1;;MSFT@SMARTLast;;1;;true;;0;;U", 3, "0.01", "1"));
        engine.inject_hmds_message(&ack("rtTicker2;;AAPL@SMARTLast;;1;;true;;0;;U", 1, "0.01", "1"));
        tx.send(ControlCommand::UnsubscribeTbt { req_id: gone }).unwrap();
        engine.poll_once();
        engine.send_due_tbt_cancels(Instant::now() + Duration::from_secs(16));
        assert_eq!(engine.hmds.tbt_ended, [(pool::PRIMARY_HMDS, 3, 5)]);

        let t = engine.hmds.tbt_guess_window.0 as u64 + 60;
        let mut e = trade_entry(9, t, 61_900, 12, 100, "ARCA");      // no stream: guessed 5
        e.extend(trade_entry(3, t, 1, 12, 100, "ARCA"));             // gone: 5
        e.extend(trade_entry(1, t, 25_000, 12, 7, "ISLAND"));
        engine.inject_hmds_message(&tbt_frame(&e));
        let trades = shared.market.drain_tbt_trades();
        assert_eq!(trades.len(), 1, "{trades:?}");
        assert_eq!((trades[0].req_id, trades[0].price), (live, 250 * PRICE_SCALE));
    }

    // ibx#455: the reference's limit: the contracts of the answered
    // streams, plus the contracts other than the request's with a query
    // out on the farm; at the limit a request for another contract is
    // 10190, one for a contract with an answered stream is not.
    #[test]
    fn tick_by_tick_limit_counts_streams_and_queries_out() {
        let shared = Arc::new(SharedState::new());
        let mut engine = HotLoop::new(shared.clone(), None, None);
        let (conn, mut side) = loopback();
        engine.hmds_conn = Some(conn);
        let (tx, rx) = crossbeam_channel::bounded(8);
        engine.set_control_rx(rx);
        shared.reference.set_tick_by_tick_limits(3, false);
        use crate::types::TbtType::{Last, BidAsk};
        subscribe_with(&mut engine, &tx, 1, 756733, "SPY", "SMART", "STK", Last, 0, false);
        subscribe_with(&mut engine, &tx, 2, 265598, "AAPL", "SMART", "STK", Last, 0, false);
        engine.inject_hmds_message(&ack("rtTicker1;;SPY@SMARTLast;;1;;true;;0;;U", 1, "0.01", "1"));
        subscribe_with(&mut engine, &tx, 3, 272093, "MSFT", "SMART", "STK", Last, 0, false);
        // SPY answered, AAPL and MSFT out. For AAPL: SPY + MSFT = 2, it
        // may go; for AMD: SPY + AAPL + MSFT = 3, refused; SPY has its
        // answered stream.
        subscribe_with(&mut engine, &tx, 4, 265598, "AAPL", "SMART", "STK", BidAsk, 0, false);
        subscribe_with(&mut engine, &tx, 5, 4391, "AMD", "SMART", "STK", Last, 0, false);
        subscribe_with(&mut engine, &tx, 6, 756733, "SPY", "SMART", "STK", BidAsk, 0, false);
        assert_eq!(shared.market.drain_tbt_errors(), [
            (5, 10190, "Max number of tick-by-tick requests has been reached".to_string()),
        ]);
        assert_eq!(plain_sent(&mut side).len(), 5);

        // A stream waiting for its cancel does not hold its contract: with
        // SPY Last left and SPY BidAsk answered, SPY counts once.
        tx.send(ControlCommand::UnsubscribeTbt { req_id: 1 }).unwrap();
        engine.poll_once();
        engine.inject_hmds_message(&ack("rtTicker5;;SPY@SMARTBidAsk;;1;;true;;0;;U", 2, "0.01", "1"));
        assert!(engine.hmds.tbt_over_limit(4391, Some(pool::PRIMARY_HMDS), 3));
        assert!(!engine.hmds.tbt_over_limit(4391, Some(pool::PRIMARY_HMDS), 4));
        assert!(!engine.hmds.tbt_over_limit(756733, Some(pool::PRIMARY_HMDS), 1));
    }

    // ibx#404: with no client left the cancel of the stream id goes 15 s
    // later, or at once on the stream's next tick, on its farm.
    #[test]
    fn cancel_by_stream_id_after_the_delay_or_the_next_tick() {
        let shared = Arc::new(SharedState::new());
        let mut engine = HotLoop::new(shared.clone(), None, None);
        let (conn, mut side) = loopback();
        engine.hmds_conn = Some(conn);
        let (tx, rx) = crossbeam_channel::bounded(8);
        engine.set_control_rx(rx);
        let mnq = subscribe(&mut engine, &tx, 815824267, "MNQ", "CME", "FUT", crate::types::TbtType::AllLast);
        let eur = subscribe(&mut engine, &tx, 12087792, "EUR", "IDEALPRO", "CASH", crate::types::TbtType::BidAsk);
        engine.inject_hmds_message(&ack("rtTicker1;;MNQ@CMEAllLast;;1;;true;;0;;U", 7, "0.25", "1"));
        engine.inject_hmds_message(&ack("rtTicker2;;EUR@IDEALPROBidAsk;;1;;true;;0;;U", 8, "0.00005", "1"));
        let _ = plain_sent(&mut side);
        let t0 = Instant::now();
        tx.send(ControlCommand::UnsubscribeTbt { req_id: mnq }).unwrap();
        tx.send(ControlCommand::UnsubscribeTbt { req_id: eur }).unwrap();
        engine.poll_once();
        engine.send_due_tbt_cancels(t0);
        assert!(plain_sent(&mut side).is_empty(), "not at once");

        // A tick of stream 7 sends its cancel; it is not delivered.
        engine.inject_hmds_message(&tbt_frame(&trade_entry(7, 1, 4, 12, 1, "CME")));
        assert!(shared.market.drain_tbt_trades().is_empty());
        engine.send_due_tbt_cancels(Instant::now());
        let sent = plain_sent(&mut side);
        assert_eq!(sent.len(), 1, "{sent:?}");
        assert!(sent[0].contains("|35=Z|") && sent[0].contains("<CancelQuery><id>rtTicker:7</id></CancelQuery>"), "{}", sent[0]);

        engine.send_due_tbt_cancels(t0 + Duration::from_secs(14));
        assert!(plain_sent(&mut side).is_empty());
        engine.send_due_tbt_cancels(t0 + Duration::from_secs(16));
        let sent = plain_sent(&mut side);
        assert!(sent.len() == 1 && sent[0].contains("<id>rtTicker:8</id>"), "{sent:?}");
        assert!(engine.hmds.tbt_subscriptions.is_empty());
    }

    // ibx#404: a request cancelled before its answer leaves a stream with
    // no request: after the answer, its first tick starts the cancel
    // delay and its next one sends the cancel, as the reference.
    #[test]
    fn a_stream_left_before_its_answer_is_cancelled_after_its_ticks() {
        let shared = Arc::new(SharedState::new());
        let mut engine = HotLoop::new(shared.clone(), None, None);
        let (conn, mut side) = loopback();
        engine.hmds_conn = Some(conn);
        let (tx, rx) = crossbeam_channel::bounded(8);
        engine.set_control_rx(rx);
        let r = subscribe(&mut engine, &tx, 265598, "AAPL", "SMART", "STK", crate::types::TbtType::Last);
        let _ = plain_sent(&mut side);
        tx.send(ControlCommand::UnsubscribeTbt { req_id: r }).unwrap();
        engine.poll_once();
        engine.send_due_tbt_cancels(Instant::now() + Duration::from_secs(60));
        assert!(plain_sent(&mut side).is_empty(), "nothing to cancel before the answer");
        engine.inject_hmds_message(&ack("rtTicker1;;AAPL@SMARTLast;;1;;true;;0;;U", 1, "0.01", "1"));
        engine.inject_hmds_message(&tbt_frame(&trade_entry(1, 1, 25_000, 12, 1, "ISLAND")));
        engine.send_due_tbt_cancels(Instant::now());
        assert!(plain_sent(&mut side).is_empty(), "the first tick starts the delay");
        engine.inject_hmds_message(&tbt_frame(&trade_entry(1, 2, 1, 12, 1, "ISLAND")));
        engine.send_due_tbt_cancels(Instant::now());
        let sent = plain_sent(&mut side);
        assert!(sent.len() == 1 && sent[0].contains("<id>rtTicker:1</id>"), "{sent:?}");
        assert!(shared.market.drain_tbt_trades().is_empty());
    }

    // ibx#404: with a historical routing table, a currency pair goes to
    // the farm of its row (opened on demand, the query waits); a SMART
    // stock whose group is not known waits for its definition, then goes
    // to the farm of its group; with no tick-by-tick row, 10189 at once.
    #[test]
    fn routed_by_the_historical_table() {
        let shared = Arc::new(SharedState::new());
        let mut engine = HotLoop::new(shared.clone(), None, None);
        let (conn, mut side) = loopback();
        engine.hmds_conn = Some(conn);
        let (ccp, mut ccp_side) = loopback();
        engine.ccp_conn = Some(ccp);
        engine.set_routing_table(TableKind::Historical, "BEST,STK,DayChart|EODChart|Bar5Sec,1,*,cdc1.example,4000,ushmds;\
            BEST,STK,DayChart|EODChart|Bar5Sec,3,*,zdc1.example,4000,euhmds;\
            BEST,STK,TickByTick,3,*,zdc1.example,4000,euhmds;\
            IDEALPRO,CASH,DayChart|EODChart|Bar5Sec,4,*,ndc1.example,4000,cashhmds;\
            IDEALPRO,CASH,TickByTick,4,*,ndc1.example,4000,cashhmds;\
            CME,FUT,DayChart|EODChart|Bar5Sec,-1,*,cdc1.example,4000,ushmds");
        let (tx, rx) = crossbeam_channel::bounded(8);
        engine.set_control_rx(rx);
        subscribe(&mut engine, &tx, 12087792, "EUR", "IDEALPRO", "CASH", crate::types::TbtType::BidAsk);
        let cash = engine.pool.find("cashhmds").expect("cashhmds opened on demand");
        assert_eq!(engine.pool.get(cash).unwrap().queued(), 1);

        subscribe(&mut engine, &tx, 14094, "BMW", "SMART", "STK", crate::types::TbtType::Last);
        let asked = plain_sent(&mut ccp_side);
        assert!(asked.len() == 1 && asked[0].contains("|35=c|") && asked[0].contains("|6008=14094|"), "{asked:?}");
        let req_id = asked[0].split('|').find_map(|p| p.strip_prefix("320=")).unwrap().to_string();
        let reply = fix::fix_build(&[(35, "d"), (320, &req_id), (6008, "14094"), (167, "CS"), (6178, "3")], 1);
        assert!(farm::round_lot_reply(&mut engine.context, &req_id, &reply));
        engine.send_lot_ready();
        engine.poll_once();
        let eu = engine.pool.find("euhmds").expect("group 3 is euhmds");
        assert_eq!(engine.pool.get(eu).unwrap().queued(), 1);
        assert!(plain_sent(&mut side).is_empty(), "nothing on the primary farm");

        // A day chart row but no tick-by-tick row.
        let mnq = subscribe(&mut engine, &tx, 815824267, "MNQ", "CME", "FUT", crate::types::TbtType::AllLast);
        assert_eq!(shared.market.drain_tbt_errors(),
            [(mnq, 10189, "Failed to request tick-by-tick data.AllLast tick-by-tick requests are not supported for MNQ".to_string())]);
        assert!(plain_sent(&mut side).is_empty());
    }
}

#[cfg(test)]
mod bars_routing_tests {
    use super::*;
    use std::sync::Arc;
    use crate::bridge::SharedState;
    use crate::engine::routing::TableKind;

    fn bars(req_id: ReqId, con_id: i64, exchange: &str, sec_type: &str, bar_size: &str) -> ControlCommand {
        ControlCommand::FetchHistorical {
            req_id, con_id, symbol: "X".into(), sec_type: sec_type.into(), exchange: exchange.into(),
            end_date_time: String::new(), duration: "1 D".into(), bar_size: bar_size.into(),
            what_to_show: "MIDPOINT".into(), use_rth: true, keep_up_to_date: false, include_expired: false, format_date: 1,
        }
    }

    // #445: bars go to the farm of their historical row (day charts, or
    // end of day charts for daily bars), opened on demand; the cancel goes
    // to the same farm; with no row the request ends with 162.
    #[test]
    fn bars_routed_by_the_historical_table() {
        let shared = Arc::new(SharedState::new());
        let mut engine = HotLoop::new(shared.clone(), None, None);
        let (client, _server) = crate::protocol::connection::mem_pair();
        engine.hmds_conn = Some(Connection::new_mem(client));
        engine.set_routing_table(TableKind::Historical,
            "IDEALPRO,CASH,DayChart|Bar5Sec,4,*,ndc1.example,4000,cashhmds;IDEALPRO,CASH,EODChart,4,*,ndc1.example,4000,cashhmds2");
        let (tx, rx) = crossbeam_channel::bounded(8);
        engine.set_control_rx(rx);
        tx.send(bars(1, 12087792, "IDEALPRO", "CASH", "1 hour")).unwrap();
        tx.send(bars(2, 12087792, "IDEALPRO", "CASH", "1 day")).unwrap();
        tx.send(bars(3, 815824267, "CME", "FUT", "1 hour")).unwrap();
        engine.poll_once();
        let day = engine.pool.find("cashhmds").expect("day charts farm");
        let eod = engine.pool.find("cashhmds2").expect("end of day farm");
        assert_eq!(engine.pool.get(day).unwrap().queued(), 1);
        assert_eq!(engine.pool.get(eod).unwrap().queued(), 1);
        let errors = shared.reference.drain_historical_errors();
        assert_eq!(errors.len(), 1);
        assert_eq!((errors[0].0, errors[0].1), (3, 162));

        tx.send(ControlCommand::CancelHistorical { req_id: 1 }).unwrap();
        engine.poll_once();
        assert_eq!(engine.pool.get(day).unwrap().queued(), 2, "the cancel goes to the same farm");
    }
}

#[cfg(test)]
mod depth_tests {
    use super::*;
    use std::sync::Arc;
    use std::time::Duration;
    use crate::bridge::SharedState;
    use crate::engine::routing::TableKind;

    const MD_TABLE: &str = "NASDAQ,STK,Top|Deep2|Deep,-1,*,cdc1.example,4000,usfarm;\
        IEX,STK,Top|Deep,-1,*,cdc1.example,4000,usfarm;\
        MEMX,STK,Top,-1,*,cdc1.example,4000,usfarm;\
        BEST,STK,Top,1,*,cdc1.example,4000,usfarm;\
        PSX,STK,Top,-1,*,cdc1.example,4000,usfarm;\
        TPLUS0,STK,Top,-1,*,ndc1.example,4000,usfarm.nj;\
        IBEOS,STK,Top,-1,*,ndc1.example,4000,usfarm.nj;\
        CME,FUT,Top|Deep,-1,*,cdc1.example,4000,usfuture";

    fn loopback() -> (Connection, crate::protocol::connection::MemTransport) {
        let (client, server) = crate::protocol::connection::mem_pair();
        (Connection::new_mem(client), server)
    }

    fn sent(server: &mut crate::protocol::connection::MemTransport) -> Vec<String> {
        use std::io::Read;
        server.set_read_timeout(Some(Duration::from_millis(300))).unwrap();
        let mut buf = Vec::new();
        let mut chunk = [0u8; 8192];
        while let Ok(n) = server.read(&mut chunk) {
            if n == 0 { break; }
            buf.extend_from_slice(&chunk[..n]);
        }
        let mut out = Vec::new();
        let mut rest = &buf[..];
        while let Some(len) = crate::protocol::fixcomp::fixcomp_length(rest) {
            for m in crate::protocol::fixcomp::fixcomp_decompress(&rest[..len]).unwrap() {
                out.push(String::from_utf8_lossy(&m).replace('\x01', "|"));
            }
            rest = &rest[len..];
        }
        out
    }

    fn depth(req_id: ReqId, con_id: i64, exchange: &str, sec_type: &str, rows: i32, smart: bool) -> ControlCommand {
        ControlCommand::SubscribeDepth {
            req_id, con_id, exchange: exchange.into(), sec_type: sec_type.into(), num_rows: rows, is_smart_depth: smart,
        }
    }

    /// An engine with the table, and the definitions of the contracts
    /// known (group and components).
    fn engine() -> (HotLoop, Arc<SharedState>, crate::protocol::connection::MemTransport, crossbeam_channel::Sender<ControlCommand>) {
        let shared = Arc::new(SharedState::new());
        let mut engine = HotLoop::new(shared.clone(), None, None);
        engine.set_farm_name("usfarm".into());
        let (farm, side) = loopback();
        engine.farm_conn = Some(farm);
        engine.set_routing_table(TableKind::MarketData, MD_TABLE);
        for con_id in [265598, 272093, 4391, 9999, 815824267] {
            engine.context.agg_groups.insert(con_id, if con_id == 815824267 { -1 } else { 1 });
        }
        engine.context.valid_exchanges.insert(265598, ["SMART", "NASDAQ", "MEMX", "IEX", "ZZZ", "TPLUS0", "PSX", "IBEOS"]
            .into_iter().map(String::from).collect());
        let (tx, rx) = crossbeam_channel::bounded(16);
        engine.set_control_rx(rx);
        (engine, shared, side, tx)
    }

    fn errors(shared: &SharedState) -> Vec<String> {
        shared.orders.drain_order_errors().into_iter().map(|(id, code, text)| format!("{}:{}:{}", id, code, text)).collect()
    }

    // #452: the reference's local refusals, in its order.
    #[test]
    fn local_refusals() {
        let (mut engine, shared, mut side, tx) = engine();
        tx.send(depth(1, 265598, "", "STK", 5, false)).unwrap();
        tx.send(depth(2, 265598, "NASDAQ", "BAG", 5, false)).unwrap();
        tx.send(depth(3, 265598, "NASDAQ", "STK", 0, false)).unwrap();
        tx.send(depth(4, 265598, "NASDAQ", "STK", 5, false)).unwrap();
        tx.send(depth(4, 265598, "IEX", "STK", 5, false)).unwrap();
        tx.send(depth(5, 272093, "NASDAQ", "STK", 5, false)).unwrap();
        tx.send(depth(6, 4391, "NASDAQ", "STK", 5, false)).unwrap();
        tx.send(depth(7, 9999, "NASDAQ", "STK", 5, false)).unwrap();
        tx.send(depth(8, 4391, "MEMX", "STK", 5, false)).unwrap();
        tx.send(ControlCommand::UnsubscribeDepth { req_id: 77 }).unwrap();
        engine.poll_once();
        assert_eq!(errors(&shared), [
            "1:321:Error validating request.-'bR' : cause - Please enter exchange.",
            "2:321:Error validating request.-'bR' : cause - Market depth does not support combos.",
            "3:321:Error validating request.-'bR' : cause - Market depth rows requested must be greater than zero.",
            "4:322:Error processing request.-'bR' : cause - Duplicate ticker id",
            "7:309:Max number (3) of market depth requests has been reached",
            "8:10092:Deep market data is not supported for this combination of security type/exchange",
            "77:310:Can't find the subscribed market depth with tickerId:77",
        ]);
        assert_eq!(sent(&mut side).len(), 3, "three books");
    }

    // #452: a single book: one depth entry on the farm of its route, with
    // its own request id; the cancel repeats the entry; a farm refusal ends
    // the request with 354, then its cancel is 310.
    #[test]
    fn single_book_entry_cancel_and_refusal() {
        let (mut engine, shared, mut side, tx) = engine();
        tx.send(depth(9, 265598, "NASDAQ", "STK", 5, false)).unwrap();
        engine.poll_once();
        let msgs = sent(&mut side);
        assert_eq!(msgs.len(), 1);
        let id = engine.farm.next_md_req_id - 1;
        assert!(msgs[0].contains(&format!("|263=1|146=1|262={id}|6008=265598|207=NASDAQ|167=CS|264=0|9830=1|")), "{}", msgs[0]);
        tx.send(ControlCommand::UnsubscribeDepth { req_id: 9 }).unwrap();
        engine.poll_once();
        let msgs = sent(&mut side);
        assert!(msgs[0].contains(&format!("|263=2|146=1|262={id}|6008=265598|207=NASDAQ|167=CS|264=0|9830=1|")), "{}", msgs[0]);

        tx.send(depth(10, 815824267, "CME", "FUT", 5, false)).unwrap();
        engine.poll_once();
        let farm = engine.pool.find("usfuture").expect("the future's book farm");
        let id = engine.farm.next_md_req_id - 1;
        engine.farm.rx_farm = farm;
        engine.inject_farm_message(&fix::fix_build(&[(35, "3"), (262, &id.to_string()), (58, "Error")], 1));
        engine.farm.rx_farm = pool::PRIMARY_MD;
        tx.send(ControlCommand::UnsubscribeDepth { req_id: 10 }).unwrap();
        engine.poll_once();
        assert_eq!(errors(&shared), [
            "10:354:Requested market data is not subscribed. Check API status by selecting the Account menu then under \
                Management choose Market Data Subscription Manager and/or availability of delayed data.".to_string(),
            "10:310:Can't find the subscribed market depth with tickerId:10".to_string(),
        ]);
    }

    // #452: SmartDepth takes the contract's valid exchanges: each is looked
    // up first (by conId and exchange, the reference's lookup names); then
    // a book where a depth route exists, the top of book where only a top
    // route exists (not when its order types hold NOT2D, not for SMART),
    // nothing for the others. A top of book's LAST entry goes to BEST,
    // once per request (the others take a farm id only); an overnight
    // venue keeps its own. The depth acknowledgement of a component maps
    // its tag to the client's request.
    #[test]
    fn smart_depth_from_the_components() {
        let (mut engine, _shared, mut side, tx) = engine();
        let (ccp, mut ccp_side) = loopback();
        engine.ccp_conn = Some(ccp);
        let nj = nj_farm(&mut engine);
        tx.send(depth(11, 265598, "SMART", "STK", 5, true)).unwrap();
        engine.poll_once();
        assert!(sent(&mut side).is_empty(), "nothing before the definitions");
        let lookups = ccp_lookups(&mut ccp_side);
        assert_eq!(lookups.iter().map(|(_, e)| e.as_str()).collect::<Vec<_>>(),
            ["BEST", "NASDAQ", "MEMX", "IEX", "ZZZ", "TPLUS0", "PSX", "IBEOS"]);
        assert!(lookups.iter().all(|(n, _)| n.starts_with("SecDefReqMsgReqByConid")));
        for (name, exch) in &lookups {
            let types = if exch == "TPLUS0" { "LMT/3,NOT2D/5" } else { "LMT/3" };
            let msg = if exch == "ZZZ" {
                format!("8=FIX.4.1\x0135=d\x01320={name}\x01322=*\x01323=4\x01146=0\x01")
            } else {
                format!("8=FIX.4.1\x0135=d\x01320={name}\x01322=*\x01323=4\x0155=AAPL\x01167=STK\x01207={exch}\x016008=265598\x01\
                    146=0\x016344=1\x016008=265598\x016432=1\x016430=1/STK\x016431={types}\x01")
            };
            engine.inject_ccp_message(msg.as_bytes());
        }
        engine.start_gathered_depth();
        let msgs = sent(&mut side);
        assert_eq!(msgs.len(), 3, "{msgs:?}");
        assert!(msgs[0].contains("|146=1|") && msgs[0].contains("|207=NASDAQ|167=CS|264=0|9830=1|"), "{}", msgs[0]);
        assert!(msgs[1].contains("|146=1|") && msgs[1].contains("|207=IEX|167=CS|264=0|9830=1|"), "{}", msgs[1]);
        let tops = &msgs[2];
        assert!(tops.contains("|146=3|"), "{tops}");
        assert!(tops.contains("|207=MEMX|167=CS|264=442|6088=Socket|9830=1|") && tops.contains("|207=BEST|167=CS|264=443|6088=Socket|9830=1|")
            && tops.contains("|207=PSX|167=CS|264=442|6088=Socket|9830=1|"), "{tops}");
        assert!(!tops.contains("ZZZ") && !tops.contains("TPLUS0") && !tops.contains("207=IBEOS"), "{tops}");
        // The ids: MEMX, BEST, then PSX after the one its LAST took.
        let ids: Vec<u32> = tops.split("|262=").skip(1).map(|p| p.split('|').next().unwrap().parse().unwrap()).collect();
        assert_eq!((ids[1] - ids[0], ids[2] - ids[1]), (1, 1), "{tops}");
        // The overnight venue on its own farm, both entries on the venue,
        // after the id PSX's LAST took.
        let req = engine.farm.depth_reqs.iter().find(|r| r.req_id == 11).unwrap();
        let on_nj: Vec<(&str, &str, u32)> = req.entries.iter().filter(|e| e.farm == nj)
            .map(|e| (e.exchange.as_str(), e.req_type, e.farm_req)).collect();
        assert_eq!(on_nj, [("IBEOS", "442", ids[2] + 2), ("IBEOS", "443", ids[2] + 3)]);
        let first: u32 = msgs[0].split("|262=").nth(1).unwrap().split('|').next().unwrap().parse().unwrap();
        engine.inject_farm_message(format!("8=O\x0135=Q\x01777,{first},0.01,0,0").as_bytes());
        let req = engine.farm.depth_reqs.iter().find(|r| r.req_id == 11).unwrap();
        assert!(req.smart && req.entries.iter().any(|e| e.server_tag == Some(777) && e.farm_req == first && e.farm == pool::PRIMARY_MD));
    }

    // #452: a cancel while the components are looked up sends nothing; a
    // lookup with no answer in 10 s is left out.
    #[test]
    fn smart_depth_cancel_while_looking_up() {
        let (mut engine, shared, mut side, tx) = engine();
        let (ccp, mut ccp_side) = loopback();
        engine.ccp_conn = Some(ccp);
        tx.send(depth(12, 265598, "SMART", "STK", 5, true)).unwrap();
        engine.poll_once();
        assert_eq!(ccp_lookups(&mut ccp_side).len(), 8);
        tx.send(depth(12, 265598, "SMART", "STK", 5, true)).unwrap();
        tx.send(ControlCommand::UnsubscribeDepth { req_id: 12 }).unwrap();
        engine.poll_once();
        assert_eq!(errors(&shared), ["12:322:Error processing request.-'bR' : cause - Duplicate ticker id"]);
        assert!(engine.context.depth_gathers.is_empty() && sent(&mut side).is_empty());

        tx.send(depth(13, 265598, "SMART", "STK", 5, true)).unwrap();
        engine.poll_once();
        let lookups = ccp_lookups(&mut ccp_side);
        let (name, _) = lookups.iter().find(|(_, e)| e == "IEX").unwrap();
        engine.inject_ccp_message(format!("8=FIX.4.1\x0135=d\x01320={name}\x01322=*\x01323=4\x0155=AAPL\x01167=STK\x01\
            207=IEX\x016008=265598\x01146=0\x01").as_bytes());
        engine.start_gathered_depth();
        assert!(sent(&mut side).is_empty(), "waits for the other lookups");
        engine.context.depth_gathers[0].deadline = Instant::now();
        engine.start_gathered_depth();
        let msgs = sent(&mut side);
        assert!(msgs.len() == 1 && msgs[0].contains("|146=1|") && msgs[0].contains("|207=IEX|167=CS|264=0|"), "{msgs:?}");
    }

    // #452: a second request on a book already on the wire shares it, as
    // the reference shares its book: no new entry; the book the farm sent
    // is given at once; one cancel leaves the book to the other request,
    // the last one cancels it. A book the farm refused ends a new single
    // request at once, with the contract in the text.
    #[test]
    fn requests_share_a_book() {
        let (mut engine, shared, mut side, tx) = engine();
        engine.context.depth_descriptions.insert(265598, "AAPL NASDAQ.NMS".into());
        tx.send(depth(20, 265598, "IEX", "STK", 5, false)).unwrap();
        engine.poll_once();
        assert_eq!(sent(&mut side).len(), 1);
        let id = engine.farm.next_md_req_id - 1;
        engine.inject_farm_message(format!("8=O\x0135=Q\x01900,{id},0.01,0,0").as_bytes());
        // Insert at 0: bid price 10.00, bid size 3.
        engine.inject_farm_message(&depth_msg(900, &[0x00, 0x00, 0x05, 0x03, 0xE8, 0x80, 0x03]));
        assert_eq!(shared.market.drain_depth_updates().len(), 1);
        tx.send(depth(21, 265598, "IEX", "STK", 5, false)).unwrap();
        engine.poll_once();
        assert!(sent(&mut side).is_empty(), "no new entry");
        let rows = shared.market.drain_depth_updates();
        assert_eq!(rows.iter().map(|u| (u.req_id, u.position, u.operation, u.side, u.price)).collect::<Vec<_>>(), [(21, 0, 0, 1, 10.0)]);
        // Insert at 0: bid price 11.00: both requests.
        engine.inject_farm_message(&depth_msg(900, &[0x00, 0x00, 0x05, 0x04, 0x4C, 0x80, 0x03]));
        let mut ids: Vec<i64> = shared.market.drain_depth_updates().iter().map(|u| u.req_id).collect();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids, [20, 21]);
        tx.send(ControlCommand::UnsubscribeDepth { req_id: 20 }).unwrap();
        engine.poll_once();
        assert!(sent(&mut side).is_empty(), "the other request has the book");
        tx.send(ControlCommand::UnsubscribeDepth { req_id: 21 }).unwrap();
        engine.poll_once();
        let msgs = sent(&mut side);
        assert!(msgs.len() == 1 && msgs[0].contains(&format!("|263=2|146=1|262={id}|6008=265598|207=IEX|")), "{msgs:?}");

        // A SmartDepth request keeps a component book the farm refused; a
        // new single request on it ends at once.
        let msgs = engine.farm.start_depth(22, 265598, "STK", true, 5, 1, false,
            vec![farm::DepthSpec::Book { farm: pool::PRIMARY_MD, exchange: "IEX".into() }], None, &shared);
        engine.send_farm_messages(msgs);
        assert_eq!(sent(&mut side).len(), 1);
        let id = engine.farm.next_md_req_id - 1;
        engine.inject_farm_message(&fix::fix_build(&[(35, "3"), (262, &id.to_string()), (58, "Error&IEX/STK/Deep"), (6763, "114,210")], 1));
        assert!(sent(&mut side).is_empty(), "a refused component stays");
        assert_eq!(errors(&shared), ["22:2152:Need additional market data permissions - Depth: IEX; "]);
        tx.send(depth(23, 265598, "IEX", "STK", 5, false)).unwrap();
        engine.poll_once();
        assert!(sent(&mut side).is_empty(), "the SmartDepth request has the book");
        assert_eq!(errors(&shared), ["23:10089:Requested market data requires additional subscription for API. See link in \
            'Market Data Connections' dialog for more details.AAPL NASDAQ.NMS/DEEP"]);
        tx.send(ControlCommand::UnsubscribeDepth { req_id: 22 }).unwrap();
        engine.poll_once();
        let msgs = sent(&mut side);
        assert!(msgs.len() == 1 && msgs[0].contains(&format!("|263=2|146=1|262={id}|")), "{msgs:?}");
    }

    // #452: SmartDepth that the contract does not allow (here a future):
    // the reference keeps the request but sends nothing and gives no error
    // (its single-exchange path does not find the request).
    #[test]
    fn smart_depth_not_allowed_sends_nothing() {
        let (mut engine, shared, mut side, tx) = engine();
        tx.send(depth(30, 815824267, "CME", "FUT", 5, true)).unwrap();
        tx.send(depth(30, 815824267, "CME", "FUT", 5, true)).unwrap();
        engine.poll_once();
        assert!(sent(&mut side).is_empty());
        assert_eq!(errors(&shared), ["30:322:Error processing request.-'bR' : cause - Duplicate ticker id"]);
        tx.send(ControlCommand::UnsubscribeDepth { req_id: 30 }).unwrap();
        engine.poll_once();
        assert!(sent(&mut side).is_empty() && errors(&shared).is_empty());
    }

    /// The farm of the overnight venues, opened on demand, with a link.
    fn nj_farm(engine: &mut HotLoop) -> pool::FarmId {
        let nj = engine.pool.ensure("usfarm.nj", "ndc1.example", FarmKind::MarketData, Instant::now());
        let (conn, peer) = loopback();
        engine.pool.get_mut(nj).unwrap().conn = Some(conn);
        std::mem::forget(peer);
        nj
    }

    /// The definition lookups sent on the CCP link: (name, exchange).
    fn ccp_lookups(server: &mut crate::protocol::connection::MemTransport) -> Vec<(String, String)> {
        use std::io::Read;
        server.set_read_timeout(Some(Duration::from_millis(300))).unwrap();
        let mut buf = Vec::new();
        let mut chunk = [0u8; 8192];
        while let Ok(n) = server.read(&mut chunk) {
            if n == 0 { break; }
            buf.extend_from_slice(&chunk[..n]);
        }
        let text = String::from_utf8_lossy(&buf).to_string();
        let mut out = Vec::new();
        let mut name = None;
        for field in text.split('\x01') {
            if let Some(v) = field.strip_prefix("320=") {
                name = Some(v.to_string());
            } else if let (Some(v), Some(n)) = (field.strip_prefix("6004="), name.as_ref()) {
                out.push((n.clone(), v.to_string()));
                name = None;
            }
        }
        out
    }

    // #452: the reference's SmartDepth requests replayed from the frames it
    // sent and received (AAPL then AXTI, 28/09/2026, paper): the component
    // lookups answered with the reference's definitions, the farm's
    // acknowledgements and refusals of the engine's entries in the
    // reference's order; the engine sends the reference's books and tops
    // and gives the warning 2152 the reference gave, word for word. AAPL's
    // BEST LAST entry was held by another client of the reference, so the
    // reference did not send it; the engine does.
    #[test]
    fn smart_depth_status_as_the_reference() {
        use base64::Engine as _;
        let b64 = |v: &serde_json::Value| base64::engine::general_purpose::STANDARD.decode(v.as_str().unwrap()).unwrap();
        let path = format!("{}/tests/fixtures/gw1040/scenarios/20260928/depth_smart_status.jsonl", env!("CARGO_MANIFEST_DIR"));
        let text = std::fs::read_to_string(path).unwrap();
        let recs: Vec<serde_json::Value> = text.lines().skip(1).map(|l| serde_json::from_str(l).unwrap()).collect();
        let shared = Arc::new(SharedState::new());
        let mut engine = HotLoop::new(shared.clone(), None, None);
        engine.set_farm_name("usfarm".into());
        engine.set_user_book(true);
        let (farm, mut side) = loopback();
        engine.farm_conn = Some(farm);
        let (ccp, mut ccp_side) = loopback();
        engine.ccp_conn = Some(ccp);
        engine.set_routing_table(TableKind::MarketData, CAPTURED_US_TABLE);
        let nj = engine.pool.ensure("usfarm.nj", "ndc1.ibllc.com", FarmKind::MarketData, Instant::now());
        let (nj_conn, mut nj_side) = loopback();
        engine.pool.get_mut(nj).unwrap().conn = Some(nj_conn);
        let (tx, rx) = crossbeam_channel::bounded(16);
        engine.set_control_rx(rx);
        let fields = |r: &serde_json::Value| -> Vec<(String, String)> {
            let raw = b64(&r["raw_b64"]);
            String::from_utf8_lossy(&raw).split('\x01').filter_map(|f| f.split_once('=')).map(|(t, v)| (t.to_string(), v.to_string())).collect()
        };
        let get = |f: &[(String, String)], tag: &str| f.iter().find(|(t, _)| t == tag).map(|(_, v)| v.clone()).unwrap_or_default();
        // The entries of each farm message: (exchange, request type).
        let entries_of = |msg: &str| -> Vec<(String, String)> {
            let mut out = Vec::new();
            let mut exch = String::new();
            for f in msg.split('|') {
                if let Some(v) = f.strip_prefix("207=") { exch = v.to_string(); }
                if let Some(v) = f.strip_prefix("264=") { out.push((exch.clone(), v.to_string())); }
            }
            out
        };
        let mut ours_2152: Vec<String> = Vec::new();
        let mut theirs_2152: Vec<String> = Vec::new();
        // Recorded (conn, farm id) -> (exchange, request type).
        let mut recorded: std::collections::HashMap<(String, String), (String, String)> = std::collections::HashMap::new();
        for r in &recs {
            if r["leg"] == "fix_out" && r["msg_type"] == "V" {
                let f = fields(r);
                let mut id = String::new();
                let mut exch = String::new();
                for (t, v) in &f {
                    match t.as_str() {
                        "262" => id = v.clone(),
                        "207" => exch = v.clone(),
                        "264" => { recorded.insert((r["conn"].as_str().unwrap().to_string(), id.clone()), (exch.clone(), v.clone())); }
                        _ => {}
                    }
                }
            }
        }
        let mut k = 0;
        while k < recs.len() {
            let r = &recs[k];
            let (leg, conn, name) = (r["leg"].as_str().unwrap(), r["conn"].as_str().unwrap_or(""), r["msg_name"].as_str().unwrap_or(""));
            if leg == "api_out" && name == "REQ_MKT_DEPTH" {
                let f = proto_fields(&b64(&r["body_b64"]));
                let req_id = f.iter().find(|x| x.0 == 1).unwrap().1 as ReqId;
                let num_rows = f.iter().find(|x| x.0 == 3).map_or(100, |x| x.1 as i32);
                // The contract: the conId of the reference's lookups that follow.
                let first = recs[k..].iter().find(|x| x["leg"] == "fix_in" && x["msg_type"] == "d").unwrap();
                let con_id: i64 = get(&fields(first), "6008").parse().unwrap();
                // The engine knows the contract from its definition.
                let best = recs[k..].iter().find(|x| x["leg"] == "fix_in" && x["msg_type"] == "d" && get(&fields(x), "207") == "BEST").unwrap();
                farm::note_definition(&mut engine.context, con_id, &b64(&best["raw_b64"]));
                // As a refusal names it (captured 25/09/2026: "...AAPL NASDAQ.NMS/DEEP").
                if con_id == 265598 {
                    assert_eq!(engine.context.depth_descriptions[&con_id], "AAPL NASDAQ.NMS");
                }
                tx.send(depth(req_id, con_id, "SMART", "STK", num_rows, true)).unwrap();
                engine.poll_once();
                let ours: std::collections::HashMap<String, String> = ccp_lookups(&mut ccp_side).into_iter().map(|(n, e)| (e, n)).collect();
                assert_eq!(ours.len(), 27, "one lookup per valid exchange");
                // The reference's definitions, under the engine's names.
                let mut j = k + 1;
                while j < recs.len() && recs[j]["msg_name"] != "REQ_MKT_DEPTH" {
                    let x = &recs[j];
                    if x["leg"] == "fix_in" && x["msg_type"] == "d" {
                        let raw = String::from_utf8_lossy(&b64(&x["raw_b64"])).to_string();
                        let theirs = get(&fields(x), "320");
                        let ours_name = &ours[&get(&fields(x), "207")];
                        engine.inject_ccp_message(raw.replace(&format!("320={theirs}\x01"), &format!("320={ours_name}\x01")).as_bytes());
                    }
                    j += 1;
                }
                engine.start_gathered_depth();
                // The engine's books and tops, as the reference's.
                let mut ours_sent: Vec<(String, String)> = sent(&mut side).iter().chain(sent(&mut nj_side).iter()).flat_map(|m| entries_of(m)).collect();
                let mut theirs_sent: Vec<(String, String)> = recs[k..j].iter()
                    .filter(|x| x["leg"] == "fix_out" && x["msg_type"] == "V" && get(&fields(x), "263") == "1")
                    .flat_map(|x| entries_of(&String::from_utf8_lossy(&b64(&x["raw_b64"])).replace('\x01', "|")))
                    .collect();
                if !theirs_sent.iter().any(|(e, t)| e == "BEST" && t == "443") {
                    theirs_sent.push(("BEST".into(), "443".into()));
                }
                ours_sent.sort();
                theirs_sent.sort();
                assert_eq!(ours_sent, theirs_sent, "request {req_id}");
                k += 1;
                continue;
            }
            if leg == "api_out" && name == "CANCEL_MKT_DEPTH" {
                let f = proto_fields(&b64(&r["body_b64"]));
                tx.send(ControlCommand::UnsubscribeDepth { req_id: f.iter().find(|x| x.0 == 1).unwrap().1 as ReqId }).unwrap();
                engine.poll_once();
                let mut ours_sent: Vec<(String, String)> = sent(&mut side).iter().chain(sent(&mut nj_side).iter())
                    .flat_map(|m| entries_of(m)).collect();
                ours_sent.sort();
                ours_sent.dedup();
                assert_eq!(ours_sent.len(), 6 + 15 + 3, "every book, top and LAST entry cancelled: {ours_sent:?}");
            } else if leg == "api_in" && name == "ERR_MSG" {
                let f = proto_fields(&b64(&r["body_b64"]));
                if f.iter().any(|x| x.0 == 3 && x.1 == 2152) {
                    theirs_2152.push(String::from_utf8(f.iter().find(|x| x.0 == 4).unwrap().2.clone()).unwrap());
                }
            } else if leg == "fix_in" && (conn == "usfarm" || conn == "usfarm.nj") && (r["msg_type"] == "Q" || r["msg_type"] == "3") {
                engine.farm.rx_farm = if conn == "usfarm" { pool::PRIMARY_MD } else { nj };
                let f = fields(r);
                let raw = String::from_utf8_lossy(&b64(&r["raw_b64"])).to_string();
                let theirs_id = if r["msg_type"] == "3" {
                    get(&f, "262")
                } else {
                    raw.split("35=Q\x01").nth(1).unwrap().split(',').nth(1).unwrap().to_string()
                };
                // The engine's entry of the reference's id.
                let Some((exch, kind)) = recorded.get(&(conn.to_string(), theirs_id.clone())) else { k += 1; continue };
                let ours_id = engine.farm.depth_reqs.iter().flat_map(|q| q.entries.iter())
                    .find(|e| e.live && &e.exchange == exch && e.req_type == kind.as_str())
                    .map(|e| e.farm_req.to_string());
                if let Some(id) = ours_id {
                    if r["msg_type"] == "3" {
                        engine.inject_farm_message(&fix::fix_build(&[(35, "3"), (262, &id), (58, &get(&f, "58")), (6763, &get(&f, "6763"))], 1));
                    } else {
                        let body = raw.split("35=Q\x01").nth(1).unwrap().split("\x018349=").next().unwrap().to_string();
                        let mut parts: Vec<String> = body.split(',').map(String::from).collect();
                        parts[1] = id;
                        engine.inject_farm_message(format!("8=O\x0135=Q\x01{}\x01", parts.join(",")).as_bytes());
                    }
                }
                engine.farm.rx_farm = pool::PRIMARY_MD;
            }
            for (_, code, text) in shared.orders.drain_order_errors() {
                if code == 2152 { ours_2152.push(text); }
            }
            k += 1;
        }
        assert_eq!(theirs_2152.len(), 2);
        assert_eq!(ours_2152, theirs_2152);
    }

    /// The US stock rows of the market data routing table of the session
    /// of 28/09/2026 (the exchanges of AAPL's and AXTI's valid exchanges).
    const CAPTURED_US_TABLE: &str = "AMEX,STK,Top,-1,*,cdc1.ibllc.com,4000,usfarm;\
        ARCA,STK,Top|Deep,-1,*,cdc1.ibllc.com,4000,usfarm;\
        BATS,STK,Top|Deep,-1,*,cdc1.ibllc.com,4000,usfarm;\
        BEST,STK,AggDeep,1,OTCBB,cdc1.ibllc.com,4000,usfarm;\
        BEST,STK,AggDeep,1,PINK,cdc1.ibllc.com,4000,usfarm;\
        BEST,STK,Top,1,*,cdc1.ibllc.com,4000,usfarm;\
        BEX,STK,Top|Deep,-1,*,cdc1.ibllc.com,4000,usfarm;\
        BYX,STK,Top,-1,*,cdc1.ibllc.com,4000,usfarm;\
        CHX,STK,Top,-1,*,cdc1.ibllc.com,4000,usfarm;\
        DRCTEDGE,STK,Top,-1,*,cdc1.ibllc.com,4000,usfarm;\
        EDGEA,STK,Top,-1,*,cdc1.ibllc.com,4000,usfarm;\
        IBEOS,STK,Top,-1,*,ndc1.ibllc.com,4000,usfarm.nj;\
        IEX,STK,Top|Deep,-1,*,cdc1.ibllc.com,4000,usfarm;\
        ISE,STK,Top,-1,*,cdc1.ibllc.com,4000,usfarm;\
        ISLAND,STK,Top|Deep2|Deep,-1,*,cdc1.ibllc.com,4000,usfarm;\
        LTSE,STK,Top,-1,*,cdc1.ibllc.com,4000,usfarm;\
        MEMX,STK,Top,-1,*,cdc1.ibllc.com,4000,usfarm;\
        NASDAQ,STK,Top|Deep2|Deep,-1,*,cdc1.ibllc.com,4000,usfarm;\
        NYSE,STK,Top|Deep,-1,*,cdc1.ibllc.com,4000,usfarm;\
        NYSENAT,STK,Top,-1,*,cdc1.ibllc.com,4000,usfarm;\
        OVERNIGHT,STK,Top,-1,*,ndc1.ibllc.com,4000,usfarm.nj;\
        PEARL,STK,Top,-1,*,cdc1.ibllc.com,4000,usfarm;\
        PSX,STK,Top,-1,*,cdc1.ibllc.com,4000,usfarm;\
        T24X,STK,Top,-1,*,cdc1.ibllc.com,4000,usfarm;\
        TPLUS0,STK,Top,-1,*,ndc1.ibllc.com,4000,usfarm.nj;\
        TXSE,STK,Top,-1,*,cdc1.ibllc.com,4000,usfarm";

    /// A depth message of one group: `entries` as raw bytes after the tag.
    fn depth_msg(tag: u32, entries: &[u8]) -> Vec<u8> {
        let bits = (4 + entries.len()) * 8;
        let mut m = b"8=O\x0135=Y\x01".to_vec();
        m.extend_from_slice(&[(bits >> 8) as u8, bits as u8]);
        m.extend_from_slice(&tag.to_be_bytes());
        m.extend_from_slice(entries);
        m.push(0x01);
        m
    }

    // #451: an update of a row the book does not have resets it, as the
    // reference: 317, the entry cancelled, asked again after the delay with
    // a new id, and the next data gives the whole book.
    #[test]
    fn refused_entry_resets_the_book() {
        let (mut engine, shared, mut side, tx) = engine();
        tx.send(depth(12, 265598, "IEX", "STK", 5, false)).unwrap();
        engine.poll_once();
        assert_eq!(sent(&mut side).len(), 1);
        let id = engine.farm.next_md_req_id - 1;
        engine.inject_farm_message(format!("8=O\x0135=Q\x01500,{id},0.01,0,0").as_bytes());
        // Update at 0, bid size 1: no such row.
        engine.inject_farm_message(&depth_msg(500, &[0x10, 0x00, 0x80, 0x01]));
        assert_eq!(errors(&shared), [format!("12:317:{}", crate::engine::depth_book::RESET_TEXT)]);
        let msgs = sent(&mut side);
        assert!(msgs.len() == 1 && msgs[0].contains(&format!("|263=2|146=1|262={id}|6008=265598|207=IEX|167=CS|264=0|9830=1|")), "{msgs:?}");
        assert!(engine.farm.sweep_depth(&shared).is_empty(), "asked again before the delay");
        for e in engine.farm.depth_reqs.iter_mut().flat_map(|r| r.entries.iter_mut()) {
            e.resubscribe_at = Some(Instant::now() - Duration::from_millis(1));
        }
        let again = engine.farm.sweep_depth(&shared);
        engine.send_farm_messages(again);
        let msgs = sent(&mut side);
        let id2 = engine.farm.next_md_req_id - 1;
        assert!(id2 != id && msgs.len() == 1 && msgs[0].contains(&format!("|263=1|146=1|262={id2}|6008=265598|207=IEX|")), "{msgs:?}");
        engine.inject_farm_message(format!("8=O\x0135=Q\x01501,{id2},0.01,0,0").as_bytes());
        // Insert at 0: bid price 10.00 (2 bytes), bid size 3.
        engine.inject_farm_message(&depth_msg(501, &[0x00, 0x00, 0x05, 0x03, 0xE8, 0x80, 0x03]));
        let rows = shared.market.drain_depth_updates();
        assert_eq!(rows.len(), 1);
        let u = &rows[0];
        assert_eq!((u.req_id, u.position, u.operation, u.side, u.price, u.size, u.l2), (12, 0, 0, 1, 10.0, 3.0, false));
    }

    // #451: a book with market makers (the Deep2 service of the routing
    // table) is written with updateMktDepthL2 and its market makers; a
    // 35=P message gives no depth row.
    #[test]
    fn market_maker_book_is_level_two() {
        let (mut engine, shared, mut side, tx) = engine();
        tx.send(depth(13, 265598, "NASDAQ", "STK", 5, false)).unwrap();
        engine.poll_once();
        sent(&mut side);
        let id = engine.farm.next_md_req_id - 1;
        engine.inject_farm_message(format!("8=O\x0135=Q\x01600,{id},0.01,0,0").as_bytes());
        // Insert at 0 with market maker NSDQ: ask price 1.00, ask size 2.
        let mut entry = vec![0x04];
        entry.extend_from_slice(b"NSDQ");
        entry.extend_from_slice(&[0x00, 0x25, 0x00, 0x64, 0xA0, 0x02]);
        engine.inject_farm_message(&depth_msg(600, &entry));
        let rows = shared.market.drain_depth_updates();
        assert_eq!(rows.len(), 1);
        let u = &rows[0];
        assert_eq!((u.position, u.operation, u.side, u.price, u.size, u.market_maker.as_str(), u.l2, u.is_smart_depth),
            (0, 0, 0, 1.0, 2.0, "NSDQ", true, false));
        // A tick message on the book's tag (bid price 5) is not depth.
        let mut tick = b"8=O\x0135=P\x01".to_vec();
        tick.extend_from_slice(&[0x00, 0x30, 0x00, 0x00, 0x02, 0x58, 0x00, 0x05, 0x01]);
        engine.inject_farm_message(&tick);
        assert!(shared.market.drain_depth_updates().is_empty());
    }

    /// Fields of a protobuf message: (field number, varint, bytes).
    fn proto_fields(b: &[u8]) -> Vec<(u64, u64, Vec<u8>)> {
        fn varint(b: &[u8], i: &mut usize) -> u64 {
            let (mut v, mut shift) = (0u64, 0);
            loop {
                let x = b[*i];
                *i += 1;
                v |= u64::from(x & 0x7f) << shift;
                shift += 7;
                if x & 0x80 == 0 { return v; }
            }
        }
        let mut out = Vec::new();
        let mut i = 0;
        while i < b.len() {
            let key = varint(b, &mut i);
            match key & 7 {
                0 => { let v = varint(b, &mut i); out.push((key >> 3, v, Vec::new())); }
                1 => { out.push((key >> 3, 0, b[i..i + 8].to_vec())); i += 8; }
                2 => {
                    let n = varint(b, &mut i) as usize;
                    out.push((key >> 3, 0, b[i..i + n].to_vec()));
                    i += n;
                }
                5 => { out.push((key >> 3, 0, b[i..i + 4].to_vec())); i += 4; }
                w => panic!("wire type {w}"),
            }
        }
        out
    }

    /// One depth callback, as compared: request, position, operation,
    /// side, price, size, market maker, SmartDepth, level two.
    type Callback = (i64, i32, i32, i32, f64, f64, String, bool, bool);

    /// #451: replay a recorded reference scenario through the engine: the
    /// depth requests with the entries the reference sent, the farm's
    /// acknowledgements, refusals, 35=Y and 35=P frames as recorded (the
    /// farm ids mapped to the engine's), and the callbacks the engine gives,
    /// next to the callbacks the reference gave the API client.
    fn replay_depth(path: &str) -> (Vec<Callback>, Vec<Callback>) {
        use base64::Engine as _;
        let b64 = |v: &serde_json::Value| base64::engine::general_purpose::STANDARD.decode(v.as_str().unwrap()).unwrap();
        let text = std::fs::read_to_string(path).unwrap();
        let recs: Vec<serde_json::Value> = text.lines().skip(1).map(|l| serde_json::from_str(l).unwrap()).collect();
        let farm_of = |conn: &str| -> pool::FarmId { if conn == "usfarm" { pool::PRIMARY_MD } else { 1 } };
        let shared = Arc::new(SharedState::new());
        let mut engine = HotLoop::new(shared.clone(), None, None);
        // A paper session: the user book is on (6247=demo).
        engine.set_user_book(true);
        let mut lots: std::collections::HashMap<i64, i64> = std::collections::HashMap::new();
        let mut ours: Vec<Callback> = Vec::new();
        let mut theirs: Vec<Callback> = Vec::new();
        // Recorded (conn, farm id) -> (exchange, request type).
        let mut sent: std::collections::HashMap<(String, String), (String, String)> = std::collections::HashMap::new();
        let drain = |shared: &SharedState, ours: &mut Vec<Callback>| {
            for u in shared.market.drain_depth_updates() {
                ours.push((u.req_id, u.position, u.operation, u.side, u.price, u.size, u.market_maker, u.is_smart_depth, u.l2));
            }
        };
        // The round lots, from the definitions the reference read (the
        // engine asks for them before it subscribes).
        for r in recs.iter().filter(|r| r["leg"] == "fix_in" && r["msg_type"] == "d") {
            let raw = b64(&r["raw_b64"]);
            if let Some(con_id) = fix::fix_parse(&raw).get(&6008).and_then(|v| v.parse::<i64>().ok()) {
                lots.entry(con_id).or_insert(crate::control::contracts::round_lot_from_secdef(&raw));
            }
        }
        for (k, r) in recs.iter().enumerate() {
            let (leg, conn) = (r["leg"].as_str().unwrap(), r["conn"].as_str().unwrap_or(""));
            let name = r["msg_name"].as_str().unwrap_or("");
            let mt = r["msg_type"].as_str().unwrap_or("");
            if leg == "api_out" && name == "REQ_MKT_DEPTH" {
                let f = proto_fields(&b64(&r["body_b64"]));
                let req_id = f.iter().find(|x| x.0 == 1).unwrap().1 as ReqId;
                let num_rows = f.iter().find(|x| x.0 == 3).map_or(100, |x| x.1 as i32);
                let smart = f.iter().any(|x| x.0 == 4 && x.1 == 1);
                // The entries the reference sent for this request, up to its cancel.
                let mut entries: Vec<(pool::FarmId, String, &str)> = Vec::new();
                let mut con_id = 0;
                for later in &recs[k + 1..] {
                    if later["msg_name"] == "CANCEL_MKT_DEPTH" { break; }
                    if later["leg"] != "fix_out" || later["msg_type"] != "V" { continue; }
                    let fields: Vec<(String, String)> = later["fields"].as_array().unwrap().iter()
                        .map(|p| (p[0].as_str().unwrap().to_string(), p[1].as_str().unwrap().to_string())).collect();
                    if !fields.iter().any(|(t, v)| t == "263" && v == "1") { continue; }
                    let lconn = later["conn"].as_str().unwrap();
                    let (mut id, mut exch) = (String::new(), String::new());
                    for (t, v) in &fields {
                        match t.as_str() {
                            "262" => id = v.clone(),
                            "6008" => con_id = v.parse().unwrap(),
                            "207" => exch = v.clone(),
                            "264" => {
                                sent.insert((lconn.to_string(), id.clone()), (exch.clone(), v.clone()));
                                let farm = farm_of(lconn);
                                match v.as_str() {
                                    "0" => entries.push((farm, exch.clone(), "0")),
                                    "442" => entries.push((farm, exch.clone(), "442")),
                                    "443" => entries.push((farm, exch.clone(), "443")),
                                    _ => {}
                                }
                            }
                            _ => {}
                        }
                    }
                }
                // A top of book's LAST entry: on its own exchange when the
                // reference sent one there, else on BEST.
                let specs: Vec<farm::DepthSpec> = entries.iter().filter_map(|(farm, exch, kind)| match *kind {
                    "0" => Some(farm::DepthSpec::Book { farm: *farm, exchange: exch.clone() }),
                    "442" => {
                        let own = entries.iter().any(|(_, e, k)| *k == "443" && e == exch);
                        let last = if own { (*farm, exch.clone()) } else { (pool::PRIMARY_MD, "BEST".to_string()) };
                        Some(farm::DepthSpec::Top { farm: *farm, exchange: exch.clone(), last: Some(last) })
                    }
                    _ => None,
                }).collect();
                let lot = lots.get(&con_id).copied().unwrap_or(1);
                engine.farm.start_depth(req_id, con_id, "STK", smart, num_rows, lot, false, specs, None, &shared);
            } else if leg == "api_out" && name == "CANCEL_MKT_DEPTH" {
                let f = proto_fields(&b64(&r["body_b64"]));
                engine.farm.stop_depth(f.iter().find(|x| x.0 == 1).unwrap().1 as ReqId);
            } else if leg == "fix_in" && (conn == "usfarm" || conn == "usfarm.nj") {
                engine.farm.rx_farm = farm_of(conn);
                let raw = b64(&r["raw_b64"]);
                // The engine's farm id of a recorded one.
                let ours_id = |engine: &HotLoop, id: &str| -> Option<String> {
                    let (exch, kind) = sent.get(&(conn.to_string(), id.to_string()))?;
                    engine.farm.depth_reqs.iter().flat_map(|q| q.entries.iter())
                        .find(|e| e.live && &e.exchange == exch && e.req_type == kind.as_str())
                        .map(|e| e.farm_req.to_string())
                };
                match mt {
                    "Q" => {
                        let text = String::from_utf8_lossy(&raw).to_string();
                        let body = text.split("35=Q\x01").nth(1).unwrap().split("\x018349=").next().unwrap().to_string();
                        let mut parts: Vec<String> = body.split(',').map(String::from).collect();
                        if let Some(id) = ours_id(&engine, &parts[1]) {
                            parts[1] = id;
                            engine.inject_farm_message(format!("8=O\x0135=Q\x01{}\x01", parts.join(",")).as_bytes());
                        }
                    }
                    "3" => {
                        let tags = fix::fix_parse(&raw);
                        if let Some(id) = tags.get(&262).and_then(|id| ours_id(&engine, id)) {
                            let text = tags.get(&58).cloned().unwrap_or_default();
                            engine.inject_farm_message(&fix::fix_build(&[(35, "3"), (262, &id), (58, &text)], 1));
                        }
                    }
                    "Y" | "P" => engine.inject_farm_message(&raw),
                    _ => {}
                }
                drain(&shared, &mut ours);
            } else if leg == "api_in" && name.starts_with("MARKET_DEPTH") {
                let f = proto_fields(&b64(&r["body_b64"]));
                let req_id = f.iter().find(|x| x.0 == 1).unwrap().1 as i64;
                let d = proto_fields(&f.iter().find(|x| x.0 == 2).unwrap().2);
                let int = |n: u64| d.iter().find(|x| x.0 == n).map_or(0, |x| x.1 as i32);
                let dbl = |n: u64| d.iter().find(|x| x.0 == n).map_or(0.0, |x| f64::from_le_bytes(x.2[..8].try_into().unwrap()));
                let txt = |n: u64| d.iter().find(|x| x.0 == n).map_or(String::new(), |x| String::from_utf8(x.2.clone()).unwrap());
                theirs.push(if name == "MARKET_DEPTH_L2" {
                    (req_id, int(1), int(2), int(3), dbl(4), txt(5).parse().unwrap(), txt(6), int(7) == 1, true)
                } else {
                    (req_id, int(1), int(2), int(3), dbl(4), txt(5).parse().unwrap(), String::new(), false, false)
                });
            }
        }
        (ours, theirs)
    }

    fn assert_same_callbacks(ours: &[Callback], theirs: &[Callback]) {
        for (k, (a, b)) in ours.iter().zip(theirs).enumerate() {
            assert_eq!(a, b, "callback {k} differs; reference before it: {:?}", &theirs[k.saturating_sub(3)..k]);
        }
        assert_eq!(ours.len(), theirs.len(), "number of callbacks");
    }

    // #451: AAPL on IEX alone, 5 rows, paper (user book on): the whole book
    // at the first data, then the index diff of the shown book inside the
    // rows, as updateMktDepth (IEX has no market makers). Recorded from
    // the reference on 28/09/2026.
    #[test]
    fn replay_single_book_as_the_reference() {
        let path = format!("{}/tests/fixtures/gw1040/scenarios/20260928/depth_single_iex.jsonl", env!("CARGO_MANIFEST_DIR"));
        let (ours, theirs) = replay_depth(&path);
        assert_eq!(theirs.len(), 610);
        assert_same_callbacks(&ours, &theirs);
    }

    // #451: AAPL SmartDepth, 50 rows (the IEX book with the top of book of
    // the other components), then AXTI SmartDepth, 10 rows: merged rows,
    // tail inserts and deletes, updates in place, as updateMktDepthL2 with
    // the venue as market maker. Recorded from the reference on 28/09/2026.
    #[test]
    fn replay_smart_depth_as_the_reference() {
        let path = format!("{}/tests/fixtures/gw1040/scenarios/20260928/depth_smart.jsonl", env!("CARGO_MANIFEST_DIR"));
        let (ours, theirs) = replay_depth(&path);
        assert_eq!(theirs.len(), 1177);
        assert!(theirs.iter().any(|c| c.2 == 2), "the slice has deletes");
        assert_same_callbacks(&ours, &theirs);
    }

    // #451: the same replay on whole recorded scenarios, given in
    // IBX_DEPTH_SCENARIOS (paths separated by ';'); nothing without it.
    #[test]
    fn replay_whole_scenarios_from_env() {
        let Ok(paths) = std::env::var("IBX_DEPTH_SCENARIOS") else { return };
        for path in paths.split(';').filter(|p| !p.is_empty()) {
            let (ours, theirs) = replay_depth(path);
            eprintln!("{path}: {} callbacks of the reference, {} of the engine", theirs.len(), ours.len());
            assert_same_callbacks(&ours, &theirs);
        }
    }
}
