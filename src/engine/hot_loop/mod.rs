pub mod farm;
pub mod ccp;
pub mod hmds;
pub mod secdef;
pub mod order_builder;
pub use crate::reliability::retry;

/// How fast a reconnect may put its subscriptions back.
///
/// Taken from the caller's
/// [`ReconnectConfig`](crate::reliability::ReconnectConfig).
#[derive(Debug, Clone, Copy)]
pub(crate) struct ReplayPacing {
    pub burst: usize,
    pub pace: std::time::Duration,
}

impl Default for ReplayPacing {
    fn default() -> Self {
        let d = crate::reliability::ReconnectConfig::default();
        Self { burst: d.replay_burst, pace: d.replay_pace }
    }
}

use crate::types::NewsSubject;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Instant;
use std::io;

use crate::bridge::{Event, SharedState};
use crate::engine::context::Context;
use crate::protocol::datetime::chrono_free_timestamp;
use crate::gateway::{connect_farm, reconnect_ccp, Farm, ReconnectAuth};
use crate::protocol::connection::Connection;
use crate::protocol::fix;
use crate::types::{ContractRef, ControlCommand, Fill, InstrumentId, Price, Qty, PRICE_SCALE, QTY_SCALE, qty_to_f64};
use std::sync::mpsc::{sync_channel, Receiver, SyncSender};

use farm::FarmState;
use ccp::CcpState;
use hmds::HmdsState;

/// Auth server heartbeat interval — single source in config (
/// removed the duplicate definitions here).
const CCP_HEARTBEAT_SECS: u64 = crate::config::CCP_HEARTBEAT;
/// Farm heartbeat interval — single source in config.
const FARM_HEARTBEAT_SECS: u64 = crate::config::FARM_HEARTBEAT;
/// Liveness, aligned with the gateway's transport thresholds:
/// send a test request when nothing has been received for this long..
pub const LIVENESS_TEST_SECS: u64 = 15;
/// ...and declare the connection dead when nothing has been received for
/// this long. The old scheme declared death at ~21s — racing the server's
/// own ~35s reset and losing to transient stalls the server tolerates.
pub const LIVENESS_DEAD_SECS: u64 = 35;
/// Longest plausible gap between two liveness checks.
///
/// A gap above this means the check did not run. Silence measured across it is
/// this process's own and says nothing about the connection.
pub const LIVENESS_STALL_SECS: u64 = 10;
/// How long liveness stays suspended after a stall.
pub const LIVENESS_STALL_COOLDOWN_SECS: u64 = 60;
// The ladder is ordered by construction, so it is checked by construction: a
// heartbeat that outlives the test window, or a test that outlives the dead
// window, fails the build rather than a test the optimizer folds away.
const _: () = assert!(CCP_HEARTBEAT_SECS < LIVENESS_TEST_SECS);
const _: () = assert!(LIVENESS_TEST_SECS < LIVENESS_DEAD_SECS);
// A frame gets one probe interval to reach the socket. The bound lives with
// the socket, which is below this, so the two are held equal here instead.
const _: () = assert!(
    LIVENESS_TEST_SECS == crate::protocol::connection::WHOLE_FRAME_TIMEOUT_SECS
);

/// The pinned-core hot loop. Pushes events to SharedState + optional event channel.
pub struct HotLoop {
    shared: Arc<SharedState>,
    event_tx: Option<EventSink>,
    context: Context,
    /// Core ID to pin the hot loop thread to. None = no pinning.
    core_id: Option<usize>,
    /// Whether a recoverable loss was announced to the client. Gates the
    /// restore notice so a reconnect that nobody was told about stays quiet.
    loss_announced: bool,
    /// How far into what the client has asked for this engine has read.
    ///
    /// Said back with a slot the engine gives up, because a slot number alone
    /// says nothing about which occupancy ended: the slot goes to the next
    /// contract that needs one, and a release read after that forgot the
    /// records of a contract that had just been given it.
    heard_up_to: u64,
    /// Set when a reconnect failed for a reason repeating cannot fix. The
    /// scheduler stops rather than climbing a ladder forever against a server
    /// that has already given its answer.
    reconnect_halted: Option<retry::DisconnectReason>,
    /// What the caller said about recovery. Defaults recover automatically and
    /// keep trying, which is what a process that must stay up wants.
    reconnect_cfg: crate::reliability::ReconnectConfig,
    budget: crate::reliability::RecoveryBudget,
    /// Slots a caller asked to free that were held open by a position. The
    /// table is bounded, so they are reconsidered once the account is flat
    /// rather than being lost until the process ends.
    pinned_by_position: Vec<InstrumentId>,
    /// Next scheduled CCP/farm reconnect attempt (jittered backoff).
    ccp_next_attempt_at: Option<Instant>,
    farm_next_attempt_at: Option<Instant>,
    /// Farm connection for market data (market data farm).
    pub farm_conn: Option<Connection>,
    /// Auth connection for order management.
    pub ccp_conn: Option<Connection>,
    /// Historical farm connection for historical data (optional).
    pub hmds_conn: Option<Connection>,
    /// Contract definitions and the calendar that rides with them. Absent
    /// where the venue stated no route for it at logon, which is a fact about
    /// the session rather than a failure.
    pub secdef_conn: Option<Connection>,
    pub secdef: secdef::SecDefState,
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
    // ── Subsystems ──
    pub(crate) farm: FarmState,
    pub(crate) ccp: CcpState,
    pub(crate) hmds: HmdsState,
    // ── Auto-reconnect ──
    reconnect_auth: Option<ReconnectAuth>,
    /// Slots whose watchers were sent to another slot, waiting to be given
    /// back once the move saying so has been read.
    slots_awaiting_their_word: Vec<InstrumentId>,
    pending_farm_reconnect: Option<Receiver<io::Result<Connection>>>,
    farm_reconnect_attempt: u32,
    pending_ccp_reconnect: Option<Receiver<io::Result<Connection>>>,
    ccp_reconnect_attempt: u32,
    /// HMDS reconnect state. Drives a background reconnect loop with
    /// exponential backoff when the historical-data farm is down — initial
    /// connect failed, or a future runtime disconnect detector trips it.
    pending_hmds_reconnect: Option<Receiver<io::Result<Connection>>>,
    hmds_reconnect_attempt: u32,
    /// When each transport last came up. The backoff is only given back once
    /// the connection has held for the stable window — a peer that accepts and
    /// drops a second later has not recovered, and counting it as a fresh start
    /// is how a session never climbs past the first rung and retries at that
    /// rung for ever. The retry budget beside it already works this way.
    farm_connected_at: Option<Instant>,
    ccp_connected_at: Option<Instant>,
    hmds_connected_at: Option<Instant>,
    secdef_connected_at: Option<Instant>,
    /// The same, for the connection carrying the calendar. Rebuilt on the
    /// same terms as the others: a farm that goes is not a session that ends.
    secdef_reconnect_attempt: u32,
    secdef_next_attempt_at: Option<Instant>,
    pending_secdef_reconnect: Option<Receiver<io::Result<Connection>>>,
    /// Earliest instant the next HMDS reconnect attempt may spawn. `None`
    /// while HMDS is healthy.
    hmds_next_attempt_at: Option<Instant>,
    /// Why a farm the session can live without was given up on, where one
    /// was. A refusal the venue will give again — credentials it will not
    /// take, an operation it does not serve — does not become an acceptance
    /// by being asked for once a minute, which is what the ladder does with
    /// one: a full handshake per rung against an answer that was final the
    /// first time. Held apart from `reconnect_halted` because this farm is
    /// finished and the session is not.
    hmds_halted: Option<retry::DisconnectReason>,
    /// The market-data farm given up on for the session, with why. The
    /// transport that ran out of recovery is what has ended, not the
    /// session: recorded as the session's end, every reader was refused as
    /// not connected while the trading connection stayed up and went on
    /// taking orders on a session the caller could no longer read.
    farm_halted: Option<retry::DisconnectReason>,
    secdef_halted: Option<retry::DisconnectReason>,
    /// What rebuilding each of those farms has cost, kept apart from the
    /// session's. They read the session's budget so that recovery the caller
    /// has abandoned abandons them too, but they must not spend it: an
    /// unreachable calendar would otherwise burn the attempts the trading
    /// connection needs, and the next drop of that connection would be given
    /// up on at once. Spending one of their own is what makes the caller's
    /// limits mean anything for a farm that is the only thing down.
    hmds_budget: crate::reliability::RecoveryBudget,
    secdef_budget: crate::reliability::RecoveryBudget,
    /// The market-data farm's own, for the same reason: spent against the
    /// session's, a quote feed the venue would not serve again left the
    /// trading connection's next drop with nothing to retry on, and a farm
    /// given up on kept the session's budget from ever settling.
    farm_budget: crate::reliability::RecoveryBudget,
    /// The reconnects this loop has in flight, and the flag that takes them
    /// back. A stop or a spent budget sets the flag; the workers check it
    /// between the phases of a handshake, so neither opens a session at the
    /// venue after the client stopped asking for one.
    reconnect_cancel: Arc<AtomicBool>,
    /// Held so the loop can say its workers have finished before it returns:
    /// a reconnect spawned detached kept dialling and authenticating after
    /// the shutdown that orphaned it.
    /// Every recovery attempt still dialling, and whether it is the trading
    /// connection's — the one whose landed session must be logged out.
    reconnect_workers: Vec<(bool, std::thread::JoinHandle<()>)>,
}

/// Consecutive HMDS reconnect failures before the loss is logged as an error. Retries
/// do not stop there: the servers go down for maintenance
/// nightly and come back on their own, and a client that gave up after ~2
/// minutes would stay dark until someone restarted the process — the same
/// failure the gateway has when its restart token is unset. Attempts continue
/// on the ladder below, which caps at 64s.
const HMDS_NOTIFY_AFTER_ATTEMPTS: u32 = 6;

/// Tracks last send/recv times and pending test requests for heartbeat management.
pub struct HeartbeatState {
    pub last_ccp_sent: Instant,
    pub last_ccp_recv: Instant,
    pub last_farm_sent: Instant,
    pub last_farm_recv: Instant,
    pub last_hmds_sent: Instant,
    pub last_hmds_recv: Instant,
    pub last_secdef_sent: Instant,
    pub last_secdef_recv: Instant,
    /// How often the venue said it expects to hear from this session, in
    /// seconds, as stated in its answer to the logon.
    ///
    /// Proposed by this client and answered by the venue, which may name a
    /// different number — so the proposal is not what the session is held to.
    /// Sending on the proposal regardless meant a venue asking for anything
    /// shorter was answered too slowly, and closed the connection for it.
    pub ccp_interval_secs: u64,
    /// Pending test request for auth: (test_req_id, sent_at).
    pub pending_ccp_test: Option<(String, Instant)>,
    /// When liveness last ran, and when it last detected a stall.
    pub last_liveness_check: Instant,
    pub stalled_at: Option<Instant>,
    /// Pending test request for farm: (test_req_id, sent_at).
    pub pending_farm_test: Option<(String, Instant)>,
    /// Pending test request for historical: (test_req_id, sent_at).
    pub pending_hmds_test: Option<(String, Instant)>,
    /// Test request outstanding on the security-definition connection.
    pub pending_secdef_test: Option<(String, Instant)>,
    /// Counter for generating unique test request IDs.
    test_req_counter: u32,
}

/// Half an interval, and never nothing.
///
/// A session that answers on the deadline has no margin: one heartbeat delayed
/// by a slow link or a scheduling hiccup is late. Half leaves room for one to
/// be lost outright, which is what the protocol allows.
fn half_of(interval_secs: u64) -> u64 {
    (interval_secs / 2).max(1)
}

impl HeartbeatState {
    /// How often to send, given what the venue asked for.
    ///
    /// Half the stated interval, which is the defined send rate: a
    /// session that answers exactly on the deadline has no margin, and one
    /// heartbeat delayed by a scheduling hiccup or a slow link is late. Half
    /// leaves room for one to be lost entirely.
    ///
    /// Never zero, so a venue naming one second is answered every second
    /// rather than on every pass of the loop.
    pub fn ccp_send_every(&self) -> u64 {
        half_of(self.ccp_interval_secs)
    }

    /// How long of silence before asking whether the venue is still there.
    ///
    /// The fixed window, or a multiple of the stated interval where that is
    /// longer. A venue that speaks every thirty seconds is not silent at
    /// fifteen, and probing it on that schedule would ask a healthy session to
    /// prove itself on every gap between its own heartbeats.
    pub fn ccp_test_after(&self) -> u64 {
        LIVENESS_TEST_SECS.max(self.ccp_interval_secs.saturating_mul(3) / 2)
    }

    /// How long of silence before the connection is treated as gone.
    ///
    /// Three of the venue's intervals at least: two heartbeats can be lost
    /// without the session being dead, and declaring it on a shorter window
    /// than the venue's cadence kills connections that are working.
    pub fn ccp_dead_after(&self) -> u64 {
        LIVENESS_DEAD_SECS.max(self.ccp_interval_secs.saturating_mul(3))
    }

    /// The same two windows for a farm, from the interval that farm stated.
    ///
    /// A farm speaks on its own cadence, and the fixed windows are sized for
    /// the auth connection's. One that heartbeats every sixty seconds is not
    /// silent at thirty-five, so a fixed window declares a working connection
    /// dead between two of its own heartbeats.
    pub fn farm_test_after(stated: Option<u64>) -> u64 {
        let interval = stated.unwrap_or(FARM_HEARTBEAT_SECS);
        LIVENESS_TEST_SECS.max(interval.saturating_mul(3) / 2)
    }

    /// How long of silence before a farm is treated as gone.
    pub fn farm_dead_after(stated: Option<u64>) -> u64 {
        let interval = stated.unwrap_or(FARM_HEARTBEAT_SECS);
        LIVENESS_DEAD_SECS.max(interval.saturating_mul(3))
    }

    /// Hold this session to the interval the venue stated.
    pub fn set_ccp_interval(&mut self, secs: u64) {
        if secs > 0 {
            self.ccp_interval_secs = secs;
        }
    }

    fn new() -> Self {
        let now = Instant::now();
        Self {
            last_ccp_sent: now,
            last_ccp_recv: now,
            last_farm_sent: now,
            last_farm_recv: now,
            last_hmds_sent: now,
            last_secdef_sent: now,
            last_secdef_recv: now,
            last_hmds_recv: now,
            ccp_interval_secs: CCP_HEARTBEAT_SECS,
            pending_ccp_test: None,
            last_liveness_check: now,
            stalled_at: None,
            pending_farm_test: None,
            pending_hmds_test: None,
            pending_secdef_test: None,
            test_req_counter: 0,
        }
    }

    fn next_test_id(&mut self) -> String {
        self.test_req_counter += 1;
        format!("T{}", self.test_req_counter)
    }
}

/// Take the holding the account has already stated onto a slot just given to
/// its contract.
///
/// The account states what it holds before anything here asks for a slot, and
/// until there is one that statement lands on the contract's number alone.
/// Left behind, the engine's book reads the slot as flat: a withdrawal of
/// everything composes its cancels from that book, and the guard that keeps a
/// slot resident while the account holds something reads the same zero and
/// hands the slot to the next contract that needs one.
///
/// Only onto a slot this registration created. Registration is also how an
/// already-live contract is looked up, and the account's row is older than any
/// fill booked since — reapplied on every call it rolled a filled position
/// back to whatever the last account frame said.
///
/// That row is restated by the venue's own feed after a fill rather than by
/// the booking, so a slot made for a contract whose last fill closed it can
/// still be seeded from a figure the feed has not caught up with. The window
/// is the feed's own latency, and it closes when the restatement lands.
/// Nothing here can tell a stale row from a current one; what it can tell is
/// that a slot with no holding on it is reclaimed, and a contract the account
/// holds losing its slot does not correct itself.
pub(super) fn take_what_the_account_already_holds(
    context: &mut Context,
    shared: &SharedState,
    con_id: i64,
    instrument: InstrumentId,
    is_new_slot: bool,
) {
    if let Some(held) = shared.portfolio.position_info(con_id).filter(|_| is_new_slot)
        && held.position != 0.0
    {
        context.update_position(instrument, held.position - context.position(instrument));
        shared.portfolio.set_position(instrument, held.position);
    }
}

impl HotLoop {
    /// Take a session that has logged on, and the connections it opened, and
    /// make the loop that will run them.
    ///
    /// The session's own data — the account, the heartbeat the venue held it
    /// to, the signing keys, and what a reconnect will need — is read here
    /// rather than pushed from there: a logon knows nothing about a loop.
    pub fn for_session(
        gateway: crate::gateway::Gateway,
        shared: Arc<SharedState>,
        event_tx: Option<EventSink>,
        farm_conn: Connection,
        ccp_conn: Connection,
        hmds_conn: Option<Connection>,
        secdef_conn: Option<Connection>,
        core_id: Option<usize>,
        caller: crate::gateway::CallerAuth,
    ) -> (HotLoop, SyncSender<ControlCommand>) {
        let (tx, rx) = sync_channel(64);
        let reconnect_auth = gateway.reconnect_auth(caller);
        // Emitted like everything else, so a logon that arrives at a channel
        // nobody has read yet is counted rather than waited on: this runs on
        // the thread that carries the session.
        emit(&event_tx, Event::GatewayLogon {
            ccp_session_id: gateway.server_session_id.clone(),
            misc_urls: gateway.misc_urls.clone(),
        });
        let mut hot_loop = HotLoop::new(shared, event_tx, core_id);
        hot_loop.set_control_rx(rx);
        hot_loop.set_account_id(gateway.account_id.clone());
        hot_loop.set_reconnect_auth(reconnect_auth);
        // Held to the interval the venue named in its answer to the logon, not
        // to the one this client proposed.
        hot_loop.set_ccp_heartbeat_interval(gateway.heartbeat_interval);
        hot_loop.farm_conn = Some(farm_conn);
        // The venue stamps the logon answer with its own clock. How far that
        // is from this machine's is learned here, and a caller asking what
        // time the venue says it is reads this machine's clock shifted by it.
        if let Some(stamped) = &ccp_conn.logged_in_at {
            hot_loop.shared.market.note_venue_time(stamped);
        }
        hot_loop.ccp_conn = Some(ccp_conn);
        // This connection has not named what it has working yet, and the
        // bound that naming is waited on starts now, when the connection
        // came up. Left to the first caller to start, a request that waited
        // the bound out spent it, and a global cancel issued straight after
        // waited nothing and said nothing.
        hot_loop.shared.orders.replay_is_pending();
        hot_loop.hmds_conn = hmds_conn;
        hot_loop.secdef_conn = secdef_conn;
        (hot_loop, tx)
    }

    pub fn new(shared: Arc<SharedState>, event_tx: Option<EventSink>, core_id: Option<usize>) -> Self {
        Self {
            shared,
            event_tx,
            context: Context::new(),
            core_id,
            farm_conn: None,
            ccp_conn: None,
            hmds_conn: None,
            secdef_conn: None,
            secdef: secdef::SecDefState::new(),
            control_rx: None,
            running: true,
            account_id: String::new(),
            hb: HeartbeatState::new(),
            cmd_buf: Vec::with_capacity(16),
            farm: FarmState::new(),
            ccp: CcpState::new(),
            hmds: HmdsState::new(),
            reconnect_auth: None,
            slots_awaiting_their_word: Vec::new(),
            pending_farm_reconnect: None,
            ccp_next_attempt_at: None,
            farm_next_attempt_at: None,
            loss_announced: false,
            heard_up_to: 0,
            reconnect_halted: None,
            reconnect_cfg: Default::default(),
            budget: Default::default(),
            pinned_by_position: Vec::new(),
            farm_reconnect_attempt: 0,
            pending_ccp_reconnect: None,
            ccp_reconnect_attempt: 0,
            pending_hmds_reconnect: None,
            hmds_reconnect_attempt: 0,
            farm_connected_at: None,
            ccp_connected_at: None,
            hmds_connected_at: None,
            secdef_connected_at: None,
            secdef_reconnect_attempt: 0,
            secdef_next_attempt_at: None,
            pending_secdef_reconnect: None,
            hmds_next_attempt_at: None,
            hmds_halted: None,
            farm_halted: None,
            secdef_halted: None,
            hmds_budget: crate::reliability::RecoveryBudget::new(),
            farm_budget: crate::reliability::RecoveryBudget::new(),
            secdef_budget: crate::reliability::RecoveryBudget::new(),
            reconnect_cancel: Arc::new(AtomicBool::new(false)),
            reconnect_workers: Vec::new(),
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

    /// Hold the auth connection to the heartbeat interval the venue stated.
    pub fn set_ccp_heartbeat_interval(&mut self, secs: u64) {
        self.hb.set_ccp_interval(secs);
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

    /// Build a HotLoop with connections and control channel, without requiring a
    /// Gateway.
    pub fn with_connections(
        shared: Arc<SharedState>,
        event_tx: Option<EventSink>,
        account_id: String,
        farm_conn: Connection,
        ccp_conn: Connection,
        hmds_conn: Option<Connection>,
        core_id: Option<usize>,
    ) -> (Self, SyncSender<ControlCommand>) {
        let (tx, rx) = sync_channel(64);
        let mut hl = Self::new(shared, event_tx, core_id);
        hl.set_control_rx(rx);
        hl.set_account_id(account_id);
        hl.farm_conn = Some(farm_conn);
        // As `for_session`: the difference between the clocks is learned from
        // the logon stamp, and the bound that naming is waited on starts when
        // the connection is taken.
        if let Some(stamped) = &ccp_conn.logged_in_at {
            hl.shared.market.note_venue_time(stamped);
        }
        hl.ccp_conn = Some(ccp_conn);
        hl.shared.orders.replay_is_pending();
        hl.hmds_conn = hmds_conn;
        (hl, tx)
    }
    /// Give back the slots whose watchers have gone to another slot.
    ///
    /// A request whose contract turned out to be held by another slot leaves
    /// its own slot holding nothing: the watchers follow the move to where the
    /// contract lives, and nothing will withdraw the slot they left, which is
    /// the only thing that gives one back. One goes out of the table on every
    /// such collision, and a table that runs out refuses the next subscription
    /// for want of a slot.
    ///
    /// Not given back at the moment of the move, because giving a slot back
    /// purges what names it — the move telling its watchers where to follow,
    /// or the reason no subscription could be made — and that is the only
    /// thing its reader will ever be told. Once it has been read, both hold.
    fn give_back_slots_their_word_has_left(&mut self) {
        if self.slots_awaiting_their_word.is_empty() {
            return;
        }
        let ready: Vec<InstrumentId> = self
            .slots_awaiting_their_word
            .iter()
            .copied()
            .filter(|slot| {
                !self.shared.market.a_move_is_pending_from(*slot)
                    && !self.shared.market.a_failure_is_pending_from(*slot)
            })
            .collect();
        self.slots_awaiting_their_word.retain(|slot| !ready.contains(slot));
        for slot in ready {
            self.try_reclaim_instrument(slot);
        }
    }

    /// Subscriptions the venue can now be asked for: named by symbol, and the
    /// lookup has come back with the contract's own id.
    fn send_resolved_subscriptions(&mut self) {
        self.give_back_slots_their_word_has_left();
        for (con_id, p) in std::mem::take(&mut self.ccp.resolved_md_subscribe) {
                // The slot keeps the id so a reconnect resubscribes by it
                // rather than starting the lookup again. Where another slot
                // already holds the contract — one caller naming it by id
                // and another by symbol — this one cannot: the venue
                // answers a second subscription on a contract with the
                // number it is already streaming under, which lands on the
                // first slot and freezes it. So the caller follows the slot
                // the contract lives in, and nothing is asked for twice.
                // What the caller named beyond the quote, where the slot it
                // was given turns out not to be the slot the contract lives
                // in. Recorded against the slot it was given, and that slot is
                // about to be handed back: the caller was moved onto the
                // contract's own slot and published as watching it, while the
                // series it asked for went back with the slot it left.
                let mut carried: Vec<u32> = Vec::new();
                let instrument = if self.context.market.adopt_con_id(p.instrument, con_id) {
                    p.instrument
                } else {
                    match self.context.market.instrument_by_con_id(con_id) {
                        Some(owner) if owner != p.instrument => {
                            // With the occupancy the slot they are moving
                            // onto is held under, so the caller that arrives
                            // holds what the engine holds and can withdraw it.
                            let held_under = self.farm.what_took_it(owner);
                            self.shared.market.push_subscription_move(p.instrument, owner, held_under);
                            carried = self.farm
                                .asked_generic_ticks
                                .remove(&p.instrument)
                                .unwrap_or_default();
                            // Where they went, so a withdrawal naming the slot
                            // they left reaches what they asked for.
                            self.farm.note_moved(p.instrument, owner);
                            // And the slot this request took is owed back. It
                            // holds nothing now — its watchers follow the move
                            // to the slot the contract lives in, and nothing
                            // will ever withdraw it, which is what gives a slot
                            // back. Left, one goes out of the table on every
                            // collision until the table is full and the next
                            // subscription is refused for want of one.
                            //
                            // Not here, though: the release purges the moves
                            // that name this slot, and that move is the only
                            // thing telling its watchers where to follow. It is
                            // given back once the move has been read.
                            if !self.slots_awaiting_their_word.contains(&p.instrument) {
                                self.slots_awaiting_their_word.push(p.instrument);
                            }
                            owner
                        }
                        _ => p.instrument,
                    }
                };
                // What is already up, and only where this request can be
                // answered by it. A snapshot is neither followed nor followable
                // — it is a request of its own, asked for under an action of
                // its own — so a snapshot skipped for a live stream was never
                // sent and the
                // caller heard the end of it off ticks it did not ask for,
                // while a stream skipped for a snapshot in flight was dropped
                // when the snapshot completed and withdrew.
                // The subscription that answers this request, whichever slot
                // it turns out to live in.
                self.farm.note_subscription_asked_on(instrument, p.issued);
                // Where this request is the one the subscription goes out for,
                // the occupancy is its own. Where the contract lives in another
                // slot the callers are moved onto it, and that slot's occupancy
                // belongs to whoever took it.
                if !self.farm.holds_a_stream(instrument) {
                    // This request begins the stream, so the occupancy is its
                    // own whatever the slot held before. Kept at the first
                    // number ever written, a slot that cannot be handed back —
                    // one pinned by a holding, a working order, a tick-by-tick
                    // stream or news — stayed named after the caller that had
                    // already gone, and every withdrawal after that named a
                    // number the engine did not hold and was refused. The
                    // venue went on streaming a contract nothing could take
                    // down, for the life of the session.
                    self.farm.note_it_changed_hands(instrument, p.issued);
                }
                // What this request named, on whichever slot it lands on: the
                // list it carried off the slot it was given, or the one
                // recorded against the slot it keeps.
                let named: Vec<u32> = if carried.is_empty() {
                    self.farm.asked_generic_ticks.get(&instrument).cloned().unwrap_or_default()
                } else {
                    carried.clone()
                };
                self.farm.note_series_asked_on(instrument, &named, p.issued);
                // Carried onto the slot the contract lives in: asked for now
                // where a stream is already up there, and added to what that
                // slot asks for where the subscription below is the one that
                // will carry them.
                if !carried.is_empty() {
                    if self.farm.holds_a_stream(instrument) {
                        self.farm.also_ask_for_series(
                            instrument,
                            con_id,
                            &carried,
                            &self.context,
                            &mut self.farm_conn,
                            &mut self.hb,
                        );
                    } else {
                        let held = self.farm.asked_generic_ticks.entry(instrument).or_default();
                        for tick in &carried {
                            if !held.contains(tick) {
                                held.push(*tick);
                            }
                        }
                    }
                }
                if !p.regulatory_snapshot && self.farm.holds_a_stream(instrument) {
                    continue;
                }
                // A feed given up on since the request was taken. The refusal
                // raised where a caller asks cannot see this one: the request
                // was accepted while the feed was alive, waited on the venue to
                // name its contract, and arrives here afterwards. Sent anyway
                // it reached a socket that is not there and was recorded for a
                // replay that is not coming, and the caller — told it had a
                // subscription when it asked — heard nothing for the rest of
                // the session.
                if let Some(why) = self.farm_halted {
                    self.shared.market.push_subscription_failure(
                        instrument,
                        format!(
                            "market data is unavailable for the rest of this session: {}",
                            why.as_str(),
                        ),
                    );
                    continue;
                }
                // A snapshot has no second chance. Every other request here
                // survives an outage in a record of its own — a stream is
                // replayed, the headlines and the book are re-sent — and the
                // snapshot is deliberately not, because a reconnect that
                // re-sent one would deliver, and bill for, a second burst
                // nobody asked for. That is right for one that went out. With
                // no transport to write it to it never went out, and there is
                // no later moment when it does: it was accepted, dropped, and
                // never answered.
                if p.regulatory_snapshot && self.farm.disconnected {
                    // Not where a stream is held on the contract. What is
                    // recorded here is about the contract and reaches everyone
                    // watching it, and the stream was neither refused nor
                    // given up — it is kept precisely so the reconnect brings
                    // it back. Told their quote had been refused, its watchers
                    // withdraw it.
                    if !self.farm.holds_a_stream(instrument) {
                        self.shared.market.push_subscription_failure(
                            instrument,
                            "the quote feed was down when this snapshot was asked for, \
                             so it was never sent: ask for it again once the feed is back"
                                .to_string(),
                        );
                    }
                    continue;
                }
                let (sec_type, exchange) =
                    self.described_as(con_id, &p.sec_type, &p.exchange);
                self.farm.send_mktdata_subscribe(
                    con_id, &p.symbol, &exchange, &sec_type,
                    &p.filters.last_trade_date_or_contract_month, p.filters.strike,
                    &p.filters.right, &p.filters.multiplier,
                    instrument, p.mode_9887, p.regulatory_snapshot,
                    &mut self.farm_conn,
                    &mut self.hb,
                );
        }    }


    /// Register a contract, or say why it could not be.
    ///
    /// A full table sends the caller an `Err` on the reply channel rather than
    /// ending the engine: the request that could not be served fails, and
    /// every subscription already running keeps running.
    fn register_or_reject(
        &mut self,
        con_id: i64,
        symbol: String,
        sec_type: &str,
        exchange: &str,
        option_key: &str,
        // What narrows the lookup that will name a contract stated by
        // description. Not part of what the contract is, and still what
        // decides which listing the venue answers with — so two descriptions
        // narrowed differently do not share a slot before either is answered.
        narrowing: &str,
        // Answered without blocking: this runs on the thread driving all
        // three transports, and a caller's channel is the caller's to drain.
        // A reply that cannot be delivered is a caller that stopped listening.
        reply_tx: &Option<std::sync::mpsc::SyncSender<Result<InstrumentId, String>>>,
    ) -> Option<InstrumentId> {
        // Whether this call is what created the slot. Registration is also how
        // an already-live contract is looked up, and the account row is older
        // than any fill booked since: reapplying it on every call rolled a
        // filled position back to whatever the last account frame said.
        let is_new_slot = self.context.market.con_id(
            self.context.market.instrument_by_con_id(con_id).unwrap_or(0),
        ) != Some(con_id);
        match self.context.market.try_register_described(
            con_id, &symbol, sec_type, exchange, option_key, narrowing,
        ) {
            Some(id) => {
                // The symbol is written by the registration itself, under a
                // guard that keeps the one the slot has. Written again here
                // without one, a registration naming no symbol — which is
                // every registration made by contract id, as orders, positions
                // and fills all are — blanked a slot already registered under
                // its name, and the order built from it went out with an empty
                // symbol tag.
                self.context.market.set_routing(id, sec_type, exchange);
                take_what_the_account_already_holds(
                    &mut self.context, &self.shared, con_id, id, is_new_slot,
                );
                self.shared.market.set_instrument_count(self.context.market.count());
                if let Some(tx) = reply_tx { let _ = tx.try_send(Ok(id)); }
                Some(id)
            }
            None => {
                log::error!("Instrument table full: rejecting registration for con_id={con_id}");
                if let Some(tx) = reply_tx {
                    let _ = tx.try_send(Err(format!(
                        "instrument table full: {} contracts are live concurrently; \
                         cancel unused market-data subscriptions to free slots",
                        crate::types::MAX_INSTRUMENTS
                    )));
                }
                None
            }
        }
    }

    /// Offer back the slots an order has just stopped holding.
    ///
    /// Asked here rather than where the order ends, because whether a slot is
    /// free is a question about everything else that could be pointing at it,
    /// and this is where all of that is known.
    fn reclaim_slots_no_order_holds(&mut self) {
        for instrument in std::mem::take(&mut self.context.slots_to_reconsider) {
            self.try_reclaim_instrument(instrument);
        }
    }

    /// Reclaim an instrument slot if nothing references it any more
    /// Reclaimed only with nothing referring to the slot: no open orders, no
    /// market data, no tick-by-tick subscription, no news subscription. A
    /// reused id would repoint those references at the wrong contract, so a
    /// referenced slot stays resident until released.
    fn try_reclaim_instrument(&mut self, instrument: InstrumentId) {
        if self.context.has_open_orders_for(instrument) {
            return;
        }
        // And what is queued to be sent. The book holds an order from the
        // moment it is built, which is a lap after the caller was told it was
        // accepted: a withdrawal drained in the same pass found the slot
        // unheld, gave it back, and the order went out on whichever contract
        // took it next.
        if self.context.pending_orders.holds(instrument) {
            return;
        }
        // Market data was missing from this list. Cancelling tick-by-tick or
        // news on an instrument that also holds an L1 subscription freed the
        // slot underneath it, and the resubscribe record then replayed the old
        // contract's descriptor against the id's new occupant.
        if self.farm.holds_market_data(instrument) {
            return;
        }
        if self.hmds.tbt_subscriptions.iter().any(|sub| sub.instrument == instrument) {
            return;
        }
        if self.farm.news_subscriptions.iter().any(|(id, ..)| *id == instrument) {
            return;
        }
        // A subscription waiting on the lookup that will name its contract is
        // as much a reference as a live one. The pending record carries the
        // slot, so a slot reclaimed while its lookup is out is handed to
        // another contract and then subscribed with the first one's id: the
        // quotes arrive, under the wrong contract, priced on its tick.
        if self.ccp.pending_md_subscribe.iter().any(|(_, p, _)| p.instrument == instrument)
            || self.ccp.resolved_md_subscribe.iter().any(|(_, p)| p.instrument == instrument)
        {
            return;
        }
        // A holding is a reference to the contract as much as a subscription
        // is. Dropping the last subscription on something the account still
        // owns handed the slot to the next contract, which then reported the
        // previous one's position as its own.
        if self.context.position(instrument) != 0.0 {
            if !self.pinned_by_position.contains(&instrument) {
                self.pinned_by_position.push(instrument);
            }
            return;
        }
        self.pinned_by_position.retain(|id| *id != instrument);
        // Releasing a slot purges what is queued under it: the move that tells
        // its watchers where the contract went, and the reason no subscription
        // could be made for it at all. Both name the slot, both are the only
        // thing their reader will ever be told, and a subscription the venue
        // refused asks for its slot back in the same breath as it states the
        // reason. Keep the slot until they have been read.
        if self.shared.market.a_move_is_pending_from(instrument)
            || self.shared.market.a_failure_is_pending_from(instrument)
        {
            if !self.slots_awaiting_their_word.contains(&instrument) {
                self.slots_awaiting_their_word.push(instrument);
            }
            return;
        }
        if self.context.market.unregister(instrument).is_some() {
            // Said, so the surfaces stop naming a slot this contract no longer
            // holds. They cache the slot a contract was given, and the slot
            // goes to the next contract that needs one: an order placed on the
            // cached number was recorded against the new occupant, and its
            // fill moved that contract's position.
            // Named by the occupancy that is ending, not by how far this engine
            // had read: a slot goes to the next contract that needs one, and
            // two requests numbered before they are sent can reach here in
            // either order, so a watermark can be higher than the occupancy
            // that followed it.
            let ending = self.farm.what_took_it(instrument);
            self.shared.market.note_released_slot(instrument, ending);
            // Zero the shared-side quote so a reused slot cannot serve the
            // previous contract's prices before its first tick.
            self.shared.market.push_quote(instrument, &crate::types::Quote::default());
            // Tick-by-tick rebuilds bid and ask from deltas against the last
            // pair it saw. Left in place, the next occupant's first delta
            // would be applied to the previous contract's prices.
            //
            // And the holding the slot carried. The engine's own book says the
            // slot is flat or the check above would have kept it, but what a
            // caller reads is a second copy of that figure: left as it was,
            // the next contract to take the slot is read as holding the
            // previous one's quantity until the venue states otherwise.
            self.shared.portfolio.set_position(instrument, 0.0);
            // And the model the venue last published for it, which is kept by
            // slot: the next contract handed this one was solved against the
            // previous contract's volatility and price, and answered finite.
            self.shared.market.forget_option_model(instrument);
            self.shared.market.forget_subscription_failures(instrument);
            // What the extra series stated and nobody read goes with the
            // subscription: held, it outlives every caller that asked and the
            // next one to watch this slot is handed readings from before it.
            self.shared.market.forget_series_ticks(instrument);
            // And what was asked for on it. The list is the caller's intent
            // and outlives a connection on purpose, but not the slot: left
            // behind, the next contract to take this slot was asked for the
            // previous contract's series, which nobody watching it named.
            self.farm.asked_generic_ticks.remove(&instrument);
            // And when each of them was asked for. Left behind, the numbers
            // belong to the contract that has gone and are read against
            // withdrawals of the one that takes the slot next.
            self.farm.forget_what_was_asked_on(instrument);
            log::info!("Reclaimed instrument slot {instrument}");
        }
    }

    /// Run the hot loop under `catch_unwind`.
    ///
    /// On a panic, the payload is logged and `Event::Disconnected` is emitted,
    /// so a consumer sees a dead engine at once rather than on whichever
    /// outbound call fails next. This is what the engine-spawn site calls,
    /// rather than `run` directly.
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
            log::error!("Engine hot loop panicked, emitting Disconnected: {msg}");
            // Before anything is told the engine died, because a worker still
            // dialling outlives the notice: taken back first, none of them can
            // open a session on behalf of a loop that is gone.
            self.take_back_the_recovery_still_in_flight();
            // Recorded before anything is told of the loss, so a reader woken
            // by it finds the reason already there. Written afterwards, a
            // caller that wakes in between cannot tell this from a loss the
            // engine is still working on, and waits on a recovery that has
            // nothing left to run it.
            shared
                .reference
                .set_session_over(retry::DisconnectReason::EngineStopped.as_str());
            shared.set_connection_lost();
            emit(&event_tx, Event::Disconnected);
        }
    }

    /// Run the hot loop. Blocks until Shutdown command received.
    pub fn run(&mut self) {
        if let Some(core) = self.core_id {
            Self::pin_to_core(core);
        }

        // The venue restates the account's figures on its own schedule, which
        // is unhurried: a loop that has just started waits tens of seconds for
        // its first set, and reports an account holding nothing until then.
        // Asking on the way in is answered in under a second.
        let account = self.account_id.clone();
        if !account.is_empty() {
            // This connection has stated nothing about the account yet. Said
            // here as the other two latch sites say it: left to the flag's
            // starting value, a loop entered on state that has run before is
            // gated out of the latch below, and its download squares nothing.
            self.shared.portfolio.account_download_is_pending();
            // The download a session opens with arrives under the key the
            // logon asked with, not under this refresh. Recorded before the
            // refresh draws a key of its own, so the end that squares the
            // account is the end of the request that is carrying it.
            self.shared.portfolio.holdings_restated_under(
                crate::engine::hot_loop::ccp::OPENING_ACCOUNT_REQUEST,
            );
            self.ccp.send_account_refresh(&account, &mut self.ccp_conn, &mut self.hb, &self.shared);
        }

        self.running = true;
        // A loop that starts again starts with no recovery in flight.
        self.reconnect_cancel.store(false, Ordering::Relaxed);

        while self.running {
            self.context.loop_iterations += 1;

            // 1. Busy-poll market data farm socket (non-blocking recv)
            self.farm.poll_market_data(
                &mut self.farm_conn, &mut self.context, &self.shared,
                &self.event_tx, &mut self.hb,
            );

            // 1b. Busy-poll historical socket for tick-by-tick data
            self.poll_historical();

            // 1c. The security definition farm, which carries the calendar.
            self.secdef.poll(
                &mut self.secdef_conn, &self.shared,
                &self.event_tx, &mut self.hb,
            );

            // 1c. Hand every scan batch to CCP, which resolves the rows whose
            // contracts are not yet held and releases a scan's batches in the
            // order they arrived.
            for (req_id, result) in std::mem::take(&mut self.hmds.scanner_batches) {
                self.ccp.start_scanner_enrichment(
                    req_id, result, &mut self.ccp_conn, &self.shared, &mut self.hb,
                );
            }

            // 2. Drain pending orders → build → sign → send to auth
            //    Skip if CCP is disconnected — orders stay in buffer for retry after
            // reconnect.
            order_builder::drain_and_send_orders(
                &mut self.ccp_conn, &mut self.context, &self.account_id, &mut self.hb,
                self.ccp.disconnected, &self.shared,
                self.ccp.recovery_sweep_at.is_some(), &self.event_tx,
            );

            // A write that abandoned the transport leaves it unable to carry
            // anything out while the peer may still be sending, so nothing
            // read-side would ever notice. Without this the liveness deadline
            // never fires, no reconnect is scheduled, and the session sits
            // write-dead while reporting itself connected.
            self.disconnect_write_dead_transports();

            // 3. Busy-poll auth socket for execution reports
            self.ccp.poll_executions(
                &mut self.ccp_conn, &mut self.context, &self.shared,
                &self.event_tx, &mut self.hb, &self.account_id,
            );
            self.send_resolved_subscriptions();

            // A holding that has since been closed releases the slot the
            // caller already asked to free.
            if !self.pinned_by_position.is_empty() {
                for instrument in std::mem::take(&mut self.pinned_by_position) {
                    if self.context.position(instrument) == 0.0 {
                        // Reclaiming may still be refused for a reason that has
                        // nothing to do with the position — a working order, a
                        // subscription taken out since. `try_reclaim_instrument`
                        // records it again where that reason is the position,
                        // and this keeps it where the reason is anything else,
                        // so the request to free the slot is not lost either way.
                        self.try_reclaim_instrument(instrument);
                        if self.context.market.con_id(instrument).is_some()
                            && !self.pinned_by_position.contains(&instrument)
                        {
                            self.pinned_by_position.push(instrument);
                        }
                    } else {
                        self.pinned_by_position.push(instrument);
                    }
                }
            }
            self.ccp.sweep_recovery(&mut self.context, &self.shared, &self.event_tx);
            self.ccp.sweep_pending_matching_symbols(&self.shared);
            self.ccp.sweep_pending_option_params(&self.shared);
            self.ccp.sweep_pending_advisor(&self.shared);
            self.ccp.sweep_pending_schedule_pairs(&self.shared, &self.event_tx);
            self.ccp.sweep_scanner_enrichments(&self.shared);
            self.ccp.sweep_contract_details(&self.shared, &self.event_tx);
            self.ccp.sweep_pending_subscribes(&mut self.context, &self.shared);
            self.ccp.sweep_pending_named(&self.shared);
            self.ccp.sweep_completed_orders_request(
                &mut self.ccp_conn, &mut self.hb, &self.shared,
            );

            // 4. Check control_plane_rx (SPSC) for commands
            self.poll_control_commands();

            // After the commands, not before them. A request a caller has sent
            // is only in the buffer once the drain above has taken it in, so a
            // reclaim asked first could not see it — and the order that frees
            // the slot is retired a few lines earlier in the same pass, which
            // is the sequence that hands a slot away with a submit for it
            // still in the channel. A slot freed one pass later costs nothing.
            self.reclaim_slots_no_order_holds();

            // 5. Heartbeat check (auth 10s, farm 30s)
            self.check_heartbeats();

            // 5b. Poll pending reconnects and schedule the next attempts
            // (jittered backoff instead of immediate re-dials)
            //
            // None of it once the stop has been processed. Control commands
            // are read earlier in this same lap, so a stop that arrived while
            // an attempt was already in flight would otherwise be followed by
            // that attempt's connection being INSTALLED — a freshly
            // authenticated session opened at the venue after the caller was
            // told the engine stopped, never logged out, and on the trading
            // connection one the account reads as competing with itself.
            // Replay is the same shape: the stop has just withdrawn every
            // subscription, and this would put back whatever was still queued.
            if self.running {
                // Before what has come back is taken. The time the caller
                // allowed recovery bounds the attempt in flight, and taking
                // its answer first cleared the record the deadline is read
                // from — so a connection that came up after the caller had
                // given up on it was installed anyway, and nothing said so.
                self.abandon_recovery_past_its_deadline(Instant::now());
                self.poll_farm_reconnect();
                // What a reconnect has still to put back. Paced, and paced on
                // the passes the loop is already making rather than by
                // holding it.
                self.drive_replay();
                self.poll_ccp_reconnect();
                self.poll_optional_farm_reconnects();
            }
            // The session's budget is the trading connection's: settled on
            // that connection alone. Settled on the quote feed as well, a
            // feed given up on for the session kept the budget from ever
            // settling, and the trading connection's next drop was given up
            // on at once.
            self.budget.settle(
                Instant::now(),
                self.reconnect_cfg.stable_window,
                !self.ccp.disconnected,
            );
            self.farm_budget.settle(
                Instant::now(), self.reconnect_cfg.stable_window, !self.farm.disconnected,
            );
            // Each optional farm's own, on its own connection: one of them
            // holding together says nothing about the other, and nothing
            // about the session.
            self.hmds_budget.settle(
                Instant::now(), self.reconnect_cfg.stable_window, self.hmds_conn.is_some(),
            );
            self.secdef_budget.settle(
                Instant::now(), self.reconnect_cfg.stable_window, self.secdef_conn.is_some(),
            );
            // Not once the stop has been processed. Control commands are read
            // further up this same lap, so a stop that arrived with a
            // transport already down would otherwise launch a fresh logon
            // after the caller had been told the engine stopped — a session
            // opened at the venue after the disconnect, never logged out, and
            // on the trading connection one the account reads as a second
            // session competing with itself.
            if self.running {
                // Before the schedulers, which return as soon as they find an
                // attempt in flight: the time the caller allowed recovery
                // bounds that attempt too.
                self.abandon_recovery_past_its_deadline(Instant::now());
                self.maybe_spawn_farm_reconnect();
                self.maybe_spawn_ccp_reconnect();
                self.maybe_spawn_hmds_reconnect();
                self.maybe_spawn_secdef_reconnect();
            }
            self.end_the_trading_connection_once_recovery_is_over();

            // 6. Wake any waiting consumers (e.g. Python event loop)
            self.shared.notify();

            // 7. With every transport down there is no socket to poll and no
            //    socket to be quick for, so the spin above buys nothing and
            //    costs a core — on a laptop that is the battery and the fans,
            // which is how this gets noticed. Parking only in that
            //    state leaves the connected path exactly as it was; a reconnect
            //    is scheduled on a backoff measured in seconds, so a millisecond
            //    here delays nothing, including shutdown.
            // A historical connection that never came up has no socket either,
            // and is not marked down because it was never up to go down: a
            // session whose token was refused at logon runs with none at all.
            // Read as still live, the spin above ran for the whole of an
            // outage on exactly the sessions that have one transport fewer to
            // be quick for.
            let no_historical_socket = self.hmds.disconnected || self.hmds_conn.is_none();
            if self.farm.disconnected && self.ccp.disconnected && no_historical_socket {
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
        }

        self.take_back_the_recovery_still_in_flight();
    }

    /// Take back whatever recovery is still in flight, wait it out, and say
    /// goodbye on anything it opened after being taken back.
    ///
    /// A worker reads the flag between the phases of a handshake, so one that
    /// is past the last of those reads finishes and hands back a session that
    /// is open at the venue. Dropped in silence that is a session the venue
    /// has to time out, and this account permits one at a time: the next
    /// connection then races one the venue still believes is live. It is told
    /// the session is going, which is what a stop tells it on the connection
    /// the loop holds.
    ///
    /// What the other three return is closed by being dropped, and dropped
    /// here rather than left in the channel: this loop can be run again, and
    /// a landed connection left behind would be installed by the next run —
    /// a socket from the session before it, resubscribed and announced as
    /// that session's own.
    ///
    /// Run where the loop returns, and again where it left by panicking,
    /// which does not reach the return. Dropping the handles there instead
    /// detached the workers with the flag still down, and they went on
    /// resolving, connecting and authenticating after the engine had
    /// announced its death.
    fn take_back_the_recovery_still_in_flight(&mut self) {
        self.reconnect_cancel.store(true, Ordering::Relaxed);
        // Only the trading connection's worker is waited for: its landed
        // session must be logged out below. The others close their socket
        // when the receiver they would hand it to is gone, and waiting on one
        // held the healthy trading connection unread and unheartbeated for
        // the length of a dial.
        for (trading, worker) in self.reconnect_workers.drain(..) {
            if trading {
                let _ = worker.join();
            }
        }
        if let Some(rx) = self.pending_ccp_reconnect.take()
            && let Ok(Ok(conn)) = rx.try_recv()
        {
            log::warn!("a trading session opened after the stop — saying so before it goes");
            let mut landed_too_late = Some(conn);
            self.ccp.send_logout(&mut landed_too_late, &mut self.hb);
        }
        self.pending_farm_reconnect = None;
        self.pending_hmds_reconnect = None;
        self.pending_secdef_reconnect = None;
    }

    fn emit_hmds_unavailable(&self, req_id: u32, from_historical: bool) {
        push_hmds_unavailable(&self.shared, req_id, from_historical);
    }

    /// Poll the historical connection, and where the poll saw it go, take the
    /// scan rows parked behind their naming with the scans the drop failed.
    /// Every scan is the historical service's; left parked, a batch was
    /// released at its deadline and the caller handed rows and an end for a
    /// scan it had just been told had failed.
    pub(crate) fn poll_historical(&mut self) {
        let historical_was_up = self.hmds_conn.is_some();
        self.hmds.poll(&mut self.hmds_conn, &self.shared, &self.event_tx, &mut self.hb);
        if historical_was_up && self.hmds_conn.is_none() {
            self.abandon_parked_scans();
        }
    }

    /// Drop the scan rows parked behind their contract lookups. The scans
    /// they belong to are failed when the historical connection is given
    /// up, so releasing their rows on the sweep would hand a caller an
    /// answer to a scan it was just told had failed.
    fn abandon_parked_scans(&mut self) {
        self.ccp.pending_scanner_enrichment.clear();
        self.hmds.scanner_batches.clear();
    }

    /// Give up the historical connection and drop what was parked behind
    /// it. Every path that abandons this connection outside `poll_historical`
    /// routes through here, so the parked scans go with it however the drop
    /// is seen.
    fn give_up_historical(&mut self) {
        self.hmds.disconnect(&mut self.hmds_conn, &self.shared, &self.event_tx);
        self.abandon_parked_scans();
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
            Err(std::sync::mpsc::TryRecvError::Empty)        => false,
            Err(std::sync::mpsc::TryRecvError::Disconnected) => true,
        };

        // Drain the buffer first, so the loop body can mutably borrow self.
        // Requests held for want of a contract id come first: they were asked
        // for before anything still in the buffer.
        let mut cmds: Vec<ControlCommand> = std::mem::take(&mut self.ccp.resolved_named);
        cmds.append(&mut self.cmd_buf);
        for cmd in cmds {
            // Nothing is carried out after the stop. A caller on another
            // thread can have a subscription or an order in the same batch as
            // the disconnect, and by then the stop has already withdrawn every
            // stream and refused everything still queued — so one sent from
            // here is live at the venue with no engine left to withdraw it,
            // and an order queued from here is queued after the refusal swept.
            //
            // A request waiting on an answer is answered by the drop: its
            // reply channel goes with the command, and the caller is told the
            // engine stopped before it answered. An order is not — it was
            // told it was accepted and given an id, and nothing is waiting —
            // so it is put where the stop's own sweep looks and refused below
            // rather than dropped and never spoken of again.
            if !self.running {
                if let ControlCommand::Order(req) = cmd {
                    self.context.pending_orders.push(req);
                }
                continue;
            }
            // A caller who passed the contract it wrote down rather than the
            // venue's id for it gets the lookup made on its behalf, and the
            // request arrives here again once the venue has named it.
            let Some(cmd) = self.ccp.hold_until_named(cmd, &mut self.ccp_conn, &mut self.hb, &self.shared)
            else {
                continue;
            };
            // A contract numbered beyond what a request carries. Refused where
            // every request passes rather than narrowed on the way out: a
            // request naming one is asked under the number that survives the
            // narrowing, and the venue answers it with that contract's data.
            if let Some(con_id) = con_id_beyond_the_wire(&cmd) {
                let told = format!(
                    "contract {con_id} is numbered beyond what a request carries it in, so \
                     this one would be answered for a different contract",
                );
                log::warn!("{told}");
                match &cmd {
                    ControlCommand::Subscribe { reply_tx, .. } => {
                        if let Some(tx) = reply_tx {
                            let _ = tx.try_send(Err(told.into()));
                        }
                    }
                    ControlCommand::SubscribeTbt { reply_tx, .. }
                    | ControlCommand::SubscribeNews { reply_tx, .. }
                    | ControlCommand::RegisterInstrument { reply_tx, .. } => {
                        if let Some(tx) = reply_tx {
                            let _ = tx.try_send(Err(told));
                        }
                    }
                    ControlCommand::FetchContractDetails { req_id, .. } => {
                        self.shared.reference.push_historical_error(
                            *req_id, crate::error_codes::Refusal::VALIDATION, told,
                        );
                        self.shared.reference.push_contract_details_end(*req_id);
                    }
                    _ => {
                        if let Some(req_id) = ccp::request_id(&cmd) {
                            // The request is malformed, not a difficulty the
                            // data service had with one it answered.
                            push_hmds_refusal(
                                &self.shared, req_id, crate::error_codes::Refusal::VALIDATION, told,
                                matches!(cmd, ControlCommand::FetchHistorical { .. }),
                            );
                        }
                    }
                }
                continue;
            }
            match cmd {
                ControlCommand::Subscribe { contract, filters, mode_9887, regulatory_snapshot, generic_ticks, reply_tx, issued, } => {
                    self.heard_up_to = self.heard_up_to.max(issued);
                    let ContractRef { con_id, symbol, exchange, sec_type, currency, last_trade_date, strike, right, multiplier } = contract;
                    // The strategies series is asked for the way every other is
                    // and the scan is what tells the venue what to look for, so
                    // it is handed to the farm before the subscription goes.
                    if con_id > 0
                        && let Some(scan) = self.shared.reference.spread_scan(con_id as u32)
                    {
                        self.farm.note_spread_scan(con_id, scan);
                    }
                    // What tells two conId-less contracts on one underlying apart.
                    // Built by the same function an order uses, or the two
                    // describe one contract differently: the slot a
                    // subscription took would not be found again by an order,
                    // which would take a second one — with no quote on it, and
                    // stating the wrong currency because the slot it did take
                    // never recorded one.
                    let option_key = crate::types::model::contract_identity(
                        &last_trade_date, strike, &right, &multiplier, &currency,
                    );
                    // Registered without answering yet: a contract with no conId
                    // has no client-side identity, so whether this is a duplicate
                    // can only be settled here, against the slot the engine just
                    // resolved. Refusing after the subscribe had already gone out
                    // left the caller told it failed while a live subscription
                    // bound the second contract's tag and minTick onto the first,
                    // with no id to cancel it by.
                    // And what narrows the lookup that will name it. None of
                    // these says what the contract is, so none of them belongs
                    // in the identity — and each of them decides which listing
                    // the venue answers with, so two descriptions the venue
                    // would answer differently are not one contract. Left out,
                    // the second description followed the first one's
                    // subscription and was served the other listing's prices
                    // under its own number.
                    let narrowing = format!(
                        "{}|{}|{}|{}|{}|{}",
                        filters.primary_exchange, filters.local_symbol, filters.trading_class,
                        filters.sec_id_type, filters.sec_id, filters.issuer_id,
                    );
                    let narrowing = if narrowing.chars().all(|c| c == '|') { String::new() } else { narrowing };
                    let registered = self.register_or_reject(con_id, symbol.clone(), &sec_type, &exchange, &option_key, &narrowing, &None);
                    // Held against the slot before the subscription goes out,
                    // so the frame that carries them is built from them and the
                    // rebuild after a reconnect asks for them again.
                    //
                    // Written against the slot only where this caller is the
                    // one the subscription goes out for. A caller landing on a
                    // slot another caller is already being served on is a
                    // joiner: its list is added to what the slot asks for as
                    // the series are asked for, in the arm below, because
                    // written here it would leave nothing new to ask for —
                    // and written over theirs it would leave the venue serving
                    // series the rebuild no longer asks for.
                    // And never from a chargeable snapshot: the message that
                    // carries one states no extra series at all, so a series
                    // named beside it went out on nothing — and stayed on the
                    // list the rebuild after a reconnect reads, which asked
                    // the venue for it as part of a stream that never named it.
                    if let Some(slot) = registered
                        && !generic_ticks.is_empty()
                        && !regulatory_snapshot
                        && !self.farm.holds_a_stream(slot)
                    {
                        let held = self.farm.asked_generic_ticks.entry(slot).or_default();
                        for tick in &generic_ticks {
                            if !held.contains(tick) {
                                held.push(*tick);
                            }
                        }
                    }
                    match registered {
                        None => {
                            if let Some(tx) = &reply_tx {
                                let _ = tx.try_send(Err(format!(
                                    "instrument table full: cannot subscribe to {symbol}"
                                ).into()));
                            }
                        }
                        // Already subscribed, so nothing goes to the venue
                        // again: one contract holds one subscription on the
                        // wire, and the caller watches the one that is up, so
                        // two parts of one program may watch one contract.
                        //
                        // Except the chargeable snapshot, which is a request of
                        // its own and not a share of somebody's stream. Handed
                        // the stream instead it was never sent, never billed
                        // and never refused for want of the entitlement, and
                        // the caller heard the snapshot end off ticks it did
                        // not ask for — a paid answer that reached nobody.
                        //
                        // And what is followed is a stream. A snapshot holds
                        // the slot but is withdrawn as soon as it completes,
                        // so a subscribe pointed at one was never sent and the
                        // withdrawal took the record out from under it.
                        Some(id) if self.farm.holds_a_stream(id) && !regulatory_snapshot => {
                            // The answer first, because nothing is done for a
                            // caller that has stopped waiting: its wait is
                            // bounded and this loop is not, and it reports a
                            // refusal and keeps no record of the slot. Asked
                            // for anyway, the series it named were served for
                            // the life of a subscription the caller has no
                            // part in, with nothing able to withdraw them.
                            if let Some(tx) = &reply_tx
                                && matches!(
                                    tx.try_send(Ok(id)),
                                    Err(std::sync::mpsc::TrySendError::Disconnected(_)),
                                )
                            {
                                continue;
                            }
                            // This caller is asking for the contract too, so
                            // the subscription it is served off is the one it
                            // asked for: a withdrawal decided before it asked
                            // is not about that subscription.
                            self.farm.note_subscription_asked_on(id, issued);
                            self.farm.note_series_asked_on(id, &generic_ticks, issued);
                            // The joiner's own series, where the stream it is
                            // joining was not asked for them. Nothing else
                            // sends them: the subscription is already up, so
                            // this is the one chance to ask.
                            if !generic_ticks.is_empty() {
                                self.farm.also_ask_for_series(
                                    id,
                                    con_id,
                                    &generic_ticks,
                                    &self.context,
                                    &mut self.farm_conn,
                                    &mut self.hb,
                                );
                            }
                        }
                        Some(id) => {
                            self.farm.note_subscription_asked_on(id, issued);
                            // The same rule as the definition path beside it:
                            // a request that begins the stream owns the
                            // occupancy, and one that joins what is already
                            // there did not begin it and must not rename it.
                            if self.farm.holds_a_stream(id) {
                                self.farm.note_subscription_began_under(id, issued);
                            } else {
                                self.farm.note_it_changed_hands(id, issued);
                            }
                            self.farm.note_series_asked_on(id, &generic_ticks, issued);
                            // The venue states it on the logon. Count streams
                            // waiting on a definition or a reconnect too: they
                            // were admitted already and still need their line.
                            let allowance = self.ccp_conn.as_ref().map_or(40, |c| c.market_data_allowance);
                            let subscribed = self.context.market.active_instruments().filter(|(at, _)| {
                                *at != id && (self.farm.holds_a_stream(*at)
                                    || self.ccp.pending_md_subscribe.iter().any(|(_, p, _)| {
                                        p.instrument == *at && !p.regulatory_snapshot
                                    })
                                    || self.ccp.resolved_md_subscribe.iter().any(|(_, p)| {
                                        p.instrument == *at && !p.regulatory_snapshot
                                    }))
                            }).count();
                            if !regulatory_snapshot && subscribed >= allowance {
                                log::warn!(
                                    "subscription refused: {subscribed} of {allowance} quote \
                                     lines are in use, which is what the venue allows this session",
                                );
                                if let Some(tx) = &reply_tx {
                                    let _ = tx.try_send(Err(crate::error_codes::Refusal::stated(
                                        101, "Max number of tickers has been reached",
                                    )));
                                }
                                self.try_reclaim_instrument(id);
                                continue;
                            }
                            // A caller that has stopped waiting gets no
                            // subscription. Its wait is bounded and this loop
                            // is not — a redial or a lookup ahead of this
                            // command outlasts it — and it reports a refusal
                            // and keeps no record of the slot. Sent anyway,
                            // the venue streamed a contract for the rest of
                            // the session that nothing could name: the
                            // withdrawal is refused for want of a
                            // subscription, the slot is never given back, and
                            // only the caller happening to ask for the same
                            // contract again recovers either.
                            if let Some(tx) = &reply_tx
                                && matches!(
                                    tx.try_send(Ok(id)),
                                    Err(std::sync::mpsc::TrySendError::Disconnected(_)),
                                )
                            {
                                self.try_reclaim_instrument(id);
                                continue;
                            }
                            // What the contract is, as the caller stated it
                            // or as the venue's own definition has it.
                            let (known_sec_type, _) =
                                self.described_as(con_id, &sec_type, &exchange);
                            if con_id == 0 || known_sec_type.is_empty() {
                                // The venue answers a subscription only when it
                                // is named by contract id, and says nothing at
                                // all — no tick and no refusal — to one named
                                // by symbol. Ask it to name the contract first.
                                //
                                // And one named by id alone is asked about for
                                // the reason beside it: nothing here states what
                                // the contract is, and the encoder describes
                                // every such contract as a smart-routed stock,
                                // which is right for a stock and wrong for a
                                // future, an option or a currency pair. Refused
                                // instead, an id was the one thing a caller
                                // could name a contract by and be turned down
                                // for, though naming it by id is what the venue
                                // answers a definition for. The definition says
                                // what it is and the subscription goes out on
                                // that; a contract the venue names nothing for
                                // is given up on where every other unanswered
                                // lookup is.
                                self.ccp.resolve_for_subscribe(
                                    crate::engine::hot_loop::ccp::PendingSubscribe {
                                        instrument: id,
                                        issued,
                                        con_id,
                                        symbol: symbol.clone(),
                                        exchange: exchange.clone(),
                                        sec_type: sec_type.clone(),
                                        currency: currency.clone(),
                                        filters: filters.clone(),
                                        mode_9887,
                                        regulatory_snapshot,
                                    },
                                    &mut self.ccp_conn,
                                    &mut self.hb,
                                    &self.shared,
                                );
                            } else if regulatory_snapshot && self.farm.disconnected {
                                // As on the resolved path: a snapshot is not
                                // recorded for replay, so one with no
                                // transport to carry it is simply lost — and
                                // said only where no stream on the contract
                                // would hear it as its own refusal.
                                if !self.farm.holds_a_stream(id) {
                                    self.shared.market.push_subscription_failure(
                                        id,
                                        "the quote feed was down when this snapshot was \
                                         asked for, so it was never sent: ask for it \
                                         again once the feed is back"
                                            .to_string(),
                                    );
                                }
                            } else {
                                let (sec_type, exchange) =
                                    self.described_as(con_id, &sec_type, &exchange);
                                self.farm.send_mktdata_subscribe(
                                    con_id, &symbol, &exchange, &sec_type,
                                    &last_trade_date, strike, &right, &multiplier,
                                    id, mode_9887, regulatory_snapshot,
                                    &mut self.farm_conn,
                                    &mut self.hb,
                                );
                            }
                        }
                    }
                }
                ControlCommand::AlsoAskForSeries {
                    instrument, con_id, generic_ticks, took_it, issued,
                } => {
                    self.heard_up_to = self.heard_up_to.max(issued);
                    // Not this subscription's caller. The caller that asked can
                    // be gone by the time this is read, and the slot given to
                    // another contract: asked for anyway, the series was served
                    // to a subscription nobody had named it on, and recorded
                    // against it so that a later withdrawal of that series was
                    // refused.
                    if self.farm.another_occupancy_holds_it_now(instrument, took_it, con_id) {
                        continue;
                    }
                    // A caller asking for more on a contract is asking for
                    // that contract, so the subscription it is served off is
                    // one it asked for.
                    self.farm.note_subscription_asked_on(instrument, issued);
                    self.farm.note_series_asked_on(instrument, &generic_ticks, issued);
                    self.farm.also_ask_for_series(
                        instrument,
                        con_id,
                        &generic_ticks,
                        &self.context,
                        &mut self.farm_conn,
                        &mut self.hb,
                    );
                }
                ControlCommand::StopAskingForSeries {
                    instrument, con_id, took_it, generic_ticks, issued,
                } => {
                    self.heard_up_to = self.heard_up_to.max(issued);
                    self.farm.stop_asking_for_series(
                        instrument,
                        con_id,
                        took_it,
                        &generic_ticks,
                        issued,
                        &mut self.farm_conn,
                        &mut self.hb,
                    );
                }
                ControlCommand::MoveInstalled { from, into, took_it } => {
                    // The callers are recorded as watching it now, so the hold
                    // that kept it up for them is over.
                    self.shared.market.note_a_move_is_read(from, into);
                    if took_it != 0 {
                        // And they hold it under a number of their own: what
                        // could withdraw it before cannot any more.
                        self.farm.note_it_changed_hands(into, took_it);
                    } else {
                        // Nobody arrived. The subscription held up for them is
                        // nobody's, and goes under the number it is held by
                        // here rather than one the client would have to guess.
                        let held_under = self.farm.what_took_it(into);
                        self.farm.send_mktdata_unsubscribe(
                            into,
                            0,
                            held_under,
                            &[],
                            self.heard_up_to,
                            false,
                            &mut self.farm_conn,
                            &mut self.hb,
                        );
                        self.try_reclaim_instrument(into);
                    }
                }
                ControlCommand::Unsubscribe { instrument, con_id, took_it, series, issued, } => {
                    self.heard_up_to = self.heard_up_to.max(issued);
                    // A subscription this caller is still waiting on the
                    // contract's name for. Left queued, the answer to that
                    // lookup arrives after the caller's record here has gone
                    // and opens the stream anyway: nothing is left that can
                    // withdraw it, so the venue serves it and holds its slot
                    // for the rest of the session.
                    // The lookup this caller is waiting on and no other. A
                    // second caller on the same description has a lookup of its
                    // own on the same slot, and retiring that one left it told
                    // it owned a subscription the venue would never be asked
                    // for.
                    let waiting_on_a_name = |p: &crate::engine::hot_loop::ccp::PendingSubscribe| {
                        p.instrument == instrument
                            && if took_it != 0 {
                                p.issued == took_it
                            } else {
                                con_id == 0 || p.con_id == con_id || p.con_id == 0
                            }
                    };
                    self.ccp.pending_md_subscribe.retain(|(_, p, _)| !waiting_on_a_name(p));
                    self.ccp.resolved_md_subscribe.retain(|(_, p)| !waiting_on_a_name(p));
                    let moving_in = self.shared.market.a_move_is_on_its_way_into(instrument);
                    self.farm.send_mktdata_unsubscribe(
                        instrument,
                        con_id,
                        took_it,
                        &series,
                        issued,
                        moving_in,
                        &mut self.farm_conn,
                        &mut self.hb,
                    );
                    // The tags are dead with the requests that earned them, and
                    // `try_reclaim_instrument` below only drops them when the
                    // slot itself goes — so a pinned instrument accumulated one
                    // per ack until the next farm drop. News is the
                    // one reader that outlives the L1 request: ticker setup
                    // registers into the same map and news routes on it, so a
                    // live news subscription keeps them.
                    //
                    // And only where the withdrawal went. Four of them do not:
                    // the slot has been given to another contract, its callers
                    // were sent elsewhere, a caller is on its way onto it, or
                    // the subscription now on it began after this withdrawal
                    // was decided. On each the subscription stays up and the
                    // venue goes on sending, and the tags were handed back
                    // anyway — so every record after it named a number no
                    // contract here held and was dropped with one line in the
                    // log, while the caller that joined was told it was
                    // watching. Nothing recovered it either: a slot with a
                    // subscription on it is never reclaimed, so nothing asked
                    // the venue again and nothing bound the number back.
                    if !self.farm.holds_market_data(instrument)
                        && !self.farm.news_subscriptions.iter().any(|(id, ..)| *id == instrument)
                    {
                        self.context.market.clear_server_tags_for(instrument);
                    }
                    self.try_reclaim_instrument(instrument);
                }
                ControlCommand::SubscribeTbt { contract, req_id, tbt_type, number_of_ticks, ignore_size, reply_tx } => {
                    let ContractRef { con_id, symbol, sec_type, exchange, .. } = contract;
                    // A stream is asked for by the venue's id for the contract.
                    // Sent with none, the venue answers "Unknown contract"
                    // against a query nothing here has told the caller about,
                    // and the caller waits on a stream that was refused before
                    // it began. Told here instead — the surfaces resolve a
                    // description before it reaches this point, and this is
                    // what catches the one that does not.
                    if con_id == 0 {
                        let reason = format!(
                            "a {sec_type} trade stream on {symbol} was asked for without the \
                             venue's id for the contract, which is what a stream is asked for by",
                        );
                        log::error!("{reason}");
                        // Malformed rather than answered badly: the stream
                        // names no contract, so nothing was ever asked.
                        push_hmds_refusal(
                            &self.shared, req_id.max(0) as u32, crate::error_codes::Refusal::VALIDATION,
                            reason.clone(), false,
                        );
                        if let Some(tx) = reply_tx.as_ref() {
                            let _ = tx.try_send(Err(reason));
                        }
                        continue;
                    }
                    // One stream per number, as the bar stream beside this
                    // already insists. Taken twice under one number, the venue
                    // answers both with the same number of its own: every tick
                    // is handed to the caller twice, and the withdrawal takes
                    // one record away, finds the other still naming that
                    // number, and returns without telling the venue anything —
                    // so the ticks go on arriving under a stream the caller
                    // has withdrawn. The surfaces refuse this already; what
                    // reaches here is a caller driving the channel itself.
                    if self.hmds.tbt_subscriptions.iter().any(|s| s.caller_req_id == req_id) {
                        let reason = format!(
                            "a trade stream is already running under {req_id}; a second one                              would be answered under the same number as the first",
                        );
                        log::error!("{reason}");
                        push_hmds_refusal(
                            &self.shared, req_id.max(0) as u32,
                            crate::error_codes::Refusal::VALIDATION, reason.clone(), false,
                        );
                        if let Some(tx) = reply_tx.as_ref() {
                            let _ = tx.try_send(Err(reason));
                        }
                        continue;
                    }
                    // With no data connection the stream is refused now, on
                    // both channels the caller reads, as a scan or a bar
                    // stream is. Accepted, the caller held a stream that would
                    // begin only if a reconnect re-sent it, and nothing said so.
                    if self.hmds_conn.is_none() {
                        self.emit_hmds_unavailable(req_id.max(0) as u32, false);
                        if let Some(tx) = reply_tx.as_ref() {
                            let _ = tx.try_send(Err(HMDS_UNAVAILABLE.to_string()));
                        }
                        continue;
                    }
                    // Registered with what the contract is, so the slot carries
                    // it and the subscription can state it.
                    if let Some(id) = self.register_or_reject(con_id, symbol, &sec_type, &exchange, "", "", &reply_tx) {
                        let mts = self.context.market.min_tick_scaled(id);
                        self.hmds.send_tbt_subscribe(
                            req_id, con_id, id, tbt_type, number_of_ticks, ignore_size,
                            &sec_type, &exchange, mts,
                            &mut self.hmds_conn, &mut self.hb,
                        );
                    }
                }
                ControlCommand::UnsubscribeTbt { req_id, instrument } => {
                    // As above: records already queued under this request are
                    // this request's, and a stream reopened under the same
                    // number before the next drain would be served them.
                    self.shared.market.purge_tbt_for(req_id);
                    self.hmds.send_tbt_unsubscribe(req_id, instrument, &mut self.hmds_conn, &mut self.hb);
                    self.try_reclaim_instrument(instrument);
                }
                ControlCommand::SubscribeNews { con_id, symbol, sec_type, providers, reply_tx } => {
                    // The command carries no exchange. Given the security
                    // type in that slot instead, it was recorded as where the
                    // contract trades, and every order on the contract went
                    // out routed to a destination of that name.
                    if let Some(id) = self.register_or_reject(con_id, symbol, &sec_type, "", "", "", &reply_tx) {
                        // Allocate req_id from farm's counter (shared ID space)
                        let req_id = self.farm.next_md_req_id;
                        self.farm.next_md_req_id += 1;
                        // Recorded where the acknowledgement arrives, or the
                        // tag is never filed and no headline is delivered.
                        self.farm.send_news_subscribe(
                            con_id, id, &sec_type, &providers, req_id, &mut self.farm_conn, &mut self.hb,
                        );
                    }
                }
                ControlCommand::UnsubscribeNews { subject } => {
                    // Named by contract where the caller holds no slot, which
                    // this side is the only one that can resolve: it is what
                    // registered the contract in the first place.
                    let instrument = match subject {
                        NewsSubject::Slot(id) => Some(id),
                        NewsSubject::Contract(con_id) => {
                            self.context.market.instrument_by_con_id(con_id)
                        }
                    };
                    if let Some(instrument) = instrument {
                        self.farm.send_news_unsubscribe(
                            instrument, &mut self.farm_conn, &mut self.hb,
                        );
                        self.try_reclaim_instrument(instrument);
                    }
                }
                ControlCommand::UpdateParam { key, value } => {
                    let _ = (key, value);
                }
                ControlCommand::Ping => {
                    // On-demand RTT sample. Reuses the liveness
                    // test-request machinery; a pending liveness test is
                    // already a measurement in flight, so don't stomp it.
                    if self.hb.pending_ccp_test.is_none()
                        && let Some(conn) = self.ccp_conn.as_mut() {
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
                ControlCommand::Order(req) => {
                    self.context.pending_orders.push(req);
                }
                ControlCommand::RegisterInstrument { contract, identity, reply_tx } => {
                    let ContractRef { con_id, symbol, sec_type, exchange, .. } = contract;
                    self.register_or_reject(con_id, symbol, &sec_type, &exchange, &identity, "", &reply_tx);
                }
                ControlCommand::FetchHistorical { contract, req_id, end_date_time, duration, bar_size, what_to_show, use_rth, keep_up_to_date, include_expired, .. } => {
                    let ContractRef { con_id, symbol, sec_type, exchange, .. } = contract;
                    if keep_up_to_date
                        && !crate::control::historical::BarSize::from_api_str(&bar_size)
                            .is_ok_and(|size| size.supports_keep_up_to_date())
                    {
                        // What keeps a bar current arrives as five-second
                        // bars, so a size the fold cannot build out of those is
                        // refused rather than folded anyway: it relabels each
                        // five-second bar as the shorter one and hands the
                        // caller five times the volume under a size nothing
                        // traded in. The surface refuses these before the
                        // command is sent; a caller reaching this loop by the
                        // control channel goes past it, as it does the reading
                        // of the series name beside this.
                        let told = format!(
                            "bars of {bar_size} cannot be kept up to date: what keeps them \
                             current arrives in five-second bars, and this one cannot be \
                             built out of those",
                        );
                        log::error!("historical req_id={req_id}: {told}");
                        // Refused without ending anything, as the duplicate
                        // number below it is: a number already answering goes
                        // on answering. Released with an empty terminal
                        // response, the live request's end fired before its
                        // bars had arrived and every bar after it went out as
                        // an update. Nothing waits on that sentinel here —
                        // the surfaces refuse this before it is sent, so only
                        // a caller on the control channel reaches it, and
                        // those do not wait.
                        push_hmds_refusal(
                            &self.shared, req_id, crate::error_codes::Refusal::VALIDATION,
                            told, false,
                        );
                    } else if self.hmds_conn.is_none() {
                        // keepUpToDate sends via CCP but bars/end arrive on
                        // HMDS — both paths need an authed HMDS socket to
                        // deliver a completion.
                        self.emit_hmds_unavailable(req_id, true);
                    } else if keep_up_to_date
                        && !self.hmds.keep_up_to_date_reqs.contains(&req_id)
                        && self.hmds.rtbar_subs.iter().any(|(_, rid, ..)| *rid == req_id)
                    {
                        // The stream half would run under a number a bar
                        // stream of its own already runs under, and the two
                        // would interleave into one queue. A stream that is
                        // itself the half of a request kept up to date is the
                        // historical query's, and that query refuses a second
                        // under its number itself, below.
                        push_hmds_refusal(
                            &self.shared, req_id, crate::error_codes::DUPLICATE_TICKER_ID,
                            format!("a live bar stream is already running under request {req_id}: withdraw it before asking for bars kept up to date under it"),
                            true,
                        );
                    } else if keep_up_to_date {
                        // The bars so far, then the stream that keeps them
                        // current. The venue answers a request to keep bars up
                        // to date with the bars and closes the query, on either
                        // connection it can be sent over; what it keeps sending
                        // is five-second bars, and the bar still forming is
                        // folded from those. Refused, nothing else goes out on
                        // its account: refused for its batch alone, the stream
                        // half still went out and fed the live request's
                        // forming bar a second five-second stream.
                        let sent = self.hmds.send_historical_request_ex(
                            req_id, con_id, &end_date_time, &duration, &bar_size, &what_to_show,
                            use_rth, false, include_expired, &symbol, &sec_type, &exchange,
                            &mut self.hmds_conn, &mut self.hb, &self.shared,
                        );
                        if sent && let Ok(size) = crate::control::historical::BarSize::from_api_str(&bar_size) {
                            self.hmds.keep_up_to_date_reqs.insert(req_id);
                            self.hmds.forming_bars.retain(|f| f.req_id != req_id);
                            self.hmds.forming_bars.push(crate::engine::hot_loop::hmds::FormingBar {
                                req_id,
                                seconds: size.seconds(),
                                opened_at: 0,
                                bar: Default::default(),
                                weighted: 0.0,
                            });
                            self.hmds.send_realtime_bar_subscribe(
                                req_id, con_id, &symbol, &sec_type, &exchange, &what_to_show,
                                use_rth, &mut self.hmds_conn, &mut self.hb,
                            );
                        }
                    } else {
                        self.hmds.send_historical_request_ex(req_id, con_id, &end_date_time, &duration, &bar_size, &what_to_show, use_rth, false, include_expired, &symbol, &sec_type, &exchange, &mut self.hmds_conn, &mut self.hb, &self.shared);
                    }
                }
                ControlCommand::CancelHistorical { req_id } => {
                    // Everything the arm below withdraws, so a request kept up
                    // to date -- whose batch is filed but whose stream is
                    // still running -- is not refused as though it had ended.
                    // A live bar stream under the same number is not one of
                    // them: it was asked for by another call in another id
                    // space. Counted as held, a withdrawal naming no historical
                    // query was not refused, and then took the stream with it
                    // in silence.
                    let held = self.hmds.pending_historical.iter().any(|(_, rid)| *rid == req_id)
                        || self.hmds.held.iter().any(|a| a.req_id == req_id)
                        || self.hmds.keep_up_to_date_reqs.contains(&req_id)
                        || self.ccp.pending_named.iter()
                            .any(|(_, cmd, _)| ccp::request_id(cmd) == Some(req_id));
                    if !held {
                        push_hmds_refusal(
                            &self.shared, req_id,
                            crate::error_codes::NO_SUCH_HISTORICAL_QUERY,
                            format!("no historical query is answering under request {req_id}"),
                            false,
                        );
                    }
                    self.ccp.withdraw_named(req_id);
                    // What the venue already sent and nobody has read yet
                    // goes with the request. Left queued, the next request
                    // under this number is answered with this one's.
                    self.shared.reference.purge_historical_for(req_id);
                    // A request keeping its bars up to date pushes them onto
                    // the live stream as well, under this same number, so
                    // withdrawing it has to take those too — the same thing
                    // the cancel for a live stream does.
                    // A keep-up-to-date request rides a five-second bar
                    // stream held as a separate query. Cancelling withdraws
                    // that query as well as the batch query below, and clears
                    // it from the resubscribe record so a reconnect does not
                    // request it again. Only where the request was kept up to
                    // date: a bar stream of its own under this number belongs
                    // to another call.
                    // By the venue's own number for the stream where it has
                    // given one, as the withdrawal below this does: a stream
                    // named by the id this client made up is not one the venue
                    // finds, and it goes on sending.
                    if self.hmds.keep_up_to_date_reqs.remove(&req_id) {
                        self.shared.market.purge_real_time_bars(req_id);
                        let rtbar_query: Option<String> = self.hmds.rtbar_subs.iter()
                            .find(|(_, rid, ..)| *rid == req_id)
                            .map(|(qid, _, ticker_id, ..)| {
                                ticker_id.map(|t| t.to_string()).unwrap_or_else(|| qid.clone())
                            });
                        self.hmds.rtbar_subs.retain(|(_, rid, ..)| *rid != req_id);
                        self.hmds.rtbar_resub.retain(|r| r.req_id != req_id);
                        if let Some(qid) = rtbar_query {
                            self.hmds.send_historical_cancel(&qid, &mut self.hmds_conn, &mut self.hb);
                        }
                        self.hmds.forming_bars.retain(|f| f.req_id != req_id);
                    }
                    // A keep-up-to-date request rides the five-second stream,
                    // and its routing and half-built bar are held apart from
                    // the request itself. Left behind, a later request under
                    // the same id folds its first bars into the cancelled
                    // one's partial.

                    if let Some(pos) = self.hmds.pending_historical.iter().position(|(_, rid)| *rid == req_id) {
                        let (query_id, _) = self.hmds.pending_historical.remove(pos);
                        self.hmds.send_historical_cancel(&query_id, &mut self.hmds_conn, &mut self.hb);
                    }
                    // The request's held pages go with the withdrawal, and so
                    // does the actions query an adjusted one may have out under
                    // this same number — or the pages are left for the life of
                    // the session and the actions query keeps being served.
                    self.hmds.held.retain(|a| a.req_id != req_id);
                    self.hmds.send_adjustments_cancel(req_id, &mut self.hmds_conn, &mut self.hb);
                }
                ControlCommand::FetchHeadTimestamp { contract, req_id, what_to_show, use_rth, include_expired, .. } => {
                    let ContractRef { con_id, .. } = contract;
                    if self.hmds_conn.is_none() {
                        self.emit_hmds_unavailable(req_id, false);
                    } else {
                        self.hmds.send_head_timestamp_request(req_id, con_id, &what_to_show, use_rth, include_expired, &mut self.hmds_conn, &mut self.hb, &self.shared);
                    }
                }
                ControlCommand::FetchContractDetails { contract, req_id, filters } => {
                    let ContractRef { con_id, symbol, sec_type, exchange, currency, .. } = contract;
                    if con_id > 0 {
                        self.ccp.send_secdef_request(req_id, con_id, &mut self.ccp_conn, &mut self.hb, &self.shared);
                    } else if let Err(reason) = self.ccp.send_secdef_request_by_symbol(req_id, &symbol, &sec_type, &exchange, &currency, &filters, &mut self.ccp_conn, &mut self.hb, &self.shared) {
                        // A lookup this client cannot ask is refused rather
                        // than made: ended like a definition the venue did
                        // not hold, so no caller waits on rows that nothing
                        // will send.
                        self.shared.reference.push_historical_error(
                            req_id, crate::error_codes::Refusal::VALIDATION, reason,
                        );
                        self.shared.reference.push_contract_details_end(req_id);
                    }
                }
                ControlCommand::CancelHeadTimestamp { req_id } => {
                    let parked = self.ccp.withdraw_named(req_id);
                    // As above: the answers already queued go with it.
                    self.shared.reference.purge_head_timestamp_for(req_id);
                    if let Some(pos) = self.hmds.pending_head_ts.iter().position(|(_, rid)| *rid == req_id) {
                        let (query_id, _) = self.hmds.pending_head_ts.remove(pos);
                        self.hmds.send_historical_cancel(&query_id, &mut self.hmds_conn, &mut self.hb);
                    } else if !parked {
                        // A withdrawal that took a parked request acted, and
                        // says nothing beside it.
                        push_hmds_refusal(
                            &self.shared, req_id, crate::error_codes::NO_SUCH_SUBSCRIPTION,
                            format!("no head timestamp is awaited under request {req_id}"),
                            false,
                        );
                    }
                }
                ControlCommand::FetchMatchingSymbols { req_id, pattern } => {
                    self.ccp.send_matching_symbols_request(req_id, &pattern, &mut self.ccp_conn, &mut self.hb, &self.shared);
                }
                ControlCommand::FetchCalendarMetaData { req_id } => {
                    self.secdef.send_calendar_meta_data_request(
                        req_id, &mut self.secdef_conn, &mut self.hb, &self.shared,
                    );
                }
                ControlCommand::FetchCalendarEvents { req_id, query } => {
                    self.secdef.send_calendar_events_request(
                        req_id, &query, &mut self.secdef_conn, &mut self.hb, &self.shared,
                    );
                }
                ControlCommand::CancelCalendar { req_id } => {
                    // As above: the answers already queued go with it. An
                    // answer thrown away is a withdrawal that acted, and one
                    // that acted says nothing beside it: refused on the
                    // pending list alone, a caller withdrawing a request the
                    // venue had just answered was told it was never made.
                    let purged = self.shared.reference.purge_calendar_for(req_id);
                    if !self.secdef.withdraw_calendar_request(req_id) && !purged {
                        // Under the number that says nothing was waiting, not
                        // the data service's own: a withdrawal naming nothing
                        // is not the service reporting a difficulty with a
                        // request it answered.
                        push_hmds_refusal(
                            &self.shared, req_id,
                            crate::error_codes::NO_SUCH_SUBSCRIPTION,
                            format!("no calendar request is waiting under request {req_id}"),
                            false,
                        );
                    }
                }
                ControlCommand::FetchOptionParams { req_id, symbol, fut_fop_exchange, underlying_sec_type, underlying_con_id } => {
                    self.ccp.send_option_params_request(
                        req_id, &symbol, &fut_fop_exchange, &underlying_sec_type, underlying_con_id,
                        &mut self.ccp_conn, &mut self.hb, &self.shared,
                    );
                }
                ControlCommand::FetchCompletedOrders { turn } => {
                    self.ccp.send_completed_orders_request(
                        turn, &mut self.ccp_conn, &mut self.hb, &self.shared,
                    );
                }
                ControlCommand::FetchMktDepthExchanges => {
                    self.ccp.send_mkt_depth_exchanges_request(&mut self.ccp_conn, &mut self.hb, &self.shared);
                }
                ControlCommand::FetchScannerParams => {
                    self.hmds.send_scanner_params_request(&mut self.hmds_conn, &mut self.hb, &self.shared);
                }
                ControlCommand::SubscribeScanner { req_id, instrument, location_code, scan_code, max_items, filters } => {
                    if self.hmds_conn.is_none() {
                        self.emit_hmds_unavailable(req_id, false);
                    } else {
                        self.hmds.send_scanner_subscribe(req_id, &instrument, &location_code, &scan_code, max_items, filters, &mut self.hmds_conn, &mut self.hb, &self.shared);
                    }
                }
                ControlCommand::CancelScanner { req_id } => {
                    if let Some(pos) = self.hmds.pending_scanner.iter().position(|(_, rid)| *rid == req_id) {
                        let (scan_id, _) = self.hmds.pending_scanner.remove(pos);
                        self.hmds.send_scanner_cancel(&scan_id, &mut self.hmds_conn, &mut self.hb);
                    } else {
                        // A caller withdrawing a scan this client is not
                        // running has a record that disagrees with ours, and
                        // it branches on being told so. Said nothing, the
                        // withdrawal is indistinguishable from one that
                        // worked, and the caller goes on believing it holds
                        // nothing while a scan it has forgotten runs on.
                        push_hmds_refusal(
                            &self.shared,
                            req_id,
                            crate::error_codes::NO_SUCH_SCANNER_SUBSCRIPTION,
                            format!("no scan is running under request {req_id}"),
                            false,
                        );
                    }
                    // Rows already found and waiting on their contracts to be
                    // named go with it. They are parked for up to five
                    // seconds, and the parking happens earlier in a lap than
                    // this does — so without this a scan answers after it was
                    // withdrawn, and under a number that may already belong to
                    // the next scan.
                    self.ccp.pending_scanner_enrichment.retain(|pe| pe.api_req_id != req_id);
                    self.shared.reference.purge_scanner_data_for(req_id);
                }
                ControlCommand::FetchHistoricalNews { req_id, con_id, provider_codes, start_time, end_time, max_results } => {
                    if self.hmds_conn.is_none() {
                        self.emit_hmds_unavailable(req_id, false);
                    } else if self.hmds.pending_news.iter().any(|(_, rid)| *rid == req_id) {
                        // As a second bar query under a live number is refused:
                        // two queries under one number answer twice, and a
                        // withdrawal reaches one of them.
                        push_hmds_refusal(
                            &self.shared, req_id, crate::error_codes::DUPLICATE_HISTORICAL_QUERY,
                            format!("request {req_id} is already answering a news query: withdraw it before asking for another under the same number"),
                            false,
                        );
                    } else {
                        self.hmds.send_historical_news_request(req_id, con_id, &provider_codes, &start_time, &end_time, max_results, &self.shared, &mut self.hmds_conn, &mut self.hb);
                    }
                }
                ControlCommand::FetchAdjustments { req_id, con_id, sec_type, exchange, start_date, end_date } => {
                    if self.hmds_conn.is_none() {
                        self.emit_hmds_unavailable(req_id, false);
                    } else {
                        self.hmds.send_adjustments_request(req_id, con_id, &sec_type, &exchange, &start_date, &end_date, &self.shared, &mut self.hmds_conn, &mut self.hb);
                    }
                }
                ControlCommand::FetchNewsArticle { req_id, provider_code, article_id } => {
                    if self.hmds_conn.is_none() {
                        self.emit_hmds_unavailable(req_id, false);
                    } else if self.hmds.pending_articles.iter().any(|(_, rid)| *rid == req_id) {
                        push_hmds_refusal(
                            &self.shared, req_id, crate::error_codes::DUPLICATE_HISTORICAL_QUERY,
                            format!("request {req_id} is already answering an article query: withdraw it before asking for another under the same number"),
                            false,
                        );
                    } else {
                        self.hmds.send_news_article_request(req_id, &provider_code, &article_id, &self.shared, &mut self.hmds_conn, &mut self.hb);
                    }
                }
                ControlCommand::FetchFundamentalData { req_id, con_id, report_type } => {
                    if self.hmds_conn.is_none() {
                        self.emit_hmds_unavailable(req_id, false);
                    } else {
                        self.hmds.send_fundamental_data_request(req_id, con_id, &report_type, &self.shared, &mut self.hmds_conn, &mut self.hb);
                    }
                }
                ControlCommand::CancelFundamentalData { req_id } => {
                    // As above: the answers already queued go with it.
                    self.shared.reference.purge_fundamental_for(req_id);
                    self.hmds.send_fundamental_cancel(req_id, &mut self.hmds_conn, &mut self.hb, &self.shared);
                }
                ControlCommand::CancelHistoricalNews { req_id } => {
                    // As its neighbours: the answers already queued go with it,
                    // or a withdrawn query was answered under its number — or
                    // under whatever request had reused it since.
                    self.shared.reference.purge_historical_news_for(req_id);
                    self.hmds.send_news_cancel(req_id, &mut self.hmds_conn, &mut self.hb, &self.shared);
                }
                ControlCommand::CancelCorporateActions { req_id } => {
                    if !self.hmds.send_adjustments_cancel(req_id, &mut self.hmds_conn, &mut self.hb) {
                        push_hmds_refusal(
                            &self.shared, req_id, crate::error_codes::NO_SUCH_SUBSCRIPTION,
                            format!("no corporate-actions query is waiting under request {req_id}"),
                            false,
                        );
                    }
                }
                ControlCommand::FetchHistogramData { req_id, con_id, sec_type, exchange, use_rth, period } => {
                    if self.hmds_conn.is_none() {
                        self.emit_hmds_unavailable(req_id, false);
                    } else {
                        self.hmds.send_histogram_request(req_id, con_id, &sec_type, &exchange, use_rth, &period, &mut self.hmds_conn, &mut self.hb);
                    }
                }
                ControlCommand::CancelHistogramData { req_id } => {
                    // As above: the answers already queued go with it.
                    self.shared.reference.purge_histogram_for(req_id);
                    if let Some(pos) = self.hmds.pending_histogram.iter().position(|(_, rid)| *rid == req_id) {
                        let (query_id, _) = self.hmds.pending_histogram.remove(pos);
                        self.hmds.send_historical_cancel(&query_id, &mut self.hmds_conn, &mut self.hb);
                    } else {
                        push_hmds_refusal(
                            &self.shared, req_id, crate::error_codes::NO_SUCH_SUBSCRIPTION,
                            format!("no histogram is awaited under request {req_id}"),
                            false,
                        );
                    }
                }
                ControlCommand::FetchHistoricalTicks { contract, req_id, start_date_time, end_date_time, number_of_ticks, what_to_show, use_rth, include_expired, .. } => {
                    let ContractRef { con_id, sec_type, exchange, .. } = contract;
                    // The engine-side reading of the name, for a caller that
                    // reached this loop by the control channel rather than
                    // through a surface. The builder falls back to trades on a
                    // name it does not know — and the file it lives in says
                    // why that is wrong: a misspelled one asks for a different
                    // series than the caller named, and what comes back reads
                    // as what was asked for. The bar batch beside this keeps
                    // the same backstop for the same reason.
                    if let Err(e) = crate::control::historical::tick_data_type(&what_to_show) {
                        log::error!("historical ticks req_id={req_id}: {e}");
                        push_hmds_refusal(
                            &self.shared, req_id, crate::error_codes::Refusal::VALIDATION, e, false,
                        );
                    } else if self.hmds_conn.is_none() {
                        self.emit_hmds_unavailable(req_id, false);
                    } else {
                        self.hmds.send_historical_ticks_request(req_id, con_id, &sec_type, &exchange, &start_date_time, &end_date_time, number_of_ticks, &what_to_show, use_rth, include_expired, &mut self.hmds_conn, &mut self.hb);
                    }
                }
                ControlCommand::SubscribeRealTimeBar { contract, req_id, what_to_show, use_rth, .. } => {
                    let ContractRef { con_id, symbol, sec_type, exchange, .. } = contract;
                    // Ahead of the transport, because a request naming a series
                    // this client cannot express is malformed whether or not
                    // there is a connection to send it on — and the reason it
                    // is refused is the caller's to hear either way. As on the
                    // ticks above: the builder falls back to trades on a name
                    // it does not know, and a caller reaching this loop by the
                    // control channel is refused nowhere else.
                    if let Err(e) =
                        crate::control::historical::BarDataType::from_api_str(&what_to_show)
                    {
                        log::error!("live bars req_id={req_id}: {e}");
                        push_hmds_refusal(
                            &self.shared, req_id, crate::error_codes::Refusal::VALIDATION, e, false,
                        );
                    } else if self.hmds_conn.is_none() {
                        self.emit_hmds_unavailable(req_id, false);
                    } else if self.hmds.rtbar_subs.iter().any(|(_, rid, ..)| *rid == req_id) {
                        // As a second scan, tick stream or bar query under a
                        // live number is refused: two streams under one number
                        // interleave into one queue, and a withdrawal reaches
                        // one of them.
                        push_hmds_refusal(
                            &self.shared, req_id, crate::error_codes::DUPLICATE_TICKER_ID,
                            format!("a live bar stream is already running under request {req_id}: withdraw it before asking for another"),
                            false,
                        );
                    } else {
                        self.hmds.send_realtime_bar_subscribe(req_id, con_id, &symbol, &sec_type, &exchange, &what_to_show, use_rth, &mut self.hmds_conn, &mut self.hb);
                    }
                }
                ControlCommand::CancelRealTimeBar { req_id } => {
                    // The stream half of a request kept up to date belongs to
                    // that request, in the historical id space; a bar
                    // withdrawal under its number names no stream of its own.
                    // Taken anyway, the request's updates stopped in silence.
                    if self.hmds.keep_up_to_date_reqs.contains(&req_id) {
                        push_hmds_refusal(
                            &self.shared, req_id, crate::error_codes::NO_SUCH_SUBSCRIPTION,
                            format!(
                                "no live bar stream of its own is running under request {req_id}: \
                                 the bars under it belong to a historical request kept up to date",
                            ),
                            false,
                        );
                        continue;
                    }
                    let parked = self.ccp.withdraw_named(req_id);
                    // What already arrived and nobody has read goes with it,
                    // or the next request under this number is served this
                    // stream's bars.
                    self.shared.market.purge_real_time_bars(req_id);
                    self.hmds.rtbar_resub.retain(|r| r.req_id != req_id);
                    if let Some(pos) = self.hmds.rtbar_subs.iter().position(|(_, rid, ..)| *rid == req_id) {
                        let (query_id, _, ticker_id, ..) = self.hmds.rtbar_subs.remove(pos);
                        let cancel_id = ticker_id.map(|t| t.to_string()).unwrap_or(query_id);
                        self.hmds.send_historical_cancel(&cancel_id, &mut self.hmds_conn, &mut self.hb);
                    } else if !parked {
                        // A withdrawal that took a parked request acted, and
                        // says nothing beside it.
                        push_hmds_refusal(
                            &self.shared, req_id, crate::error_codes::NO_SUCH_SUBSCRIPTION,
                            format!("no live bar stream is running under request {req_id}"),
                            false,
                        );
                    }
                }
                ControlCommand::FetchHistoricalSchedule { contract, req_id, end_date_time, duration, use_rth, .. } => {
                    let ContractRef { con_id, sec_type, exchange, .. } = contract;
                    if self.hmds_conn.is_none() {
                        self.emit_hmds_unavailable(req_id, false);
                    } else {
                        self.hmds.send_schedule_request(req_id, con_id, &sec_type, &exchange, &end_date_time, &duration, use_rth, &mut self.hmds_conn, &mut self.hb);
                    }
                }
                ControlCommand::SubscribeDepth { contract, req_id, num_rows, is_smart_depth, filters, .. } => {
                    let ContractRef { con_id, exchange, sec_type, .. } = contract;
                    self.farm.send_depth_subscribe(
                        req_id, con_id, &exchange, &filters.primary_exchange, &sec_type,
                        num_rows, is_smart_depth,
                        &mut self.farm_conn,
                        &mut self.hb,
                        &self.shared,
                    );
                }
                ControlCommand::UnsubscribeDepth { req_id } => {
                    self.ccp.withdraw_named(req_id);
                    self.farm.send_depth_unsubscribe(
                        req_id,
                        &mut self.farm_conn,
                        &mut self.hb,
                    );
                    // Purge any already-buffered depth updates so callers never see
                    // stale data
                    self.shared.market.purge_depth_updates(req_id);
                }
                ControlCommand::SubscribePnl { req_id, account } => {
                    self.ccp.send_pnl_subscribe(req_id, &account, &mut self.ccp_conn, &mut self.hb);
                }
                ControlCommand::RefreshAccount { account } => {
                    self.ccp.send_account_refresh(&account, &mut self.ccp_conn, &mut self.hb, &self.shared);
                }
                ControlCommand::AdvisorConfig { req_id, command, partition, fa_data_type, document } => {
                    self.ccp.send_advisor_config(
                        req_id, command, &partition, fa_data_type, document.as_deref(),
                        &mut self.ccp_conn, &mut self.hb, &self.shared,
                    );
                }
                ControlCommand::CancelPnl { req_id } => {
                    // No withdrawal message for this subscription has been
                    // observed on the wire, so none is sent. Updates continue
                    // until the session ends, and a reconnect no longer renews
                    // the subscription. Logged so the caller learns this from
                    // the log rather than from continuing updates.
                    self.ccp.withdraw_pnl_subscription(req_id);
                    log::warn!(
                        "P&L subscription {req_id} was asked to stop; this client sends no \
                         withdrawal for one, so the venue goes on reporting it",
                    );
                }
                ControlCommand::Logout => {
                    // Orders handed over before the goodbye go out before it.
                    // Stopping sends this and then the stop, both read in one
                    // pass, so draining only at the stop put an accepted order
                    // on the wire after the venue had been told the session was
                    // ending.
                    order_builder::drain_and_send_orders(
                        &mut self.ccp_conn, &mut self.context, &self.account_id, &mut self.hb,
                        self.ccp.disconnected, &self.shared,
                        self.ccp.recovery_sweep_at.is_some(), &self.event_tx,
                    );
                    // Tell the venue the session is going rather than leaving it
                    // to notice. This ends the session, so it is not part of
                    // stopping the loop: a caller that stops the engine and keeps
                    // its connections — reusing them for the next piece of work —
                    // must not have the session logged out from under it.
                    self.ccp.send_logout(&mut self.ccp_conn, &mut self.hb);
                }
                ControlCommand::ForceDisconnect => {
                    // What a maintenance window does, on demand. The recovery
                    // that follows is the engine's own: nothing here helps it
                    // along, which is the point.
                    //
                    // Taken away the way the venue takes them, through the
                    // paths every other drop goes through. Only the flags were
                    // set before, so both sockets stayed installed while the
                    // pollers ignored them and the heartbeat passed them over:
                    // held open, never written to, and given up only when a
                    // reconnect replaced them. And none of the bookkeeping a
                    // drop does ran — orders were not marked uncertain, so one
                    // that completed at the venue during the window was absent
                    // from the recovery's own reckoning and stood in the book
                    // as working for the rest of the session; the account went
                    // on answering from a book the download had not restated;
                    // and the lookups waiting on the connection waited out
                    // their deadlines rather than ending with it.
                    log::warn!("both transports taken away on request");
                    self.ccp.handle_disconnect(
                        &mut self.ccp_conn, &mut self.context, &self.shared, &self.event_tx,
                    );
                    self.farm.handle_disconnect(
                        &mut self.farm_conn, &mut self.context, &self.event_tx, &self.shared,
                    );
                }
                ControlCommand::Shutdown => {
                    // Read here, so a worker sees it while the rest of this
                    // arm withdraws what the session holds — a subscription
                    // apiece, and an account that carries many. Raised only
                    // where the loop returns, a reconnect went on climbing
                    // through all of that and could pass the last of its own
                    // checks meanwhile, which is a session opened at the venue
                    // after the caller asked for the one it had to end.
                    self.reconnect_cancel.store(true, Ordering::Relaxed);
                    // Orders handed over before the stop go out before it.
                    // They are drained at the top of a lap and this arm runs
                    // further down one, so an order batched with the stop was
                    // buffered and never reached the venue — while its caller
                    // had been told it was accepted, and given an id for it.
                    // The buffer's own rule is that nothing is dropped from it
                    // because a dropped order is one nobody was told about.
                    order_builder::drain_and_send_orders(
                        &mut self.ccp_conn, &mut self.context, &self.account_id, &mut self.hb,
                        self.ccp.disconnected, &self.shared,
                        self.ccp.recovery_sweep_at.is_some(), &self.event_tx,
                    );
                    // The account subscription this loop opened is closed with
                    // it. Left open, each loop holds one for the life of the
                    // connection, and the venue stops answering new ones.
                    let account = self.account_id.clone();
                    if !account.is_empty() {
                        self.ccp.send_account_unsubscribe(
                            &account, &mut self.ccp_conn, &mut self.hb,
                        );
                    }
                    // Unsubscribe all active market data before stopping
                    let instruments: Vec<InstrumentId> = self.farm.instrument_md_reqs
                        .iter().map(|(id, _)| *id).collect();
                    for instrument in instruments {
                        // The session is closing, so every subscription goes
                        // whatever it was asked for under.
                        self.farm.send_mktdata_unsubscribe(
                            instrument,
                            // The session is closing, so every subscription
                            // goes whatever contract it went out under and
                            // whoever took it.
                            0,
                            0,
                            &[],
                            u64::MAX,
                            false,
                            &mut self.farm_conn,
                            &mut self.hb,
                        );
                    }
                    // Every tick stream withdrawn before stopping, each named
                    // by the request that opened it: a contract can carry
                    // several, and withdrawing by contract leaves the rest.
                    let open: Vec<(i64, InstrumentId)> = self.hmds.tbt_subscriptions
                        .iter().map(|sub| (sub.caller_req_id, sub.instrument)).collect();
                    for (req_id, instrument) in open {
                        self.hmds.send_tbt_unsubscribe(
                            req_id, instrument, &mut self.hmds_conn, &mut self.hb,
                        );
                    }
                    // Every book withdrawn too. A caller may stop the engine
                    // and keep its connections for the next piece of work, and
                    // a book left standing keeps arriving on one — under
                    // server tags the next engine has no record of, and, once
                    // it subscribes and the venue reuses a tag for the same
                    // contract and venue, merged into the new caller's book.
                    // Named the way a caller named them. `depth_subs` holds
                    // the ids this client asked the venue under, which is what
                    // the withdrawal resolves TO — handed those, it looks for
                    // a caller behind a caller and finds none, and no book is
                    // withdrawn at all.
                    let mut books: Vec<u32> =
                        self.farm.depth_fanout_map.iter().map(|(_, user)| *user).collect();
                    books.sort_unstable();
                    books.dedup();
                    for req_id in books {
                        self.farm.send_depth_unsubscribe(
                            req_id, &mut self.farm_conn, &mut self.hb,
                        );
                    }
                    // Unsubscribe all news subscriptions before stopping
                    let news_instruments: Vec<InstrumentId> = self.farm.news_subscriptions
                        .iter().map(|(id, ..)| *id).collect();
                    for instrument in news_instruments {
                        self.farm.send_news_unsubscribe(instrument, &mut self.farm_conn, &mut self.hb);
                    }
                    // And whatever the drain above could not send is said
                    // rather than left in a buffer nothing will read again.
                    order_builder::refuse_what_is_left(
                        &mut self.context, &self.shared, "the engine stopped",
                    );
                    self.running = false;
                    // Records the reason alongside the flag. The flag alone
                    // does not distinguish a venue-initiated drop from a
                    // caller-requested stop.
                    self.shared.reference
                        .set_session_over(retry::DisconnectReason::ByDesign.as_str());
                    self.shared.set_connection_lost();
                    emit(&self.event_tx, Event::Stopped);
                }
            }
        }

        // Whatever arrived behind the stop, said rather than dropped. Nothing
        // sends the buffer again after this point in the lap, so these are
        // refused and not carried.
        if !self.running {
            order_builder::refuse_what_is_left(
                &mut self.context, &self.shared, "the engine stopped",
            );
        }

        // All senders dropped — treat as implicit shutdown.
        if sender_dropped && self.running {
            log::warn!("Control channel disconnected — shutting down hot loop");
            // As above: what was accepted goes out before the loop stops.
            order_builder::drain_and_send_orders(
                &mut self.ccp_conn, &mut self.context, &self.account_id, &mut self.hb,
                self.ccp.disconnected, &self.shared,
                self.ccp.recovery_sweep_at.is_some(), &self.event_tx,
            );
            order_builder::refuse_what_is_left(
                &mut self.context, &self.shared, "the engine stopped",
            );
            self.running = false;
            self.shared.reference
                .set_session_over(retry::DisconnectReason::ByDesign.as_str());
            self.shared.set_connection_lost();
            emit(&self.event_tx, Event::Stopped);
        }
    }

    /// Security type (tag 167) and exchange (tag 207) for a contract the
    /// caller identified by contract id alone.
    ///
    /// The server routes a subscription by these two tags. Absent both, the
    /// encoder defaults to a smart-routed stock, which misroutes a future,
    /// option or currency pair. Falls back to the cached contract definition
    /// where one has been fetched; empty otherwise.
    fn described_as(&self, con_id: i64, sec_type: &str, exchange: &str) -> (String, String) {
        if !sec_type.is_empty() && !exchange.is_empty() {
            return (sec_type.to_string(), exchange.to_string());
        }
        let known = self.shared.reference.get_contract(con_id);
        let fill = |stated: &str, from_venue: Option<&String>| {
            if stated.is_empty() {
                from_venue.cloned().unwrap_or_default()
            } else {
                stated.to_string()
            }
        };
        (
            fill(sec_type, known.as_ref().map(|c| &c.sec_type)),
            fill(exchange, known.as_ref().map(|c| &c.exchange)),
        )
    }

    fn check_heartbeats(&mut self) {
        let now = Instant::now();
        let ts = chrono_free_timestamp();

        // Liveness is measured as elapsed silence, which is only valid across
        // a window in which this process was running.
        //
        // The gap between two checks is read first: one longer than the
        // connection's own cadence indicates a stall here rather than silence
        // at the venue. After a stall the silence baselines are reset and
        // liveness is suspended, because a resumed process first drains a
        // backlog.
        let since_check = now.duration_since(self.hb.last_liveness_check).as_secs();
        self.hb.last_liveness_check = now;
        if since_check > LIVENESS_STALL_SECS {
            log::info!(
                "liveness suspects a long stall — {since_check}s between checks, which is this \
                 process not looking rather than the venue not speaking; \
                 measuring silence from now",
            );
            self.hb.last_ccp_recv = now;
            self.hb.last_farm_recv = now;
            self.hb.last_hmds_recv = now;
            // Applies to the security-definition connection as well.
            self.hb.last_secdef_recv = now;
            self.hb.stalled_at = Some(now);
        }
        // The round that ends the cooldown is still skipped: what a resumed
        // process reads first is a backlog, and judging on it is the mistake
        // the cooldown exists to avoid. Clearing here and skipping once more
        // costs one pass and leaves the next one measuring a settled link.
        let recovering = match self.hb.stalled_at {
            Some(at) if now.duration_since(at).as_secs() < LIVENESS_STALL_COOLDOWN_SECS => true,
            Some(_) => {
                self.hb.stalled_at = None;
                true
            }
            None => false,
        };
        // Nothing is sent while recovering either, heartbeats included. A
        // process that stopped running long enough to be noticed has a
        // connection the venue has very likely closed already, and the reply to
        // that is a reconnect rather than a heartbeat onto a socket that is
        // gone. If the venue closes it meanwhile, the reconnect is what answers.
        if recovering {
            return;
        }

        // --- Auth heartbeat (skip if already disconnected) ---
        if !self.ccp.disconnected
        && let Some(conn) = self.ccp_conn.as_mut() {
            let since_sent = now.duration_since(self.hb.last_ccp_sent).as_secs();
            let since_recv = now.duration_since(self.hb.last_ccp_recv).as_secs();

            if since_sent >= self.hb.ccp_send_every() {
                let _ = conn.send_fix(&[
                    (fix::TAG_MSG_TYPE, fix::MSG_HEARTBEAT),
                    (fix::TAG_SENDING_TIME, &ts),
                ]);
                self.hb.last_ccp_sent = now;
            }

            if since_recv > self.hb.ccp_test_after() {
                if since_recv > self.hb.ccp_dead_after() {
                    log::error!("CCP liveness timeout ({since_recv}s silent) — connection lost");
                    self.ccp.handle_disconnect(
                        &mut self.ccp_conn, &mut self.context, &self.shared, &self.event_tx,
                    );
                } else if self.hb.pending_ccp_test.is_none() {
                    let test_id = self.hb.next_test_id();
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

        // --- Farm heartbeat (skip if already disconnected) ---
        if !self.farm.disconnected
        && let Some(conn) = self.farm_conn.as_mut() {
            let since_sent = now.duration_since(self.hb.last_farm_sent).as_secs();
            let since_recv = now.duration_since(self.hb.last_farm_recv).as_secs();

            // Half of what this farm said it expects, or half of what was
            // proposed where it said nothing. Answering exactly on the
            // deadline leaves no room for one heartbeat to be late, which is
            // the same reason the auth connection sends at half.
            if since_sent >= half_of(conn.heartbeat_secs.unwrap_or(FARM_HEARTBEAT_SECS)) {
                let _ = conn.send_fix(&[
                    (fix::TAG_MSG_TYPE, fix::MSG_HEARTBEAT),
                    (fix::TAG_SENDING_TIME, &ts),
                ]);
                self.hb.last_farm_sent = now;
            }

            let stated = conn.heartbeat_secs;
            if since_recv > HeartbeatState::farm_test_after(stated) {
                if since_recv > HeartbeatState::farm_dead_after(stated) {
                    log::error!("Farm liveness timeout ({since_recv}s silent) — connection lost");
                    self.farm.handle_disconnect(
                        &mut self.farm_conn, &mut self.context, &self.event_tx, &self.shared,
                    );
                } else if self.hb.pending_farm_test.is_none() {
                    let test_id = self.hb.next_test_id();
                    let _ = conn.send_fix(&[
                        (fix::TAG_MSG_TYPE, fix::MSG_TEST_REQUEST),
                        (fix::TAG_SENDING_TIME, &ts),
                        (fix::TAG_TEST_REQ_ID, &test_id),
                    ]);
                    self.hb.pending_farm_test = Some((test_id, now));
                    self.hb.last_farm_sent = now;
                }
            }
        }

        // --- Historical heartbeat (skip if disconnected or no historical activity) ---
        if !self.hmds.disconnected && self.hmds_conn.is_some()
        && let Some(conn) = self.hmds_conn.as_mut() {
            let since_sent = now.duration_since(self.hb.last_hmds_sent).as_secs();
            let since_recv = now.duration_since(self.hb.last_hmds_recv).as_secs();

            // Half of what this farm said it expects, as on the farm channel
            // above. Sending on a fixed thirty is late for a farm that asked
            // for ten, and the venue closes a session it stopped hearing from.
            if since_sent >= half_of(conn.heartbeat_secs.unwrap_or(FARM_HEARTBEAT_SECS)) {
                let _ = conn.send_fix(&[
                    (fix::TAG_MSG_TYPE, fix::MSG_HEARTBEAT),
                    (fix::TAG_SENDING_TIME, &ts),
                ]);
                self.hb.last_hmds_sent = now;
            }

            let stated = conn.heartbeat_secs;
            if since_recv > HeartbeatState::farm_test_after(stated) {
                if since_recv > HeartbeatState::farm_dead_after(stated) {
                    log::error!("HMDS liveness timeout ({since_recv}s silent) — connection lost");
                    self.give_up_historical();
                } else if self.hb.pending_hmds_test.is_none() {
                    let test_id = self.hb.next_test_id();
                    let _ = conn.send_fix(&[
                        (fix::TAG_MSG_TYPE, fix::MSG_TEST_REQUEST),
                        (fix::TAG_SENDING_TIME, &ts),
                        (fix::TAG_TEST_REQ_ID, &test_id),
                    ]);
                    self.hb.pending_hmds_test = Some((test_id, now));
                    self.hb.last_hmds_sent = now;
                }
            }
        }

        // --- Calendar heartbeat ---
        //
        // Both timestamps drive the liveness check. Unwritten, a socket that
        // goes half open here is never probed, never declared dead, and never
        // rebuilt, and the calendar stops answering on a connection that still
        // reads as healthy. Probed on the same terms as the other three.
        if let Some(conn) = self.secdef_conn.as_mut() {
            let since_sent = now.duration_since(self.hb.last_secdef_sent).as_secs();
            let since_recv = now.duration_since(self.hb.last_secdef_recv).as_secs();

            if since_sent >= half_of(conn.heartbeat_secs.unwrap_or(FARM_HEARTBEAT_SECS)) {
                let _ = conn.send_fix(&[
                    (fix::TAG_MSG_TYPE, fix::MSG_HEARTBEAT),
                    (fix::TAG_SENDING_TIME, &ts),
                ]);
                self.hb.last_secdef_sent = now;
            }

            let stated = conn.heartbeat_secs;
            if since_recv > HeartbeatState::farm_test_after(stated) {
                if since_recv > HeartbeatState::farm_dead_after(stated) {
                    log::error!(
                        "Security-definition liveness timeout ({since_recv}s silent) — connection lost",
                    );
                    self.hb.pending_secdef_test = None;
                    self.secdef.give_up_silent(&mut self.secdef_conn, &self.shared, &self.event_tx);
                } else if self.hb.pending_secdef_test.is_none() {
                    let test_id = self.hb.next_test_id();
                    let _ = conn.send_fix(&[
                        (fix::TAG_MSG_TYPE, fix::MSG_TEST_REQUEST),
                        (fix::TAG_SENDING_TIME, &ts),
                        (fix::TAG_TEST_REQ_ID, &test_id),
                    ]);
                    self.hb.pending_secdef_test = Some((test_id, now));
                    self.hb.last_secdef_sent = now;
                }
            }
        }
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

    /// Put back as much of a reconnect's subscription book as the pacing allows.
    fn drive_replay(&mut self) {
        let replay = ReplayPacing {
            burst: self.reconnect_cfg.replay_burst,
            pace: self.reconnect_cfg.replay_pace,
        };
        self.farm.drive_replay(replay, &mut self.farm_conn, &mut self.hb);
    }

    /// Replace the farm connection (after reconnection) and re-subscribe to all
    /// instruments.
    pub fn reconnect_farm(&mut self, conn: Connection) {
        // A feed given up on is given up on by the engine's own recovery. This
        // is a caller handing in a transport instead, and that one is live — so
        // the refusal raised when the recovery stopped is taken back with it,
        // and the recovery is allowed to run again. Left set, every
        // subscription on the feed the caller had just rebuilt was refused for
        // the rest of the session, and nothing would have tried to rebuild it
        // again either.
        self.farm_halted = None;
        self.shared.market.clear_market_data_over();
        let replay = ReplayPacing {
            burst: self.reconnect_cfg.replay_burst,
            pace: self.reconnect_cfg.replay_pace,
        };
        self.farm.reconnect(
            conn,
            &mut self.farm_conn,
            &mut self.context, &mut self.hb,
            replay,
            &self.shared,
        );
    }

    /// Whether the session ended for good while this attempt was still
    /// dialling.
    ///
    /// Transports recover side by side, and one of them can be told something
    /// final — the credentials refused, the account taken over — while another
    /// is midway through a handshake that then succeeds. The halt that answer
    /// records is never lifted, so what the successful one carries belongs to
    /// a session the caller has already been told was over.
    fn the_session_ended_while_it_dialled(&self) -> bool {
        matches!(&self.reconnect_halted, Some(why) if why.is_terminal())
    }

    /// Forget a halt, unless it was one nothing can undo.
    ///
    /// One transport coming back says nothing about the other. A session the
    /// venue took away stays taken away while the market-data side reconnects
    /// happily, and clearing the halt on that success let the auth side be
    /// retried — straight back into taking the account from whoever holds it.
    fn clear_halt_if_it_was_not_settled(&mut self) {
        if matches!(&self.reconnect_halted, Some(reason) if reason.is_terminal()) {
            return;
        }
        self.reconnect_halted = None;
        self.shared.reference.clear_session_over();
        self.shared.reference.clear_trading_over();
    }

    /// Replace the auth connection (after reconnection) and reconcile order state.
    pub fn reconnect_ccp(&mut self, conn: Connection) {
        // Whoever held the account when this reconnect arrived, and the
        // interval the venue stated on it. Both are answers to this
        // connection's logon and belong to this connection: kept from the one
        // that went, a session would be held to an interval nobody agreed and
        // a takeover would be reported once and never again.
        // Set from this connection whether or not it names anybody. A logon
        // that names no other session is the venue saying the account is this
        // client's alone, and keeping the previous answer reports a holder
        // who has since gone — with their address and the time they took it —
        // for as long as the process runs.
        self.shared.reference.set_competing_session(conn.competing.clone());
        // What this connection stated, or what its own logon proposed where it
        // stated nothing — an ack that echoes no interval has accepted the one
        // it was offered. Either way it is this connection's, and leaving the
        // previous one in place holds the session to an interval neither end
        // of it agreed.
        self.hb.set_ccp_interval(conn.heartbeat_secs.unwrap_or(crate::config::CCP_HEARTBEAT));
        // The clock this connection opened on, where it stated one. A
        // reconnect is allowed to carry no stamp, and one that carries none
        // has said nothing about the venue's clock — so the difference already
        // learned stands, which is a far better answer than none, and the two
        // machines have not moved apart in the seconds a reconnect takes.
        if let Some(stamped) = conn.logged_in_at.as_deref() {
            self.shared.market.note_venue_time(stamped);
        }
        // This session's logon is now the newer one. Left at the first, every
        // later reconnect would find its own previous logon listed as a
        // competing session and give the account up to itself.
        //
        // Which is what an unstamped reconnect did. A reconnect is allowed to
        // carry no stamp, and kept only where one arrived, this stayed at the
        // first connect's while the venue recorded the session later. The next
        // reconnect is then told of this client's own logon, not yet reaped,
        // under a time later than the one held here — reads it as another
        // client, gives the account up and reports a competing login, sending
        // whoever owns the account after a competitor that does not exist. The
        // local clock stands in, as it already does on the first connect,
        // where the venue states nothing. What the venue itself said is
        // reported above and stays the venue's alone.
        if let Some(auth) = self.reconnect_auth.as_mut() {
            auth.logged_in_at = conn
                .logged_in_at
                .clone()
                .unwrap_or_else(|| chrono_free_timestamp().to_string());
        }
        // Where this attempt landed. A reconnect can be redirected, and a
        // session that does not remember it dials the door again every time
        // and never learns the one host it knows answers for this account.
        if let Some(landed) = conn.connected_host.clone()
            && let Some(auth) = self.reconnect_auth.as_mut()
            && auth.host != landed
        {
            log::info!("this session is now on {landed}");
            auth.alternate_hosts.retain(|host| *host != landed);
            if !auth.alternate_hosts.contains(&auth.host) {
                auth.alternate_hosts.push(auth.host.clone());
            }
            auth.host = landed;
        }
        self.ccp.reconnect(conn, &mut self.ccp_conn, &mut self.hb, &self.account_id, &self.shared);
    }

    /// Give up any transport that can no longer be written to, or whose
    /// inbound chain has stopped and cannot be restarted.
    fn disconnect_write_dead_transports(&mut self) {
        if !self.ccp.disconnected
            && self.ccp_conn.as_ref().is_some_and(|c| c.write_failed() || c.read_failed())
        {
            log::error!("CCP transport can no longer carry traffic — giving it up");
            self.ccp.handle_disconnect(
                &mut self.ccp_conn, &mut self.context, &self.shared, &self.event_tx,
            );
        }
        if !self.farm.disconnected
            && self.farm_conn.as_ref().is_some_and(|c| c.write_failed() || c.read_failed())
        {
            log::error!("Farm transport can no longer carry traffic — giving it up");
            self.farm.handle_disconnect(
                &mut self.farm_conn, &mut self.context, &self.event_tx, &self.shared,
            );
        }
        if !self.hmds.disconnected
            && self.hmds_conn.as_ref().is_some_and(|c| c.write_failed() || c.read_failed())
        {
            log::error!("HMDS transport can no longer carry traffic — giving it up");
            self.give_up_historical();
        }
        // The calendar's connection, on the same terms as the other three. Its
        // read path gives it up when the socket goes, but a write that fails
        // without a read error after it leaves the connection installed — and
        // the reconnect declines to build another while one is, so the calendar
        // stays on a socket nothing can be sent through.
        if self.secdef_conn.as_ref().is_some_and(|c| c.write_failed() || c.read_failed()) {
            log::error!("Security-definition transport can no longer carry traffic — giving it up");
            self.secdef.give_up(&mut self.secdef_conn, &self.shared, &self.event_tx);
        }
    }

    /// Keep a reconnect worker so the loop can wait for it, and let go of the
    /// ones that have already finished.
    ///
    /// A thread nobody joins holds its stack and its handle until that handle
    /// goes. Every attempt a session ever made was held to the end of the
    /// session, and recovery is unbounded unless the caller bounds it — so a
    /// process reconnecting all day accumulated one per attempt and released
    /// none of them.
    fn own_reconnect_worker(&mut self, trading: bool, worker: Option<std::thread::JoinHandle<()>>) {
        self.reconnect_workers.retain(|(_, w)| !w.is_finished());
        self.reconnect_workers.extend(worker.map(|w| (trading, w)));
    }

    /// Give up recovery for good, and take back whatever is still dialling.
    ///
    /// The halt is the session's rather than one transport's — nothing is
    /// dialled again once it is set — so the attempts other transports have
    /// in flight belong to the recovery that has just ended. Dropping what
    /// one of them returns is too late: by then it has authenticated, and on
    /// this protocol that is a session open at the venue and a second factor
    /// somebody may have approved on their phone.
    ///
    /// Said once where every request can read it, too: a session nothing is
    /// trying to rebuild answers nothing, and a caller that is not told waits
    /// out a timeout per call for an answer that cannot come.
    ///
    /// Once, on the first transport to reach it. What the cancellation above
    /// takes back comes home as a failed attempt of its own, and its poller
    /// reads that as one more thing ending the session — so the notice went
    /// out twice for the one loss. The Rust surface latches its own and shows
    /// one; the Python surface folds only the events one dispatch drains, and
    /// a dispatch between the two announced connectivity lost a second time.
    fn halt_recovery(&mut self, reason: retry::DisconnectReason) {
        self.reconnect_cancel.store(true, Ordering::Relaxed);
        if self.reconnect_halted.is_some() {
            return;
        }
        self.reconnect_halted = Some(reason);
        self.shared.reference.set_session_over(reason.as_str());
        // And the trading connection's own, where that is the connection that
        // is down — whichever transport heard the answer that ended recovery,
        // because nothing dials any of them again once this is set. Said with
        // the notice rather than after it, so a caller answering the notice
        // with an order reads it. Only where that connection is actually down:
        // a quote feed spending the budget while trading carries on happily
        // would otherwise refuse orders that connection would have taken.
        if self.ccp.disconnected {
            self.shared.reference.set_trading_over(reason.as_str());
        }
        self.shared.set_connection_lost();
        emit(&self.event_tx, Event::Disconnected);
        // Said, and recorded as said. The attempt this halt gives up on is
        // left to land on its own thread rather than waited out here, so its
        // poller runs again afterwards — and an answer that is not final
        // reaches the branch that announces a loss once. Without this the one
        // loss was announced twice, which is the thing this whole notice is
        // written to do exactly once.
        self.loss_announced = true;
        // And nothing is left to take an answer from. A worker already dialling
        // still finishes, and its answer would otherwise be installed on the
        // next lap — a session coming back after the caller had been told it
        // would not, with nothing said. The cancellation above ends the worker;
        // this ends what it could still be believed for.
        self.stop_dialling_without_waiting();
    }

    /// Take back whatever recovery is still in flight, without waiting on it.
    ///
    /// For the halts raised from inside the loop. A worker reads the flag
    /// between the phases of a handshake, and one past the last of those
    /// reads is inside a dial, a key exchange or a logon poll, each bounded
    /// in tens of seconds and none of them interruptible. Waited out here,
    /// that is the thread that polls every other transport, answers the
    /// venue's own test requests, sends every heartbeat and drains every
    /// command — so transports with nothing wrong with them are pushed toward
    /// the venue's own silence reset by the wait for one that is already
    /// being given up on. The guard that raises this halt exists because a
    /// caller should not be made to wait out an attempt; waiting out that
    /// attempt to raise it is the same wait under another name.
    ///
    /// What lands after this is not lost. The trading worker and the receiver
    /// it hands its session to are left standing, and `poll_ccp_reconnect`
    /// reads them on a later lap: recovery is halted by then, so the session
    /// is told it is going and dropped — which is what the wait did, a lap
    /// later and off the loop's own thread. Dropping the receiver here
    /// instead would drop that session in silence, and the venue holds one
    /// session per account and has to time out one that just goes.
    ///
    /// The other three return sockets that close when the receiver is gone,
    /// so those are given up here, as they are at the loop's exit.
    fn stop_dialling_without_waiting(&mut self) {
        self.reconnect_cancel.store(true, Ordering::Relaxed);
        self.pending_farm_reconnect = None;
        self.pending_hmds_reconnect = None;
        self.pending_secdef_reconnect = None;
    }

    /// Give up recovery that has outrun the time the caller allowed it, while
    /// the attempt it is spending that time on is still running.
    ///
    /// The schedulers return as soon as they find an attempt in flight, so
    /// the elapsed limit was only ever read between attempts — and a single
    /// attempt outlasts a short one by a wide margin, a handshake that asks
    /// for a second factor most of all, because that one waits on a person to
    /// its own timeout. A caller who bounded recovery to thirty seconds
    /// waited out every second of that instead.
    ///
    /// The attempt limit is not read here. The attempt in flight is one the
    /// caller allowed, and it is left to finish.
    fn abandon_recovery_past_its_deadline(&mut self, now: Instant) {
        if self.reconnect_halted.is_some() {
            return;
        }
        // Each transport spends its own recovery clock: the trading budget for
        // the trading connection, the farm's for the farm. Measured against the
        // trading budget, a farm-only recovery was ended by a deadline the farm
        // had not reached — or, with the trading clock never started, bounded
        // by no deadline at all.
        //
        // And each is read whatever the other is doing. Read as one or the
        // other, a farm attempt running beside a trading one was not measured
        // at all: the trading attempt answered for both, and one waiting on a
        // person waits to its own timeout, so the farm's limit went unread for
        // as long as that lasted and a farm connection landing well past it was
        // installed as though it had arrived in time. The farm is asked first,
        // because the trading connection running out ends the session.
        if self.pending_farm_reconnect.is_some()
            && self.farm_budget.out_of_time(&self.reconnect_cfg, now)
        {
            self.report_recovery_exhausted("farm");
        }
        if self.pending_ccp_reconnect.is_some()
            && self.budget.out_of_time(&self.reconnect_cfg, now)
        {
            self.report_recovery_exhausted("ccp");
        }
    }

    /// Say once that recovery has stopped, so a caller waiting on a connection
    /// that is never coming back is told rather than left waiting.
    fn report_recovery_exhausted(&mut self, which: &str) {
        // The market-data farm running out is the transport ending, never the
        // session. Ended as the session, every reader was refused as not
        // connected and both event loops exited, while the trading connection
        // stayed up and went on taking orders on a session the caller could
        // no longer read.
        //
        // Read once against the trading connection being up, which made the
        // quote feed's spent budget end the session whenever the trading
        // connection happened to be down as well — spending, on the feed's
        // exhaustion, attempts the caller had allowed the trading connection
        // and it had not used. The session's budget is the trading
        // connection's alone: where that one runs out it is reported under
        // `"ccp"`, and that is what ends the session.
        if which == "farm" {
            if self.farm_halted.is_none() {
                log::error!(
                    "market-data farm recovery abandoned after {} attempts — the limits the \
                     caller set are spent; market data is unavailable for the rest of this session",
                    self.farm_budget.attempts(),
                );
            }
            self.farm_halted = Some(retry::DisconnectReason::RecoveryExhausted);
            self.shared.market.set_market_data_over(
                retry::DisconnectReason::RecoveryExhausted.as_str(),
            );
            self.pending_farm_reconnect = None;
            // A feed given up on is no longer one the return waits for, which
            // is what the guard in `announce_reconnected` says. Nothing called
            // it at the moment the waiting ended, though: a trading connection
            // that came back during a joint outage was held back by a farm
            // still dialling, and when that farm ran out there was no second
            // recovery to announce on. The caller read the session as lost for
            // the rest of a healthy one.
            self.announce_reconnected();
            return;
        }
        // Said on the first transport to reach it, and not again: what the
        // halt takes back comes home as a failed attempt of its own. The halt
        // itself is raised either way, because it is what takes that attempt
        // back — the budget that ran out was spent on it as well.
        if self.reconnect_halted.is_none() {
            log::error!(
                "{which} recovery abandoned after {} attempts — the limits the caller set are spent",
                self.budget.attempts(),
            );
        }
        self.halt_recovery(retry::DisconnectReason::RecoveryExhausted);
    }

    /// End the trading connection once the recovery of it has been given up,
    /// and say what is still buffered for it.
    ///
    /// Recovery is the session's rather than one transport's: the answer that
    /// ends it can come back on the quote feed, and nothing dials the trading
    /// connection again afterwards. Told only that the session ended, both
    /// order surfaces went on taking orders — they read this connection's own
    /// state, so that a quote feed the venue will not serve again does not
    /// refuse what a live trading connection would carry — and the engine
    /// buffered each one for a connection that was never coming.
    ///
    /// Read every pass as well as written where the halt is installed, because
    /// the connection can go after it: a session already given up on schedules
    /// nothing for a transport that drops next, and says nothing about it
    /// either.
    ///
    /// Orders accepted while the connection was down were accepted and given
    /// ids, and they are refused here rather than left until the engine stops,
    /// which may be never. An attempt still in flight is no reason to hold
    /// them: what a halt leaves running is dropped where it lands, so nothing
    /// it returns can carry them. A repeat pass finds nothing, because the
    /// refusal drains the buffer.
    fn end_the_trading_connection_once_recovery_is_over(&mut self) {
        let Some(reason) = self.reconnect_halted.filter(|why| why.is_terminal()) else {
            return;
        };
        if !self.ccp.disconnected {
            return;
        }
        // Said here as well as where the halt is installed, because the
        // connection can go after it: a session already given up on schedules
        // nothing for a transport that drops next.
        self.shared.reference.set_trading_over(reason.as_str());
        order_builder::refuse_what_is_left(
            &mut self.context, &self.shared,
            "recovery of the trading connection was given up",
        );
    }

    /// Tell the client a transport it was told about is carrying traffic
    /// again. Silent unless a loss was announced, so the routine drops a
    /// reconnect handles on its own stay invisible.
    fn announce_reconnected(&mut self) {
        if !self.loss_announced { return; }
        // Both transports answer to the one notice, so the one that comes back
        // first must not speak for the other: a network cut takes down both,
        // and announcing on the first recovery told the caller everything was
        // back while half of it still was not.
        // A quote feed given up on for the session is not one to wait for:
        // its loss was said under its own numbers, and waiting on it kept the
        // trading connection's return from ever being announced.
        if self.ccp.disconnected || (self.farm.disconnected && self.farm_halted.is_none()) { return; }
        self.loss_announced = false;
        log::info!("Connection restored — subscriptions re-established");
        self.shared.set_connection_restored();
        emit(&self.event_tx, Event::Reconnected);
    }

    /// Take the caller's recovery settings.
    pub fn set_reconnect_config(&mut self, cfg: crate::reliability::ReconnectConfig) {
        self.reconnect_cfg = cfg;
    }

    /// Set cached auth credentials for farm auto-reconnect.
    pub fn set_reconnect_auth(&mut self, auth: ReconnectAuth) {
        self.reconnect_auth = Some(auth);
    }


    /// Schedule-then-spawn farm reconnects on the jittered backoff ladder. Called every
    /// loop iteration; no-op while connected or an
    /// attempt is in flight.
    fn maybe_spawn_farm_reconnect(&mut self) {
        if !self.farm.disconnected || self.pending_farm_reconnect.is_some() {
            return;
        }
        // A reason the server has already given does not change by being asked
        // again, and a ladder climbed against one is just noise on someone
        // else's server.
        if self.reconnect_halted.is_some() || self.farm_halted.is_some() {
            return;
        }
        if !self.farm_budget.may_retry(&self.reconnect_cfg, Instant::now()) {
            // Only where they ran out. A caller that asked to do the
            // reconnecting itself allowed no attempts and spent none, and its
            // first dropped feed was reported as limits running out — and the
            // feed declared over for the session on it. The loss itself is
            // announced where the connection went, which is where that caller
            // reads it.
            if self.farm_budget.is_spent(&self.reconnect_cfg, Instant::now()) {
                self.report_recovery_exhausted("farm");
            }
            return;
        }
        // Given back before the delay below is read off it, and only by a
        // connection that held for the stable window: see the fields. Applied
        // after the schedule was set, a transport that had been up for hours
        // still waited the rung it reached long ago for its first retry, which
        // is the case this exists for.
        if self.farm_connected_at
            .is_some_and(|up| up.elapsed() >= self.reconnect_cfg.stable_window)
        {
            self.farm_reconnect_attempt = 0;
        }
        self.farm_connected_at = None;
        match self.farm_next_attempt_at {
            None => {
                let delay = reconnect_backoff(self.farm_reconnect_attempt);
                log::info!("Farm reconnect attempt {} scheduled in {:?}",
                    self.farm_reconnect_attempt + 1, delay);
                self.farm_next_attempt_at = Some(Instant::now() + delay);
            }
            Some(due) if Instant::now() >= due => {
                self.farm_next_attempt_at = None;
                self.farm_budget.record_attempt(Instant::now());
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
        if self.reconnect_halted.is_some() {
            return;
        }
        if !self.budget.may_retry(&self.reconnect_cfg, Instant::now()) {
            if self.budget.is_spent(&self.reconnect_cfg, Instant::now()) {
                self.report_recovery_exhausted("ccp");
                return;
            }
            // Nothing ran out: this caller asked to be told and to reconnect
            // itself. Told once, then left alone — the session stands, because
            // it is the caller's to rebuild and nothing here has decided
            // otherwise.
            if !self.loss_announced {
                log::error!(
                    "the trading connection went and this client was asked not to \
                     rebuild it; the caller is told once and nothing is dialled",
                );
                self.loss_announced = true;
                self.shared.set_connection_lost();
                emit(&self.event_tx, Event::Disconnected);
            }
            return;
        }
        // Given back before the delay below is read off it, and only by a
        // connection that held for the stable window: see the fields. Applied
        // after the schedule was set, a transport that had been up for hours
        // still waited the rung it reached long ago for its first retry, which
        // is the case this exists for.
        if self.ccp_connected_at
            .is_some_and(|up| up.elapsed() >= self.reconnect_cfg.stable_window)
        {
            self.ccp_reconnect_attempt = 0;
        }
        self.ccp_connected_at = None;
        match self.ccp_next_attempt_at {
            None => {
                let delay = reconnect_backoff(self.ccp_reconnect_attempt);
                log::info!("CCP reconnect attempt {} scheduled in {:?}",
                    self.ccp_reconnect_attempt + 1, delay);
                self.ccp_next_attempt_at = Some(Instant::now() + delay);
            }
            Some(due) if Instant::now() >= due => {
                self.ccp_next_attempt_at = None;
                self.budget.record_attempt(Instant::now());
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
                log::error!(
                    "Farm is down and cannot be reconnected: no credentials were cached. \
                     The connection has to be rebuilt by the caller.",
                );
                // The same rule the two branches below keep, which this one
                // did not: what cannot be rebuilt is the quote feed, and with
                // the trading connection up that is a transport ending, not
                // the session. Ended as the session it refused every reader as
                // not connected and exited both event loops, while the trading
                // connection stayed up and went on taking orders on a session
                // the caller could no longer read.
                if !self.ccp.disconnected {
                    self.farm_halted = Some(retry::DisconnectReason::AuthorizationFailed);
                    self.shared.market.set_market_data_over(
                        retry::DisconnectReason::AuthorizationFailed.as_str(),
                    );
                    self.announce_reconnected();
                    return;
                }
                self.halt_recovery(retry::DisconnectReason::AuthorizationFailed);
                return;
            }
        };
        self.farm_reconnect_attempt += 1;
        let attempt = self.farm_reconnect_attempt;
        log::info!(
            "Farm auto-reconnect attempt {} starting (host={}, user={})",
            attempt, auth.host, crate::logging::redacted(&auth.username)
        );

        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        let cancel = Arc::clone(&self.reconnect_cancel);
        let worker = std::thread::Builder::new()
            .name(format!("farm-reconnect-{attempt}"))
            .spawn(move || {
                let (farm_host, farm_name) =
                    crate::gateway::reconnect_trading_route(&auth);
                let result = connect_farm(
                    &auth.settings, &farm_host, &farm_name,
                    &auth.username, &auth.password, auth.paper,
                    &auth.server_session_id, &auth.session_key,
                    &auth.hw_info, &auth.encoded, Farm::MarketData, auth.trading_port,
                    Some(&cancel),
                );
                let _ = tx.send(result);
            })
            .ok();
        self.own_reconnect_worker(false, worker);
        self.pending_farm_reconnect = Some(rx);
    }

    /// Whether the quote feed is being rebuilt right now.
    ///
    /// The engine holds no connection while an attempt is out, which is a
    /// different thing from never having had one — and from the outside the
    /// two look alike. A reader that finds no feed and cannot tell them apart
    /// reports the wrong one.
    pub fn rebuilding_the_quote_feed(&self) -> bool {
        self.pending_farm_reconnect.is_some()
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
                // Put nowhere: the session it belongs to ended while it was
                // still dialling, and the halt that ended it is never lifted.
                // Installed, this is a live socket — resubscribed, and
                // announced to the caller as data back — on a session the
                // caller was told was over. Dropped, which closes it: the
                // goodbye this protocol has is the trading connection's, and
                // dropping is what a stop does with a farm that lands after
                // it.
                if self.the_session_ended_while_it_dialled() {
                    log::warn!(
                        "farm reconnect landed after the session ended — dropping it",
                    );
                    self.pending_farm_reconnect = None;
                    return;
                }
                self.reconnect_farm(conn);
                self.farm_connected_at = Some(Instant::now());
                self.clear_halt_if_it_was_not_settled();
                self.farm_budget.record_connected(Instant::now());
                // Said whether or not a loss was ever announced, because the
                // break was said the same way.
                announce_venue_data(
                    &self.shared, &self.event_tx, crate::bridge::VenueDataConnection::MarketData, true,
                );
                self.announce_reconnected();
                self.farm_next_attempt_at = None;
                self.pending_farm_reconnect = None;
            }
            Ok(Err(e)) => {
                let reason = retry::DisconnectReason::from_error(&e);
                log::error!(
                    "Farm auto-reconnect failed (attempt {}): {} — {}",
                    self.farm_reconnect_attempt, e, reason.as_str(),
                );
                if reason.is_terminal() {
                    log::error!(
                        "Farm reconnect stopped: {}. Retrying cannot change this; \
                         the caller has to act.",
                        reason.as_str(),
                    );
                    // The market-data farm alone, with the trading connection
                    // up: the transport the venue will not take back is what
                    // has ended, not the session — the rule the spent-budget
                    // path beside this one already keeps. Ended as the
                    // session, an answer that belongs to the quote feed's own
                    // logon refused every reader as not connected and exited
                    // both event loops, while the trading connection stayed up
                    // and went on taking orders on a session the caller could
                    // no longer read.
                    if !self.ccp.disconnected {
                        self.farm_halted = Some(reason);
                        self.shared.market.set_market_data_over(reason.as_str());
                        self.pending_farm_reconnect = None;
                        // And the return the farm was holding back is said, as
                        // where its budget runs out: this is the same moment,
                        // reached by a different answer.
                        self.announce_reconnected();
                        return;
                    }
                    self.halt_recovery(reason);
                    self.pending_farm_reconnect = None;
                    return;
                }
                self.farm_next_attempt_at = Some(
                    Instant::now()
                        + retry::delay_for(reason, reconnect_backoff(self.farm_reconnect_attempt)),
                );
                self.pending_farm_reconnect = None;
                // The drop already told the caller the market-data feed is
                // broken, under 2103. A retry that also fails is that feed's
                // own connection, not the trading socket 1100 speaks for, so it
                // is not raised as a session loss; retries continue on the
                // backoff ladder.
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => {}
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                log::error!("Farm reconnect thread dropped without result");
                self.pending_farm_reconnect = None;
                // A worker that never answered is a spent attempt like any
                // other: one the machine had no thread to give, or one that
                // stopped before it sent. Left with no schedule the next pass
                // dials at once and every pass after it, and left out of the
                self.farm_next_attempt_at = Some(
                    Instant::now() + reconnect_backoff(self.farm_reconnect_attempt),
                );
                // A worker that never answered is a failed attempt of the
                // market-data feed's own connection, said at the drop under
                // 2103. It is not the trading socket 1100 speaks for, so it is
                // not raised as a session loss; retries continue on the ladder.
            }
        }
    }

    /// Spawn a background thread to reconnect CCP using cached credentials.
    fn spawn_ccp_reconnect(&mut self) {
        if self.pending_ccp_reconnect.is_some() { return; }
        let auth = match self.reconnect_auth.clone() {
            Some(a) if !a.host.is_empty() => a,
            _ => {
                // Nothing to reconnect with, and nothing about waiting changes
                // that. Retrying it quietly every minute leaves a caller on a
                // dead trading connection with no way to learn it is dead —
                // which is the state this whole path exists to avoid.
                log::error!(
                    "CCP is down and cannot be reconnected: no credentials were cached. \
                     The connection has to be rebuilt by the caller.",
                );
                self.halt_recovery(retry::DisconnectReason::AuthorizationFailed);
                return;
            }
        };
        self.ccp_reconnect_attempt += 1;
        let attempt = self.ccp_reconnect_attempt;
        log::info!("CCP auto-reconnect attempt {} starting (host={})", attempt, auth.host);

        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        let cancel = Arc::clone(&self.reconnect_cancel);
        let worker = std::thread::Builder::new()
            .name(format!("ccp-reconnect-{attempt}"))
            .spawn(move || {
                let _ = tx.send(reconnect_ccp(&auth, &cancel));
            })
            .ok();
        self.own_reconnect_worker(true, worker);
        self.pending_ccp_reconnect = Some(rx);
    }

    /// Poll for a completed CCP reconnect. Non-blocking.
    fn poll_ccp_reconnect(&mut self) {
        let rx = match self.pending_ccp_reconnect.as_ref() {
            Some(rx) => rx,
            None => return,
        };
        match rx.try_recv() {
            Ok(Ok(conn)) => {
                log::info!("CCP auto-reconnect succeeded (attempt {})", self.ccp_reconnect_attempt);
                // Put nowhere: the session it belongs to ended while it was
                // still dialling, and the halt that ended it is never lifted.
                // Installed, this is a live socket — resubscribed, and
                // announced to the caller as data back — on a session the
                // caller was told was over. Told the session is going and
                // then dropped, which is what a stop does with an attempt
                // that lands after it.
                if self.the_session_ended_while_it_dialled() {
                    // Told the session is going, rather than dropped in
                    // silence. The venue holds one session per account and has
                    // to time out one that just goes, so the next connection
                    // races a session the venue still believes is live —
                    // which is why a stop says this on the connection the loop
                    // holds, and why one that arrives after the stop is no
                    // different.
                    log::warn!(
                        "trading reconnect landed after the session ended — saying so \
                         before it goes",
                    );
                    let mut landed_too_late = Some(conn);
                    self.ccp.send_logout(&mut landed_too_late, &mut self.hb);
                    self.pending_ccp_reconnect = None;
                    return;
                }
                self.reconnect_ccp(conn);
                self.ccp_connected_at = Some(Instant::now());
                self.clear_halt_if_it_was_not_settled();
                self.budget.record_connected(Instant::now());
                self.announce_reconnected();
                self.ccp_next_attempt_at = None;
                self.pending_ccp_reconnect = None;
            }
            Ok(Err(e)) => {
                let reason = retry::DisconnectReason::from_error(&e);
                log::error!(
                    "CCP auto-reconnect failed (attempt {}): {} — {}",
                    self.ccp_reconnect_attempt, e, reason.as_str(),
                );
                if reason.is_terminal() {
                    log::error!(
                        "CCP reconnect stopped: {}. Retrying cannot change this; \
                         the caller has to act.",
                        reason.as_str(),
                    );
                    self.halt_recovery(reason);
                    self.pending_ccp_reconnect = None;
                    return;
                }
                self.ccp_next_attempt_at = Some(
                    Instant::now()
                        + retry::delay_for(reason, reconnect_backoff(self.ccp_reconnect_attempt)),
                );
                self.pending_ccp_reconnect = None;
                // See the farm path: notify once, keep retrying.
                if self.ccp_reconnect_attempt >= 3 && !self.loss_announced {
                    log::error!("CCP auto-reconnect failed 3 times — notifying (retries continue)");
                    self.loss_announced = true;
                    self.shared.set_connection_lost();
                    emit(&self.event_tx, Event::Disconnected);
                }
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => {}
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                log::error!("CCP reconnect thread dropped without result");
                self.pending_ccp_reconnect = None;
                // A spent attempt like any other, and told like one: see the
                // farm path.
                self.ccp_next_attempt_at = Some(
                    Instant::now() + reconnect_backoff(self.ccp_reconnect_attempt),
                );
                if self.ccp_reconnect_attempt >= 3 && !self.loss_announced {
                    log::error!("CCP auto-reconnect failed 3 times — notifying (retries continue)");
                    self.loss_announced = true;
                    self.shared.set_connection_lost();
                    emit(&self.event_tx, Event::Disconnected);
                }
            }
        }
    }

    /// If HMDS is down and a backoff window has elapsed, spawn the next attempt.
    /// Auto-schedules the first attempt when the engine starts with no HMDS
    /// connection — covers the case where initial soft-token returned
    /// FAILED and the gateway dropped the socket.
    fn maybe_spawn_hmds_reconnect(&mut self) {
        if self.hmds_conn.is_some() { return; }
        // The time the caller allowed this farm, read while the attempt
        // spending it is still running. The check below returns as soon as it
        // finds one in flight, so the budget further down bounded when the
        // next attempt could start and nothing about how long this one may
        // take: a dial against a host that never answers held the deadline
        // open for as long as that dial lasted, and what it finally returned
        // was installed on a farm the session had stopped waiting for. The
        // attempt goes with the recovery — nothing is left to hand its answer
        // to, so a connection that lands after this is closed where it was
        // built. The attempt limit is not read here, for the reason the
        // session's own is not: one the caller allowed is left to finish.
        if self.pending_hmds_reconnect.is_some()
            && self.hmds_halted.is_none()
            && self.hmds_budget.out_of_time(&self.reconnect_cfg, Instant::now())
        {
            log::error!(
                "historical farm recovery abandoned {} attempts in — the time the \
                 caller allowed it is spent; historical data is unavailable for the \
                 rest of this session",
                self.hmds_budget.attempts(),
            );
            self.hmds_halted = Some(retry::DisconnectReason::ByDesign);
            self.pending_hmds_reconnect = None;
        }
        if self.pending_hmds_reconnect.is_some() { return; }
        // A session that has ended for good is not one to keep dialling for.
        // Without this, a connection refused because the account was taken
        // over, or because the credentials were not accepted, was retried
        // every minute or so for the life of the process — full handshake
        // each time, against a session key the venue had already finished
        // with.
        if self.reconnect_halted.is_some() { return; }
        // Nor is this farm's own final answer one to keep asking about.
        if self.hmds_halted.is_some() { return; }
        // And the caller's own limits, once spent, are spent for this too.
        // Not reported here: what this rebuilds is historical data and
        // contract definitions, and a session can trade without either, so
        // losing them is not the end of one.
        // Declined, and nothing else is touched. This transport's own attempt
        // in flight is what the check above already returned on, so the flag
        // reached no attempt but another transport's — and this runs whether
        // or not the venue named a farm to rebuild, so a session that has
        // none took the market-data connection's last permitted attempt away
        // from it, and every later one after the budget was given back.
        if !self.budget.may_retry(&self.reconnect_cfg, Instant::now()) {
            return;
        }
        // And this farm's own share of them. Read but never spent, the
        // caller's limits bounded nothing here: with the rest of the session
        // healthy the session's budget is untouched, so a farm that answers
        // nothing was dialled again every rung for the life of the process.
        // Spent, it is this farm that is finished rather than the session,
        // which is what the halt beside it says.
        if !self.hmds_budget.may_retry(&self.reconnect_cfg, Instant::now()) {
            log::error!(
                "historical farm recovery abandoned after {} attempts — the limits the \
                 caller set are spent; historical data is unavailable for the rest of \
                 this session",
                self.hmds_budget.attempts(),
            );
            self.hmds_halted = Some(retry::DisconnectReason::ByDesign);
            return;
        }
        let auth = match self.reconnect_auth.as_ref() {
            Some(a) if !a.host.is_empty() && !a.hmds_host.is_empty() => a,
            _ => return,
        };
        // Given back before the delay below is read off it, and only by a
        // connection that held for the stable window: see the fields. Applied
        // after the schedule was set, a transport that had been up for hours
        // still waited the rung it reached long ago for its first retry, which
        // is the case this exists for.
        if self.hmds_connected_at
            .is_some_and(|up| up.elapsed() >= self.reconnect_cfg.stable_window)
        {
            self.hmds_reconnect_attempt = 0;
        }
        self.hmds_connected_at = None;
        // Schedule the first attempt if not already scheduled.
        if self.hmds_next_attempt_at.is_none() {
            self.hmds_next_attempt_at = Some(Instant::now() + reconnect_backoff(self.hmds_reconnect_attempt));
            return;
        }
        let due = self.hmds_next_attempt_at.unwrap();
        if Instant::now() < due { return; }
        let auth = auth.clone();
        self.hmds_budget.record_attempt(Instant::now());
        self.hmds_reconnect_attempt += 1;
        let attempt = self.hmds_reconnect_attempt;
        log::info!(
            "HMDS reconnect attempt {} starting (host={}/{})",
            attempt, auth.hmds_host, auth.hmds_farm,
        );
        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        let cancel = Arc::clone(&self.reconnect_cancel);
        let worker = std::thread::Builder::new()
            .name(format!("hmds-reconnect-{attempt}"))
            .spawn(move || {
                let result = connect_farm(
                    &auth.settings, &auth.hmds_host, &auth.hmds_farm,
                    &auth.username, &auth.password, auth.paper,
                    &auth.server_session_id, &auth.session_key,
                    &auth.hw_info, &auth.encoded, Farm::Historical, auth.hmds_port,
                    Some(&cancel),
                );
                let _ = tx.send(result);
            })
            .ok();
        self.own_reconnect_worker(false, worker);
        self.pending_hmds_reconnect = Some(rx);
    }

    /// Rebuild the connection carrying the calendar, on the same terms as the
    /// others: a farm that goes is not a session that ends.
    fn maybe_spawn_secdef_reconnect(&mut self) {
        if self.secdef_conn.is_some() { return; }
        // And the time it was allowed, while it is being spent: see the
        // historical farm.
        if self.pending_secdef_reconnect.is_some()
            && self.secdef_halted.is_none()
            && self.secdef_budget.out_of_time(&self.reconnect_cfg, Instant::now())
        {
            log::error!(
                "calendar farm recovery abandoned {} attempts in — the time the \
                 caller allowed it is spent; the calendar is unavailable for the \
                 rest of this session",
                self.secdef_budget.attempts(),
            );
            self.secdef_halted = Some(retry::DisconnectReason::ByDesign);
            self.pending_secdef_reconnect = None;
        }
        if self.pending_secdef_reconnect.is_some() { return; }
        // A session that has ended for good is not one to keep dialling for.
        // Without this, a connection refused because the account was taken
        // over, or because the credentials were not accepted, was retried
        // every minute or so for the life of the process — full handshake
        // each time, against a session key the venue had already finished
        // with.
        if self.reconnect_halted.is_some() { return; }
        // Nor is this farm's own final answer one to keep asking about.
        if self.secdef_halted.is_some() { return; }
        // And the caller's own limits, once spent, are spent for this too.
        // Not reported here: what this rebuilds is historical data and
        // contract definitions, and a session can trade without either, so
        // losing them is not the end of one.
        // Declined, and nothing else is touched. This transport's own attempt
        // in flight is what the check above already returned on, so the flag
        // reached no attempt but another transport's — and this runs whether
        // or not the venue named a farm to rebuild, so a session that has
        // none took the market-data connection's last permitted attempt away
        // from it, and every later one after the budget was given back.
        if !self.budget.may_retry(&self.reconnect_cfg, Instant::now()) {
            return;
        }
        // And this farm's own share of them: see the historical farm.
        if !self.secdef_budget.may_retry(&self.reconnect_cfg, Instant::now()) {
            log::error!(
                "calendar farm recovery abandoned after {} attempts — the limits the \
                 caller set are spent; the calendar is unavailable for the rest of \
                 this session",
                self.secdef_budget.attempts(),
            );
            self.secdef_halted = Some(retry::DisconnectReason::ByDesign);
            return;
        }
        let auth = match self.reconnect_auth.as_ref() {
            Some(a) if !a.secdef_host.is_empty() && !a.secdef_farm.is_empty() => a,
            // A session the venue named no such farm for has none to rebuild.
            _ => return,
        };
        // Given back before the delay below is read off it, and only by a
        // connection that held for the stable window: see the fields. Applied
        // after the schedule was set, a transport that had been up for hours
        // still waited the rung it reached long ago for its first retry, which
        // is the case this exists for.
        if self.secdef_connected_at
            .is_some_and(|up| up.elapsed() >= self.reconnect_cfg.stable_window)
        {
            self.secdef_reconnect_attempt = 0;
        }
        self.secdef_connected_at = None;
        if self.secdef_next_attempt_at.is_none() {
            self.secdef_next_attempt_at =
                Some(Instant::now() + reconnect_backoff(self.secdef_reconnect_attempt));
            return;
        }
        if Instant::now() < self.secdef_next_attempt_at.unwrap() { return; }
        let auth = auth.clone();
        self.secdef_budget.record_attempt(Instant::now());
        self.secdef_reconnect_attempt += 1;
        let attempt = self.secdef_reconnect_attempt;
        log::info!(
            "Security definition farm reconnect attempt {} starting ({}/{})",
            attempt, auth.secdef_host, auth.secdef_farm,
        );
        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        let cancel = Arc::clone(&self.reconnect_cancel);
        let worker = std::thread::Builder::new()
            .name(format!("secdef-reconnect-{attempt}"))
            .spawn(move || {
                let result = connect_farm(
                    &auth.settings, &auth.secdef_host, &auth.secdef_farm,
                    &auth.username, &auth.password, auth.paper,
                    &auth.server_session_id, &auth.session_key,
                    &auth.hw_info, &auth.encoded, Farm::SecurityDefinition, auth.secdef_port,
                    Some(&cancel),
                );
                let _ = tx.send(result);
            })
            .ok();
        self.own_reconnect_worker(false, worker);
        self.pending_secdef_reconnect = Some(rx);
    }

    /// Take the rebuilt connection, or schedule another attempt.
    fn poll_secdef_reconnect(&mut self) {
        let rx = match self.pending_secdef_reconnect.as_ref() {
            Some(rx) => rx,
            None => return,
        };
        match rx.try_recv() {
            Ok(Ok(conn)) => {
                log::info!(
                    "Security definition farm reconnected (attempt {})",
                    self.secdef_reconnect_attempt,
                );
                // Put nowhere: the session it belongs to ended while it was
                // still dialling, and the halt that ended it is never lifted.
                // Installed, this is a live socket — resubscribed, and
                // announced to the caller as data back — on a session the
                // caller was told was over. Dropped, which closes it: the
                // goodbye this protocol has is the trading connection's, and
                // dropping is what a stop does with a farm that lands after
                // it.
                if self.the_session_ended_while_it_dialled() {
                    log::warn!(
                        "calendar reconnect landed after the session ended — dropping it",
                    );
                    self.pending_secdef_reconnect = None;
                    return;
                }
                self.secdef_conn = Some(conn);
                announce_venue_data(
                    &self.shared, &self.event_tx, crate::bridge::VenueDataConnection::SecurityDefinition, true,
                );
                self.hb.last_secdef_recv = Instant::now();
                self.hb.pending_secdef_test = None;
                self.hb.last_secdef_sent = Instant::now();
                self.secdef_connected_at = Some(Instant::now());
                self.secdef_budget.record_connected(Instant::now());
                self.secdef_next_attempt_at = None;
                self.pending_secdef_reconnect = None;
            }
            Ok(Err(e)) => {
                let reason = retry::DisconnectReason::from_error(&e);
                log::warn!(
                    "Security definition farm reconnect attempt {} failed: {e} — {}",
                    self.secdef_reconnect_attempt, reason.as_str(),
                );
                self.pending_secdef_reconnect = None;
                if reason.is_terminal() {
                    log::error!(
                        "Security definition farm reconnect stopped: {}. Retrying cannot \
                         change this, so the calendar is unavailable for the rest of \
                         this session.",
                        reason.as_str(),
                    );
                    self.secdef_halted = Some(reason);
                    return;
                }
                self.secdef_next_attempt_at = Some(
                    Instant::now() + reconnect_backoff(self.secdef_reconnect_attempt),
                );
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => {}
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                log::error!("secdef reconnect thread dropped without result");
                self.pending_secdef_reconnect = None;
                // Spent like any other attempt: see the HMDS arm.
                self.secdef_next_attempt_at = Some(
                    Instant::now() + reconnect_backoff(self.secdef_reconnect_attempt),
                );
            }
        }
    }

    /// Take back what the optional farms' reconnects returned, each farm's own
    /// clock read first.
    ///
    /// Those clocks live in the schedulers, and a scheduler answers at once
    /// once the connection is there. Taken first, a connection that came up
    /// long after the caller had given up on it was installed anyway and the
    /// deadline it passed was never read — the sweep beside this one answers
    /// for the trading connection and the quote feed alone.
    fn poll_optional_farm_reconnects(&mut self) {
        self.maybe_spawn_hmds_reconnect();
        self.poll_hmds_reconnect();
        self.maybe_spawn_secdef_reconnect();
        self.poll_secdef_reconnect();
    }

    /// Poll for a completed HMDS reconnect. Non-blocking.
    fn poll_hmds_reconnect(&mut self) {
        let rx = match self.pending_hmds_reconnect.as_ref() {
            Some(rx) => rx,
            None => return,
        };
        match rx.try_recv() {
            Ok(Ok(conn)) => {
                log::info!("HMDS reconnect succeeded (attempt {})", self.hmds_reconnect_attempt);
                // Put nowhere: the session it belongs to ended while it was
                // still dialling, and the halt that ended it is never lifted.
                // Installed, this is a live socket — resubscribed, and
                // announced to the caller as data back — on a session the
                // caller was told was over. Dropped, which closes it: the
                // goodbye this protocol has is the trading connection's, and
                // dropping is what a stop does with a farm that lands after
                // it.
                if self.the_session_ended_while_it_dialled() {
                    log::warn!(
                        "historical reconnect landed after the session ended — dropping it",
                    );
                    self.pending_hmds_reconnect = None;
                    return;
                }
                self.hmds.reconnect(conn, &mut self.hmds_conn, &self.context.market, &mut self.hb);
                self.hb.last_hmds_recv = Instant::now();
                self.hb.last_hmds_sent = Instant::now();
                // The probe that went unanswered belonged to the dead session.
                // Leaving it pending suppressed the next one — the liveness
                // check only sends a TestRequest when none is outstanding — so
                // a fresh connection went silent-to-dead without ever being
                // probed. What silence is measured from restarts with it, or
                // the new connection is judged on the old one's.
                self.hb.pending_hmds_test = None;
                self.hmds_connected_at = Some(Instant::now());
                self.hmds_budget.record_connected(Instant::now());
                self.hmds_next_attempt_at = None;
                self.pending_hmds_reconnect = None;
                announce_venue_data(
                    &self.shared, &self.event_tx, crate::bridge::VenueDataConnection::Historical, true,
                );
            }
            Ok(Err(e)) => {
                let reason = retry::DisconnectReason::from_error(&e);
                log::warn!(
                    "HMDS reconnect failed (attempt {}): {} — {}",
                    self.hmds_reconnect_attempt, e, reason.as_str(),
                );
                self.pending_hmds_reconnect = None;
                if reason.is_terminal() {
                    log::error!(
                        "HMDS reconnect stopped: {}. Retrying cannot change this, so \
                         historical data is unavailable for the rest of this session.",
                        reason.as_str(),
                    );
                    self.hmds_halted = Some(reason);
                    return;
                }
                if self.hmds_reconnect_attempt == HMDS_NOTIFY_AFTER_ATTEMPTS {
                    log::error!(
                        "HMDS still down after {HMDS_NOTIFY_AFTER_ATTEMPTS} attempts — historical data is unavailable until it answers; retries continue",
                    );
                }
                self.hmds_next_attempt_at = Some(Instant::now() + reconnect_backoff(self.hmds_reconnect_attempt));
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => {}
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                log::error!("HMDS reconnect thread dropped without result");
                self.pending_hmds_reconnect = None;
                // A thread that never answered is a spent attempt like any
                // other. Leaving the due time on the instant that authorised
                // this one makes the next pass spawn immediately, and the one
                // after that, for as long as the rest of the session is up.
                self.hmds_next_attempt_at = Some(Instant::now() + reconnect_backoff(self.hmds_reconnect_attempt));
            }
        }
    }

    /// Access heartbeat state for testing.
    pub fn heartbeat_state(&self) -> &HeartbeatState {
        &self.hb
    }

    /// Test-only: force farm into disconnected state.
    pub fn force_farm_disconnect(&mut self) {
        self.farm.handle_disconnect_for_test();
    }

    /// Test-only: force the auth transport into disconnected state.
    ///
    /// Sets the flag and nothing else, so recovery is exercised without
    /// invalidating the session server-side — which is what happens if the
    /// connection is taken away by logging in again from elsewhere, and why
    /// that is not a way to test this.
    pub fn force_ccp_disconnect(&mut self) {
        self.ccp.disconnected = true;
    }

    /// Test-only: trigger farm reconnect spawn.
    pub fn spawn_farm_reconnect_for_test(&mut self) {
        self.spawn_farm_reconnect();
    }

    /// Test-only: poll pending farm reconnect.
    pub fn poll_farm_reconnect_for_test(&mut self) {
        self.poll_farm_reconnect();
    }

    /// Test-only: hand the farm reconnect an answer to read on the next poll.
    pub fn answer_farm_reconnect_for_test(&mut self, attempt: u32, answer: io::Result<Connection>) {
        let (tx, rx) = std::sync::mpsc::channel();
        tx.send(answer).unwrap();
        self.farm_reconnect_attempt = attempt;
        self.pending_farm_reconnect = Some(rx);
    }

    /// Mutably access heartbeat state for testing (e.g., setting timestamps).
    pub fn heartbeat_state_mut(&mut self) -> &mut HeartbeatState {
        &mut self.hb
    }

    /// Inject a raw auth message for testing. Processes execution reports, etc.
    pub fn inject_ccp_message(&mut self, msg: &[u8]) {
        self.ccp.process_ccp_message(msg, &mut self.ccp_conn, &mut self.context, &self.shared, &self.event_tx, &mut self.hb, &self.account_id);
    }

    /// Inject a simulated tick for testing.
    pub fn inject_tick(&mut self, instrument: InstrumentId) {
        self.shared.market.push_quote(instrument, self.context.quote(instrument));
        emit(&self.event_tx, Event::Tick(instrument));
    }

    /// Simulate a fill for testing. Updates position and notifies.
    pub fn inject_fill(&mut self, fill: &Fill) {
        let delta = match fill.side {
            crate::types::Side::Buy => fill.qty,
            crate::types::Side::Sell | crate::types::Side::ShortSell => -fill.qty,
        };
        self.context.update_position(fill.instrument, qty_to_f64(delta));
        self.shared.orders.push_fill(*fill);
        self.shared.portfolio.set_position(fill.instrument, self.context.position(fill.instrument));
        emit(&self.event_tx, Event::Fill(*fill));
    }
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
        // SAFETY: only ASCII digits, '.', '-' and ':' are written
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


/// Where an event goes, and where a loss against that channel is counted.
///
/// The two travel together because the only place that knows an event was
/// dropped is the send that dropped it. Counted on the session rather than on
/// the process: two sessions in one program each fall behind on their own, and
/// a count shared between them tells a reader about a channel it does not hold.
#[derive(Clone)]
pub struct EventSink {
    tx: SyncSender<Event>,
    lost: std::sync::Arc<AtomicU64>,
}

impl EventSink {
    /// Where events go and where losses against that channel are counted.
    pub fn new(tx: SyncSender<Event>, lost: std::sync::Arc<AtomicU64>) -> Self {
        Self { tx, lost }
    }
}

/// Emit an event to the channel (if connected). Non-blocking: the engine drops
/// rather than waits, because a session that stalled on a slow reader would
/// stop carrying market data.
///
/// Dropping is right; dropping silently is not, because a program that acted on
/// every fill it saw cannot tell that from every fill there was. A full channel
/// is counted on the session and reported by `events_lost`. A closed one is
/// not: nobody is behind, the reader has gone.
#[inline]
pub(crate) fn emit(event_tx: &Option<EventSink>, event: Event) {
    if let Some(sink) = event_tx
        && let Err(std::sync::mpsc::TrySendError::Full(_)) = sink.tx.try_send(event)
    {
        sink.lost.fetch_add(1, Ordering::Relaxed);
    }
}

/// Say that one of the venue's data connections went away or came back: on
/// the event stream, and in the shared state for a client with none.
pub(crate) fn announce_venue_data(
    shared: &SharedState, event_tx: &Option<EventSink>, which: crate::bridge::VenueDataConnection, up: bool,
) {
    shared.push_venue_data_notice(which, up);
    emit(event_tx, Event::VenueData { which, up });
}

/// Clone a payload for the event channel, but only when one is attached.
///
/// Use this wherever the payload is a deep copy (bar batches, contract
/// definitions): the value goes to `SharedState` by move and the clone is paid
/// for only when someone is listening. With no channel — the default for the
/// Rust client — nothing is copied at all.
///
/// Clone first, push second, emit last, so the event never becomes visible
/// before the same data is readable from `SharedState`.
#[inline]
pub(crate) fn clone_for_event<T: Clone>(event_tx: &Option<EventSink>, value: &T) -> Option<T> {
    event_tx.as_ref().map(|_| value.clone())
}

/// How long to wait before the next reconnect attempt: a two-second floor,
/// the rung this many consecutive failures has reached, and a spread across
/// the window that rung opens, to a ceiling of 82 seconds.
///
/// Every transport climbs this one ladder. The spread is not decoration —
/// without it two transports that fail together retry together for as long as
/// they keep failing, and re-dialling on the beat is what a server rate-limits.
/// How long each consecutive failure waits before the next attempt, in
/// milliseconds, before the spread below is added to it.
/// Whether frames are being kept, read once.
///
/// Every setter is a capture tool stamping it before the engine starts, so it
/// cannot change under a running session. Read per message, it cost a lock and
/// an allocation on the decode path of every transport.
pub(crate) static CAPTURE_WIRE: std::sync::LazyLock<bool> =
    std::sync::LazyLock::new(|| std::env::var("IBX_CAPTURE_WIRE").is_ok());

const LADDER_MS: [u64; 6] = [0, 5_000, 15_000, 30_000, 50_000, 60_000];

/// The width of the window a wait is spread across, which opens by a step per
/// consecutive failure up to its own ceiling.
#[inline]
fn jitter_span(failures: u32) -> u64 {
    (5_000 + 5_000 * failures as u64).min(20_000)
}

pub(crate) fn reconnect_backoff(failures: u32) -> std::time::Duration {
    let ladder = LADDER_MS[(failures as usize).min(LADDER_MS.len() - 1)];
    let jitter = rand::random::<u64>() % jitter_span(failures);
    std::time::Duration::from_millis((2_000 + ladder + jitter).min(82_000))
}

/// What a request made with no historical-data connection is told.
pub(crate) const HMDS_UNAVAILABLE: &str = "Historical data service connection is not available";

/// Surface an "HMDS unavailable" error for `req_id` when the historical-data
/// socket isn't connected. Told under the not-connected code
/// via `push_historical_error` for the consumer's `error()` callback, plus —
/// for historical-bar requests only — a terminal empty-bars response so
/// `historical_data_end` fires. Without this, requests issued while HMDS is
/// down hang silently.
pub(crate) fn push_hmds_unavailable(shared: &SharedState, req_id: u32, from_historical: bool) {
    // Under the number for a request made with no connection to send it on,
    // not the data service's own.
    //
    // Every caller of this asks because there is no socket: the request never
    // left, so the service reported no difficulty with it and never saw it. A
    // caller branching on the service's number retried against a service it
    // had not asked -- which is every request made inside a reconnect window.
    push_hmds_refusal(
        shared, req_id,
        crate::error_codes::Refusal::NOT_CONNECTED,
        HMDS_UNAVAILABLE.to_string(),
        from_historical,
    );
}

/// The id of the contract a request names, where the wire cannot carry it.
///
/// Contracts are numbered in 32 bits and every request that names one by id
/// states it in that width. A wider number narrowed to fit names a different
/// contract — one already listed, on this account's own feed — and a negative
/// one names the highest id there is. Neither is the contract the caller asked
/// about.
fn con_id_beyond_the_wire(cmd: &ControlCommand) -> Option<i64> {
    let con_id = match cmd {
        ControlCommand::Subscribe { contract, .. }
        | ControlCommand::SubscribeTbt { contract, .. }
        | ControlCommand::RegisterInstrument { contract, .. } => contract.con_id,
        ControlCommand::SubscribeNews { con_id, .. } => *con_id,
        // Names its contract itself: it is the lookup the rest wait on.
        ControlCommand::FetchContractDetails { contract, .. } => contract.con_id,
        other => ccp::contract_of(other)?.con_id,
    };
    u32::try_from(con_id).is_err().then_some(con_id)
}

/// Surface an HMDS-side request failure: error 162 plus, for bar requests,
/// the terminal completion sentinel so a blocked wait unblocks.
pub(crate) fn push_hmds_error(shared: &SharedState, req_id: u32, message: String, from_historical: bool) {
    const HMDS_ERROR_CODE: i32 = 162;
    push_hmds_refusal(shared, req_id, HMDS_ERROR_CODE, message, from_historical);
}

/// The same, under a number of its own rather than the service's.
///
/// A request refused before it is sent is not the service reporting a
/// difficulty with one it answered, and a caller branching on the number reads
/// the two apart.
pub(crate) fn push_hmds_refusal(
    shared: &SharedState,
    req_id: u32,
    code: i32,
    message: String,
    from_historical: bool,
) {
    shared.reference.push_historical_error(req_id, code, message);
    if from_historical {
        shared.reference.push_historical_data(
            req_id,
            crate::control::historical::HistoricalResponse {
                query_id: String::new(),
                timezone: String::new(),
                is_complete: true,
                bars: Vec::new(),
            },
        );
    }
}

/// Format a fixed-point Price as a decimal string for FIX tags. Zero alloc.
pub(crate) fn format_price(price: Price) -> StackStr {
    let whole = price / PRICE_SCALE;
    let frac = (price % PRICE_SCALE).unsigned_abs();
    let mut s = StackStr::new();
    // Division truncates toward zero, so a price in (-1, 0) has a whole part
    // of 0 and carries its sign only in the fraction. Writing the integer
    // alone would turn -0.35 into 0.35.
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
        s.buf[frac_start..frac_start + end].copy_from_slice(&digits[..end]);
        s.len = (frac_start + end) as u8;
    }
    s
}

/// Parse a FIX tag value as a Price (fixed-point). Returns 0 if absent,
/// unparseable, or non-finite. Rust's f64 parser accepts "nan"/"inf", but on
/// the wire those are not-available sentinels, not values: the venue's
/// field parser maps nan/unparseable to unset. Without the finite
/// filter, "nan" saturated to 0 and "inf" to i64::MAX.
pub(crate) fn parse_price_tag(val: Option<&String>) -> Price {
    val.and_then(|s| s.parse::<f64>().ok())
        .filter(|f| f.is_finite())
        .map(crate::types::price_from_f64)
        .unwrap_or(0)
}

/// Parse a FIX quantity tag into the `QTY_SCALE` fixed-point form `Qty`
/// holds. Returns `None` where the tag is absent, unparseable, or non-finite,
/// so a caller can tell "the venue said nothing" from "the venue said zero" —
/// `38=0` on a cancel and an absent 38 mean different things.
///
/// Quantities arrive as decimals: a fractional order fills in fractions, and
/// `32=0.5` parsed as an integer is a fill of nothing. Rounded rather than
/// truncated, because a tenth is not exact in binary and truncation takes
/// `0.3` to one hundred-millionth under three tenths.
///
/// A magnitude past what the fixed-point form holds is unreadable too. The
/// cast saturates rather than wrapping, so such a figure booked as the largest
/// quantity there is — and the sums it then entered, against the order's total
/// and the position, have no room left to run.
pub(crate) fn parse_qty_tag(val: Option<&String>) -> Option<Qty> {
    val.and_then(|s| s.trim().parse::<f64>().ok())
        .map(|f| (f * QTY_SCALE as f64).round())
        .filter(|scaled| scaled.is_finite() && scaled.abs() < Qty::MAX as f64)
        .map(|scaled| scaled as Qty)
}

/// Decode a wire TIF byte to the API TIF string. Exact inverse of
/// `api::types::Order::tif_byte`.
///
/// The old inline map decoded '7' (never emitted) as OPG and dropped
/// OPG ('2') and AUC ('8') to "".
pub(crate) fn decode_tif(tif: u8) -> &'static str {
    match tif {
        b'0' => "DAY", b'1' => "GTC", b'2' => "OPG", b'3' => "IOC",
        b'4' => "FOK", b'5' => "GTX", b'6' => "GTD", b'8' => "AUC",
        _ => "",
    }
}

/// Format a fixed-point `Qty` as a decimal string. Zero alloc.
///
/// The number of places comes from `QTY_SCALE` rather than being written out,
/// so a quantity held more finely is written more finely. Fixed at four
/// places while the scale said a hundred-millionth, half a share went out as
/// something else entirely.
pub(crate) fn format_qty(qty: Qty) -> StackStr {
    /// How many decimal places `QTY_SCALE` holds.
    const PLACES: usize = {
        let mut places = 0;
        let mut scale = QTY_SCALE;
        while scale > 1 {
            scale /= 10;
            places += 1;
        }
        places
    };
    const _: () = assert!(
        10i64.pow(PLACES as u32) == QTY_SCALE,
        "the quantity scale is a power of ten, or a fraction of it cannot be written out",
    );

    let whole = qty / QTY_SCALE;
    let frac = (qty % QTY_SCALE).unsigned_abs();
    let mut s = StackStr::new();
    // A magnitude below one unit has a whole part of zero, and zero carries no
    // sign. Without this a negative fraction writes as its own magnitude,
    // naming the opposite side of the same trade.
    if qty < 0 && whole == 0 {
        s.push(b'-');
    }
    s.write_i64(whole);
    if frac != 0 {
        s.push(b'.');
        let frac_start = s.len as usize;
        let mut digits = [b'0'; PLACES];
        let mut rest = frac;
        for slot in digits.iter_mut().rev() {
            *slot = b'0' + (rest % 10) as u8;
            rest /= 10;
        }
        let mut end = PLACES;
        while end > 0 && digits[end - 1] == b'0' { end -= 1; }
        s.buf[frac_start..frac_start + end].copy_from_slice(&digits[..end]);
        s.len = (frac_start + end) as u8;
    }
    s
}

/// Fast extraction of FIX tag 35 (MsgType) value via byte scan.
pub(crate) fn fast_extract_msg_type(msg: &[u8]) -> Option<&[u8]> {
    let limit = msg.len().min(48);
    let mut i = 0;
    while i + 3 < limit {
        if msg[i] == b'3' && msg[i + 1] == b'5' && msg[i + 2] == b'='
            && (i == 0 || msg[i - 1] == 0x01)
        {
            let val_start = i + 3;
            let mut j = val_start;
            while j < msg.len() && msg[j] != 0x01 {
                j += 1;
            }
            if j > val_start {
                return Some(&msg[val_start..j]);
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
    if let Some(len_val) = extract_text_tag(msg, len_tag)
        && let Ok(data_len) = len_val.parse::<usize>() {
            let needle = format!("{tag}=");
            let needle_bytes = needle.as_bytes();
            if let Some(idx) = msg.windows(needle_bytes.len()).position(|w| w == needle_bytes) {
                let val_start = idx + needle_bytes.len();
                // The length is the peer's to state and is read as a usize, so
                // a frame naming one near the top of the range overflows the
                // add: a panic where overflow is checked, and a wrapped end
                // before the start where it is not. Either way the reader
                // thread goes, and with it the session.
                let val_end = val_start.saturating_add(data_len).min(msg.len());
                return Some(msg[val_start..val_end].to_vec());
            }
        }
    let needle = format!("{tag}=");
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
    let needle = format!("{tag}=");
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
mod tests {

    /// The session is held to the interval the venue named, not the one this
    /// client proposed at logon.
    ///
    /// The proposal is 10 seconds and the venue may answer with anything. Sent
    /// on the proposal regardless, a venue asking for 6 was answered every 10
    /// and closed the connection for being slow — which reads as a disconnect
    /// with no cause, minutes into a session that was working.
    #[test]
    fn the_heartbeat_follows_the_interval_the_venue_stated() {
        let mut hb = HeartbeatState::new();
        assert_eq!(hb.ccp_interval_secs, CCP_HEARTBEAT_SECS, "the proposal, until answered");
        assert_eq!(hb.ccp_send_every(), CCP_HEARTBEAT_SECS / 2);

        hb.set_ccp_interval(6);
        assert_eq!(hb.ccp_send_every(), 3, "half of what the venue asked for");

        // Half, because answering on the deadline leaves no room for one
        // heartbeat to be late. Half the interval is sent for the same
        // reason.
        hb.set_ccp_interval(30);
        assert_eq!(hb.ccp_send_every(), 15);

        // A venue naming one second is answered every second, not on every
        // pass of the loop.
        hb.set_ccp_interval(1);
        assert_eq!(hb.ccp_send_every(), 1);

        // Nothing stated leaves the session on what it proposed, rather than
        // sending on every pass forever.
        hb.set_ccp_interval(0);
        assert_eq!(hb.ccp_send_every(), 1, "the last stated value stands");
    }

    /// Half of whatever a connection is held to, and never nothing.
    ///
    /// The farms carry their own interval and may name one this client did not
    /// propose, so the rule cannot live inside the auth side's state.
    #[test]
    fn half_an_interval_is_never_nothing() {
        assert_eq!(super::half_of(30), 15);
        assert_eq!(super::half_of(10), 5);
        assert_eq!(super::half_of(6), 3);
        // A venue naming one second is answered every second rather than on
        // every pass of the loop.
        assert_eq!(super::half_of(1), 1);
        assert_eq!(super::half_of(0), 1);
    }

    /// A reconnect that names nobody says the account is this client's alone.
    ///
    /// Keeping the previous connection's answer reports a holder who has since
    /// gone — their address and the time they took it — for the life of the
    /// process, and a caller reading it gives up an account nobody is holding.
    #[test]
    fn a_reconnect_that_names_nobody_clears_the_one_that_did() {
        let shared = SharedState::new();
        shared.reference.set_competing_session(Some((
            "10.0.0.7".to_string(), "20260815-09:30:00".to_string(), false,
        )));
        assert!(shared.reference.competing_session().is_some());

        // What `reconnect_ccp` does with a connection naming no other session.
        shared.reference.set_competing_session(None);

        assert!(
            shared.reference.competing_session().is_none(),
            "the account is this client's, and stays that way until told otherwise",
        );
    }

    /// A reconnect that carries no stamp leaves the venue's clock where it was.
    ///
    /// The venue states its clock on the logon it answers, and a reconnect is
    /// allowed to carry no stamp. One that carries none has said nothing about
    /// the clock, and the difference already learned is still the best thing
    /// known about it — a reconnect takes seconds, and the two machines have
    /// not drifted apart in them. Dropped instead, a caller who had a good
    /// answer was handed this machine's raw clock, or none at all.
    #[test]
    fn a_reconnect_that_states_no_time_keeps_the_clock_already_learned() {
        let shared = Arc::new(SharedState::new());
        shared.market.note_venue_time("20260815-12:00:00");
        let mut hl = HotLoop::new(shared.clone(), None, None);

        // A reconnect the venue stamped nothing on, which this wire permits.
        let (conn, _peer) = crate::protocol::connection::Connection::for_test();
        assert!(conn.logged_in_at.is_none());
        hl.reconnect_ccp(conn);
        assert!(
            (shared.market.venue_time_millis() - 1_786_795_200_000).abs() < 2_000,
            "the difference the venue stated is still the one in force",
        );

        // One that does carry a stamp states the clock anew, and the session
        // answers from that connection's from then on.
        let (mut conn, _peer_stamped) = crate::protocol::connection::Connection::for_test();
        conn.logged_in_at = Some("20260815-13:30:00".to_string());
        hl.reconnect_ccp(conn);
        assert!(
            (shared.market.venue_time_millis() - 1_786_800_600_000).abs() < 2_000,
            "the new connection's stamp",
        );
    }

    /// A transport that cannot reconnect does not take another's attempt away.
    ///
    /// The historical and calendar farms share the caller's retry budget but
    /// do not spend it. Reading it spent, each raised the flag that abandons
    /// recovery — and its own attempt in flight is what the check above it
    /// already returned on, so the only attempt the flag could reach was
    /// another transport's. A session the venue named no historical farm for
    /// raised it every lap, which took the market-data connection's last
    /// permitted attempt away from it while that attempt was still running.
    #[test]
    fn a_transport_with_no_route_does_not_abandon_anothers_recovery() {
        let mut hl = HotLoop::new(Arc::new(SharedState::new()), None, None);
        hl.set_reconnect_config(crate::reliability::ReconnectConfig {
            max_attempts: Some(1),
            ..Default::default()
        });
        // The market-data connection has spent the one attempt it was allowed,
        // and its worker is still running.
        hl.budget.record_attempt(Instant::now());
        // No historical farm was ever named, and none is being dialled.
        assert!(hl.hmds_conn.is_none() && hl.pending_hmds_reconnect.is_none());

        hl.maybe_spawn_hmds_reconnect();

        assert!(
            !hl.reconnect_cancel.load(Ordering::Relaxed),
            "the attempt still running was taken back by a transport that has no route",
        );
    }

    /// A farm states its own cadence, and it is not the auth connection's.
    ///
    /// The fixed windows are sized against ten seconds. A farm asking for
    /// sixty is declared dead at thirty-five — between two of its own
    /// heartbeats — and a farm asking for ten is answered on a thirty-second
    /// schedule, which is the venue closing a session it stopped hearing from.
    #[test]
    fn a_farm_is_measured_against_the_cadence_it_stated() {
        // Nothing stated: the farm default, and the fixed windows with it.
        assert_eq!(HeartbeatState::farm_test_after(None), LIVENESS_TEST_SECS.max(45));
        assert_eq!(HeartbeatState::farm_dead_after(None), LIVENESS_DEAD_SECS.max(90));

        // A slower farm is not a silent one.
        assert_eq!(HeartbeatState::farm_test_after(Some(60)), 90);
        assert_eq!(HeartbeatState::farm_dead_after(Some(60)), 180);
        assert!(
            HeartbeatState::farm_dead_after(Some(60)) > LIVENESS_DEAD_SECS,
            "a farm on a sixty-second cadence outlives the fixed window",
        );

        // A faster one keeps the fixed windows, which are already generous.
        assert_eq!(HeartbeatState::farm_test_after(Some(10)), LIVENESS_TEST_SECS);
        assert_eq!(HeartbeatState::farm_dead_after(Some(10)), LIVENESS_DEAD_SECS);

        for stated in [None, Some(10), Some(30), Some(60)] {
            assert!(
                HeartbeatState::farm_test_after(stated) < HeartbeatState::farm_dead_after(stated),
                "probe before declaring, at {stated:?}",
            );
        }
    }

    /// An ack that echoes no interval has accepted the one it was offered.
    ///
    /// Keeping the previous connection's holds the new session to a cadence
    /// neither end of it agreed to.
    #[test]
    fn a_reconnect_that_states_no_interval_takes_the_one_it_proposed() {
        let mut hb = HeartbeatState::new();
        hb.set_ccp_interval(30);
        assert_eq!(hb.ccp_send_every(), 15);

        // An ack carrying no tag 108 leaves `reconnect_ccp` with the interval
        // its own logon proposed, which is what it sets.
        hb.set_ccp_interval(crate::config::CCP_HEARTBEAT);
        assert_eq!(
            hb.ccp_send_every(), crate::config::CCP_HEARTBEAT / 2,
            "the new connection's proposal, not the dead one's interval",
        );
    }

    /// A venue that speaks less often is not a venue that has gone away.
    ///
    /// The silence windows are fixed numbers chosen against a ten-second
    /// interval. Left fixed, a venue stating thirty would be probed while it
    /// was still inside its own cadence, and declared dead at thirty-five
    /// having missed one heartbeat.
    #[test]
    fn silence_is_measured_against_the_venues_own_cadence() {
        let mut hb = HeartbeatState::new();
        assert_eq!(hb.ccp_test_after(), LIVENESS_TEST_SECS);
        assert_eq!(hb.ccp_dead_after(), LIVENESS_DEAD_SECS);

        hb.set_ccp_interval(30);
        assert_eq!(hb.ccp_test_after(), 45, "not silent inside its own interval");
        assert_eq!(hb.ccp_dead_after(), 90, "three of its own heartbeats");
        assert!(hb.ccp_send_every() < hb.ccp_test_after(), "send before probing");
        assert!(hb.ccp_test_after() < hb.ccp_dead_after(), "probe before declaring");

        // A shorter interval keeps the fixed windows: they are already more
        // generous than the venue's cadence requires.
        hb.set_ccp_interval(5);
        assert_eq!(hb.ccp_test_after(), LIVENESS_TEST_SECS);
        assert_eq!(hb.ccp_dead_after(), LIVENESS_DEAD_SECS);
    }

    /// A reconnected transport must not inherit the dead session's outstanding
    /// probe. The liveness check only sends a TestRequest when none is pending,
    /// so a stale one suppressed every probe on the new connection and it went
    /// from silent to declared dead without being asked anything. What silence
    /// A caller who names a contract by its id alone states no security type
    /// and no exchange, and the encoder describes every such contract as a
    /// smart-routed stock. That is right for a stock and wrong for a future,
    /// an option or a currency pair, which the venue answers with a partial
    /// acknowledgement.
    #[test]
    fn a_subscription_by_id_alone_is_described_as_the_venue_describes_it() {
        let shared = Arc::new(SharedState::new());
        shared.reference.cache_contract(893091670, crate::types::model::Contract {
            con_id: 893091670,
            symbol: "MES".into(),
            sec_type: "FUT".into(),
            exchange: "CME".into(),
            ..Default::default()
        });
        let hl = HotLoop::new(shared, None, None);

        assert_eq!(
            hl.described_as(893091670, "", ""),
            ("FUT".to_string(), "CME".to_string()),
            "a bare id is described by the definition the venue gave",
        );
        // What the caller states stands.
        assert_eq!(
            hl.described_as(893091670, "STK", "SMART"),
            ("STK".to_string(), "SMART".to_string()),
        );
        // A contract nothing has looked up carries no description; the
        // encoder answers for it.
        assert_eq!(hl.described_as(1, "", ""), (String::new(), String::new()));
    }

    /// And a contract named by its id alone is looked up, not turned down.
    ///
    /// Nothing on such a request says what the contract is, and the encoder
    /// describes every one of them as a smart-routed stock. Refused for that,
    /// an id was the one thing a caller could name a contract by and be told
    /// no for, though naming it by id is exactly what the venue answers a
    /// definition for. Asked, the definition states the type and the
    /// subscription goes out on what the venue said it was.
    #[test]
    fn a_subscription_by_id_alone_asks_the_venue_what_the_contract_is() {
        let shared = Arc::new(SharedState::new());
        let mut hl = HotLoop::new(shared.clone(), None, None);
        let (tx, rx) = std::sync::mpsc::sync_channel(4);
        hl.set_control_rx(rx);

        // An id and nothing beside it: no security type stated here, and no
        // definition cached for the engine to read one off.
        tx.send(ControlCommand::Subscribe {
            filters: Default::default(),
            contract: ContractRef { con_id: 893091670, ..Default::default() },
            mode_9887: 0,
            regulatory_snapshot: false,
            generic_ticks: Vec::new(),
            reply_tx: None,
            issued: 0,
        })
        .expect("the engine holds the other end");
        hl.poll_once();

        assert!(
            shared.market.drain_subscription_failures().is_empty(),
            "a contract the venue will name was turned down for not naming itself",
        );
        assert_eq!(
            hl.ccp.pending_md_subscribe.len(),
            1,
            "and the venue was asked what it is, so the subscription can go out \
             on the answer rather than on a description invented here",
        );
    }

    #[test]
    fn subscriptions_stop_at_the_logon_allowance_with_101() {
        for (allowance, sec_type) in [(100, "STK"), (100, ""), (40, "STK")] {
            let mut hl = HotLoop::new(Arc::new(SharedState::new()), None, None);
            let (mut conn, _peer) = Connection::for_test();
            if allowance != 40 { conn.market_data_allowance = allowance; }
            hl.ccp_conn = Some(conn);
            let (tx, rx) = sync_channel(4);
            hl.set_control_rx(rx);
            // A slot held for something other than quotes spends no line.
            hl.context.market.register(1000);
            let subscribe = |hl: &mut HotLoop, con_id| {
                let (reply_tx, reply_rx) = sync_channel(1);
                tx.send(ControlCommand::Subscribe {
            filters: Default::default(),
                    contract: ContractRef {
                        con_id, sec_type: sec_type.into(), exchange: "SMART".into(),
                        ..Default::default()
                    },
                    mode_9887: 0,
                    regulatory_snapshot: false,
                    generic_ticks: Vec::new(),
                    reply_tx: Some(reply_tx),
                    issued: 0,
                }).unwrap();
                hl.poll_once();
                reply_rx.try_recv().expect("the subscriber is answered")
            };
            let first = subscribe(&mut hl, 1).expect("inside the allowance");
            for con_id in 2..=allowance as i64 {
                subscribe(&mut hl, con_id).expect("inside the allowance");
            }
            let past = allowance as i64 + 1;
            let refused = subscribe(&mut hl, past).expect_err("past the allowance");
            assert_eq!(refused.code, 101);
            assert!(hl.context.market.instrument_by_con_id(past).is_none(), "no slot left behind");
            assert_eq!(subscribe(&mut hl, 1).unwrap(), first, "a shared contract spends no second line");
            assert_eq!(hl.farm.instrument_md_reqs.len(), if sec_type.is_empty() { 0 } else { allowance });
            assert!(hl.ccp.pending_md_subscribe.iter().all(|(_, p, _)| p.con_id != past));
            assert!(hl.is_running(), "existing subscriptions keep running");

            if !sec_type.is_empty() {
                tx.send(ControlCommand::Unsubscribe { instrument: first, con_id: 0, took_it: 0, series: Vec::new(), issued: 0 }).unwrap();
                hl.poll_once();
                subscribe(&mut hl, past).expect("a withdrawn subscription gives its line back");
            }
        }
    }

    /// The calendar's connection is watched like the other three.
    ///
    /// Its send and receive timestamps drive the same liveness check: a socket
    /// that goes half open is probed, declared dead, given up and rebuilt.
    /// Without them a half-open calendar socket stays in service and the
    /// calendar stops answering on a connection that still reads as healthy.
    #[test]
    fn a_calendar_connection_that_stopped_answering_is_given_up() {
        let shared = Arc::new(SharedState::new());
        let mut hl = HotLoop::new(shared, None, None);
        let (conn, _peer) = crate::protocol::connection::Connection::for_test();
        hl.secdef_conn = Some(conn);

        // Silent for longer than anything on this wire waits.
        hl.hb.last_secdef_recv = Instant::now() - Duration::from_secs(600);
        hl.hb.last_secdef_sent = Instant::now();
        hl.check_heartbeats();

        assert!(hl.secdef_conn.is_none(), "the connection that stopped answering was put down");
    }

    /// A calendar connection that has only gone quiet is asked whether it is
    /// still there before it is given up.
    #[test]
    fn a_quiet_calendar_connection_is_asked_before_it_is_given_up() {
        let shared = Arc::new(SharedState::new());
        let mut hl = HotLoop::new(shared, None, None);
        let (conn, _peer) = crate::protocol::connection::Connection::for_test();
        hl.secdef_conn = Some(conn);

        let quiet = HeartbeatState::farm_test_after(None) + 1;
        assert!(quiet < HeartbeatState::farm_dead_after(None), "quiet, not dead");
        hl.hb.last_secdef_recv = Instant::now() - Duration::from_secs(quiet);
        hl.hb.last_secdef_sent = Instant::now();
        hl.check_heartbeats();

        assert!(hl.secdef_conn.is_some(), "quiet is not dead");
        assert!(hl.hb.pending_secdef_test.is_some(), "and it was asked");
    }

    /// is measured from restarts with it, or the new connection is judged on
    /// the old one's.
    #[test]
    fn an_hmds_reconnect_does_not_inherit_the_dead_session_probe() {
        let mut hl = HotLoop::new(Arc::new(SharedState::new()), None, None);
        hl.hb.pending_hmds_test = Some(("t-1".to_string(), Instant::now()));
        hl.hb.last_hmds_recv = Instant::now() - Duration::from_secs(600);
        hl.hmds.disconnected = true;

        // A reconnect that has just succeeded.
        let listener = std::net::TcpListener::bind("127.0.1:0").unwrap();
        let sock = std::net::TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (_peer, _) = listener.accept().unwrap();
        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        tx.send(Ok(crate::protocol::connection::Connection::new_raw(sock).unwrap())).unwrap();
        hl.pending_hmds_reconnect = Some(rx);

        hl.poll_hmds_reconnect();

        assert!(hl.hmds_conn.is_some(), "the connection is installed");
        assert!(!hl.hmds.disconnected, "and the transport is live again");
        assert!(
            hl.hb.pending_hmds_test.is_none(),
            "the dead session's probe is dropped, or it suppresses every probe on the new one",
        );
        assert!(
            hl.hb.last_hmds_recv.elapsed() < Duration::from_secs(2),
            "and silence is measured from the reconnect, not from before it",
        );
    }

    /// The liveness timeout clears the connection as well as setting the
    /// disconnected flag. The reconnect scheduler returns early while a
    /// connection is present, so leaving the dead socket in place makes the
    /// first HMDS liveness timeout permanent for the life of the process, with
    /// every later historical request targeting the abandoned socket.
    #[test]
    fn an_hmds_disconnect_lets_its_reconnect_run() {
        let mut hl = HotLoop::new(Arc::new(SharedState::new()), None, None);
        hl.set_reconnect_auth(crate::gateway::ReconnectAuth {
            account_id: String::new(),
            trading_port: None,
            hmds_port: None,
            secdef_port: None,
            logged_in_at: String::new(),
            alternate_hosts: Vec::new(),
            settings: Default::default(),
            host: "gw.example".into(),
            username: "u".into(),
            password: zeroize::Zeroizing::new(String::new()),
            paper: true,
            code_provider: None,
            ib_key_timeout_secs: crate::auth::session::IB_KEY_DEFAULT_TIMEOUT_SECS,
            ib_key_token_sub_type: crate::auth::session::IB_KEY_DEFAULT_TOKEN_SUB_TYPE.into(),
            session_key: Default::default(),
            session_token: Default::default(),
            server_session_id: String::new(),
            hw_info: String::new(),
            encoded: String::new(),
            hmds_host: "hmds.example".into(),
            hmds_farm: "hfarm".into(),
            trading_host: "trade.example".into(),
            trading_farm: "tfarm".into(),
            secdef_host: String::new(),
            secdef_farm: String::new(),
        });

        // A live transport, so releasing it is observable.
        let listener = std::net::TcpListener::bind("127.0.1:0").unwrap();
        let sock = std::net::TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (_peer, _) = listener.accept().unwrap();
        hl.hmds_conn = Some(crate::protocol::connection::Connection::new_raw(sock).unwrap());
        hl.hmds.disconnected = false;
        assert!(hl.hmds_next_attempt_at.is_none(), "nothing scheduled while it is up");

        // Driven through the receive path rather than by calling the helper:
        // the peer is gone, so `try_recv` errors and the poll gives up on the
        // transport. This is the ordinary way HMDS dies.
        drop(_peer);
        hl.hmds.poll(&mut hl.hmds_conn, &SharedState::new(), &None, &mut hl.hb);

        assert!(hl.hmds.disconnected, "the transport is marked dead");
        assert!(hl.hmds_conn.is_none(), "and the socket is released with it");

        // The scheduler now gets past its early return and arms the first attempt.
        hl.maybe_spawn_hmds_reconnect();
        assert!(
            hl.hmds_next_attempt_at.is_some(),
            "an HMDS reconnect is scheduled rather than skipped forever",
        );
    }

    /// A cancelled keep-up-to-date request leaves nothing behind under its id.
    ///
    /// The stream's routing and its half-built bar are held apart from the
    /// request, so cancelling the request alone leaves them. A later request
    /// numbered the same then folds its first bars into the cancelled one's
    /// partial and reports a bar built from two contracts.
    #[test]
    fn cancelling_a_kept_up_to_date_request_takes_its_stream_with_it() {
        let mut hl = HotLoop::new(Arc::new(SharedState::new()), None, None);
        let req_id = 7u32;

        hl.hmds.keep_up_to_date_reqs.insert(req_id);
        hl.hmds.rtbar_subs.push(("hist_1".to_string(), req_id, Some(99), 0.01, 1.0));
        hl.hmds.forming_bars.push(crate::engine::hot_loop::hmds::FormingBar {
            req_id,
            seconds: 60,
            opened_at: 0,
            bar: Default::default(),
            weighted: 0.0,
        });

        let (tx, rx) = std::sync::mpsc::sync_channel(4);
        hl.set_control_rx(rx);
        tx.send(crate::types::ControlCommand::CancelHistorical { req_id }).unwrap();
        hl.poll_control_commands();


        assert!(!hl.hmds.keep_up_to_date_reqs.contains(&req_id));
        assert!(
            !hl.hmds.rtbar_subs.iter().any(|(_, rid, ..)| *rid == req_id),
            "the stream's routing goes with the request",
        );
        assert!(
            !hl.hmds.forming_bars.iter().any(|f| f.req_id == req_id),
            "and so does the bar it was part way through",
        );
    }

    /// The five-second stream a keep-up-to-date request rides is a query of
    /// its own. Only the batch query was withdrawn, so the venue went on
    /// sending bars this session discarded without a word, and the record the
    /// reconnect rebuilds from still named the cancelled stream.
    #[test]
    fn cancelling_a_kept_up_to_date_request_withdraws_its_stream_from_the_venue() {
        use std::io::Read;
        let mut hl = HotLoop::new(Arc::new(SharedState::new()), None, None);
        let req_id = 7u32;
        let (conn, mut peer) = crate::protocol::connection::Connection::for_test();
        hl.hmds_conn = Some(conn);

        hl.hmds.keep_up_to_date_reqs.insert(req_id);
        hl.hmds.rtbar_subs.push(("rt_4".to_string(), req_id, Some(99), 0.01, 1.0));
        hl.hmds.rtbar_resub.push(crate::engine::hot_loop::hmds::RtBarRequest {
            req_id,
            con_id: 756733,
            sec_type: "STK".into(),
            exchange: "SMART".into(),
            what_to_show: "TRADES".into(),
            use_rth: true,
        });

        let (tx, rx) = std::sync::mpsc::sync_channel(4);
        hl.set_control_rx(rx);
        tx.send(crate::types::ControlCommand::CancelHistorical { req_id }).unwrap();
        hl.poll_control_commands();

        assert!(
            !hl.hmds.rtbar_resub.iter().any(|r| r.req_id == req_id),
            "a reconnect does not ask for a stream the caller cancelled",
        );
        let mut buf = [0u8; 4096];
        let n = peer.read(&mut buf).unwrap();
        let sent = String::from_utf8_lossy(&buf[..n]).to_string();
        // By the venue's number for the stream, not the name this client asked
        // under. Named the second way the withdrawal is accepted and the stream
        // carries on, which is what a live session showed it doing.
        assert!(sent.contains("ticker:99"), "the venue was told to stop: {sent}");
        assert!(!sent.contains("rt_4"), "and not by a name it never issued: {sent}");
    }

    /// An order that reached the engine behind the stop is refused, not
    /// dropped.
    ///
    /// A caller on one thread can have its order in the same batch as another
    /// thread's disconnect. The stop refuses everything the buffer holds and
    /// ends the loop, and an order behind it in that batch was carried out
    /// nowhere and reported nowhere — while its caller had been told it was
    /// accepted and given an id for it. A request that waits on an answer is
    /// answered by the drop of its reply channel; an order has nothing
    /// waiting, so it is said here.
    #[test]
    fn an_order_behind_the_stop_is_refused_rather_than_dropped() {
        let shared = Arc::new(SharedState::new());
        let mut hl = HotLoop::new(shared.clone(), None, None);
        hl.running = true;

        let (tx, rx) = std::sync::mpsc::sync_channel(4);
        hl.set_control_rx(rx);
        tx.send(ControlCommand::Shutdown).unwrap();
        tx.send(ControlCommand::Order(OrderRequest::SubmitEx {
            order_id: 41,
            instrument: 0, con_id: 0,
            side: Side::Buy,
            qty: QTY_SCALE,
            kind: OrderKind::Limit { price: 5 * PRICE_SCALE },
            tif: b'0',
            attrs: Default::default(),
        })).unwrap();
        hl.poll_control_commands();

        assert!(!hl.running, "the stop was processed");
        // And read by whatever is still dialling, while the rest of this pass
        // withdraws what the session holds — a subscription apiece, and an
        // account that carries many. Raised only where the loop returns, a
        // reconnect climbed through all of that and could pass the last of its
        // own checks meanwhile, opening a session at the venue after the caller
        // had asked for the one it had to end.
        assert!(
            hl.reconnect_cancel.load(Ordering::Relaxed),
            "a reconnect went on dialling through the whole of the stop",
        );
        let told = shared.orders.drain_order_inactive();
        assert!(
            told.iter().any(|(id, code, said)| *id == 41
                && *code == crate::error_codes::Refusal::NOT_CONNECTED
                && said.contains("the engine stopped")),
            "the caller is told the order it holds an id for never went: {told:?}",
        );
    }

    /// A head-timestamp or histogram query still waiting on its answer is
    /// withdrawn from the venue, not just dropped from this client's list.
    ///
    /// Dropped without a withdrawal, the venue keeps working the query and
    /// its eventual answer names a request nobody holds any more.
    #[test]
    fn cancelling_a_head_timestamp_or_histogram_withdraws_the_query_from_the_venue() {
        use std::io::Read;
        let (tx, rx) = std::sync::mpsc::sync_channel(4);
        let (tx2, rx2) = std::sync::mpsc::sync_channel(4);
        let req_id = 11u32;

        let (conn, mut peer) = crate::protocol::connection::Connection::for_test();
        let mut hl = HotLoop::new(Arc::new(SharedState::new()), None, None);
        hl.hmds_conn = Some(conn);
        hl.hmds.pending_head_ts.push(("TickHeadClient1;;265598@BEST TRADES;;0;;true;;0;;U".to_string(), req_id));
        hl.set_control_rx(rx);
        tx.send(crate::types::ControlCommand::CancelHeadTimestamp { req_id }).unwrap();
        hl.poll_control_commands();
        assert!(hl.hmds.pending_head_ts.is_empty(), "the local record goes with it");
        let mut buf = [0u8; 4096];
        let n = peer.read(&mut buf).unwrap();
        let sent = String::from_utf8_lossy(&buf[..n]).to_string();
        assert!(
            sent.contains("CancelQuery") && sent.contains("ticker:TickHeadClient1;;265598@BEST TRADES;;0;;true;;0;;U"),
            "the venue was told to stop the query by the id it went out under: {sent}",
        );

        let (conn, mut peer) = crate::protocol::connection::Connection::for_test();
        let mut hl = HotLoop::new(Arc::new(SharedState::new()), None, None);
        hl.hmds_conn = Some(conn);
        hl.hmds.pending_histogram.push(("hg_1000;;265598@BEST Histogram;;0;;true;;0;;U".to_string(), req_id));
        hl.set_control_rx(rx2);
        tx2.send(crate::types::ControlCommand::CancelHistogramData { req_id }).unwrap();
        hl.poll_control_commands();
        assert!(hl.hmds.pending_histogram.is_empty(), "the local record goes with it");
        let mut buf = [0u8; 4096];
        let n = peer.read(&mut buf).unwrap();
        let sent = String::from_utf8_lossy(&buf[..n]).to_string();
        assert!(
            sent.contains("CancelQuery") && sent.contains("ticker:hg_1000;;265598@BEST Histogram;;0;;true;;0;;U"),
            "the venue was told to stop the query by the id it went out under: {sent}",
        );
    }

    /// A subscription still waiting to be told which contract it is for.
    ///
    /// The pending record carries the slot, not the contract — the contract is
    /// what the lookup is for. Reclaim it and the slot goes to the next
    /// registration; the lookup then comes back, adopts the first contract's
    /// id onto the second contract's slot, and subscribes it there. The quotes
    /// arrive under the wrong contract, priced on its tick increment.
    #[test]
    fn a_slot_waiting_on_its_lookup_is_not_reclaimed() {
        for stage in ["asked", "answered"] {
            let mut hl = HotLoop::new(Arc::new(SharedState::new()), None, None);
            let instrument = hl.context.market.register(0);
            let pending = crate::engine::hot_loop::ccp::PendingSubscribe {
                issued: 0,
                filters: Default::default(),
                con_id: 0,
                instrument,
                symbol: "SPY".into(),
                exchange: "SMART".into(),
                sec_type: "STK".into(),
                currency: "USD".into(),
                mode_9887: 0, regulatory_snapshot: false,
            };
            match stage {
                // Out on the wire, no answer yet.
                "asked" => hl.ccp.pending_md_subscribe.push((1, pending, Instant::now())),
                // Answered, not yet sent.
                _ => hl.ccp.resolved_md_subscribe.push((756733, pending)),
            }

            hl.try_reclaim_instrument(instrument);

            // The slot is reclaimed by being handed to the next contract that
            // asks, which is exactly what must not happen here.
            let next = hl.context.market.register(265598);
            assert_ne!(
                next, instrument,
                "the slot was handed on while its lookup was {stage}",
            );
        }
    }

    /// A subscription the venue names after the feed is given up on is
    /// refused, not sent to a socket that is not there.
    ///
    /// This is the one path the refusal on the caller's side cannot see. The
    /// request was accepted while the feed was alive — it had to be, the
    /// contract was stated by description and the venue had to name it — and it
    /// arrives back here once the naming lands, which can be after the feed has
    /// been given up on. Sent anyway it went to nothing and was recorded for a
    /// replay that is not coming, and the caller, told it had a subscription
    /// when it asked, heard nothing for the rest of the session.
    #[test]
    fn a_subscription_named_after_the_feed_was_given_up_is_refused() {
        let shared = Arc::new(SharedState::new());
        let mut hl = HotLoop::new(shared.clone(), None, None);
        let instrument = hl.context.market.register(0);
        hl.ccp.resolved_md_subscribe.push((756733, crate::engine::hot_loop::ccp::PendingSubscribe {
            issued: 0,
            filters: Default::default(),
            con_id: 0,
            instrument,
            symbol: "SPY".into(),
            exchange: "SMART".into(),
            sec_type: "STK".into(),
            currency: "USD".into(),
            mode_9887: 0, regulatory_snapshot: false,
        }));
        hl.farm_halted = Some(retry::DisconnectReason::RecoveryExhausted);

        hl.send_resolved_subscriptions();

        assert!(
            !hl.farm.holds_market_data(instrument),
            "nothing was recorded for a replay that is not coming",
        );
        let told = shared.market.drain_subscription_failures();
        assert!(
            told.iter().any(|(at, _)| *at == instrument),
            "and the caller is told the feed is done: {told:?}",
        );
    }

    /// A slot whose watchers followed the contract elsewhere is given back.
    ///
    /// Two callers name one contract, one of them by a description the venue
    /// has to resolve. The second finds the contract already living in another
    /// slot, and its own watchers are sent there — so nothing will ever
    /// withdraw the slot it took, and withdrawing is the only thing that gives
    /// one back. One went out of the table on every such collision, until the
    /// table was full and the next subscription was refused for want of a slot.
    ///
    /// It is given back once the move has been read, and not before: giving a
    /// slot back purges the moves that name it, and that move is the only thing
    /// telling this slot's watchers where their contract went.
    #[test]
    fn a_slot_whose_watchers_followed_the_contract_is_given_back_after_the_move() {
        let shared = Arc::new(SharedState::new());
        let mut hl = HotLoop::new(shared.clone(), None, None);

        // The contract already lives in a slot of its own.
        let holds_it = hl.context.market.register(756733);
        // And a second request, taken before the venue named its contract.
        let followed = hl.context.market.register(0);
        assert_ne!(holds_it, followed, "two slots to begin with");

        hl.ccp.resolved_md_subscribe.push((756733, crate::engine::hot_loop::ccp::PendingSubscribe {
            issued: 0,
            filters: Default::default(),
            con_id: 0,
            instrument: followed,
            symbol: "SPY".into(),
            exchange: "SMART".into(),
            sec_type: "STK".into(),
            currency: "USD".into(),
            mode_9887: 0, regulatory_snapshot: false,
        }));
        hl.send_resolved_subscriptions();

        let moves = shared.market.drain_subscription_moves();
        assert!(
            moves.iter().any(|(from, to, _)| *from == followed && *to == holds_it),
            "the watchers are sent to the slot the contract lives in: {moves:?}",
        );
        assert!(
            !hl.slots_awaiting_their_word.is_empty(),
            "and the slot they left is owed back rather than dropped",
        );

        // Installed, so the watchers know where to follow. Reading the move off
        // the queue is not enough: the surface says when it has been installed,
        // and only then is the slot they left safe to give back.
        shared.market.note_a_move_is_read(followed, holds_it);
        hl.send_resolved_subscriptions();
        assert!(
            hl.slots_awaiting_their_word.is_empty(),
            "the slot was owed back and never given",
        );
    }

    /// Every reclamation path leaves an unread move intact. Once the caller has
    /// installed it, the deferred sweep gives the otherwise unused slot back.
    #[test]
    fn a_slot_with_an_unread_move_survives_reclamation() {
        let shared = Arc::new(SharedState::new());
        let mut hl = HotLoop::new(shared.clone(), None, None);
        let destination = hl.context.register_instrument(756733);
        let source = hl.context.register_instrument(0);
        shared.market.push_subscription_move(source, destination, 0);
        hl.context.insert_order(crate::types::Order::new(
            7, source, crate::types::Side::Buy, crate::types::QTY_SCALE,
            PRICE_SCALE, b'2', b'0', 0,
        ));
        hl.context.remove_order(7);
        hl.reclaim_slots_no_order_holds();
        hl.try_reclaim_instrument(source);

        assert_eq!(hl.context.market.con_id(source), Some(0), "the source still holds its slot");
        assert_eq!(hl.slots_awaiting_their_word, vec![source], "queued once for release after the move");
        assert_eq!(shared.market.drain_subscription_moves(), vec![(source, destination, 0)]);
        shared.market.note_a_move_is_read(source, destination);
        hl.give_back_slots_their_word_has_left();
        assert!(hl.slots_awaiting_their_word.is_empty());
        assert!(hl.context.market.con_id(source).is_none());
        assert_eq!(hl.context.register_instrument(265598), source, "the slot is reusable after the move");
    }

    /// A slot whose subscription the venue refused keeps it until the reason
    /// has been read.
    ///
    /// Nothing will ever withdraw a subscription that never opened, so the
    /// slot is asked for back in the same breath as the reason is stated — and
    /// giving a slot back drops what is queued under it, so that the next
    /// contract to take the slot is not handed the last one's reasons. The two
    /// together left every caller who named a contract the venue does not know
    /// waiting on a stream that could not arrive, told nothing, for ever.
    #[test]
    fn a_slot_with_an_unread_refusal_survives_reclamation() {
        let shared = Arc::new(SharedState::new());
        let mut hl = HotLoop::new(shared.clone(), None, None);
        let refused = hl.context.register_instrument(0);
        shared.market.push_subscription_failure(
            refused, "no security definition has been found".to_string(),
        );
        hl.try_reclaim_instrument(refused);

        assert_eq!(
            hl.context.market.con_id(refused), Some(0),
            "the slot is still held, so the reason under it still stands",
        );
        assert_eq!(
            shared.market.drain_subscription_failures().len(), 1,
            "and the caller is told why",
        );

        // Read, so the slot is owed back and nothing is lost by giving it.
        hl.give_back_slots_their_word_has_left();
        assert!(hl.slots_awaiting_their_word.is_empty());
        assert!(hl.context.market.con_id(refused).is_none());
    }

    /// A request answered by what is already up, and only where it can be.
    ///
    /// The chargeable snapshot is not a subscription anybody shares. It is a
    /// request of its own, asked for under an action of its own, and it is
    /// withdrawn as soon as it completes. Read as one, both directions were dropped: a stream skipped
    /// because a snapshot held the slot went out never, and the snapshot's own
    /// withdrawal then took the record it had been pointed at; a snapshot
    /// skipped because a stream held the slot was never sent and never refused
    /// for want of the entitlement, and the caller heard the end of it off
    /// ticks it did not ask for.
    #[test]
    fn a_snapshot_and_a_stream_are_each_sent_over_the_other() {
        fn a_pending(
            instrument: InstrumentId, regulatory_snapshot: bool,
        ) -> crate::engine::hot_loop::ccp::PendingSubscribe {
            crate::engine::hot_loop::ccp::PendingSubscribe {
                issued: 0,
                filters: Default::default(),
                con_id: 756733,
                instrument,
                symbol: "SPY".into(),
                exchange: "SMART".into(),
                sec_type: "STK".into(),
                currency: "USD".into(),
                mode_9887: 0,
                regulatory_snapshot,
            }
        }

        // The stream, where a snapshot already holds the slot.
        let shared = Arc::new(SharedState::new());
        let mut hl = HotLoop::new(shared.clone(), None, None);
        let instrument = hl.context.market.register(756733);
        hl.farm.instrument_md_reqs.push((instrument, crate::engine::hot_loop::farm::MdReqRecord {
            con_id: 756733,
            sec_type: "CS".into(),
            mode_9887: 0,
            entries: vec![crate::engine::hot_loop::farm::MdReqEntry {
                req_id: 7, request_type: 624, venue: "BEST".into(),
            }],
        }));
        hl.ccp.resolved_md_subscribe.push((756733, a_pending(instrument, false)));

        hl.send_resolved_subscriptions();

        assert!(
            hl.farm.holds_a_stream(instrument),
            "the subscribe was skipped for a snapshot that is about to withdraw",
        );

        // And the snapshot, where a stream already holds it.
        let shared = Arc::new(SharedState::new());
        let mut hl = HotLoop::new(shared.clone(), None, None);
        let instrument = hl.context.market.register(756733);
        hl.farm.instrument_md_reqs.push((instrument, crate::engine::hot_loop::farm::MdReqRecord {
            con_id: 756733,
            sec_type: "CS".into(),
            mode_9887: 0,
            entries: vec![crate::engine::hot_loop::farm::MdReqEntry {
                req_id: 7, request_type: 442, venue: "BEST".into(),
            }],
        }));
        hl.ccp.resolved_md_subscribe.push((756733, a_pending(instrument, true)));

        hl.send_resolved_subscriptions();

        let entries = hl.farm.instrument_md_reqs.iter()
            .find(|(id, _)| *id == instrument)
            .map(|(_, r)| r.entries.len())
            .unwrap_or(0);
        assert!(
            entries > 1,
            "the snapshot was answered with somebody else's stream, so nothing \
             went to the venue at all",
        );
    }

    /// A snapshot the feed cannot carry is refused, not dropped.
    ///
    /// Every other request here survives an outage in a record of its own —
    /// a stream is replayed, the headlines and the book are re-sent — and the
    /// snapshot is deliberately not, because a reconnect that re-sent one
    /// would deliver, and bill for, a second burst nobody asked for. That is
    /// right for one that went out. With no transport to write it to it never
    /// went out, and there is no later moment when it does: it was accepted,
    /// dropped, and never answered.
    #[test]
    fn a_snapshot_the_feed_cannot_carry_is_refused_rather_than_lost() {
        let shared = Arc::new(SharedState::new());
        let mut hl = HotLoop::new(shared.clone(), None, None);
        let (tx, rx) = std::sync::mpsc::sync_channel(4);
        hl.set_control_rx(rx);
        // The feed has gone away, which is the outage. Not merely "no
        // connection yet": the bookkeeping a subscribe does is meant to happen
        // whether or not a transport is in hand, and only a feed that went is
        // one nothing will carry this before it is forgotten.
        hl.farm.handle_disconnect_for_test();

        tx.send(ControlCommand::Subscribe {
            filters: Default::default(),
            contract: ContractRef {
                con_id: 756733,
                sec_type: "STK".into(),
                exchange: "SMART".into(),
                ..Default::default()
            },
            mode_9887: 0,
            regulatory_snapshot: true,
            generic_ticks: Vec::new(),
            reply_tx: None,
            issued: 0,
        })
        .expect("the engine holds the other end");
        hl.poll_once();

        let told = shared.market.drain_subscription_failures();
        assert!(
            !told.is_empty(),
            "the snapshot was accepted, never sent, and never recorded for \
             replay — so nothing was ever going to answer it",
        );
    }

    /// A snapshot lost to an outage is not reported as the contract's refusal.
    ///
    /// What is recorded here is about the contract and reaches everyone
    /// watching it. A stream held on the same contract was neither refused nor
    /// given up — it is kept precisely so the reconnect brings it back — and
    /// its watchers, told their quote had been refused, withdraw it.
    #[test]
    fn a_snapshot_lost_to_an_outage_does_not_refuse_the_stream_beside_it() {
        let shared = Arc::new(SharedState::new());
        let mut hl = HotLoop::new(shared.clone(), None, None);
        let (tx, rx) = std::sync::mpsc::sync_channel(4);
        hl.set_control_rx(rx);
        let instrument = hl.context.market.register(756733);
        // A stream on the contract, sent the way the engine sends one, and
        // kept across the outage for the replay.
        hl.farm.send_mktdata_subscribe(
            756733, "SPY", "SMART", "STK", "", 0.0, "", "",
            instrument, 0, false, &mut hl.farm_conn, &mut hl.hb,
        );
        hl.farm.handle_disconnect_for_test();

        tx.send(ControlCommand::Subscribe {
            filters: Default::default(),
            contract: ContractRef {
                con_id: 756733,
                sec_type: "STK".into(),
                exchange: "SMART".into(),
                ..Default::default()
            },
            mode_9887: 0,
            regulatory_snapshot: true,
            generic_ticks: Vec::new(),
            reply_tx: None,
            issued: 0,
        })
        .expect("the engine holds the other end");
        hl.poll_once();

        let told = shared.market.drain_subscription_failures();
        assert!(
            told.is_empty(),
            "the stream's watchers were told their quote had been refused: {told:?}",
        );
    }

    /// Refusing one request does not end another running under its number.
    ///
    /// A number already answering goes on answering. Released with an empty
    /// terminal response, the live request's end fires before its bars have
    /// arrived and every bar after it goes out as an update — which is the
    /// reason the refusal for a number already in use ends nothing either.
    #[test]
    fn refusing_a_request_does_not_end_one_already_running_under_its_number() {
        let shared = Arc::new(SharedState::new());
        let mut hl = HotLoop::new(shared.clone(), None, None);
        let (tx, rx) = std::sync::mpsc::sync_channel(4);
        hl.set_control_rx(rx);

        tx.send(ControlCommand::FetchHistorical {
            contract: ContractRef { con_id: 756733, sec_type: "STK".into(), ..Default::default() },
            req_id: 64,
            end_date_time: String::new(),
            duration: "1 D".into(),
            bar_size: "1 secs".into(),
            what_to_show: "TRADES".into(),
            use_rth: true,
            keep_up_to_date: true,
            include_expired: false,
            filters: Default::default(),
        })
        .expect("the engine holds the other end");
        hl.poll_once();

        assert!(
            shared.reference.drain_historical_errors().iter().any(|(r, ..)| *r == 64),
            "the caller is told why",
        );
        assert!(
            shared.reference.drain_historical_data().is_empty(),
            "and nothing was ended under that number: a request answering there \
             would have its end fired before its bars arrived",
        );
    }

    /// A bar the fold cannot build is refused, not folded anyway.
    ///
    /// What keeps a bar current arrives as five-second bars, so a size that is
    /// not made of those cannot be kept up to date: folding into it relabels
    /// each five-second bar as the shorter one and hands the caller five times
    /// the volume under a size nothing traded in. The surface refuses these
    /// before the command is sent, and a caller reaching this loop by the
    /// control channel goes past it.
    #[test]
    fn a_bar_that_cannot_be_kept_up_to_date_is_refused_on_the_control_channel() {
        let shared = Arc::new(SharedState::new());
        let mut hl = HotLoop::new(shared.clone(), None, None);
        let (tx, rx) = std::sync::mpsc::sync_channel(4);
        hl.set_control_rx(rx);

        tx.send(ControlCommand::FetchHistorical {
            contract: ContractRef { con_id: 756733, sec_type: "STK".into(), ..Default::default() },
            req_id: 63,
            end_date_time: String::new(),
            duration: "1 D".into(),
            bar_size: "1 secs".into(),
            what_to_show: "TRADES".into(),
            use_rth: true,
            keep_up_to_date: true,
            include_expired: false,
            filters: Default::default(),
        })
        .expect("the engine holds the other end");
        hl.poll_once();

        let told = shared.reference.drain_historical_errors();
        assert!(
            told.iter().any(|(_, _, said)| said.contains("cannot be kept up to date")),
            "each five-second bar is relabelled as a one-second one and handed \
             back with five times the volume: {told:?}",
        );
    }

    /// A series named by something this client does not know is refused, not
    /// answered with trades.
    ///
    /// The builders fall back to trades on a name they do not know, and the
    /// file they live in says why that is wrong: a misspelled name asks for a
    /// different series than the caller named, and what comes back reads as
    /// what was asked for. The surfaces refuse it before the command is sent
    /// — and a caller reaching this loop by the control channel goes past
    /// them, which is what the backstop on the bar batch beside these exists
    /// for. These two had none.
    #[test]
    fn a_series_this_client_does_not_know_is_refused_on_the_control_channel() {
        for command in [
            ControlCommand::FetchHistoricalTicks {
                contract: ContractRef { con_id: 756733, sec_type: "STK".into(), ..Default::default() },
                req_id: 61,
                start_date_time: String::new(),
                end_date_time: String::new(),
                number_of_ticks: 10,
                what_to_show: "NOT_A_SERIES".into(),
                use_rth: true,
                include_expired: false,
                filters: Default::default(),
            },
            ControlCommand::SubscribeRealTimeBar {
                contract: ContractRef { con_id: 756733, sec_type: "STK".into(), ..Default::default() },
                req_id: 62,
                what_to_show: "NOT_A_SERIES".into(),
                use_rth: true,
                filters: Default::default(),
            },
        ] {
            let shared = Arc::new(SharedState::new());
            let mut hl = HotLoop::new(shared.clone(), None, None);
            let (tx, rx) = std::sync::mpsc::sync_channel(4);
            hl.set_control_rx(rx);
            tx.send(command).expect("the engine holds the other end");
            hl.poll_once();

            let told = shared.reference.drain_historical_errors();
            assert!(
                told.iter().any(|(_, _, said)| said.contains("Unsupported what_to_show")),
                "a series this client does not know was asked for as trades, and \
                 what came back read as what the caller wanted: {told:?}",
            );
        }
    }

    /// A caller that stopped waiting is sent no subscription.
    ///
    /// The wait on this side's answer is bounded and this loop is not: a redial
    /// or a lookup ahead of the command outlasts it, the caller reports a
    /// refusal and keeps no record of the slot. Subscribed anyway, the venue
    /// streamed a contract nothing could name for the rest of the session —
    /// the withdrawal is refused for want of a subscription, the slot is never
    /// given back, and only the caller happening to ask for the same contract
    /// again recovers either.
    #[test]
    fn a_caller_that_stopped_waiting_is_sent_no_subscription() {
        let shared = Arc::new(SharedState::new());
        let mut hl = HotLoop::new(shared.clone(), None, None);
        let (tx, rx) = std::sync::mpsc::sync_channel(4);
        hl.set_control_rx(rx);

        let (reply_tx, reply_rx) = std::sync::mpsc::sync_channel(1);
        // The caller gave up before the answer reached it.
        drop(reply_rx);
        tx.send(ControlCommand::Subscribe {
            filters: Default::default(),
            contract: ContractRef {
                con_id: 756733,
                sec_type: "STK".into(),
                exchange: "SMART".into(),
                ..Default::default()
            },
            mode_9887: 0,
            regulatory_snapshot: false,
            generic_ticks: Vec::new(),
            reply_tx: Some(reply_tx),
            issued: 0,
        })
        .expect("the engine holds the other end");
        hl.poll_once();

        assert!(
            hl.context.market.instrument_by_con_id(756733).is_none(),
            "a slot is held for a subscription nothing on the caller's side can name",
        );
    }

    /// And where the caller asks by contract id, which is the shorter road to
    /// the same wrong.
    ///
    /// A snapshot on the contract is enough to answer a subscribe with "you
    /// already have one". The caller was told a number and nothing went out on
    /// it; the snapshot completed, withdrew, and took the record with it.
    #[test]
    fn a_subscribe_is_not_answered_with_a_snapshot_somebody_else_has_out() {
        let shared = Arc::new(SharedState::new());
        let mut hl = HotLoop::new(shared.clone(), None, None);
        let (tx, rx) = std::sync::mpsc::sync_channel(4);
        hl.set_control_rx(rx);

        let instrument = hl.context.market.register(756733);
        hl.farm.instrument_md_reqs.push((instrument, crate::engine::hot_loop::farm::MdReqRecord {
            con_id: 756733,
            sec_type: "CS".into(),
            mode_9887: 0,
            entries: vec![crate::engine::hot_loop::farm::MdReqEntry {
                req_id: 7, request_type: 624, venue: "BEST".into(),
            }],
        }));

        let (reply_tx, reply_rx) = std::sync::mpsc::sync_channel(1);
        tx.send(ControlCommand::Subscribe {
            filters: Default::default(),
            contract: ContractRef {
                con_id: 756733,
                sec_type: "STK".into(),
                exchange: "SMART".into(),
                ..Default::default()
            },
            mode_9887: 0,
            regulatory_snapshot: false,
            generic_ticks: Vec::new(),
            reply_tx: Some(reply_tx),
            issued: 0,
        })
        .expect("the engine holds the other end");
        hl.poll_once();

        assert_eq!(
            reply_rx.try_recv().ok(), Some(Ok(instrument)),
            "the caller is given the contract's slot",
        );
        assert!(
            hl.farm.holds_a_stream(instrument),
            "and a subscription on it, rather than a snapshot about to withdraw",
        );
    }

    /// The slot guard lists what still refers to a contract, and a holding was
    /// not on it. Dropping the last subscription on something the account owns
    /// freed the slot, and the contract that took it inherited the position.
    #[test]
    fn a_slot_the_account_still_holds_is_not_reclaimed() {
        let mut hl = HotLoop::new(Arc::new(SharedState::new()), None, None);
        let instrument = hl.context.register_instrument(265598);
        hl.context.update_position(instrument, 100.0);

        hl.try_reclaim_instrument(instrument);
        assert_eq!(
            hl.context.market.con_id(instrument), Some(265598),
            "the slot stays with the contract the account is long",
        );

        assert_eq!(
            hl.pinned_by_position, vec![instrument],
            "and the request to free it is remembered rather than lost",
        );

        // Half a share is a holding. A whole-number table read this as flat and
        // handed the slot to the next contract.
        hl.context.update_position(instrument, -99.5);
        assert_eq!(hl.context.position(instrument), 0.5);
        hl.try_reclaim_instrument(instrument);
        assert_eq!(
            hl.context.market.con_id(instrument), Some(265598),
            "a fractional holding keeps its slot",
        );

        // Flat, and nothing else refers to it: now it may go.
        hl.context.update_position(instrument, -0.5);
        hl.try_reclaim_instrument(instrument);
        assert_eq!(hl.context.market.con_id(instrument), None, "the freed slot holds no contract");
        assert!(
            hl.hmds.tbt_subscriptions.iter().all(|sub| sub.instrument != instrument),
            "and no stream left for the next occupant to inherit prices from",
        );
        assert!(hl.pinned_by_position.is_empty(), "and nothing is still waiting on it");
    }

    /// A caller who would rather be told about a loss than have it handled
    /// gets exactly that: nothing is scheduled and nothing is attempted.
    #[test]
    fn a_manual_policy_leaves_the_reconnect_to_the_caller() {
        let mut hl = HotLoop::new(Arc::new(SharedState::new()), None, None);
        hl.set_reconnect_config(crate::reliability::ReconnectConfig::manual());
        hl.farm.disconnected = true;

        hl.maybe_spawn_farm_reconnect();
        assert!(hl.farm_next_attempt_at.is_none(), "nothing is scheduled");
        assert!(hl.pending_farm_reconnect.is_none(), "and nothing is in flight");
    }

    /// A budget the caller set is spent and then recovery stops, rather than
    /// climbing forever against a connection that will not come back.
    #[test]
    fn a_spent_budget_stops_the_reconnect() {
        let shared = Arc::new(SharedState::new());
        let (tx, rx) = std::sync::mpsc::sync_channel(4096);
        let mut hl = HotLoop::new(shared, Some(EventSink::new(tx, Default::default())), None);
        hl.set_reconnect_config(
            crate::reliability::ReconnectConfig::default().with_max_attempts(2),
        );
        hl.farm.disconnected = true;

        // Spend it.
        for _ in 0..2 {
            hl.farm_budget.record_attempt(Instant::now());
        }
        hl.maybe_spawn_farm_reconnect();

        assert!(hl.farm_next_attempt_at.is_none(), "no further attempt is scheduled");
        assert!(hl.farm_halted.is_some(), "the farm is given up on");
        // The farm alone ran out, with the trading connection up: the caller
        // was told the farm was down when it dropped, and the session stands.
        assert!(
            !matches!(rx.try_recv(), Ok(Event::Disconnected)),
            "and the session is not said to have ended",
        );
    }

    /// A connection that comes up after the caller gave up on it is not taken.
    ///
    /// What a worker answered was taken before the elapsed limit was read, and
    /// taking it cleared the record that limit is read from — so a connection
    /// arriving past the time the caller allowed was installed anyway, the
    /// session came back after the caller had been told it would not, and
    /// nothing said so.
    #[test]
    fn a_connection_arriving_past_the_limit_is_not_installed() {
        let shared = Arc::new(SharedState::new());
        let (tx, _rx) = std::sync::mpsc::sync_channel(4096);
        let mut hl = HotLoop::new(
            shared.clone(), Some(EventSink::new(tx, Default::default())), None,
        );
        hl.set_reconnect_config(
            crate::reliability::ReconnectConfig::default()
                .with_max_elapsed(Duration::from_secs(30)),
        );
        hl.ccp.disconnected = true;

        let t = Instant::now();
        hl.budget.record_attempt(t);
        let (worker, worker_rx) = std::sync::mpsc::sync_channel(1);
        hl.pending_ccp_reconnect = Some(worker_rx);

        // The limit runs out while the worker is still dialling.
        hl.abandon_recovery_past_its_deadline(t + Duration::from_secs(31));
        assert!(hl.reconnect_halted.is_some(), "recovery is over");

        // And then it lands, which is what it was still doing. Waiting for it
        // here is the wait this halt exists to spare the caller, so it is left
        // to arrive on its own thread and read on a later lap.
        let (conn, mut peer) = crate::protocol::connection::Connection::for_test();
        worker.send(Ok(conn)).unwrap();
        hl.poll_ccp_reconnect();

        assert!(
            hl.ccp_conn.is_none(),
            "a connection that lands after the caller gave up is not installed behind \
             their back",
        );
        // Told it is going rather than dropped in silence: the venue holds one
        // session per account and has to time out one that just goes, so the
        // next connection would race a session it still believes is live.
        use std::io::Read;
        let mut buf = [0u8; 4096];
        let n = peer.read(&mut buf).unwrap();
        let said = String::from_utf8_lossy(&buf[..n]).replace('\u{1}', "|");
        assert!(said.contains("35=5|"), "and it is told the session is going: {said}");
    }

    /// The time the caller allowed recovery bounds the attempt being spent on
    /// it, not only the ones that have not started yet.
    ///
    /// The schedulers return as soon as they find an attempt in flight, so the
    /// elapsed limit was read between attempts and nowhere else. One attempt
    /// outlasts a short limit easily — a handshake that asks for a second
    /// factor waits on a person, to its own timeout — and a caller who bounded
    /// recovery to thirty seconds waited out every second of that instead,
    /// with nothing said and the worker still dialling.
    #[test]
    fn an_elapsed_limit_ends_the_attempt_it_is_being_spent_on() {
        let shared = Arc::new(SharedState::new());
        let (tx, rx) = std::sync::mpsc::sync_channel(4096);
        let mut hl = HotLoop::new(
            shared.clone(), Some(EventSink::new(tx, Default::default())), None,
        );
        hl.set_reconnect_config(
            crate::reliability::ReconnectConfig::default()
                .with_max_elapsed(Duration::from_secs(30)),
        );
        hl.ccp.disconnected = true;

        // One attempt, still dialling: this is what the scheduler returns on.
        let t = Instant::now();
        hl.budget.record_attempt(t);
        let (_worker, worker_rx) = std::sync::mpsc::sync_channel(1);
        hl.pending_ccp_reconnect = Some(worker_rx);

        hl.abandon_recovery_past_its_deadline(t + Duration::from_secs(29));
        assert!(
            !hl.reconnect_cancel.load(Ordering::Relaxed),
            "inside the limit the attempt is left to run",
        );

        hl.abandon_recovery_past_its_deadline(t + Duration::from_secs(31));
        assert!(
            hl.reconnect_cancel.load(Ordering::Relaxed),
            "past it the attempt is taken back rather than left to its own timeout",
        );
        assert!(hl.reconnect_halted.is_some(), "and recovery is over");
        assert!(
            shared.reference.session_over().is_some(),
            "and the caller is told, rather than waiting on a session nothing is rebuilding",
        );
        assert!(
            matches!(rx.try_recv(), Ok(Event::Disconnected)),
            "on the channel as well as in the session's state",
        );

        // The attempt limit says how many more may start, not how long the one
        // running may take: an attempt the caller allowed is left to finish.
        let mut hl = HotLoop::new(Arc::new(SharedState::new()), None, None);
        hl.set_reconnect_config(
            crate::reliability::ReconnectConfig::default().with_max_attempts(1),
        );
        hl.ccp.disconnected = true;
        hl.budget.record_attempt(t);
        let (_worker, worker_rx) = std::sync::mpsc::sync_channel(1);
        hl.pending_ccp_reconnect = Some(worker_rx);
        hl.abandon_recovery_past_its_deadline(t + Duration::from_secs(3600));
        assert!(
            !hl.reconnect_cancel.load(Ordering::Relaxed),
            "the last attempt the caller allowed is not taken back for being the last",
        );
    }

    /// A transport told something final takes back the transport still
    /// dialling.
    ///
    /// The connections recover side by side, and an answer like refused
    /// credentials or an account taken elsewhere ends the session rather than
    /// the one connection that heard it. The sibling worker went on
    /// authenticating regardless, and what it opened was only dropped once it
    /// came back — by which time a session is open at the venue and somebody
    /// may have approved a second factor for it.
    #[test]
    fn a_terminal_answer_takes_back_the_reconnect_still_dialling() {
        let shared = Arc::new(SharedState::new());
        let mut hl = HotLoop::new(shared.clone(), None, None);
        hl.farm.disconnected = true;
        hl.ccp.disconnected = true;

        // The trading side is midway through a handshake.
        let (_worker, worker_rx) = std::sync::mpsc::sync_channel(1);
        hl.pending_ccp_reconnect = Some(worker_rx);

        // And the quote feed's attempt comes back refused for good.
        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        tx.send(Err(io::Error::new(
            io::ErrorKind::PermissionDenied, "the venue would not take the credentials",
        ))).unwrap();
        hl.pending_farm_reconnect = Some(rx);

        hl.poll_farm_reconnect();

        assert!(
            hl.reconnect_halted.is_some_and(|why| why.is_terminal()),
            "the session is over for good",
        );
        assert!(
            hl.reconnect_cancel.load(Ordering::Relaxed),
            "and the handshake still running was taken back rather than left to \
             open a session at the venue",
        );
    }

    /// Each transport spends its own attempts. One still dialling leaves the
    /// other's unspent budget available, even when the limit is one.
    #[test]
    fn the_scheduler_beside_an_attempt_does_not_take_back_the_last_one_allowed() {
        for farm_dialling in [true, false] {
            let shared = Arc::new(SharedState::new());
            let mut hl = HotLoop::new(shared.clone(), None, None);
            hl.set_reconnect_config(
                crate::reliability::ReconnectConfig::default().with_max_attempts(1),
            );
            hl.farm.disconnected = true;
            hl.ccp.disconnected = true;
            let (_worker, rx) = std::sync::mpsc::sync_channel(1);
            if farm_dialling {
                hl.farm_budget.record_attempt(Instant::now());
                hl.pending_farm_reconnect = Some(rx);
                hl.maybe_spawn_ccp_reconnect();
                assert!(hl.ccp_next_attempt_at.is_some(), "trading still has its attempt");
            } else {
                hl.budget.record_attempt(Instant::now());
                hl.pending_ccp_reconnect = Some(rx);
                hl.maybe_spawn_farm_reconnect();
                assert!(hl.farm_next_attempt_at.is_some(), "the farm still has its attempt");
            }
            assert!(!hl.reconnect_cancel.load(Ordering::Relaxed));
            assert!(hl.reconnect_halted.is_none());
            assert!(hl.farm_halted.is_none());
            assert!(shared.reference.session_over().is_none());
        }
    }

    /// Trading has spent its last attempt. A farm dial spends a separate
    /// budget and cannot restore the trading connection that orders need.
    #[test]
    fn exhausted_trading_recovery_ends_the_session_while_the_farm_dials() {
        let shared = Arc::new(SharedState::new());
        let (tx, rx) = std::sync::mpsc::sync_channel(8);
        let mut hl = HotLoop::new(shared.clone(), Some(EventSink::new(tx, Default::default())), None);
        hl.set_reconnect_config(
            crate::reliability::ReconnectConfig::default().with_max_attempts(1),
        );
        hl.farm.disconnected = true;
        hl.ccp.disconnected = true;
        hl.budget.record_attempt(Instant::now());
        hl.farm_budget.record_attempt(Instant::now());
        let (_worker, farm_rx) = std::sync::mpsc::sync_channel(1);
        hl.pending_farm_reconnect = Some(farm_rx);

        hl.maybe_spawn_ccp_reconnect();

        assert_eq!(hl.reconnect_halted, Some(retry::DisconnectReason::RecoveryExhausted));
        assert!(shared.reference.session_over().is_some());
        assert!(shared.reference.trading_over().is_some());
        assert!(hl.reconnect_cancel.load(Ordering::Relaxed));
        assert!(hl.pending_farm_reconnect.is_none());
        assert!(matches!(rx.try_recv(), Ok(Event::Disconnected)));
        hl.maybe_spawn_ccp_reconnect();
        assert!(rx.try_recv().is_err(), "the end is announced once");
    }

    /// The farm's exhaustion is reported while trading uses its own attempt.
    #[test]
    fn exhausted_farm_recovery_is_reported_while_trading_dials() {
        let shared = Arc::new(SharedState::new());
        let mut hl = HotLoop::new(shared.clone(), None, None);
        hl.set_reconnect_config(
            crate::reliability::ReconnectConfig::default().with_max_attempts(1),
        );
        hl.farm.disconnected = true;
        hl.ccp.disconnected = true;
        hl.farm_budget.record_attempt(Instant::now());
        hl.budget.record_attempt(Instant::now());
        let (_worker, ccp_rx) = std::sync::mpsc::sync_channel(1);
        hl.pending_ccp_reconnect = Some(ccp_rx);

        hl.maybe_spawn_farm_reconnect();

        assert_eq!(hl.farm_halted, Some(retry::DisconnectReason::RecoveryExhausted));
        assert!(shared.market.market_data_over().is_some());
        assert!(shared.reference.session_over().is_none());
        assert!(hl.reconnect_halted.is_none());
        assert!(hl.pending_ccp_reconnect.is_some());
        assert!(!hl.reconnect_cancel.load(Ordering::Relaxed));
    }

    /// An answer that ends recovery ends the trading connection with it,
    /// whichever connection heard it.
    ///
    /// The connections recover side by side and the halt is the session's:
    /// nothing dials the trading connection again once the quote feed has been
    /// told something final. Told only that the session was over, both order
    /// surfaces went on taking orders — they read this connection's own state,
    /// so that a quote feed the venue will not serve again does not refuse what
    /// a live trading connection would carry — and the engine buffered each one
    /// for a connection that was never coming.
    #[test]
    fn a_final_answer_on_one_connection_ends_the_trading_connection_that_is_down() {
        let shared = Arc::new(SharedState::new());
        let mut hl = HotLoop::new(shared.clone(), None, None);
        hl.farm.disconnected = true;
        hl.ccp.disconnected = true;

        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        tx.send(Err(io::Error::new(
            io::ErrorKind::PermissionDenied, "the venue would not take the credentials",
        ))).unwrap();
        hl.pending_farm_reconnect = Some(rx);

        hl.poll_farm_reconnect();

        assert!(hl.reconnect_halted.is_some(), "recovery is over");
        assert_eq!(
            shared.reference.trading_over(),
            Some(retry::DisconnectReason::AuthorizationFailed.as_str()),
            "orders go on being taken and buffered for a connection nothing will dial again",
        );
    }

    /// A farm that came up past its deadline is not installed.
    ///
    /// The clock is read in the scheduler, and the scheduler answers at once
    /// once the connection is there — so a success taken before it is read is
    /// one whose deadline never fires. The caller was told historical data was
    /// coming back on a farm they had already stopped waiting for.
    #[test]
    fn a_farm_that_came_up_past_its_deadline_is_not_installed() {
        let mut hl = HotLoop::new(Arc::new(SharedState::new()), None, None);
        hl.set_reconnect_config(
            crate::reliability::ReconnectConfig::default()
                .with_max_elapsed(Duration::from_secs(30)),
        );
        // Dialled for longer than the caller allowed this farm's whole
        // recovery, and answered just before the loop got back to it.
        hl.hmds_budget.record_attempt(Instant::now() - Duration::from_secs(31));
        let (worker, worker_rx) = std::sync::mpsc::sync_channel(1);
        let (conn, _peer) = crate::protocol::connection::Connection::for_test();
        worker.send(Ok(conn)).unwrap();
        hl.pending_hmds_reconnect = Some(worker_rx);

        hl.poll_optional_farm_reconnects();

        assert_eq!(
            hl.hmds_halted, Some(retry::DisconnectReason::ByDesign),
            "the deadline the dial ran past was never read",
        );
        assert!(
            hl.hmds_conn.is_none(),
            "a farm the session had stopped waiting for was installed anyway",
        );
    }

    /// The time the caller allowed a farm reaches the attempt spending it.
    ///
    /// The scheduler returns as soon as it finds an attempt in flight, so the
    /// budget beside it bounded when the next attempt could start and nothing
    /// about how long this one may take: a dial against a host that never
    /// answers held the deadline open for as long as that dial lasted, and what
    /// it finally returned was installed on a farm the session had stopped
    /// waiting for.
    #[test]
    fn the_time_an_optional_farm_was_allowed_reaches_the_attempt_spending_it() {
        let mut hl = HotLoop::new(Arc::new(SharedState::new()), None, None);
        hl.set_reconnect_config(
            crate::reliability::ReconnectConfig::default()
                .with_max_elapsed(Duration::from_secs(30)),
        );
        // An attempt in flight, started further back than the caller allowed
        // the whole of this farm's recovery.
        hl.hmds_budget.record_attempt(Instant::now() - Duration::from_secs(31));
        let (_worker, worker_rx) = std::sync::mpsc::sync_channel(1);
        hl.pending_hmds_reconnect = Some(worker_rx);

        hl.maybe_spawn_hmds_reconnect();

        assert_eq!(
            hl.hmds_halted, Some(retry::DisconnectReason::ByDesign),
            "the farm went on dialling past the time the caller allowed it",
        );
        assert!(
            hl.pending_hmds_reconnect.is_none(),
            "and what that dial finally returned would have been installed",
        );

        // And the calendar's, rebuilt on the same terms as the historical
        // farm and bounded on them too.
        hl.secdef_budget.record_attempt(Instant::now() - Duration::from_secs(31));
        let (_worker, worker_rx) = std::sync::mpsc::sync_channel(1);
        hl.pending_secdef_reconnect = Some(worker_rx);

        hl.maybe_spawn_secdef_reconnect();

        assert_eq!(
            hl.secdef_halted, Some(retry::DisconnectReason::ByDesign),
            "the calendar went on dialling past the time the caller allowed it",
        );
        assert!(
            hl.pending_secdef_reconnect.is_none(),
            "and what that dial finally returned would have been installed",
        );
    }

    /// A loop that leaves by panicking takes its reconnects back on the way,
    /// as one that returns does.
    ///
    /// Unwinding skips the return, so the flag stayed down and the handles
    /// were dropped rather than waited for — which detaches the threads. A
    /// worker went on resolving, connecting and authenticating after the
    /// engine had announced its death, and opened a session at the venue on
    /// behalf of a loop that was gone.
    #[test]
    fn a_loop_that_panics_takes_back_the_reconnects_it_left_running() {
        use std::io::Read;
        let mut hl = HotLoop::new(Arc::new(SharedState::new()), None, None);

        // A worker midway through a handshake. It reads the flag between the
        // phases, and finishes once the flag is raised.
        let cancel = Arc::clone(&hl.reconnect_cancel);
        let (finished, has_finished) = std::sync::mpsc::sync_channel(1);
        hl.reconnect_workers.push((true,
            std::thread::Builder::new()
                .name("reconnect-under-a-panic".into())
                .spawn(move || {
                    while !cancel.load(Ordering::Relaxed) {
                        std::thread::yield_now();
                    }
                    let _ = finished.send(());
                })
                .expect("a thread for the attempt"),
        ));
        let cancel = Arc::clone(&hl.reconnect_cancel);

        // A lap the loop cannot finish: the counter it opens with is at its
        // last value. A build with the overflow unchecked runs the lap
        // instead and finds its control channel gone, which stops it the
        // ordinary way rather than leaving this waiting on a loop that never
        // ends.
        let (control, control_rx) = std::sync::mpsc::sync_channel(1);
        drop(control);
        hl.set_control_rx(control_rx);
        // And an attempt that finished between the flag going up and the wait
        // for it: a session open at the venue, which holds one per account and
        // has to time out one that just goes.
        let (conn, mut peer) = crate::protocol::connection::Connection::for_test();
        let (landed, landed_rx) = std::sync::mpsc::sync_channel(1);
        landed.send(Ok(conn)).unwrap();
        hl.pending_ccp_reconnect = Some(landed_rx);
        hl.context.loop_iterations = u64::MAX;
        hl.run_with_panic_recovery();

        assert!(
            cancel.load(Ordering::Relaxed),
            "a worker was left dialling with the flag down after the engine said it died",
        );
        assert!(
            has_finished.try_recv().is_ok(),
            "and was detached rather than waited for, so it went on opening a session",
        );
        let mut buf = [0u8; 4096];
        let n = peer.read(&mut buf).expect("the goodbye that session is owed");
        assert!(
            String::from_utf8_lossy(&buf[..n]).contains("35=5"),
            "the venue is left holding a session it has to time out",
        );

        // And a loop that runs again does not install what the last one left
        // dialling. Running again starts with the flag down, so a connection
        // left in its channel is a socket from the session before it —
        // resubscribed, and announced as this session's own.
        let mut hl = HotLoop::new(Arc::new(SharedState::new()), None, None);
        let (conn, _peer) = crate::protocol::connection::Connection::for_test();
        let (landed, landed_rx) = std::sync::mpsc::sync_channel(1);
        landed.send(Ok(conn)).unwrap();
        hl.pending_farm_reconnect = Some(landed_rx);

        hl.take_back_the_recovery_still_in_flight();

        assert!(
            hl.pending_farm_reconnect.is_none(),
            "the next run installs a socket the session before it opened",
        );
    }

    /// A trading session that lands after the session ended is told the
    /// session is going, rather than dropped in silence.
    ///
    /// The venue holds one session per account and has to time out one that
    /// just goes, so the next connection races a session the venue still
    /// believes is live. That is why a stop says this on the connection the
    /// loop holds, and one that arrives after the stop is no different.
    #[test]
    fn a_trading_session_that_lands_after_the_end_is_told_the_session_is_going() {
        use std::io::Read;
        let mut hl = HotLoop::new(Arc::new(SharedState::new()), None, None);
        hl.ccp.disconnected = true;
        // The session ended for good while this attempt was still dialling.
        hl.reconnect_halted = Some(retry::DisconnectReason::TakenOver);

        let (conn, mut peer) = crate::protocol::connection::Connection::for_test();
        let (worker, rx) = std::sync::mpsc::sync_channel(1);
        worker.send(Ok(conn)).unwrap();
        hl.pending_ccp_reconnect = Some(rx);

        hl.poll_ccp_reconnect();

        assert!(
            hl.ccp_conn.is_none(),
            "a session the caller was told was over is not installed",
        );
        let mut buf = [0u8; 4096];
        let n = peer.read(&mut buf).expect("the goodbye this connection is owed");
        let sent = String::from_utf8_lossy(&buf[..n]).to_string();
        assert!(
            sent.contains("35=5"),
            "the venue is left holding a session it has to time out, and this account \
             permits one at a time: {sent}",
        );
    }

    /// A transport the engine has given up does not go on owning the socket it
    /// was carried on.
    ///
    /// A liveness timeout says the venue stopped answering, not that the socket
    /// closed: kept through the outage it is a session this client still holds
    /// while it dials another, and this account permits one at a time. On a
    /// hard error it is a descriptor held for the whole of the outage. The
    /// historical and calendar farms have always cleared theirs here.
    #[test]
    fn a_transport_given_up_does_not_go_on_owning_its_socket() {
        let mut hl = HotLoop::new(Arc::new(SharedState::new()), None, None);
        let (ccp, _ccp_peer) = crate::protocol::connection::Connection::for_test();
        let (farm, _farm_peer) = crate::protocol::connection::Connection::for_test();
        hl.ccp_conn = Some(ccp);
        hl.farm_conn = Some(farm);

        hl.ccp.handle_disconnect(
            &mut hl.ccp_conn, &mut hl.context, &hl.shared, &hl.event_tx,
        );
        hl.farm.handle_disconnect(&mut hl.farm_conn, &mut hl.context, &hl.event_tx, &hl.shared);

        assert!(
            hl.ccp_conn.is_none(),
            "the trading socket is held for the whole of the outage",
        );
        assert!(hl.farm_conn.is_none(), "and the quote feed's with it");
    }

    /// A quote feed given up on leaves the trading connection its recovery.
    ///
    /// The feed spent the session's budget and, halted for good, kept it from
    /// ever settling; the trading connection's next drop, hours later, was
    /// given up on with no attempt made and every buffered order refused.
    #[test]
    fn a_farm_given_up_on_leaves_the_trading_connection_its_recovery() {
        let shared = Arc::new(SharedState::new());
        let mut hl = HotLoop::new(shared.clone(), None, None);
        hl.set_reconnect_config(crate::reliability::ReconnectConfig::default().with_max_attempts(1));
        hl.farm.disconnected = true;
        hl.farm_budget.record_attempt(Instant::now());
        hl.maybe_spawn_farm_reconnect();
        assert!(hl.farm_halted.is_some(), "the feed is given up on");

        hl.ccp.disconnected = true;
        hl.maybe_spawn_ccp_reconnect();
        assert!(shared.reference.session_over().is_none(), "the session stands");
        assert!(hl.ccp_next_attempt_at.is_some(), "and the trading connection gets its attempt");
    }

    /// One loss is announced once, including when the halt announces it.
    ///
    /// The attempt a halt gives up on is left to land on its own thread rather
    /// than waited out on the loop's, so the poller that reads it runs again
    /// afterwards — and an answer that is not final reaches the branch that
    /// announces a loss. The halt has already announced it; unrecorded, the
    /// one loss went out twice, and a caller counting them saw two.
    #[test]
    fn a_loss_the_halt_announced_is_not_announced_again() {
        let shared = Arc::new(SharedState::new());
        let (events, heard) = std::sync::mpsc::sync_channel(8);
        let mut hl = HotLoop::new(
            shared, Some(EventSink::new(events, Default::default())), None,
        );
        hl.ccp.disconnected = true;
        // Two attempts already spent, which is what puts the branch below
        // within reach of the third.
        hl.ccp_reconnect_attempt = 3;

        let (worker, rx) = std::sync::mpsc::sync_channel(1);
        hl.pending_ccp_reconnect = Some(rx);
        hl.halt_recovery(retry::DisconnectReason::AuthorizationFailed);

        // The attempt lands, having failed for a reason that is not final.
        worker.send(Err(std::io::Error::other("the connection broke"))).unwrap();
        hl.poll_ccp_reconnect();

        let said = heard.try_iter()
            .filter(|e| matches!(e, Event::Disconnected))
            .count();
        assert_eq!(said, 1, "one loss, said once");

        // And the same where the worker goes without answering at all, which
        // is the other way that poller reaches the same notice.
        let shared = Arc::new(SharedState::new());
        let (events, heard) = std::sync::mpsc::sync_channel(8);
        let mut hl = HotLoop::new(
            shared, Some(EventSink::new(events, Default::default())), None,
        );
        hl.ccp.disconnected = true;
        hl.ccp_reconnect_attempt = 3;
        let (worker, rx) = std::sync::mpsc::sync_channel::<Result<Connection, std::io::Error>>(1);
        hl.pending_ccp_reconnect = Some(rx);
        hl.halt_recovery(retry::DisconnectReason::AuthorizationFailed);
        drop(worker);
        hl.poll_ccp_reconnect();

        let said = heard.try_iter()
            .filter(|e| matches!(e, Event::Disconnected))
            .count();
        assert_eq!(said, 1, "and once where the worker answered nothing at all");
    }

    /// A caller who asked to do the reconnecting is told, and left with a
    /// session.
    ///
    /// "Never recover on its own. The client reports the loss and waits; the
    /// caller decides whether and when to reconnect" is what the policy says.
    /// What that caller got was a session declared over for good on its first
    /// dropped connection, under the words "the recovery limits the caller set
    /// are spent" — for limits it had never set, off a predicate that answers
    /// "no" both to a budget that ran out and to a caller who allowed no
    /// attempts at all.
    #[test]
    fn a_manual_policy_is_told_of_a_loss_and_keeps_its_session() {
        let shared = Arc::new(SharedState::new());
        let (events, heard) = std::sync::mpsc::sync_channel(8);
        let mut hl = HotLoop::new(
            shared.clone(), Some(EventSink::new(events, Default::default())), None,
        );
        hl.set_reconnect_config(crate::reliability::ReconnectConfig::manual());

        // The feed goes first, then the trading connection — the order the run
        // loop polls them in.
        hl.farm.disconnected = true;
        hl.maybe_spawn_farm_reconnect();
        assert!(hl.farm_halted.is_none(), "a feed nobody was asked to rebuild is not given up on");
        assert!(
            shared.market.market_data_over().is_none(),
            "and market data is not declared over for the session",
        );

        hl.ccp.disconnected = true;
        hl.maybe_spawn_ccp_reconnect();
        assert!(hl.reconnect_halted.is_none(), "nor is the session");
        assert!(shared.reference.session_over().is_none(), "which every request reads");
        assert!(shared.reference.trading_over().is_none());
        assert!(hl.pending_ccp_reconnect.is_none(), "and nothing is dialled, as asked");

        // Told once, which is the other half of what that policy promises.
        let said = heard.try_iter().filter(|e| matches!(e, Event::Disconnected)).count();
        assert_eq!(said, 1, "the loss is reported");

        hl.maybe_spawn_ccp_reconnect();
        let again = heard.try_iter().filter(|e| matches!(e, Event::Disconnected)).count();
        assert_eq!(again, 0, "and reported once, not every lap");
    }

    /// The trading connection's return is announced whether or not a quote
    /// feed given up on for the session is still down: that feed's loss was
    /// said under its own numbers.
    #[test]
    fn a_return_is_announced_past_a_farm_given_up_on() {
        let shared = Arc::new(SharedState::new());
        let mut hl = HotLoop::new(shared.clone(), None, None);
        hl.loss_announced = true;
        hl.farm.disconnected = true;
        hl.farm_halted = Some(retry::DisconnectReason::RecoveryExhausted);
        hl.ccp.disconnected = false;
        hl.announce_reconnected();
        assert!(shared.take_connection_restored(), "the return is announced");
        assert!(!hl.loss_announced);
    }

    /// The quote feed's spent budget is not the session's, whatever the trading
    /// connection is doing at the time.
    ///
    /// Read once against the trading connection being up, so a farm that ran out
    /// while the trading connection happened to also be down ended the session —
    /// spending, on the feed's exhaustion, attempts the caller had allowed the
    /// trading connection and it had not used yet.
    #[test]
    fn a_spent_farm_budget_does_not_end_a_session_the_trading_connection_is_still_recovering() {
        let shared = Arc::new(SharedState::new());
        let mut hl = HotLoop::new(shared.clone(), None, None);
        hl.farm.disconnected = true;
        hl.ccp.disconnected = true;
        hl.report_recovery_exhausted("farm");
        assert!(shared.reference.session_over().is_none(), "the farm's budget is not the session's");
        assert!(shared.reference.trading_over().is_none(), "and trading is not ended");
        assert!(hl.farm_halted.is_some(), "the farm alone is given up on");

        // The trading connection running out is what ends it.
        hl.report_recovery_exhausted("ccp");
        assert!(shared.reference.session_over().is_some(), "the trading connection's budget is");
    }

    /// A disconnect the caller asks for is the one the venue would have given.
    ///
    /// Only the flags were set. Both sockets stayed installed while the pollers
    /// ignored them and the heartbeat passed them over — held open and never
    /// written to until a reconnect replaced them — and none of the bookkeeping
    /// a drop does ran, so an order that completed at the venue during the
    /// window was absent from the recovery's reckoning and stood in the book as
    /// working for the rest of the session.
    #[test]
    fn a_disconnect_the_caller_asks_for_gives_the_sockets_up() {
        let shared = Arc::new(SharedState::new());
        let mut hl = HotLoop::new(shared.clone(), None, None);
        let (tx, rx) = std::sync::mpsc::sync_channel(4);
        hl.set_control_rx(rx);

        let (ccp, _ccp_peer) = crate::protocol::connection::Connection::for_test();
        let (farm, _farm_peer) = crate::protocol::connection::Connection::for_test();
        hl.ccp_conn = Some(ccp);
        hl.farm_conn = Some(farm);

        tx.send(ControlCommand::ForceDisconnect).expect("the engine holds the other end");
        hl.poll_once();

        assert!(hl.ccp.disconnected && hl.farm.disconnected, "both are said to be down");
        assert!(
            hl.ccp_conn.is_none(),
            "the trading socket was left installed on a connection said to be gone",
        );
        assert!(
            hl.farm_conn.is_none(),
            "the quote socket was left installed on a connection said to be gone",
        );
    }

    /// The farm's own clock is read whatever the trading connection is doing.
    ///
    /// The two budgets are separate and start at different moments, and this
    /// read took one or the other: with a trading attempt in flight the farm's
    /// limit was not read at all. A trading handshake that waits on a person
    /// waits to its own timeout, so the farm went on dialling long past what
    /// the caller allowed it, and a farm connection landing after that was
    /// installed as though it had arrived in time.
    #[test]
    fn a_farm_out_of_time_is_given_up_though_a_trading_attempt_is_in_flight() {
        let shared = Arc::new(SharedState::new());
        let mut hl = HotLoop::new(shared.clone(), None, None);
        hl.set_reconnect_config(
            crate::reliability::ReconnectConfig::default()
                .with_max_elapsed(Duration::from_secs(30)),
        );

        // Both transports are recovering. The senders are held so the receivers
        // read as attempts still in flight rather than as workers that ended.
        let (farm_tx, farm_rx) = std::sync::mpsc::channel();
        let (ccp_tx, ccp_rx) = std::sync::mpsc::channel();
        hl.pending_farm_reconnect = Some(farm_rx);
        hl.pending_ccp_reconnect = Some(ccp_rx);

        let now = Instant::now();
        // The farm has been at it past what the caller allowed; the trading
        // connection started later and is still inside its own limit.
        hl.farm_budget.record_attempt(now - Duration::from_secs(60));
        hl.budget.record_attempt(now - Duration::from_secs(5));

        hl.abandon_recovery_past_its_deadline(now);

        assert!(
            hl.farm_halted.is_some(),
            "the farm outran its own clock and went on dialling because the \
             trading connection was still inside its",
        );
        assert!(
            shared.reference.session_over().is_none(),
            "and the farm running out is not the session running out",
        );
        drop((farm_tx, ccp_tx));
    }

    /// And the loop says it, rather than only a test calling by hand.
    ///
    /// The guard above was already right; nothing called it at the moment the
    /// waiting ended. A trading connection that came back during a joint outage
    /// was held by a farm still dialling, and when that farm ran out there was
    /// no further recovery to announce on — so the caller went on reading a
    /// healthy session as lost, for the rest of it.
    #[test]
    fn giving_up_on_the_farm_announces_the_return_it_was_holding_back() {
        let shared = Arc::new(SharedState::new());
        let mut hl = HotLoop::new(shared.clone(), None, None);
        hl.loss_announced = true;
        hl.ccp.disconnected = false;
        hl.farm.disconnected = true;
        hl.report_recovery_exhausted("farm");
        assert!(shared.reference.session_over().is_none(), "the session stands");
        assert!(hl.farm_halted.is_some(), "the farm is given up on");
        assert!(
            shared.take_connection_restored(),
            "and the return the farm was holding back is announced",
        );
        assert!(!hl.loss_announced);
    }

    /// A feed that cannot be rebuilt for want of credentials is the feed
    /// ending, on the same rule as a feed the venue refuses and a feed whose
    /// budget runs out. Only those two kept it: the third path ended the
    /// session, so every reader was refused as not connected and both event
    /// loops exited, while the trading connection stayed up and went on taking
    /// orders on a session the caller could no longer read.
    #[test]
    fn a_farm_with_no_cached_credentials_ends_the_farm_not_the_session() {
        let shared = Arc::new(SharedState::new());
        let mut hl = HotLoop::new(shared.clone(), None, None);
        hl.loss_announced = true;
        hl.ccp.disconnected = false;
        hl.farm.disconnected = true;
        // Nothing was cached, which is the case under test.
        assert!(hl.reconnect_auth.is_none());
        hl.spawn_farm_reconnect();
        assert!(shared.reference.session_over().is_none(), "the session stands");
        assert!(shared.reference.trading_over().is_none(), "and trading is not ended");
        assert!(hl.farm_halted.is_some(), "the farm alone is given up on");
        assert!(
            shared.take_connection_restored(),
            "and the return the farm was holding back is announced",
        );
    }

    /// Every way of giving up on the feed says so where a subscription is
    /// asked for.
    ///
    /// The refusal is raised on the caller's side, because the two ways a
    /// request can be told it has a subscription part company before the engine
    /// sees either: a request joining a contract already watched is answered
    /// there and never arrives here. So the halt has to be readable from there,
    /// and each of the ways of reaching it has to say so.
    ///
    /// The third way — a reason the venue will not take back — is reached from
    /// a worker thread's answer and is not driven here; it sets the same flag
    /// beside the same field.
    #[test]
    fn giving_up_on_the_feed_says_so_where_a_subscription_is_asked_for() {
        for reach_it in ["the budget ran out", "no credentials were cached"] {
            let shared = Arc::new(SharedState::new());
            let mut hl = HotLoop::new(shared.clone(), None, None);
            hl.ccp.disconnected = false;
            hl.farm.disconnected = true;
            match reach_it {
                "the budget ran out" => hl.report_recovery_exhausted("farm"),
                _ => hl.spawn_farm_reconnect(),
            }
            assert!(hl.farm_halted.is_some(), "{reach_it}: the feed is given up on");
            assert!(
                shared.market.market_data_over().is_some(),
                "{reach_it}: a subscription asked for after this cannot be served",
            );
        }
    }

    /// A feed the caller rebuilds by hand serves subscriptions again.
    ///
    /// Giving up is the engine giving up on its OWN recovery, and it never
    /// picks that up again — which is what the refusal beside it means. A
    /// caller handing in a transport of its own is not that: the feed it hands
    /// in is live. Left standing, every subscription on it was refused for the
    /// rest of the session, and nothing would have tried to rebuild it either.
    #[test]
    fn a_feed_the_caller_rebuilds_serves_subscriptions_again() {
        let shared = Arc::new(SharedState::new());
        let mut hl = HotLoop::new(shared.clone(), None, None);
        hl.ccp.disconnected = false;
        hl.farm.disconnected = true;
        hl.report_recovery_exhausted("farm");
        assert!(shared.market.market_data_over().is_some(), "given up on");

        let (conn, _peer) = crate::protocol::connection::Connection::for_test();
        hl.reconnect_farm(conn);

        assert!(
            shared.market.market_data_over().is_none(),
            "the feed the caller handed in serves what is asked of it",
        );
        assert!(hl.farm_halted.is_none(), "and the engine may recover it again");
    }


    /// And with the trading connection down as well, it is the session: there
    /// is nothing left to keep.
    #[test]
    fn a_farm_with_no_cached_credentials_ends_the_session_when_trading_is_down_too() {
        let shared = Arc::new(SharedState::new());
        let mut hl = HotLoop::new(shared.clone(), None, None);
        hl.ccp.disconnected = true;
        hl.farm.disconnected = true;
        hl.spawn_farm_reconnect();
        assert!(shared.reference.session_over().is_some(), "the session is over");
    }

    /// The market-data farm running out of recovery ends the market-data farm,
    /// not the session. Recorded as the session's end, every read was refused
    /// as not connected and both event loops exited, while the trading
    /// connection stayed up and went on taking orders on a session the caller
    /// could no longer read. The trading connection running out ends it.
    #[test]
    fn a_market_data_farm_out_of_recovery_does_not_end_the_session() {
        let shared = Arc::new(SharedState::new());
        let mut hl = HotLoop::new(shared.clone(), None, None);
        // Limits that are genuinely spent, which is what this is about. Asking
        // for no recovery at all is a different answer and says so elsewhere:
        // a caller who allowed no attempts has spent none.
        hl.reconnect_cfg = crate::reliability::ReconnectConfig::default().with_max_attempts(0);
        hl.farm.disconnected = true;
        hl.maybe_spawn_farm_reconnect();
        assert!(shared.reference.session_over().is_none(), "the session stands");
        assert!(hl.farm_halted.is_some(), "and the farm is given up on");

        hl.ccp.disconnected = true;
        hl.maybe_spawn_ccp_reconnect();
        assert!(shared.reference.session_over().is_some(), "the trading connection running out ends it");
    }

    /// An answer the quote feed's own logon cannot come back from — credentials
    /// the venue refuses — ends that feed, not the session, while the trading
    /// connection is up. The spent-budget path beside it already keeps that
    /// rule; this one reached `halt_recovery` directly, so a farm the venue
    /// would not take back refused every reader as not connected and ended both
    /// event loops on a session whose trading connection was still carrying
    /// orders.
    #[test]
    fn a_farm_the_venue_will_not_take_back_does_not_end_the_session() {
        let refused = || std::io::Error::new(
            std::io::ErrorKind::PermissionDenied, "Farm logon rejected",
        );

        let shared = Arc::new(SharedState::new());
        let mut hl = HotLoop::new(shared.clone(), None, None);
        hl.farm.disconnected = true;
        hl.ccp.disconnected = false;
        hl.answer_farm_reconnect_for_test(1, Err(refused()));
        hl.poll_farm_reconnect_for_test();
        assert!(shared.reference.session_over().is_none(), "the session stands");
        assert!(shared.reference.trading_over().is_none(), "and trading is not ended");
        assert!(hl.farm_halted.is_some(), "the feed alone is given up on");

        // And with nothing else carrying the session, the same answer ends it.
        let alone = Arc::new(SharedState::new());
        let mut only_farm = HotLoop::new(alone.clone(), None, None);
        only_farm.farm.disconnected = true;
        only_farm.ccp.disconnected = true;
        only_farm.answer_farm_reconnect_for_test(1, Err(refused()));
        only_farm.poll_farm_reconnect_for_test();
        assert!(alone.reference.session_over().is_some(), "with the trading connection down too, it ends");
    }

    /// A recovery budget that runs out is recorded as that, not as a stop the
    /// caller asked for: the caller was handed "the client asked to stop" and
    /// went looking for the call in its own code that did it, and the reading
    /// that says whether a loss was deliberate said it was.
    #[test]
    fn a_recovery_budget_that_runs_out_is_not_a_stop_the_caller_asked_for() {
        let shared = Arc::new(SharedState::new());
        let mut hl = HotLoop::new(shared.clone(), None, None);
        hl.ccp.disconnected = true;
        hl.report_recovery_exhausted("ccp");
        assert_eq!(shared.reference.session_over(), Some("the recovery limits the caller set are spent"));
        assert!(!shared.connection_lost_by_design(), "nobody asked for this");
    }

    /// Giving recovery up waits on the trading connection's worker, whose
    /// landed session must be logged out, and on no other: the market-data
    /// worker past its last cancel check is inside a logon exchange, and
    /// waiting on it held the healthy trading connection unread and
    /// unheartbeated for the length of a dial.
    #[test]
    fn giving_up_does_not_wait_on_a_data_farm_still_dialling() {
        let mut hl = HotLoop::new(Arc::new(SharedState::new()), None, None);
        hl.own_reconnect_worker(false, std::thread::Builder::new().spawn(|| {
            std::thread::sleep(std::time::Duration::from_secs(2));
        }).ok());
        let started = std::time::Instant::now();
        hl.halt_recovery(retry::DisconnectReason::Transport);
        assert!(started.elapsed() < std::time::Duration::from_secs(1), "the data farm's dial is not waited on");
    }

    /// Giving recovery up does not wait on the trading connection's handshake
    /// either.
    ///
    /// The guard that raises this halt fires only while a trading handshake is
    /// in flight — that is what it is for: a caller who bounded recovery to
    /// thirty seconds should not wait out an attempt that outlasts it. Waited
    /// out here, the wait is on the thread that polls every other transport,
    /// answers the venue's own test requests and sends every heartbeat, so
    /// raising the halt cost exactly what the halt exists to avoid, and
    /// charged it to the connections that were still healthy.
    #[test]
    fn giving_up_does_not_wait_on_a_trading_handshake_still_dialling() {
        let mut hl = HotLoop::new(Arc::new(SharedState::new()), None, None);
        hl.own_reconnect_worker(true, std::thread::Builder::new().spawn(|| {
            std::thread::sleep(std::time::Duration::from_secs(2));
        }).ok());
        let started = std::time::Instant::now();
        hl.halt_recovery(retry::DisconnectReason::AuthorizationFailed);
        assert!(
            started.elapsed() < std::time::Duration::from_secs(1),
            "the trading dial is waited out on the loop's own thread",
        );
    }

    /// A trading session that lands after recovery was given up is told it is
    /// going, rather than dropped in silence.
    ///
    /// The halt no longer waits for the worker, so the session arrives on a
    /// later lap. The venue holds one session per account and has to time out
    /// one that just goes, so the next connection would race a session it
    /// still believes is live.
    #[test]
    fn a_trading_session_landing_after_the_halt_is_told_it_is_going() {
        let mut hl = HotLoop::new(Arc::new(SharedState::new()), None, None);
        let (conn, mut peer) = crate::protocol::connection::Connection::for_test();
        let (worker, rx) = std::sync::mpsc::sync_channel(1);
        hl.pending_ccp_reconnect = Some(rx);

        hl.halt_recovery(retry::DisconnectReason::AuthorizationFailed);
        assert!(
            hl.pending_ccp_reconnect.is_some(),
            "the attempt is left somewhere to land",
        );

        // The attempt lands, the way it does on its own thread.
        worker.send(Ok(conn)).unwrap();
        hl.poll_ccp_reconnect();

        use std::io::Read;
        let mut buf = [0u8; 4096];
        let n = peer.read(&mut buf).unwrap();
        let said = String::from_utf8_lossy(&buf[..n]).replace('\u{1}', "|");
        assert!(said.contains("35=5|"), "the session is told it is going: {said}");
        assert!(hl.ccp_conn.is_none(), "and nothing is installed on a session that is over");
    }

    /// The calendar connection's loss and recovery are announced as the other
    /// two data connections' are, under the venue's own numbers for it.
    /// Announced nowhere, a caller watching for them got them for the other
    /// two farms and nothing for this one either way.
    #[test]
    fn the_calendar_connections_loss_and_recovery_are_announced() {
        let shared = Arc::new(SharedState::new());
        let (events, heard) = std::sync::mpsc::sync_channel(8);
        let mut hl = HotLoop::new(shared, Some(EventSink::new(events, Default::default())), None);
        let (conn, _peer) = crate::protocol::connection::Connection::for_test();
        hl.secdef_conn = Some(conn);
        hl.secdef.give_up(&mut hl.secdef_conn, &hl.shared, &hl.event_tx);
        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        let (conn, _peer) = crate::protocol::connection::Connection::for_test();
        tx.send(Ok(conn)).unwrap();
        hl.pending_secdef_reconnect = Some(rx);
        hl.poll_secdef_reconnect();
        let said: Vec<bool> = heard.try_iter().filter_map(|e| match e {
            Event::VenueData { which: crate::bridge::VenueDataConnection::SecurityDefinition, up } => Some(up),
            _ => None,
        }).collect();
        assert_eq!(said, [false, true], "lost, then back");
    }

    /// Recovery given up is said once, however many attempts the giving-up
    /// takes back.
    ///
    /// Cancelling the attempt in flight brings it home as a failure of its
    /// own, and its poller reads that as one more thing ending the session.
    /// Said twice, the Rust surface latches its own notice and shows one while
    /// the Python surface, which folds only the events a single dispatch
    /// drains, announces connectivity lost a second time.
    #[test]
    fn recovery_given_up_is_said_once_however_many_attempts_it_takes_back() {
        let shared = Arc::new(SharedState::new());
        let (tx, heard) = std::sync::mpsc::sync_channel(64);
        let mut hl = HotLoop::new(
            shared.clone(), Some(EventSink::new(tx, Default::default())), None,
        );
        hl.ccp.disconnected = true;

        // The limits run out with the attempt still dialling.
        hl.report_recovery_exhausted("ccp");
        // And that attempt comes home carrying the cancellation it was given.
        let (worker, rx) = std::sync::mpsc::sync_channel(1);
        worker.send(Err(io::Error::new(
            io::ErrorKind::Interrupted, "CCP reconnect cancelled by the client",
        ))).unwrap();
        hl.pending_ccp_reconnect = Some(rx);
        hl.poll_ccp_reconnect();

        let losses = heard.try_iter().filter(|e| matches!(e, Event::Disconnected)).count();
        assert_eq!(losses, 1, "one loss, and one notice of it");
    }

    /// The market-data feed's own reconnect failing is not a session loss.
    ///
    /// The drop already told the caller the feed is broken under 2103. A retry
    /// that also fails is that feed's own connection, not the trading socket
    /// 1100 speaks for, and raising a session 1100 for it left a caller reading
    /// a healthy trading session as gone, with no 1102 ever to follow.
    #[test]
    fn a_farm_only_reconnect_failure_is_not_a_session_loss() {
        let shared = Arc::new(SharedState::new());
        let (tx, heard) = std::sync::mpsc::sync_channel(64);
        let mut hl = HotLoop::new(
            shared.clone(), Some(EventSink::new(tx, Default::default())), None,
        );
        // The feed alone is down, its retries failing; the trading connection
        // never dropped, so no session loss stands.
        hl.farm.disconnected = true;
        hl.farm_reconnect_attempt = 3;
        let (worker, rx) = std::sync::mpsc::sync_channel::<io::Result<Connection>>(1);
        drop(worker);
        hl.pending_farm_reconnect = Some(rx);

        hl.poll_farm_reconnect();

        assert!(
            heard.try_recv().is_err(),
            "the feed's own retry failing is not a session Disconnected",
        );
        assert!(
            !shared.take_connection_lost(),
            "and not a session 1100: it speaks for the trading socket alone",
        );
    }

    /// A reconnect worker that never answered is a spent attempt like any
    /// other.
    ///
    /// The machine can refuse a thread, and a worker can stop before it sends.
    /// Its receiver comes back disconnected, which was cleared and nothing
    /// else: no rung was scheduled, so the next pass dialled at once and every
    /// pass after it, and the notice that the connection is gone lived only on
    /// the path where the venue answered. However often this repeated, both
    /// surfaces went on reporting the session as connected.
    #[test]
    fn a_reconnect_worker_that_never_answered_is_a_spent_attempt() {
        let shared = Arc::new(SharedState::new());
        let (tx, heard) = std::sync::mpsc::sync_channel(64);
        let mut hl = HotLoop::new(
            shared.clone(), Some(EventSink::new(tx, Default::default())), None,
        );
        hl.ccp.disconnected = true;
        // The third attempt, which is the one the loss is announced on.
        hl.ccp_reconnect_attempt = 3;
        let (worker, rx) = std::sync::mpsc::sync_channel::<io::Result<Connection>>(1);
        drop(worker);
        hl.pending_ccp_reconnect = Some(rx);

        hl.poll_ccp_reconnect();

        assert!(
            hl.ccp_next_attempt_at.is_some(),
            "the next attempt is on the ladder rather than at once, and at once again",
        );
        assert!(
            matches!(heard.try_recv(), Ok(Event::Disconnected)),
            "and the caller is told the connection is gone",
        );
        assert!(shared.take_connection_lost(), "and a caller without an event channel too");

        // And the quote feed's own, which failed the same way. Both transports
        // answer to the one notice, so the loss already standing is not said
        // again -- a handler that reconnected on the first would take the
        // second into the session it had just built.
        hl.farm.disconnected = true;
        hl.farm_reconnect_attempt = 3;
        let (worker, rx) = std::sync::mpsc::sync_channel::<io::Result<Connection>>(1);
        drop(worker);
        hl.pending_farm_reconnect = Some(rx);

        hl.poll_farm_reconnect();

        assert!(
            hl.farm_next_attempt_at.is_some(),
            "the next attempt is on the ladder rather than at once, and at once again",
        );
        assert!(
            heard.try_recv().is_err(),
            "one outage, one notice",
        );
        assert!(!shared.take_connection_lost(), "and the same on the other channel");
    }

    /// A reconnect worker that has finished is let go of, rather than held to
    /// the end of the session.
    ///
    /// A thread nobody joins holds its stack and its handle until that handle
    /// goes, and recovery is unbounded unless the caller bounds it: a process
    /// reconnecting all day accumulated one per attempt and released none of
    /// them until it stopped.
    #[test]
    fn a_reconnect_worker_that_has_finished_is_not_held_to_the_end_of_the_session() {
        let mut hl = HotLoop::new(Arc::new(SharedState::new()), None, None);
        for attempt in 1..=5 {
            hl.own_reconnect_worker(true, std::thread::Builder::new().spawn(|| {}).ok());
            // A rung apart, each attempt is over before the next one starts.
            while !hl.reconnect_workers.last().unwrap().1.is_finished() {
                std::thread::yield_now();
            }
            assert!(
                hl.reconnect_workers.len() <= 1,
                "attempt {attempt} left {} finished threads unreaped",
                hl.reconnect_workers.len(),
            );
        }
    }

    /// A transport that is down and cannot be rebuilt is not a transport that
    /// is recovering. Retrying it quietly leaves the caller on a dead trading
    /// connection with nothing to tell them so.
    #[test]
    fn an_unrecoverable_transport_is_reported_rather_than_retried_in_silence() {
        let shared = Arc::new(SharedState::new());
        let (tx, rx) = std::sync::mpsc::sync_channel(4096);
        let mut hl = HotLoop::new(shared.clone(), Some(EventSink::new(tx, Default::default())), None);
        hl.ccp.disconnected = true;
        // No credentials were ever cached, so there is nothing to reconnect with.

        hl.spawn_ccp_reconnect();

        assert!(
            matches!(rx.try_recv(), Ok(Event::Disconnected)),
            "the caller is told the connection is gone",
        );
        assert!(shared.take_connection_lost(), "and a caller without an event channel too");
        assert!(hl.reconnect_halted.is_some(), "and nothing keeps retrying what cannot work");
        // And every request made from here on is answered at once instead of
        // waiting out a timeout apiece for a session that has ended.
        assert!(
            shared.reference.session_over().is_some(),
            "a caller that asks for data is told the session is over, not left to time out",
        );
    }

    /// An order accepted while the trading connection was down is buffered,
    /// and its caller was told it was accepted and given an id. Once the
    /// connection's recovery has been given up there is nothing left that can
    /// carry it, so it is refused then rather than left in a buffer nothing
    /// will read again.
    ///
    /// An attempt still dialling is not a reason to hold it. A halt is never
    /// lifted and what it leaves running is dropped where it lands, so held
    /// for that attempt an order waited on a connection that would be closed
    /// rather than installed — for as long as the dial took, which is well
    /// past the bound the caller put on the whole of recovery.
    #[test]
    fn buffered_orders_are_refused_when_recovery_is_given_up() {
        let shared = Arc::new(SharedState::new());
        let mut hl = HotLoop::new(shared.clone(), None, None);
        hl.ccp.disconnected = true;
        hl.context.pending_orders.push(crate::types::OrderRequest::SubmitEx {
            order_id: 31,
            instrument: 0, con_id: 0,
            side: crate::types::Side::Buy,
            qty: 100,
            kind: crate::types::OrderKind::Limit { price: 100_000_000 },
            tif: 0,
            attrs: crate::types::OrderAttrs::default(),
        });

        hl.reconnect_halted = Some(retry::DisconnectReason::ByDesign);
        hl.pending_ccp_reconnect = Some(sync_channel::<io::Result<Connection>>(1).1);
        hl.end_the_trading_connection_once_recovery_is_over();

        let refused = shared.orders.drain_order_inactive();
        assert_eq!(refused.len(), 1, "the order is told about, not dropped");
        assert_eq!(refused[0].0, 31, "under the id its caller was given");
        assert!(
            refused[0].2.contains("never placed"),
            "and told it was never placed: {}", refused[0].2,
        );
        assert_eq!(
            shared.reference.trading_over(),
            Some(retry::DisconnectReason::ByDesign.as_str()),
            "and the connection they travel on is over, so the next order is refused \
             where it is given rather than buffered behind this one",
        );
    }

    /// A login the server refused is refused the same way next time. Climbing
    /// a ladder against that answer forever is noise on someone else's server
    /// and silence to the caller, who is the only one who can fix it.
    #[test]
    fn a_refused_login_stops_the_reconnect_rather_than_looping() {
        let mut hl = HotLoop::new(Arc::new(SharedState::new()), None, None);
        hl.farm.disconnected = true;
        hl.reconnect_halted = Some(retry::DisconnectReason::AuthorizationFailed);

        hl.maybe_spawn_farm_reconnect();
        assert!(hl.farm_next_attempt_at.is_none(), "nothing is scheduled against a settled answer");
        assert!(hl.pending_farm_reconnect.is_none(), "and no attempt is in flight");

        // Whatever the server objected to may have been changed since.
        hl.reconnect_halted = None;
        hl.maybe_spawn_farm_reconnect();
        assert!(hl.farm_next_attempt_at.is_some(), "an ordinary loss still retries");
    }

    /// A client that stood down on the loss needs the other edge to come back
    /// up. Without it an overnight outage the engine recovered from on its own
    /// still ended the session, from the caller's side.
    #[test]
    fn a_recovered_loss_is_announced_once_and_only_after_a_loss() {
        let shared = Arc::new(SharedState::new());
        let mut hl = HotLoop::new(shared.clone(), None, None);

        // Reconnects the client was never told about stay quiet.
        hl.announce_reconnected();
        assert!(!shared.take_connection_restored(), "nothing was announced to recover from");

        // One transport back is not the connection back.
        hl.loss_announced = true;
        hl.ccp.disconnected = true;
        hl.announce_reconnected();
        assert!(!shared.take_connection_restored(), "the other transport is still down");

        hl.ccp.disconnected = false;
        hl.announce_reconnected();
        assert!(shared.take_connection_restored(), "the recovery reaches the client");

        hl.announce_reconnected();
        assert!(!shared.take_connection_restored(), "and reaches it once");
    }

    /// A second market-data-farm outage is announced too, not only the first.
    ///
    /// The drop tells the caller the feed is broken under 2103; a retry that
    /// fails is that feed's own connection, not the trading socket 1100 speaks
    /// for, so it is not raised as a session loss. Every drop announces, so a
    /// second outage after a recovery reaches the caller as well.
    #[test]
    fn an_outage_after_a_recovery_is_announced_as_well() {
        let shared = Arc::new(SharedState::new());
        let mut hl = HotLoop::new(shared.clone(), None, None);
        let mut context = Context::new();
        let is_feed_broken =
            |ns: &[(crate::bridge::VenueDataConnection, bool)]| {
                ns.iter().any(|(w, up)| {
                    matches!(w, crate::bridge::VenueDataConnection::MarketData) && !up
                })
            };

        // The farm drops: 2103, and not a session 1100.
        hl.farm.handle_disconnect(&mut hl.farm_conn, &mut context, &None, &shared);
        let first = shared.drain_venue_data_notices();
        assert!(is_feed_broken(&first), "the drop is announced under 2103: {first:?}");
        assert!(
            !shared.take_connection_lost(),
            "the market-data feed going is not the trading session ending",
        );

        // It comes back (its notice consumed), then drops again: announced
        // again, not swallowed after the first.
        hl.farm.handle_disconnect(&mut hl.farm_conn, &mut context, &None, &shared);
        let second = shared.drain_venue_data_notices();
        assert!(is_feed_broken(&second), "the second outage reaches the caller too: {second:?}");
    }

    /// The servers go down for maintenance most nights and come back on their
    /// own. Stopping after six attempts — about two and a half minutes on the
    /// ladder — left historical data dead until someone restarted the process,
    /// which is the failure the gateway has when its restart token is unset.
    #[test]
    fn hmds_keeps_retrying_through_an_outage_longer_than_the_ladder() {
        let mut hl = HotLoop::new(Arc::new(SharedState::new()), None, None);
        hl.set_reconnect_auth(crate::gateway::ReconnectAuth {
            account_id: String::new(),
            trading_port: None,
            hmds_port: None,
            secdef_port: None,
            logged_in_at: String::new(),
            alternate_hosts: Vec::new(),
            settings: Default::default(),
            host: "gw.example".into(),
            username: "u".into(),
            password: zeroize::Zeroizing::new(String::new()),
            paper: true,
            code_provider: None,
            ib_key_timeout_secs: crate::auth::session::IB_KEY_DEFAULT_TIMEOUT_SECS,
            ib_key_token_sub_type: crate::auth::session::IB_KEY_DEFAULT_TOKEN_SUB_TYPE.into(),
            session_key: Default::default(),
            session_token: Default::default(),
            server_session_id: String::new(),
            hw_info: String::new(),
            encoded: String::new(),
            hmds_host: "hmds.example".into(),
            hmds_farm: "hfarm".into(),
            trading_host: "trade.example".into(),
            trading_farm: "tfarm".into(),
            secdef_host: String::new(),
            secdef_farm: String::new(),
        });

        // Well past the old cap, with the socket still down.
        hl.hmds_reconnect_attempt = 60;
        hl.hmds_next_attempt_at = None;
        hl.maybe_spawn_hmds_reconnect();

        let due = hl.hmds_next_attempt_at.expect("another attempt is armed");
        assert!(
            due.saturating_duration_since(Instant::now()) <= Duration::from_secs(82),
            "and the ladder stays capped rather than drifting out to nothing",
        );
    }

    /// A reconnect the venue stamped nothing on still moves the logon time on.
    ///
    /// A reconnect is allowed to carry no stamp. Kept only where one arrived,
    /// the time held stayed at the first connect's while the venue recorded
    /// this session later — so the next reconnect, told of this client's own
    /// logon not yet reaped, read a time later than the one it held, called it
    /// another client, gave the account up and reported a competing login
    /// against a competitor that does not exist.
    #[test]
    fn a_reconnect_the_venue_stamped_nothing_on_still_moves_the_logon_on() {
        let mut hl = HotLoop::new(Arc::new(SharedState::new()), None, None);
        hl.set_reconnect_auth(crate::gateway::ReconnectAuth {
            account_id: String::new(),
            trading_port: None,
            hmds_port: None,
            secdef_port: None,
            logged_in_at: "20260101-00:00:00".into(),
            alternate_hosts: Vec::new(),
            settings: Default::default(),
            host: "gw.example".into(),
            username: "u".into(),
            password: zeroize::Zeroizing::new(String::new()),
            paper: true,
            code_provider: None,
            ib_key_timeout_secs: crate::auth::session::IB_KEY_DEFAULT_TIMEOUT_SECS,
            ib_key_token_sub_type: crate::auth::session::IB_KEY_DEFAULT_TOKEN_SUB_TYPE.into(),
            session_key: Default::default(),
            session_token: Default::default(),
            server_session_id: String::new(),
            hw_info: String::new(),
            encoded: String::new(),
            hmds_host: "hmds.example".into(),
            hmds_farm: "hfarm".into(),
            trading_host: "trade.example".into(),
            trading_farm: "tfarm".into(),
            secdef_host: String::new(),
            secdef_farm: String::new(),
        });

        // A connection the venue answered without stamping.
        let (conn, _peer) = crate::protocol::connection::Connection::for_test();
        assert!(conn.logged_in_at.is_none(), "the venue stamped nothing on it");
        hl.reconnect_ccp(conn);

        let held = &hl.reconnect_auth.as_ref().expect("the session reconnects").logged_in_at;
        assert_ne!(
            held, "20260101-00:00:00",
            "the logon this session is compared against is the one it opened with, \
             so its own later logon reads as somebody else's",
        );
        assert!(!held.is_empty(), "and an empty one gives the account up to everybody");
    }

    /// The limits the caller set bound a farm the session can live without,
    /// too.
    ///
    /// The historical and calendar farms read the session's budget but never
    /// spent it, so with the rest of the session healthy there was nothing in
    /// it to spend: a farm answering nothing was dialled again on every rung
    /// for the life of the process, whatever the caller allowed. Each spends
    /// one of its own, and running out ends that farm rather than the session
    /// — an unreachable calendar taking the trading connection's attempts
    /// would have the next drop of that connection given up on at once.
    #[test]
    fn an_optional_farm_spends_a_budget_of_its_own() {
        let shared = Arc::new(SharedState::new());
        let mut hl = HotLoop::new(shared.clone(), None, None);
        hl.set_reconnect_config(
            crate::reliability::ReconnectConfig::default().with_max_attempts(2),
        );
        // A port on this machine that nothing is listening on: the attempt is
        // refused where it starts, without reaching anything.
        let closed = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = closed.local_addr().unwrap().port();
        drop(closed);
        hl.set_reconnect_auth(crate::gateway::ReconnectAuth {
            account_id: String::new(),
            trading_port: None,
            hmds_port: Some(port),
            secdef_port: None,
            logged_in_at: String::new(),
            alternate_hosts: Vec::new(),
            settings: Default::default(),
            host: "gw.example".into(),
            username: "u".into(),
            password: zeroize::Zeroizing::new(String::new()),
            paper: true,
            code_provider: None,
            ib_key_timeout_secs: crate::auth::session::IB_KEY_DEFAULT_TIMEOUT_SECS,
            ib_key_token_sub_type: crate::auth::session::IB_KEY_DEFAULT_TOKEN_SUB_TYPE.into(),
            session_key: Default::default(),
            session_token: Default::default(),
            server_session_id: String::new(),
            hw_info: String::new(),
            encoded: String::new(),
            hmds_host: "127.0.0.1".into(),
            hmds_farm: "hfarm".into(),
            trading_host: "trade.example".into(),
            trading_farm: "tfarm".into(),
            secdef_host: String::new(),
            secdef_farm: String::new(),
        });

        // Both the attempts the caller allowed, one rung apart.
        for _ in 0..2 {
            hl.hmds_next_attempt_at = Some(Instant::now());
            hl.maybe_spawn_hmds_reconnect();
            while hl.pending_hmds_reconnect.is_some() {
                hl.poll_hmds_reconnect();
                std::thread::yield_now();
            }
        }
        assert_eq!(hl.hmds_budget.attempts(), 2, "each attempt is spent out of this farm's own");
        assert_eq!(hl.budget.attempts(), 0, "and none of it out of the session's");

        hl.hmds_next_attempt_at = Some(Instant::now());
        hl.maybe_spawn_hmds_reconnect();

        assert_eq!(
            hl.hmds_halted, Some(retry::DisconnectReason::ByDesign),
            "the farm is finished once the limits the caller set are spent on it",
        );
        assert!(hl.pending_hmds_reconnect.is_none(), "and it is not dialled again");
        assert!(
            hl.reconnect_halted.is_none() && shared.reference.session_over().is_none(),
            "while the session it is not the whole of goes on",
        );
    }

    /// A farm the venue has finally refused is not dialled once a minute for
    /// the rest of the session.
    ///
    /// The ladder rescheduled on any failure whatever, without reading which
    /// one: credentials the venue would not take, or an operation it does not
    /// serve, were carried back to it through a full handshake per rung, and
    /// the answer had been final the first time. The session goes on without
    /// this farm — it carries historical data, not the account — so the halt
    /// is this farm's and not the session's.
    #[test]
    fn a_farm_the_venue_finally_refused_is_not_dialled_again() {
        let mut hl = HotLoop::new(Arc::new(SharedState::new()), None, None);
        hl.set_reconnect_auth(crate::gateway::ReconnectAuth {
            account_id: String::new(),
            trading_port: None,
            hmds_port: None,
            secdef_port: None,
            logged_in_at: String::new(),
            alternate_hosts: Vec::new(),
            settings: Default::default(),
            host: "gw.example".into(),
            username: "u".into(),
            password: zeroize::Zeroizing::new(String::new()),
            paper: true,
            code_provider: None,
            ib_key_timeout_secs: crate::auth::session::IB_KEY_DEFAULT_TIMEOUT_SECS,
            ib_key_token_sub_type: crate::auth::session::IB_KEY_DEFAULT_TOKEN_SUB_TYPE.into(),
            session_key: Default::default(),
            session_token: Default::default(),
            server_session_id: String::new(),
            hw_info: String::new(),
            encoded: String::new(),
            hmds_host: "hmds.example".into(),
            hmds_farm: "hfarm".into(),
            trading_host: "trade.example".into(),
            trading_farm: "tfarm".into(),
            secdef_host: String::new(),
            secdef_farm: String::new(),
        });

        // The venue's answer to the attempt that has just come back.
        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        tx.send(Err(io::Error::new(
            io::ErrorKind::PermissionDenied, "the account may not have this farm",
        ))).unwrap();
        hl.pending_hmds_reconnect = Some(rx);
        hl.hmds_next_attempt_at = None;

        hl.poll_hmds_reconnect();

        assert!(hl.hmds_halted.is_some(), "the farm's final answer was not recorded");
        assert!(
            hl.hmds_next_attempt_at.is_none(),
            "another rung was put on the ladder for an answer that cannot change",
        );

        // And nothing arms it again while that answer stands.
        hl.maybe_spawn_hmds_reconnect();
        assert!(
            hl.hmds_next_attempt_at.is_none(),
            "the ladder was rearmed for a farm the venue has finished with",
        );

        // The session itself is untouched: this farm carries historical data,
        // and a session trades without it.
        assert!(hl.reconnect_halted.is_none(), "one farm's refusal ended the session");
    }

    /// A connection that lands after the session ended is not put in place.
    ///
    /// The transports recover side by side, and one can be told something
    /// final — the account taken over, the credentials refused — while
    /// another is midway through a handshake that then succeeds. Installed,
    /// that one is a live socket, resubscribed and announced to the caller as
    /// data back, on a session the caller was already told was over.
    #[test]
    fn a_reconnect_that_lands_after_the_session_ended_is_not_installed() {
        let shared = Arc::new(SharedState::new());
        let (events, heard) = std::sync::mpsc::sync_channel(8);
        let mut hl = HotLoop::new(
            shared, Some(EventSink::new(events, Default::default())), None,
        );
        hl.farm.disconnected = true;
        // What the trading connection was told while the farm was still
        // dialling.
        hl.reconnect_halted = Some(retry::DisconnectReason::TakenOver);

        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        let (conn, _peer) = crate::protocol::connection::Connection::for_test();
        tx.send(Ok(conn)).unwrap();
        hl.pending_farm_reconnect = Some(rx);

        hl.poll_farm_reconnect();

        assert!(hl.farm_conn.is_none(), "a socket was opened on a session that is over");
        assert!(hl.farm.disconnected, "and the farm was marked as carrying again");
        assert!(hl.pending_farm_reconnect.is_none(), "the attempt is finished with");
        let said: Vec<Event> = heard.try_iter().collect();
        assert!(
            !said.iter().any(|e| matches!(e, Event::VenueData { up: true, .. })),
            "the caller was told data was back on a session that had ended: {said:?}",
        );
    }
    use super::*;
    use std::sync::Arc;
    use crate::bridge::{Event, SharedState};
    use crate::types::*;
    use std::time::Duration;

    /// A loop built around a fresh connection starts owing the naming of
    /// what the account has working, and the bound that naming is waited on
    /// is spent from the moment the connection was taken. Left for the
    /// first waiter to start, a first request that waited the bound out
    /// spent it, and a global cancel issued straight after waited nothing
    /// and said nothing.
    #[test]
    fn the_replay_bound_starts_when_the_connection_is_taken() {
        let shared = Arc::new(SharedState::new());
        let (farm_conn, _farm_peer) = crate::protocol::connection::Connection::for_test();
        let (ccp_conn, _ccp_peer) = crate::protocol::connection::Connection::for_test();
        let (_hl, _tx) = HotLoop::with_connections(
            shared.clone(), None, "DU1".into(), farm_conn, ccp_conn, None, None,
        );
        // The bound passes with nobody asking.
        std::thread::sleep(Duration::from_millis(3_200));
        let started = Instant::now();
        assert!(!shared.orders.wait_for_replay(), "nothing has been named yet");
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "the bound began when the connection was taken, so a first waiter arriving \
             after it has passed is answered at once, not held for a fresh bound",
        );
    }

    /// A session learns how far the venue's clock is from this machine's from
    /// the stamp on its logon, so a caller asking the server time before any
    /// other message has arrived is answered from the venue's clock and not
    /// from this machine's raw one.
    #[test]
    fn the_logon_stamp_states_the_venues_clock() {
        let shared = Arc::new(SharedState::new());
        let (farm_conn, _farm_peer) = crate::protocol::connection::Connection::for_test();
        let (mut ccp_conn, _ccp_peer) = crate::protocol::connection::Connection::for_test();
        ccp_conn.logged_in_at = Some("20260904-09:30:00".to_string());
        let (_hl, _tx) = HotLoop::with_connections(
            shared.clone(), None, "DU1".into(), farm_conn, ccp_conn, None, None,
        );
        assert!(
            (shared.market.venue_time_millis() - 1_788_514_200_000).abs() < 2_000,
            "the clock a caller asks for is the one the logon stamp states",
        );
    }

    /// A registration naming no symbol keeps the one the slot has.
    ///
    /// Orders, positions and fills all register by contract id with no symbol.
    /// Written unconditionally, that blanked the name of a slot already
    /// registered under one, and the order built from it went out with an
    /// empty symbol tag.
    #[test]
    fn a_registration_naming_no_symbol_keeps_the_one_the_slot_has() {
        let mut hl = HotLoop::new(Arc::new(SharedState::new()), None, None);
        let id = hl
            .register_or_reject(756733, "SPY".into(), "STK", "SMART", "", "", &None)
            .expect("registered under its name");
        assert_eq!(
            hl.register_or_reject(756733, String::new(), "", "", "", "", &None),
            Some(id),
            "the same contract is the same slot",
        );
        assert_eq!(
            hl.context.market.symbol(id),
            "SPY",
            "a registration by id alone blanked the name tag 55 reads",
        );
    }

    /// A lookup naming a contract another slot holds follows that slot.
    ///
    /// One caller names a contract by its id, another by its symbol, and the
    /// lookup resolves the second to the first's contract. Subscribed anyway,
    /// the venue answers with the number it is already streaming under, which
    /// lands on the first slot and freezes it — and withdrawing the second
    /// gives that number up, so the first caller's stream is dropped for good.
    #[test]
    fn a_lookup_naming_a_held_contract_follows_its_slot() {
        let shared = Arc::new(SharedState::new());
        let mut hl = HotLoop::new(shared.clone(), None, None);
        let by_id = hl.context.market.register(756733);
        let by_name = hl
            .context
            .market
            .try_register_contract(0, "SPY", "STK", "SMART", "")
            .expect("a slot for the contract named by symbol");
        assert_ne!(by_id, by_name, "two callers, two slots, one contract");

        hl.ccp.resolved_md_subscribe.push((
            756733,
            crate::engine::hot_loop::ccp::PendingSubscribe {
                issued: 0,
                filters: Default::default(),
                instrument: by_name,
                con_id: 0,
                symbol: "SPY".into(),
                exchange: "SMART".into(),
                sec_type: "STK".into(),
                currency: "USD".into(),
                mode_9887: 0,
                regulatory_snapshot: false,
            },
        ));
        hl.send_resolved_subscriptions();

        assert_eq!(
            shared.market.drain_subscription_moves(),
            vec![(by_name, by_id, 0)],
            "the caller given the second slot is told to read the first",
        );
    }

    /// And the series it named follow it there.
    ///
    /// What a caller asks for beyond the quote is recorded against the slot it
    /// was given, and a lookup naming a contract another slot holds gives that
    /// slot back. Left behind, the caller was published as watching the
    /// contract's own slot while the series it asked for went back with the
    /// slot it left: nothing asked the venue for them, and the rebuild after a
    /// reconnect did not either.
    #[test]
    fn the_series_a_moved_caller_named_follow_it_to_the_slot_it_moves_to() {
        let shared = Arc::new(SharedState::new());
        let mut hl = HotLoop::new(shared.clone(), None, None);
        let by_id = hl.context.market.register(756733);
        let by_name = hl
            .context
            .market
            .try_register_contract(0, "SPY", "STK", "SMART", "")
            .expect("a slot for the contract named by symbol");

        // The caller named by symbol asked for a series, recorded against the
        // slot it was given.
        hl.farm.asked_generic_ticks.insert(by_name, vec![236]);
        hl.ccp.resolved_md_subscribe.push((
            756733,
            crate::engine::hot_loop::ccp::PendingSubscribe {
                issued: 0,
                filters: Default::default(),
                instrument: by_name,
                con_id: 0,
                symbol: "SPY".into(),
                exchange: "SMART".into(),
                sec_type: "STK".into(),
                currency: "USD".into(),
                mode_9887: 0,
                regulatory_snapshot: false,
            },
        ));
        hl.send_resolved_subscriptions();

        assert_eq!(
            hl.farm.asked_generic_ticks.get(&by_id).map(Vec::as_slice),
            Some([236u32].as_slice()),
            "the subscription that goes out on the contract's own slot asks for it",
        );
        assert!(
            !hl.farm.asked_generic_ticks.contains_key(&by_name),
            "and nothing is left on the slot that was given back",
        );
    }

    /// Asking for news on a contract does not decide where its orders go.
    ///
    /// The command carries no exchange, and the security type was passed in
    /// that slot. Recorded as where the contract trades, it came back out as
    /// the destination on tags 100 and 6210 of every order, replace and
    /// bracket leg on that contract — routed to a venue of that name, which is
    /// no venue.
    #[test]
    fn asking_for_news_does_not_route_orders_to_the_security_type() {
        let mut hl = HotLoop::new(Arc::new(SharedState::new()), None, None);
        let (tx, rx) = std::sync::mpsc::sync_channel(4);
        hl.set_control_rx(rx);
        tx.send(ControlCommand::SubscribeNews {
            con_id: 265598,
            symbol: "AAPL".into(),
            sec_type: "STK".into(),
            providers: String::new(),
            reply_tx: None,
        })
        .expect("the engine is holding the other end");
        hl.poll_control_commands();

        let id = hl
            .context
            .market
            .instrument_by_con_id(265598)
            .expect("the subscription registers the contract");
        let (sec_type, destination) = hl.context.market.order_routing(id);
        assert_eq!(sec_type, "CS", "the security type belongs in its own slot");
        assert_eq!(
            destination, "BEST",
            "an unnamed venue is the smart one, and a security type is not a venue",
        );
    }

    /// A contract id wider than a request carries it in is refused, on the
    /// live path and the historical one alike.
    ///
    /// Both state the id in 32 bits. Narrowed to fit, `2^32 + 756733` is
    /// `756733` — a listed contract of its own — so the venue answers with
    /// that contract's quotes and that contract's bars under this caller's
    /// request, and nothing says the caller is reading another instrument.
    #[test]
    fn a_contract_numbered_past_the_wire_is_refused_rather_than_narrowed() {
        const PAST_THE_WIRE: i64 = (1i64 << 32) + 756_733;
        let shared = Arc::new(SharedState::new());
        let mut hl = HotLoop::new(shared.clone(), None, None);
        let (tx, rx) = std::sync::mpsc::sync_channel(4);
        hl.set_control_rx(rx);

        let (reply_tx, reply_rx) = std::sync::mpsc::sync_channel(1);
        tx.send(ControlCommand::Subscribe {
            filters: Default::default(),
            contract: ContractRef {
                con_id: PAST_THE_WIRE,
                sec_type: "STK".into(),
                exchange: "SMART".into(),
                ..Default::default()
            },
            mode_9887: 0,
            regulatory_snapshot: false,
            generic_ticks: Vec::new(),
            reply_tx: Some(reply_tx),
            issued: 0,
        })
        .expect("the engine holds the other end");
        tx.send(ControlCommand::FetchHistorical {
            req_id: 91,
            contract: ContractRef {
                con_id: PAST_THE_WIRE,
                sec_type: "STK".into(),
                exchange: "SMART".into(),
                ..Default::default()
            },
            end_date_time: String::new(),
            duration: "1 D".into(),
            bar_size: "1 min".into(),
            what_to_show: "TRADES".into(),
            use_rth: true,
            keep_up_to_date: false,
            include_expired: false,
            filters: Default::default(),
        })
        .expect("the engine holds the other end");
        hl.poll_control_commands();

        assert!(
            hl.context.market.instrument_by_con_id(756_733).is_none()
                && hl.context.market.instrument_by_con_id(PAST_THE_WIRE).is_none(),
            "neither the id asked for nor the one it narrows to took a slot",
        );
        let refused = reply_rx.try_recv().expect("the subscriber is answered");
        assert!(
            refused.is_err_and(|why| why.message.contains(&PAST_THE_WIRE.to_string())),
            "and is told which contract could not be asked about",
        );
        let (_, why) = shared.reference.take_error_for(91)
            .expect("the historical request is answered too");
        assert!(
            why.contains(&PAST_THE_WIRE.to_string()),
            "and names the same contract: {why}",
        );
    }

    /// A command batched behind the stop is not carried out.
    ///
    /// One caller disconnecting while another subscribes puts both in the same
    /// batch. The stop withdraws every stream and refuses everything queued,
    /// and then the loop went on to the next command — so a subscription sent
    /// from there is live at the venue with no engine left to withdraw it.
    #[test]
    fn nothing_batched_behind_the_stop_is_carried_out() {
        let shared = Arc::new(SharedState::new());
        let mut hl = HotLoop::new(shared.clone(), None, None);
        let (tx, rx) = std::sync::mpsc::sync_channel(4);
        hl.set_control_rx(rx);

        tx.send(ControlCommand::Shutdown).expect("the engine holds the other end");
        tx.send(ControlCommand::SubscribeNews {
            con_id: 265598,
            symbol: "AAPL".into(),
            sec_type: "STK".into(),
            providers: String::new(),
            reply_tx: None,
        })
        .expect("the engine holds the other end");
        hl.poll_control_commands();

        assert!(
            hl.context.market.instrument_by_con_id(265598).is_none(),
            "the subscription behind the stop registered nothing",
        );
    }

    /// A contract specified the ordinary ibapi way — symbol, secType,
    /// exchange, no conId — arrives here with conId 0. Keyed on that alone,
    /// every such contract resolves to whichever one registered first: quotes
    /// land in one slot and an order built from it goes out under the first
    /// contract's symbol.
    /// Two conId-less options on one underlying differ only by strike and right,
    /// which the descriptor did not carry: both landed in one slot, so the put's
    /// quotes and its minTick overwrote the call's — and minTick is what snaps an
    /// order's price.
    #[test]
    fn two_conid_less_options_on_one_underlying_do_not_share_a_slot() {
        let mut hl = HotLoop::new(Arc::new(SharedState::new()), None, None);
        let call = hl.register_or_reject(
            0, "AAPL".into(), "OPT", "SMART", "20260619|230|C|100", "", &None).expect("call");
        let put = hl.register_or_reject(
            0, "AAPL".into(), "OPT", "SMART", "20260619|240|P|100", "", &None).expect("put");
        assert_ne!(call, put, "the call and the put must not resolve to one slot");

        // The same option still resolves to its own slot rather than a third.
        assert_eq!(
            hl.register_or_reject(0, "AAPL".into(), "OPT", "SMART", "20260619|230|C|100", "", &None),
            Some(call), "the same contract keeps its slot",
        );
    }

    /// A slot allocated before any identity was stated — which is what the
    /// pre-flight registration does — must be adopted by the first caller that
    /// states one, not stranded while a second slot is allocated for the same
    /// contract.
    #[test]
    fn a_slot_with_no_identity_is_adopted_by_the_first_that_states_one() {
        let mut hl = HotLoop::new(Arc::new(SharedState::new()), None, None);
        let pre = hl.register_or_reject(0, "AAPL".into(), "OPT", "SMART", "", "", &None).expect("pre");
        assert_eq!(
            hl.register_or_reject(0, "AAPL".into(), "OPT", "SMART", "20260619|230|C|100", "", &None),
            Some(pre), "the identity-less slot is adopted, not stranded",
        );
        // And once adopted it belongs to that contract alone.
        assert_ne!(
            hl.register_or_reject(0, "AAPL".into(), "OPT", "SMART", "20260619|240|P|100", "", &None),
            Some(pre), "a different contract does not inherit it",
        );
    }

    #[test]
    fn con_id_less_contracts_do_not_share_one_slot() {
        let mut hl = HotLoop::new(Arc::new(SharedState::new()), None, None);
        let aapl = hl.register_or_reject(0, "AAPL".into(), "STK", "SMART", "", "", &None).expect("AAPL");
        let qqq = hl.register_or_reject(0, "QQQ".into(), "STK", "SMART", "", "", &None).expect("QQQ");

        assert_ne!(aapl, qqq, "two symbols must not resolve to one instrument");
        assert_eq!(hl.context.market.symbol(aapl), "AAPL");
        assert_eq!(hl.context.market.symbol(qqq), "QQQ", "an order on this slot names QQQ");

        // The same contract again is the same slot, or every re-registration
        // burns another one.
        assert_eq!(hl.register_or_reject(0, "AAPL".into(), "STK", "SMART", "", "", &None), Some(aapl));
        // Tick-by-tick and news register with neither secType nor exchange,
        // and must land on the slot the L1 subscription already has.
        assert_eq!(hl.register_or_reject(0, "QQQ".into(), "", "", "", "", &None), Some(qqq));
    }

    // F64::from_str accepts "nan"/"inf", so a not-available sentinel
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

    /// Absent and zero are different answers: `38=0` on a cancel states a
    /// quantity, an absent 38 states none, and a caller that cannot tell them
    /// apart falls back where it should not.
    #[test]
    fn parse_qty_tag_separates_absent_from_zero_and_holds_fractions() {
        let s = |v: &str| v.to_string();
        assert_eq!(parse_qty_tag(None), None, "absent is not a quantity");
        assert_eq!(parse_qty_tag(Some(&s("0"))), Some(0), "zero is");
        assert_eq!(parse_qty_tag(Some(&s("nan"))), None);
        assert_eq!(parse_qty_tag(Some(&s("inf"))), None);
        assert_eq!(parse_qty_tag(Some(&s("n/a"))), None);
        assert_eq!(parse_qty_tag(Some(&s("100"))), Some(100 * QTY_SCALE));
        assert_eq!(parse_qty_tag(Some(&s(" 5 "))), Some(5 * QTY_SCALE), "padding is trimmed");
        assert_eq!(parse_qty_tag(Some(&s("0.5"))), Some(QTY_SCALE / 2));
        assert_eq!(parse_qty_tag(Some(&s("-1.5"))), Some(-3 * QTY_SCALE / 2), "a bust is signed");
        // Rounded, not truncated: a tenth is not exact in binary, and
        // truncation puts three tenths one hundred-millionth low.
        assert_eq!(parse_qty_tag(Some(&s("0.3"))), Some(3 * QTY_SCALE / 10));
        assert_eq!(parse_qty_tag(Some(&s("0.00000001"))), Some(1), "the finest the scale holds");
    }

    #[test]
    fn inject_tick_emits_events() {
        let shared = Arc::new(SharedState::new());
        let (event_tx, event_rx) = std::sync::mpsc::sync_channel(4096);
        let mut engine = HotLoop::new(shared.clone(), Some(EventSink::new(event_tx, Default::default())), None);
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
        let (event_tx, event_rx) = std::sync::mpsc::sync_channel(4096);
        let mut engine = HotLoop::new(shared.clone(), Some(EventSink::new(event_tx, Default::default())), None);
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
        let (event_tx, _event_rx) = std::sync::mpsc::sync_channel(4096);
        let mut engine = HotLoop::new(shared.clone(), Some(EventSink::new(event_tx, Default::default())), None);
        engine.context_mut().market.register(265598);

        let fill = Fill {
            instrument: 0,
            order_id: 1001,
            side: Side::Buy,
            price: 150_00000000,
            qty: 100 * QTY_SCALE,
            remaining: 0,
            timestamp_ns: 0,
            cum_qty: 100 * QTY_SCALE, avg_price: 150_00000000,
        };
        engine.inject_fill(&fill);
        assert_eq!(engine.context_mut().position(0), 100.0);
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
        let (tx, rx) = std::sync::mpsc::sync_channel(1);
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
        let (event_tx, event_rx) = std::sync::mpsc::sync_channel(4096);
        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        let mut engine = HotLoop::new(shared, Some(EventSink::new(event_tx, Default::default())), None);
        engine.set_control_rx(rx);
        engine.running = true;

        // Drop sender — simulates EClient being dropped without disconnect.
        drop(tx);

        engine.poll_once();
        assert!(!engine.is_running(), "hot loop should stop when control channel disconnects");

        // The session ended because the side that owned it went away, which
        // is a stop and not a loss: nothing is coming back, and a consumer
        // told it lost connectivity would stand by for a reconnect.
        let events: Vec<Event> = event_rx.try_iter().collect();
        assert!(events.iter().any(|e| matches!(e, Event::Stopped)));
    }

    #[test]
    fn shutdown_sets_connection_lost_flag_without_event_channel() {
        // The flag path must work with no event channel attached,
        // which is the default for the Rust client.
        let shared = Arc::new(SharedState::new());
        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        let mut engine = HotLoop::new(shared.clone(), None, None);
        engine.set_control_rx(rx);
        engine.running = true;
        tx.send(ControlCommand::Shutdown).unwrap();
        engine.poll_once();

        assert!(shared.take_connection_lost(), "shutdown must signal connection lost");
        assert!(!shared.take_connection_lost(), "flag must clear after being read");
    }

    #[test]
    fn channel_disconnect_sets_connection_lost_flag() {
        let shared = Arc::new(SharedState::new());
        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        let mut engine = HotLoop::new(shared.clone(), None, None);
        engine.set_control_rx(rx);
        engine.running = true;
        drop(tx);
        engine.poll_once();

        assert!(shared.take_connection_lost());
    }

    /// A fully disconnected engine parks rather than running the loop flat
    /// out. Every poll returns immediately with nothing, so an unparked loop is
    /// bounded only by clock speed and holds one core at full occupancy.
    ///
    /// Counted in iterations rather than CPU time, because that is what tells a
    /// parked loop from a spinning one on any machine. A free spin manages
    /// millions in this window; parked it is one per millisecond.
    #[test]
    fn a_loop_with_every_transport_down_does_not_spin() {
        let shared = Arc::new(SharedState::new());
        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        let mut engine = HotLoop::new(shared, None, None);
        engine.set_control_rx(rx);
        engine.farm.disconnected = true;
        engine.ccp.disconnected = true;
        engine.hmds.disconnected = true;

        let handle = std::thread::spawn(move || {
            engine.run();
            engine.context.loop_iterations
        });
        std::thread::sleep(std::time::Duration::from_millis(60));
        tx.send(ControlCommand::Shutdown).unwrap();
        let iterations = handle.join().expect("the loop must exit on Shutdown");

        assert!(
            iterations < 10_000,
            "a parked loop runs on the order of 60 iterations in 60ms; got {iterations}",
        );
    }

    #[test]
    fn clone_for_event_skips_the_copy_when_no_channel() {
    // With no listener the deep copy must not happen at all.
        let payload = vec![1u8, 2, 3];
        assert!(clone_for_event(&None, &payload).is_none());

        let (tx, _rx) = std::sync::mpsc::sync_channel::<Event>(1);
        assert_eq!(clone_for_event(&Some(EventSink::new(tx, Default::default())), &payload), Some(payload));
    }

    #[test]
    fn run_exits_on_shutdown() {
        let shared = Arc::new(SharedState::new());
        let (tx, rx) = std::sync::mpsc::sync_channel(1);
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
        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        let mut engine = HotLoop::new(shared, None, None);
        engine.set_control_rx(rx);

        // Drop sender — run() should detect disconnect and exit.
        drop(tx);

        engine.run();
        assert!(!engine.is_running());
    }

    /// A request with no connection to send it on is answered under the
    /// number for that, not the data service's own.
    ///
    /// The service reported no difficulty with a request it never saw. A
    /// caller branching on the service's number retried against a service it
    /// had not asked, which is every request made inside a reconnect window.
    #[test]
    fn push_hmds_unavailable_historical_emits_error_and_terminal_sentinel() {
        let shared = SharedState::new();
        push_hmds_unavailable(&shared, 7, true);

        let errors = shared.reference.drain_historical_errors();
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].0, 7);
        assert_eq!(errors[0].1, 504, "the request never left, so nothing answered it");
        assert!(errors[0].2.contains("not available"));

        let hist = shared.reference.drain_historical_data();
        assert_eq!(hist.len(), 1, "terminal sentinel required so historical_data_end fires");
        assert_eq!(hist[0].0, 7);
        assert!(hist[0].1.is_complete);
        assert!(hist[0].1.bars.is_empty());
    }

    /// A price between -1 and 0 has a whole part of 0, so the sign lives only
    /// in the fraction. Writing the integer alone turns a credit into a debit —
    /// the same number with the opposite meaning, and nothing rejects it.
    #[test]
    fn format_price_keeps_the_sign_below_one() {
        let p = |v: f64| format_price((v * PRICE_SCALE as f64) as Price).to_string();
        assert_eq!(p(-0.35), "-0.35");
        assert_eq!(p(-0.5), "-0.5");
        assert_eq!(p(-0.01), "-0.01");
        // Either side of the boundary, and the cases that already worked.
        assert_eq!(p(-1.5), "-1.5");
        assert_eq!(p(-1.0), "-1");
        assert_eq!(p(0.0), "0");
        assert_eq!(p(0.35), "0.35");
        assert_eq!(p(1.5), "1.5");
        assert_eq!(p(737.53), "737.53");

        // Raw fixed-point units, so the boundary is pinned at the smallest
        // representable tick rather than only at prices a float can express.
        // A guard that fires slightly too late still formats these positive.
        let raw = |v: Price| format_price(v).to_string();
        assert_eq!(raw(-1), "-0.00000001", "the smallest negative tick keeps its sign");
        assert_eq!(raw(-PRICE_SCALE + 1), "-0.99999999", "just inside the interval");
        assert_eq!(raw(-PRICE_SCALE), "-1", "the boundary itself");
        assert_eq!(raw(-PRICE_SCALE - 1), "-1.00000001", "just outside it");
        assert_eq!(raw(0), "0");
        assert_eq!(raw(1), "0.00000001");
    }

    /// A decimal price a caller states reaches the wire as that price.
    ///
    /// The whole path, because each half is right on its own and the fault was
    /// in the join: converting to fixed point and then writing the field. A
    /// double holding a decimal sits a hair below it about as often as above,
    /// so a conversion that drops the remainder writes `0.28999999` for a
    /// caller who asked for `0.29` — off the cent grid the instrument trades
    /// on, and not the price that was asked for.
    ///
    /// Every cent from 0.01 to 99.99, because better than five in a hundred of
    /// them land on that side and picking a few by hand is how this was missed:
    /// the one place that carried a decimal to the venue asked for 1.00, which
    /// converts exactly and so could never show it.
    #[test]
    fn a_decimal_price_reaches_the_wire_as_the_price_stated() {
        for cents in 1..10_000u32 {
            let stated = f64::from(cents) / 100.0;
            let on_the_wire = format_price(crate::types::price_from_f64(stated)).to_string();
            let expected = if cents % 100 == 0 {
                format!("{}", cents / 100)
            } else if cents % 10 == 0 {
                format!("{}.{}", cents / 100, (cents % 100) / 10)
            } else {
                format!("{}.{:02}", cents / 100, cents % 100)
            };
            assert_eq!(on_the_wire, expected, "{stated} did not reach the wire as itself");
        }
    }

    // The CCP/farm ladder — floor 2s, ladder 0/5/15/30/50/60s,
    // jitter range growing 5s -> 20s, ceiling 82s.
    #[test]
    fn reconnect_backoff_ladder_bounds() {
        use std::time::Duration;
        let expect = |failures: u32, lo: u64, hi: u64| {
            for _ in 0..50 {
                let d = reconnect_backoff(failures);
                assert!(d >= Duration::from_millis(lo) && d < Duration::from_millis(hi),
                    "failures={failures} got {d:?}, expected [{lo}ms, {hi}ms)");
            }
        };
        expect(0, 2_000, 7_000);    // 2s + 0 + jitter(0..5s)
        expect(1, 7_000, 17_000);   // 2s + 5s + jitter(0..10s)
        expect(2, 17_000, 32_000);  // 2s + 15s + jitter(0..15s)
        expect(3, 32_000, 52_000);  // 2s + 30s + jitter(0..20s)
        expect(5, 62_000, 82_001);  // 2s + 60s + jitter(0..20s), capped 82s
        expect(50, 62_000, 82_001); // ladder index capped
    }

    // Liveness ladder: heartbeat at 10s, test request at 15s, declare dead at
    // 35s, suspend for 60s after a stall. Ordering is a `const _` assert.
    #[test]
    fn liveness_thresholds_ordered() {
        assert_eq!(LIVENESS_TEST_SECS, 15);
        assert_eq!(LIVENESS_DEAD_SECS, 35);
        assert_eq!(LIVENESS_STALL_SECS, 10);
        assert_eq!(LIVENESS_STALL_COOLDOWN_SECS, 60);
    }

    /// Every reconnect this client makes climbs the one ladder, and each of the
    /// observed historical-data retries lands inside the rung it belongs to.
    ///
    /// Those retries were read as a doubling schedule of their own and given
    /// one — five samples fitted with a curve. They are the same ladder every
    /// other transport climbs: each sample falls inside the window its rung
    /// opens, which a schedule of 3/6/12/24/48 does not explain and this does.
    #[test]
    fn the_observed_retries_land_in_the_rung_they_belong_to() {
        const OBSERVED_SECS: [f64; 5] = [3.2, 11.4, 18.5, 42.7, 63.7];
        for (rung, observed) in OBSERVED_SECS.iter().enumerate() {
            // The rung is a floor plus a step, and the wait is somewhere in it.
            let floor = std::time::Duration::from_millis(2_000 + LADDER_MS[rung]);
            let ceiling = floor + std::time::Duration::from_millis(jitter_span(rung as u32));
            let waited = std::time::Duration::from_secs_f64(*observed);
            assert!(
                waited >= floor && waited < ceiling,
                "rung {rung}: {observed}s is outside {floor:?}..{ceiling:?}",
            );
            // And what this client waits is inside the same window.
            for _ in 0..64 {
                let ours = reconnect_backoff(rung as u32);
                assert!(ours >= floor && ours < ceiling, "rung {rung}: {ours:?}");
            }
        }
    }

    #[test]
    fn push_hmds_unavailable_non_historical_emits_error_without_sentinel() {
        let shared = SharedState::new();
        push_hmds_unavailable(&shared, 42, false);

        let errors = shared.reference.drain_historical_errors();
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].0, 42);
        assert_eq!(errors[0].1, 504, "the request never left, so nothing answered it");
        // Head-ts / histogram / ticks / schedule / scanner / news / fundamental:
        // no bar-stream consumer waiting for historical_data_end.
        assert!(shared.reference.drain_historical_data().is_empty());
    }

    /// The tags a `35=Q` ack bound are dead the moment the subscription is
    /// cancelled, but `try_reclaim_instrument` was the only path that dropped
    /// them and it returns early while an open order, a tick-by-tick or a news
    /// subscription pins the slot. Every subscribe/unsubscribe cycle on a
    /// pinned instrument then left one behind until the next farm drop.
    #[test]
    fn an_l1_unsubscribe_hands_back_its_tags_on_a_pinned_instrument() {
        let mut hl = HotLoop::new(Arc::new(SharedState::new()), None, None);
        let (tx, rx) = sync_channel(4);
        hl.set_control_rx(rx);

        let id = hl.context.market.register(4001);
        hl.context.market.register_server_tag(910_001, id);
        hl.farm.instrument_md_reqs.push((id, crate::engine::hot_loop::farm::MdReqRecord {
            con_id: 4001,
            sec_type: "CS".into(),
            mode_9887: 0,
            entries: vec![crate::engine::hot_loop::farm::MdReqEntry { req_id: 7, request_type: 442, venue: "BEST".into() }],
        }));
        hl.hmds.tbt_subscriptions.push(crate::engine::hot_loop::hmds::TbtSubscription {
            ignore_size: false,
            instrument: id,
            query_id: "AAPL".to_string(),
            kind: TbtType::AllLast,
            caller_req_id: 0,
            venue_id: 0,
            min_tick: 0,
            size_tick: 0.0,
            running: Default::default(),
        });

        tx.send(ControlCommand::Unsubscribe { instrument: id, con_id: 0, took_it: 0, series: Vec::new(), issued: 0 }).unwrap();
        hl.poll_once();

        assert_eq!(
            hl.context.market.instrument_by_server_tag(910_001), None,
            "the cancelled subscription's tag is handed back, not held to the next farm drop",
        );
        assert_eq!(
            hl.context.market.con_id(id), Some(4001),
            "and the tick-by-tick subscription still pins the slot itself",
        );
    }

    /// A slot that cannot be handed back is named after whoever holds it now,
    /// not after the caller that first took it.
    ///
    /// The occupancy number is what tells a withdrawal of a dead subscription
    /// from one of the live subscription on the same slot, and a slot is
    /// reusable, so it has to name the occupancy that is there. It was written
    /// once and never replaced: on a slot the engine can hand back, the number
    /// goes with the slot and the next caller gets a fresh one — but a slot
    /// pinned by a holding, a working order, a tick-by-tick stream or news is
    /// never handed back, so it kept the number of a caller that had already
    /// gone.
    ///
    /// Every withdrawal after the first then named a number the engine did not
    /// hold, was read as belonging to some other occupancy, and was refused
    /// with the subscription left standing. `cancel_mkt_data` returned
    /// success, the venue went on streaming, and nothing could ever take it
    /// down again.
    #[test]
    fn a_pinned_slot_is_named_after_the_subscription_now_on_it() {
        let mut hl = HotLoop::new(Arc::new(SharedState::new()), None, None);
        let (tx, rx) = sync_channel(8);
        hl.set_control_rx(rx);
        let id = hl.context.market.register(4004);
        // What pins it: a tick-by-tick stream, which outlives the quote.
        hl.hmds.tbt_subscriptions.push(crate::engine::hot_loop::hmds::TbtSubscription {
            ignore_size: false,
            instrument: id,
            query_id: "tbt_1".to_string(),
            kind: TbtType::AllLast,
            caller_req_id: 0,
            venue_id: 0,
            min_tick: 0,
            size_tick: 0.0,
            running: Default::default(),
        });

        // The first caller takes the slot under its own number and gives it up.
        hl.farm.note_it_changed_hands(id, 1);
        hl.farm.instrument_md_reqs.push((id, crate::engine::hot_loop::farm::MdReqRecord {
            con_id: 4004,
            sec_type: "CS".into(),
            mode_9887: 0,
            entries: vec![crate::engine::hot_loop::farm::MdReqEntry {
                req_id: 7, request_type: 442, venue: "BEST".into(),
            }],
        }));
        tx.send(ControlCommand::Unsubscribe {
            instrument: id, con_id: 4004, took_it: 1, series: Vec::new(), issued: 2,
        }).unwrap();
        hl.poll_once();
        assert!(!hl.farm.holds_market_data(id), "the first subscription went");

        // The next caller takes the same slot, which was never handed back.
        tx.send(ControlCommand::Subscribe {
            contract: crate::types::ContractRef {
                con_id: 4004, symbol: "AAPL".into(), sec_type: "STK".into(),
                exchange: "SMART".into(), ..Default::default()
            },
            filters: Default::default(),
            mode_9887: 0,
            regulatory_snapshot: false,
            generic_ticks: Vec::new(),
            issued: 3,
            reply_tx: None,
        }).unwrap();
        hl.poll_once();

        assert_eq!(
            hl.farm.what_took_it(id), 3,
            "the slot names the occupancy now on it, not the one that has gone",
        );
    }

    /// A trade stream asked for twice under one number is refused.
    ///
    /// The venue answers two streams on one contract and kind with one number
    /// of its own, so both records name it: every tick is handed to the caller
    /// twice, and the withdrawal removes one record, finds the other still
    /// naming that number, and returns without telling the venue anything —
    /// the ticks go on arriving under a stream the caller has withdrawn, for
    /// the rest of the session. The bar stream beside this already insists on
    /// one per number.
    #[test]
    fn a_second_trade_stream_under_one_number_is_refused() {
        let mut hl = HotLoop::new(Arc::new(SharedState::new()), None, None);
        let (tx, rx) = sync_channel(4);
        hl.set_control_rx(rx);
        let (conn, _peer) = crate::protocol::connection::Connection::for_test();
        hl.hmds_conn = Some(conn);
        hl.hmds.tbt_subscriptions.push(crate::engine::hot_loop::hmds::TbtSubscription {
            ignore_size: false,
            instrument: 0,
            query_id: "tbt_1".to_string(),
            kind: TbtType::AllLast,
            caller_req_id: 5,
            venue_id: 0,
            min_tick: 0,
            size_tick: 0.0,
            running: Default::default(),
        });

        tx.send(ControlCommand::SubscribeTbt {
            contract: crate::types::ContractRef {
                con_id: 265_598, symbol: "AAPL".into(), sec_type: "STK".into(),
                exchange: "SMART".into(), ..Default::default()
            },
            req_id: 5,
            tbt_type: TbtType::AllLast,
            number_of_ticks: 0,
            ignore_size: false,
            reply_tx: None,
        }).unwrap();
        hl.poll_once();

        assert_eq!(
            hl.hmds.tbt_subscriptions.len(), 1,
            "the second stream under that number is not taken",
        );
    }

    /// A withdrawal that leaves the subscription standing leaves its tags
    /// standing too.
    ///
    /// Four withdrawals do not go: the slot has been given to another contract,
    /// its callers were sent elsewhere, a caller is on its way onto it, or the
    /// subscription now on it began after the withdrawal was decided. On each
    /// of those the subscription stays up and the venue goes on sending — and
    /// the tags were handed back anyway, so every record that followed named a
    /// number no contract in this session held and was dropped with one line in
    /// the log.
    ///
    /// The caller that joined that subscription was told it was watching, and
    /// received nothing at all for the rest of the session: the slot is never
    /// reclaimed while a subscription stands on it, so nothing ever asks the
    /// venue again and nothing ever binds the number back.
    #[test]
    fn a_withdrawal_that_does_not_go_leaves_the_tags_it_routes_on() {
        let mut hl = HotLoop::new(Arc::new(SharedState::new()), None, None);
        let (tx, rx) = sync_channel(4);
        hl.set_control_rx(rx);

        let id = hl.context.market.register(4003);
        hl.context.market.register_server_tag(910_003, id);
        hl.farm.instrument_md_reqs.push((id, crate::engine::hot_loop::farm::MdReqRecord {
            con_id: 4003,
            sec_type: "CS".into(),
            mode_9887: 0,
            entries: vec![crate::engine::hot_loop::farm::MdReqEntry {
                req_id: 7, request_type: 442, venue: "BEST".into(),
            }],
        }));

        // The subscription now on the slot was asked for after this withdrawal
        // was decided — a caller asked for the same contract in between and was
        // answered off the subscription that is up.
        hl.farm.note_subscription_asked_on(id, 10);
        tx.send(ControlCommand::Unsubscribe {
            instrument: id, con_id: 4003, took_it: 0, series: Vec::new(), issued: 5,
        }).unwrap();
        hl.poll_once();

        assert!(
            hl.farm.holds_market_data(id),
            "the withdrawal did not go, which is what this case is about",
        );
        assert_eq!(
            hl.context.market.instrument_by_server_tag(910_003), Some(id),
            "so the number the venue is still sending on still names the contract",
        );
    }

    /// The map is not L1-only: `35=L` ticker setup registers into it too and
    /// news resolves against it, so an unsubscribe that clears an instrument's
    /// tags while its news subscription is live ends the news feed silently.    /// The map is not L1-only: `35=L` ticker setup registers into it too and
    /// news resolves against it, so an unsubscribe that clears an instrument's
    /// tags while its news subscription is live ends the news feed silently.
    #[test]
    fn an_l1_unsubscribe_keeps_the_tags_a_live_news_subscription_routes_on() {
        let mut hl = HotLoop::new(Arc::new(SharedState::new()), None, None);
        let (tx, rx) = sync_channel(4);
        hl.set_control_rx(rx);

        let id = hl.context.market.register(4002);
        hl.context.market.register_server_tag(910_002, id);
        hl.farm.instrument_md_reqs.push((id, crate::engine::hot_loop::farm::MdReqRecord {
            con_id: 4002,
            sec_type: "CS".into(),
            mode_9887: 0,
            entries: vec![crate::engine::hot_loop::farm::MdReqEntry { req_id: 8, request_type: 442, venue: "BEST".into() }],
        }));
        hl.farm.news_subscriptions.push((id, 55, "BRFG".to_string(), 756733, "STK".to_string()));

        tx.send(ControlCommand::Unsubscribe { instrument: id, con_id: 0, took_it: 0, series: Vec::new(), issued: 0 }).unwrap();
        hl.poll_once();

        assert_eq!(
            hl.context.market.instrument_by_server_tag(910_002), Some(id),
            "news routes through this map, so its tag outlives the L1 request",
        );
    }

    /// One news record framed the way the venue frames a generic tick: a
    /// two-byte bit length, the tag, a two-byte payload length, the payload.
    fn news_frame(server_tag: u32) -> Vec<u8> {
        let mut article = Vec::new();
        article.extend_from_slice(&1u32.to_be_bytes());
        article.extend_from_slice(&4u32.to_be_bytes());
        article.extend_from_slice(b"BRFG");
        article.extend_from_slice(&0u32.to_be_bytes());
        article.extend_from_slice(&2u16.to_be_bytes());
        article.extend_from_slice(b"id");
        article.extend_from_slice(&0u32.to_be_bytes());
        article.extend_from_slice(&1_785_325_554u32.to_be_bytes());
        article.extend_from_slice(&8u32.to_be_bytes());
        article.extend_from_slice(b"headline");
        let mut body = server_tag.to_be_bytes().to_vec();
        body.extend_from_slice(&(article.len() as u16).to_be_bytes());
        body.extend_from_slice(&article);
        let mut msg = b"35=G\x01".to_vec();
        msg.extend_from_slice(&(((body.len() * 8) % 65_536) as u16).to_be_bytes());
        msg.extend_from_slice(&body);
        msg
    }

    /// A news subscription's headlines reach the caller. The venue numbers a
    /// generic tick apart from the prices and says what it carries once, on
    /// the acknowledgement, and the reader tells a frame's bytes apart only by
    /// what was asked for under the tag. Asking recorded nothing on the
    /// market-data side, so the acknowledgement filed no tag and every
    /// headline was dropped as a tick nothing had asked for.
    #[test]
    fn a_news_subscription_delivers_its_headlines() {
        let shared = Arc::new(SharedState::new());
        let mut hl = HotLoop::new(shared.clone(), None, None);
        let (tx, rx) = std::sync::mpsc::sync_channel(4);
        hl.set_control_rx(rx);
        tx.send(ControlCommand::SubscribeNews {
            con_id: 265598,
            symbol: "AAPL".into(),
            sec_type: "STK".into(),
            providers: String::new(),
            reply_tx: None,
        })
        .expect("the engine is holding the other end");
        hl.poll_control_commands();
        let req_id = hl.farm.next_md_req_id - 1;
        let id = hl.context.market.instrument_by_con_id(265598).expect("registered");

        // The venue takes the subscription on under a number of its own, then
        // sends a headline under that number.
        let ack = format!("35=Q\x0133082,{req_id},0.01,0,3");
        hl.farm.process_farm_message(ack.as_bytes(), &mut None, &mut hl.context, &shared, &None, &mut hl.hb);
        hl.farm.process_farm_message(&news_frame(33082), &mut None, &mut hl.context, &shared, &None, &mut hl.hb);
        let delivered = shared.market.drain_tick_news();
        assert_eq!(delivered.len(), 1, "the headline reaches the caller: {delivered:?}");
        assert_eq!(delivered[0].instrument, id, "on the contract it was asked for");

        // Withdrawn, the tag goes with it: a headline after is nobody's.
        tx.send(ControlCommand::UnsubscribeNews { subject: NewsSubject::Slot(id) }).unwrap();
        hl.poll_control_commands();
        hl.farm.process_farm_message(&news_frame(33082), &mut None, &mut hl.context, &shared, &None, &mut hl.hb);
        assert!(shared.market.drain_tick_news().is_empty(), "nothing after the withdrawal");
    }

    /// A second historical-news request under a number still answering is
    /// refused, as a second bar query is: two queries under one number answer
    /// twice, and a withdrawal reaches one of them.
    #[test]
    fn a_second_news_request_under_a_live_number_is_refused() {
        let shared = Arc::new(SharedState::new());
        let mut hl = HotLoop::new(shared.clone(), None, None);
        let (tx, rx) = std::sync::mpsc::sync_channel(4);
        hl.set_control_rx(rx);
        let (conn, _peer) = crate::protocol::connection::Connection::for_test();
        hl.hmds_conn = Some(conn);
        for _ in 0..2 {
            tx.send(ControlCommand::FetchHistoricalNews {
                req_id: 7, con_id: 265598, provider_codes: "BRFG".into(),
                start_time: String::new(), end_time: String::new(), max_results: 10,
            })
            .unwrap();
        }
        hl.poll_control_commands();
        assert_eq!(hl.hmds.pending_news.len(), 1, "one query answers under the number");
        let told = shared.reference.drain_historical_errors();
        assert_eq!(told.len(), 1, "the second is refused: {told:?}");
        assert_eq!((told[0].0, told[0].1), (7, 386), "{told:?}");
    }

    /// A calendar withdrawal that throws away an answer already queued acted,
    /// and is not told nothing was waiting. The answer had arrived; refusing
    /// the withdrawal said the request was never made.
    #[test]
    fn withdrawing_a_calendar_request_whose_answer_is_queued_is_not_refused() {
        let shared = Arc::new(SharedState::new());
        let mut hl = HotLoop::new(shared.clone(), None, None);
        let (tx, rx) = std::sync::mpsc::sync_channel(4);
        hl.set_control_rx(rx);
        shared.reference.push_calendar_meta_data(7, "{}".to_string());
        tx.send(ControlCommand::CancelCalendar { req_id: 7 }).unwrap();
        hl.poll_control_commands();
        assert!(shared.reference.drain_calendar_meta_data().is_empty(), "the answer goes with the request");
        let told = shared.reference.drain_historical_errors();
        assert!(told.is_empty(), "and a withdrawal that acted says nothing beside it: {told:?}");
    }

    fn stock(con_id: i64, symbol: &str) -> ContractRef {
        ContractRef {
            con_id, symbol: symbol.into(), exchange: "SMART".into(),
            sec_type: "STK".into(), currency: "USD".into(), ..Default::default()
        }
    }

    /// A second keep-up-to-date request under a live number is refused whole.
    /// Refused for its batch alone, its stream half still went out: two
    /// five-second streams fed one forming bar, doubling its volume, and the
    /// reconnect record kept only the second contract.
    #[test]
    fn a_second_keep_up_to_date_request_under_a_live_number_takes_no_stream() {
        let shared = Arc::new(SharedState::new());
        let mut hl = HotLoop::new(shared.clone(), None, None);
        let (tx, rx) = std::sync::mpsc::sync_channel(4);
        hl.set_control_rx(rx);
        let (conn, _peer) = crate::protocol::connection::Connection::for_test();
        hl.hmds_conn = Some(conn);
        let ask = |con_id: i64, symbol: &str| ControlCommand::FetchHistorical {
            req_id: 7, contract: stock(con_id, symbol),
            end_date_time: String::new(), duration: "1 D".into(), bar_size: "1 min".into(),
            what_to_show: "TRADES".into(), use_rth: true, keep_up_to_date: true,
            include_expired: false, filters: Default::default(),
        };
        tx.send(ask(756733, "SPY")).unwrap();
        tx.send(ask(265598, "AAPL")).unwrap();
        hl.poll_control_commands();
        assert_eq!(
            hl.hmds.rtbar_subs.iter().filter(|(_, rid, ..)| *rid == 7).count(), 1,
            "one stream under the number",
        );
        let resub: Vec<i64> = hl.hmds.rtbar_resub.iter().filter(|r| r.req_id == 7).map(|r| r.con_id).collect();
        assert_eq!(resub, [756733], "and the reconnect record names the first contract");
        let told = shared.reference.drain_historical_errors();
        assert!(told.iter().any(|(rid, code, _)| *rid == 7 && *code == 386), "{told:?}");
    }

    /// A historical withdrawal does not take a live bar stream that happens to
    /// run under the same number: the two are asked for by different calls in
    /// different id spaces. It counted the stream as held, so no refusal was
    /// answered, and then withdrew the stream in silence.
    #[test]
    fn withdrawing_a_historical_query_leaves_a_bar_stream_under_the_same_number() {
        let shared = Arc::new(SharedState::new());
        let mut hl = HotLoop::new(shared.clone(), None, None);
        hl.hmds.rtbar_subs.push(("rt_4".to_string(), 7, Some(99), 0.01, 1.0));
        let (tx, rx) = std::sync::mpsc::sync_channel(4);
        hl.set_control_rx(rx);
        tx.send(ControlCommand::CancelHistorical { req_id: 7 }).unwrap();
        hl.poll_control_commands();
        assert!(hl.hmds.rtbar_subs.iter().any(|(_, rid, ..)| *rid == 7), "the stream is left running");
        let told = shared.reference.drain_historical_errors();
        assert!(
            told.iter().any(|(rid, code, _)| *rid == 7 && *code == 366),
            "no historical query was answering under the number: {told:?}",
        );
    }

    /// A bar-stream withdrawal does not take the stream half of a request
    /// kept up to date: that stream belongs to the historical request, and a
    /// caller withdrawing bars under the number is told no stream is running.
    #[test]
    fn withdrawing_bars_leaves_a_kept_up_to_date_requests_stream() {
        let shared = Arc::new(SharedState::new());
        let mut hl = HotLoop::new(shared.clone(), None, None);
        hl.hmds.keep_up_to_date_reqs.insert(7);
        hl.hmds.rtbar_subs.push(("rt_4".to_string(), 7, Some(99), 0.01, 1.0));
        let (tx, rx) = std::sync::mpsc::sync_channel(4);
        hl.set_control_rx(rx);
        tx.send(ControlCommand::CancelRealTimeBar { req_id: 7 }).unwrap();
        hl.poll_control_commands();
        assert!(hl.hmds.rtbar_subs.iter().any(|(_, rid, ..)| *rid == 7), "the stream half is left running");
        assert!(hl.hmds.keep_up_to_date_reqs.contains(&7), "and the request is still kept up to date");
        let told = shared.reference.drain_historical_errors();
        assert!(
            told.iter().any(|(rid, code, _)| *rid == 7 && *code == 300),
            "no bar stream of its own runs under the number: {told:?}",
        );
    }

    /// A second bar stream under a live number is refused, as a second scan,
    /// tick stream or bar query is: two streams under one number interleave
    /// into one queue, and a withdrawal reaches one of them.
    #[test]
    fn a_second_bar_stream_under_a_live_number_is_refused() {
        let shared = Arc::new(SharedState::new());
        let mut hl = HotLoop::new(shared.clone(), None, None);
        let (tx, rx) = std::sync::mpsc::sync_channel(4);
        hl.set_control_rx(rx);
        let (conn, _peer) = crate::protocol::connection::Connection::for_test();
        hl.hmds_conn = Some(conn);
        for (con_id, symbol) in [(756733, "SPY"), (265598, "AAPL")] {
            tx.send(ControlCommand::SubscribeRealTimeBar {
                req_id: 7, contract: stock(con_id, symbol), what_to_show: "TRADES".into(),
                use_rth: true, filters: Default::default(),
            })
            .unwrap();
        }
        hl.poll_control_commands();
        assert_eq!(hl.hmds.rtbar_subs.iter().filter(|(_, rid, ..)| *rid == 7).count(), 1);
        let told = shared.reference.drain_historical_errors();
        assert!(told.iter().any(|(rid, code, _)| *rid == 7 && *code == 102), "{told:?}");
    }

    /// Withdrawing a request still parked for its contract's name acted, and
    /// is not told nothing was awaited: the parked request was removed, then
    /// refused for not being in the list it had not reached yet.
    #[test]
    fn withdrawing_a_parked_head_timestamp_request_is_not_refused() {
        let shared = Arc::new(SharedState::new());
        let mut hl = HotLoop::new(shared.clone(), None, None);
        hl.ccp.pending_named.push((7, ControlCommand::FetchHeadTimestamp {
            req_id: 7, contract: stock(0, "SPY"), what_to_show: "TRADES".into(),
            use_rth: true, include_expired: false, filters: Default::default(),
        }, std::time::Instant::now()));
        let (tx, rx) = std::sync::mpsc::sync_channel(4);
        hl.set_control_rx(rx);
        tx.send(ControlCommand::CancelHeadTimestamp { req_id: 7 }).unwrap();
        hl.poll_control_commands();
        assert!(hl.ccp.pending_named.is_empty(), "the parked request is withdrawn");
        let told = shared.reference.drain_historical_errors();
        assert!(told.is_empty(), "and a withdrawal that acted says nothing beside it: {told:?}");
    }

    /// A withdrawn news query's queued answer goes with it, as its
    /// neighbours' answers do. Left queued, a withdrawn query was answered
    /// under its number, or under whatever request had reused it since.
    #[test]
    fn withdrawing_a_news_query_takes_its_queued_answer_with_it() {
        let shared = Arc::new(SharedState::new());
        let mut hl = HotLoop::new(shared.clone(), None, None);
        shared.reference.push_historical_news(9, Vec::new(), false);
        let (tx, rx) = std::sync::mpsc::sync_channel(4);
        hl.set_control_rx(rx);
        tx.send(ControlCommand::CancelHistoricalNews { req_id: 9 }).unwrap();
        hl.poll_control_commands();
        assert!(shared.reference.drain_historical_news().is_empty(), "the queued answer went with the withdrawal");
    }

    /// A historical drop takes the scan rows parked behind their naming with
    /// the scans it failed. Left parked, their deadline released them and the
    /// caller was handed rows and an end for a scan it had just been told had
    /// failed.
    #[test]
    fn a_historical_drop_takes_the_parked_scan_rows_with_it() {
        let shared = Arc::new(SharedState::new());
        let mut hl = HotLoop::new(shared.clone(), None, None);
        let (conn, peer) = crate::protocol::connection::Connection::for_test();
        drop(peer);
        hl.hmds_conn = Some(conn);
        hl.ccp.pending_scanner_enrichment.push(crate::engine::hot_loop::ccp::PendingScannerEnrichment {
            api_req_id: 9,
            result: crate::control::scanner::ScannerResult { con_ids: Vec::new(), entries: Vec::new(), scan_time: String::new(), error_text: String::new() },
            awaiting: Default::default(),
            deadline: std::time::Instant::now() + std::time::Duration::from_secs(60),
        });
        hl.poll_historical();
        assert!(hl.hmds_conn.is_none(), "the drop is seen");
        assert!(hl.ccp.pending_scanner_enrichment.is_empty(), "and the parked rows go with the scans");
    }

    /// A contract numbered beyond what a request carries is refused on a
    /// details request as it is on every other request naming one, rather
    /// than sent under the number that survives the narrowing.
    #[test]
    fn a_details_request_for_a_contract_numbered_beyond_the_wire_is_refused() {
        let shared = Arc::new(SharedState::new());
        let mut hl = HotLoop::new(shared.clone(), None, None);
        let (tx, rx) = std::sync::mpsc::sync_channel(4);
        hl.set_control_rx(rx);
        tx.send(ControlCommand::FetchContractDetails {
            req_id: 7, contract: stock(1 << 32, "SPY"), filters: Default::default(),
        })
        .unwrap();
        hl.poll_control_commands();
        assert!(hl.ccp.pending_secdef.is_empty(), "nothing is asked");
        let told = shared.reference.drain_historical_errors();
        assert!(
            told.iter().any(|(rid, code, _)| *rid == 7 && *code == crate::error_codes::Refusal::VALIDATION),
            "{told:?}",
        );
        assert_eq!(shared.reference.drain_contract_details_end(), [7], "and the request is ended");
    }

    /// A news subscription is a market-data request and goes out on the
    /// market-data connection, and is withdrawn there. Sent on the trading
    /// connection, the venue refused it as a message that connection does not
    /// carry, so no headline could ever arrive; and the withdrawal, lacking
    /// the time it was sent, was refused too, and the venue closed the trading
    /// connection behind it.
    #[test]
    fn a_news_subscription_goes_out_on_the_market_data_connection() {
        let shared = Arc::new(SharedState::new());
        let mut hl = HotLoop::new(shared.clone(), None, None);
        let (conn, peer) = crate::protocol::connection::Connection::for_test();
        let mut peer = crate::protocol::connection::Connection::new_raw(peer).unwrap();
        hl.farm_conn = Some(conn);
        let (tx, rx) = std::sync::mpsc::sync_channel(4);
        hl.set_control_rx(rx);
        let (reply_tx, reply_rx) = std::sync::mpsc::sync_channel(1);
        tx.send(ControlCommand::SubscribeNews {
            con_id: 793356217, symbol: "MES".into(), sec_type: "FUT".into(),
            providers: "BRFG".into(), reply_tx: Some(reply_tx),
        })
        .unwrap();
        hl.poll_control_commands();
        let id = reply_rx.try_recv().expect("registered").expect("a slot");
        std::thread::sleep(std::time::Duration::from_millis(50));
        let sent = farm::tests::drain_inner(&mut peer);
        let text = sent.iter().map(|f| String::from_utf8_lossy(f).replace('\u{1}', "|")).collect::<Vec<_>>().join("\n");
        for field in ["|263=1|", "|6008=793356217|", "|207=NEWS|", "|167=FUT|", "|264=292|", "|6472=BRFG|"] {
            assert!(text.contains(field), "the subscription states {field}: {text:?}");
        }
        assert!(
            hl.farm.generic_tick_reqs.iter().any(|(_, tick)| *tick == 292),
            "and its tag is filed for the acknowledgement",
        );

        tx.send(ControlCommand::UnsubscribeNews { subject: NewsSubject::Slot(id) }).unwrap();
        hl.poll_control_commands();
        std::thread::sleep(std::time::Duration::from_millis(50));
        let sent = farm::tests::drain_inner(&mut peer);
        let text = sent.iter().map(|f| String::from_utf8_lossy(f).replace('\u{1}', "|")).collect::<Vec<_>>().join("\n");
        for field in ["|263=2|", "|6008=793356217|", "|207=NEWS|", "|167=FUT|", "|264=292|"] {
            assert!(text.contains(field), "the withdrawal states {field}: {text:?}");
        }
        assert!(!hl.farm.generic_tick_reqs.iter().any(|(_, tick)| *tick == 292), "and the tag is forgotten");
    }

    /// A tick stream asked for while the data connection is down is refused
    /// now, as a scan or a bar stream is. Accepted, the caller held a stream
    /// that would begin only if a reconnect re-sent it, and nothing said so.
    #[test]
    fn a_tick_stream_asked_for_with_no_data_connection_is_refused() {
        let shared = Arc::new(SharedState::new());
        let mut hl = HotLoop::new(shared.clone(), None, None);
        let (tx, rx) = std::sync::mpsc::sync_channel(4);
        hl.set_control_rx(rx);
        let (reply_tx, reply_rx) = std::sync::mpsc::sync_channel(1);
        tx.send(ControlCommand::SubscribeTbt {
            req_id: 7, contract: stock(756733, "SPY"), tbt_type: crate::types::TbtType::AllLast,
            number_of_ticks: 0, ignore_size: false, reply_tx: Some(reply_tx),
        })
        .unwrap();
        hl.poll_control_commands();
        assert!(matches!(reply_rx.try_recv(), Ok(Err(_))), "refused on the channel the caller waits on");
        assert!(hl.hmds.tbt_subscriptions.is_empty(), "and nothing is left to be sent later");
        let told = shared.reference.drain_historical_errors();
        assert!(
            told.iter().any(|(rid, code, _)| *rid == 7 && *code == crate::error_codes::Refusal::NOT_CONNECTED),
            "{told:?}",
        );
    }

    /// A chargeable snapshot is a request of its own, not a share of a stream
    /// somebody else opened.
    ///
    /// A contract already being streamed serves the next caller off the stream
    /// that is up, which is right for a subscription and wrong for the one-shot
    /// the venue bills for: nothing went out, nothing was billed, and the venue
    /// never got the chance to refuse it for want of the entitlement — while
    /// the caller was handed the snapshot's end off ticks it never asked for.
    #[test]
    fn a_chargeable_snapshot_is_asked_for_even_where_the_contract_is_streaming() {
        let mut hl = HotLoop::new(Arc::new(SharedState::new()), None, None);
        let (tx, rx) = std::sync::mpsc::sync_channel(4);
        hl.set_control_rx(rx);
        let contract = || ContractRef {
            con_id: 756733,
            symbol: "SPY".into(),
            exchange: "SMART".into(),
            sec_type: "STK".into(),
            currency: "USD".into(),
            ..Default::default()
        };
        // One caller streams the contract; another asks for the one-shot on it.
        for chargeable in [false, true] {
            tx.send(ControlCommand::Subscribe {
            filters: Default::default(),
                contract: contract(),
                mode_9887: 0,
                regulatory_snapshot: chargeable,
                reply_tx: None,
                generic_ticks: Vec::new(), issued: 0,
            })
            .expect("the engine is holding the other end");
            hl.poll_control_commands();
        }

        let instrument = hl.context.market.instrument_by_con_id(756733)
            .expect("the contract holds a slot");
        let asked: Vec<u32> = hl.farm.instrument_md_reqs.iter()
            .find(|(id, _)| *id == instrument)
            .expect("the contract has requests standing on it")
            .1
            .entries.iter().map(|e| e.request_type).collect();
        // 624 is the venue's request type for the chargeable snapshot.
        assert!(
            asked.contains(&624),
            "the snapshot is asked for under its own request rather than riding \
             the stream: {asked:?}",
        );
    }

    /// A second caller on a contract already streaming has the series it named
    /// asked for, and the first caller's are still asked for after a
    /// reconnect.
    ///
    /// The list that reached the venue is the first caller's. A joiner naming a
    /// series nobody had asked for waited on a stream that was never
    /// requested; and written over the list held against the slot, the first
    /// caller's series were the ones the rebuild stopped asking for.
    #[test]
    fn a_second_caller_on_a_streaming_contract_has_its_series_asked_for() {
        let mut hl = HotLoop::new(Arc::new(SharedState::new()), None, None);
        let (tx, rx) = std::sync::mpsc::sync_channel(4);
        hl.set_control_rx(rx);
        let contract = || ContractRef {
            con_id: 756733,
            symbol: "SPY".into(),
            exchange: "SMART".into(),
            sec_type: "STK".into(),
            currency: "USD".into(),
            ..Default::default()
        };
        for wanted in [vec![233u32], vec![236u32]] {
            tx.send(ControlCommand::Subscribe {
                filters: Default::default(),
                contract: contract(),
                mode_9887: 0,
                regulatory_snapshot: false,
                reply_tx: None,
                generic_ticks: wanted, issued: 0,
            })
            .expect("the engine is holding the other end");
            hl.poll_control_commands();
        }

        let instrument = hl.context.market.instrument_by_con_id(756733)
            .expect("the contract holds a slot");
        let mut held = hl.farm.asked_generic_ticks
            .get(&instrument)
            .cloned()
            .unwrap_or_default();
        held.sort_unstable();
        assert_eq!(
            held, [233u32, 236],
            "both callers' series are what the rebuild asks for: {held:?}",
        );
        let asked: Vec<u32> = hl.farm.instrument_md_reqs.iter()
            .find(|(id, _)| *id == instrument)
            .expect("the contract has requests standing on it")
            .1
            .entries.iter().map(|e| e.request_type).collect();
        for series in [233u32, 236] {
            assert!(
                asked.contains(&series),
                "{series} is an entry of the subscription: {asked:?}",
            );
        }
    }

    /// A lookup whose caller has withdrawn does not open a subscription.
    ///
    /// A contract stated by description is registered before the venue has
    /// named it, so a caller can withdraw while the lookup is still out. The
    /// answer to that lookup opened the stream anyway, after the caller's
    /// record here had gone: nothing was left that could withdraw it, so the
    /// venue served it and held its slot for the rest of the session.
    #[test]
    fn a_lookup_whose_caller_withdrew_does_not_open_a_subscription() {
        let mut hl = HotLoop::new(Arc::new(SharedState::new()), None, None);
        let (tx, rx) = std::sync::mpsc::sync_channel(4);
        hl.set_control_rx(rx);
        let instrument = hl.context.market
            .try_register_contract(0, "SPY", "STK", "SMART", "")
            .expect("a slot for the contract named by symbol");
        hl.ccp.resolved_md_subscribe.push((756733, crate::engine::hot_loop::ccp::PendingSubscribe {
            issued: 0,
            filters: Default::default(),
            instrument,
            con_id: 0,
            symbol: "SPY".into(),
            exchange: "SMART".into(),
            sec_type: "STK".into(),
            currency: "USD".into(),
            mode_9887: 0,
            regulatory_snapshot: false,
        }));

        // The caller gives up before the venue names the contract.
        tx.send(ControlCommand::Unsubscribe {
            instrument,
            con_id: 0,
            took_it: 0,
            series: Vec::new(),
            issued: 1,
        })
        .expect("the engine is holding the other end");
        hl.poll_control_commands();
        hl.send_resolved_subscriptions();

        assert!(
            !hl.farm.instrument_md_reqs.iter().any(|(id, _)| *id == instrument),
            "the answer to a lookup nobody is waiting on opens nothing",
        );
    }

    /// A move installed by the surface changes who can withdraw the slot.
    ///
    /// The subscription on a slot a caller is moving onto is held up until the
    /// move is installed, and the callers that arrive hold it under a number of
    /// their own from then on. Said by a flag the surface clears rather than on
    /// this queue, a withdrawal already decided against the occupancy before
    /// them became valid again simply by arriving late, and it took the
    /// subscription down underneath them.
    #[test]
    fn a_move_installed_changes_who_can_withdraw_the_slot() {
        let mut hl = HotLoop::new(Arc::new(SharedState::new()), None, None);
        let (tx, rx) = std::sync::mpsc::sync_channel(4);
        hl.set_control_rx(rx);
        tx.send(ControlCommand::Subscribe {
            filters: Default::default(),
            contract: ContractRef {
                con_id: 756733,
                symbol: "SPY".into(),
                exchange: "SMART".into(),
                sec_type: "STK".into(),
                currency: "USD".into(),
                ..Default::default()
            },
            mode_9887: 0,
            regulatory_snapshot: false,
            reply_tx: None,
            generic_ticks: Vec::new(),
            issued: 5,
        })
        .expect("the engine is holding the other end");
        hl.poll_control_commands();
        let instrument = hl.context.market.instrument_by_con_id(756733)
            .expect("the contract holds a slot");

        // Callers of another slot are moved onto it and hold it under 9.
        tx.send(ControlCommand::MoveInstalled { from: instrument + 1, into: instrument, took_it: 9 })
            .expect("the engine is holding the other end");
        hl.poll_control_commands();

        // The caller that held it before them cannot take it down.
        tx.send(ControlCommand::Unsubscribe {
            instrument, con_id: 0, took_it: 5, series: Vec::new(), issued: 20,
        })
        .expect("the engine is holding the other end");
        hl.poll_control_commands();
        assert!(
            hl.farm.instrument_md_reqs.iter().any(|(id, _)| *id == instrument),
            "the subscription the arriving callers are being served off stands",
        );

        // And the callers that arrived can.
        tx.send(ControlCommand::Unsubscribe {
            instrument, con_id: 0, took_it: 9, series: Vec::new(), issued: 21,
        })
        .expect("the engine is holding the other end");
        hl.poll_control_commands();
        assert!(
            !hl.farm.instrument_md_reqs.iter().any(|(id, _)| *id == instrument),
            "the callers that hold it can withdraw it",
        );
    }

    /// A series is not asked for on a subscription its caller never joined.
    ///
    /// The caller that asked can be gone by the time the request is read here,
    /// and the slot given to another contract: asked for anyway, the series was
    /// served to a subscription nobody had named it on, and recorded against it
    /// so that a later withdrawal of that series was refused.
    #[test]
    fn a_series_is_not_asked_for_on_a_subscription_its_caller_never_joined() {
        let mut hl = HotLoop::new(Arc::new(SharedState::new()), None, None);
        let (tx, rx) = std::sync::mpsc::sync_channel(4);
        hl.set_control_rx(rx);
        tx.send(ControlCommand::Subscribe {
            filters: Default::default(),
            contract: ContractRef {
                con_id: 756733,
                symbol: "SPY".into(),
                exchange: "SMART".into(),
                sec_type: "STK".into(),
                currency: "USD".into(),
                ..Default::default()
            },
            mode_9887: 0,
            regulatory_snapshot: false,
            reply_tx: None,
            generic_ticks: Vec::new(),
            issued: 5,
        })
        .expect("the engine is holding the other end");
        hl.poll_control_commands();
        let instrument = hl.context.market.instrument_by_con_id(756733)
            .expect("the contract holds a slot");

        // A joiner of the occupancy before this one.
        tx.send(ControlCommand::AlsoAskForSeries {
            instrument,
            con_id: 756733,
            generic_ticks: vec![236],
            took_it: 2,
            issued: 30,
        })
        .expect("the engine is holding the other end");
        hl.poll_control_commands();

        assert!(
            !hl.farm.asked_generic_ticks.get(&instrument).is_some_and(|a| a.contains(&236)),
            "nothing is asked for a caller that never joined this subscription",
        );
    }

    /// Nothing is asked for on behalf of a caller that has stopped waiting.
    ///
    /// A caller's wait is bounded and this loop is not, so a request can arrive
    /// here after the caller has been told the venue did not answer — and it
    /// keeps no record of the slot. The series such a request named were asked
    /// for anyway: served for the life of a subscription that caller has no
    /// part in, with nothing able to withdraw them.
    #[test]
    fn nothing_is_asked_for_a_caller_that_has_stopped_waiting() {
        let mut hl = HotLoop::new(Arc::new(SharedState::new()), None, None);
        let (tx, rx) = std::sync::mpsc::sync_channel(4);
        hl.set_control_rx(rx);
        let contract = || ContractRef {
            con_id: 756733,
            symbol: "SPY".into(),
            exchange: "SMART".into(),
            sec_type: "STK".into(),
            currency: "USD".into(),
            ..Default::default()
        };
        // One caller streams the contract.
        tx.send(ControlCommand::Subscribe {
            filters: Default::default(),
            contract: contract(),
            mode_9887: 0,
            regulatory_snapshot: false,
            reply_tx: None,
            generic_ticks: Vec::new(),
            issued: 1,
        })
        .expect("the engine is holding the other end");
        hl.poll_control_commands();
        let instrument = hl.context.market.instrument_by_con_id(756733)
            .expect("the contract holds a slot");

        // And a second asks for a series of its own, then stops waiting: its
        // end of the answer is gone before the request is read here.
        let (reply_tx, reply_rx) = std::sync::mpsc::sync_channel(1);
        drop(reply_rx);
        tx.send(ControlCommand::Subscribe {
            filters: Default::default(),
            contract: contract(),
            mode_9887: 0,
            regulatory_snapshot: false,
            reply_tx: Some(reply_tx),
            generic_ticks: vec![236],
            issued: 2,
        })
        .expect("the engine is holding the other end");
        hl.poll_control_commands();

        assert!(
            !hl.farm.asked_generic_ticks.get(&instrument).is_some_and(|a| a.contains(&236)),
            "nothing was asked for the caller that is no longer listening",
        );
    }
}

#[cfg(test)]
mod smart_depth_tests {
    use super::*;

    /// A book on no particular venue goes out at once. It was held for the
    /// venue's directory of book venues and refused after fifteen seconds
    /// without it, on the grounds that the request would gather from one
    /// venue instead of all — but the request goes to the smart destination
    /// whatever the directory says, and nothing asks the venue for the
    /// directory any more, so the hold waited on a message nothing asked for
    /// and the caller kept the book slot for the refused request.
    #[test]
    fn a_book_on_no_particular_venue_goes_out_at_once() {
        let mut hl = HotLoop::new(Arc::new(SharedState::new()), None, None);
        let (conn, peer) = crate::protocol::connection::Connection::for_test();
        let mut peer = crate::protocol::connection::Connection::new_raw(peer).unwrap();
        hl.farm_conn = Some(conn);
        let (tx, rx) = std::sync::mpsc::sync_channel(4);
        hl.set_control_rx(rx);
        tx.send(ControlCommand::SubscribeDepth {
            req_id: 9, num_rows: 5, is_smart_depth: true, filters: Default::default(),
            contract: crate::types::ContractRef {
                con_id: 756733, exchange: "SMART".into(), sec_type: "STK".into(), ..Default::default()
            },
        })
        .unwrap();
        hl.poll_control_commands();
        std::thread::sleep(std::time::Duration::from_millis(50));
        assert_eq!(hl.farm.depth_subs.len(), 1, "one book asked for on the wire");
        assert!(!farm::tests::drain_inner(&mut peer).is_empty(), "and it went out at once");
    }
}

#[cfg(test)]
mod qty_formatting_tests {
    use super::format_qty;
    use crate::types::QTY_SCALE;

    /// A quantity goes out as the number it is. The places were written out as
    /// four while the scale held a hundred-millionth, so half a share left
    /// this machine as something else.
    #[test]
    fn a_fraction_of_a_share_is_written_as_itself() {
        for (held, written) in [
            (QTY_SCALE, "1"),
            (QTY_SCALE / 2, "0.5"),
            (QTY_SCALE / 4, "0.25"),
            (100 * QTY_SCALE, "100"),
            (-3 * QTY_SCALE / 2, "-1.5"),
            // Below one unit the whole part is zero, and a zero carries no
            // sign of its own.
            (-QTY_SCALE / 2, "-0.5"),
            (-1, "-0.00000001"),
            (1, "0.00000001"),
        ] {
            let out = format_qty(held);
            assert_eq!(&*out, written, "{held} was written as {}", &*out);
        }
    }

    /// Whatever the scale holds is written, and nothing beyond it.
    #[test]
    fn every_place_the_scale_holds_survives() {
        let smallest = format_qty(1);
        let places = smallest.split_once('.').expect("a fraction").1.len();
        assert_eq!(10i64.pow(places as u32), QTY_SCALE, "a place the scale holds went unwritten");
    }
}

#[cfg(test)]
mod calendar_farm_reconnect_tests {
    use super::*;

    /// A farm that goes is not a session that ends. The connection carrying
    /// the calendar is rebuilt on the same terms as the others, so a caller
    /// that asks again after one drops is served rather than told forever
    /// that this session has no such connection.
    #[test]
    fn a_calendar_connection_that_went_is_scheduled_to_come_back() {
        let mut hl = HotLoop::new(Arc::new(SharedState::new()), None, None);
        hl.set_reconnect_auth(crate::gateway::ReconnectAuth {
            account_id: String::new(),
            trading_port: None,
            hmds_port: None,
            secdef_port: None,
            logged_in_at: String::new(),
            alternate_hosts: Vec::new(),
            settings: Default::default(),
            host: "gw.example".into(),
            username: "u".into(),
            password: zeroize::Zeroizing::new(String::new()),
            paper: true,
            code_provider: None,
            ib_key_timeout_secs: crate::auth::session::IB_KEY_DEFAULT_TIMEOUT_SECS,
            ib_key_token_sub_type: crate::auth::session::IB_KEY_DEFAULT_TOKEN_SUB_TYPE.into(),
            session_key: Default::default(),
            session_token: Default::default(),
            server_session_id: String::new(),
            hw_info: String::new(),
            encoded: String::new(),
            hmds_host: String::new(),
            hmds_farm: String::new(),
            trading_host: String::new(),
            trading_farm: String::new(),
            secdef_host: "sd.example".into(),
            secdef_farm: "secdefeu".into(),
        });
        assert!(hl.secdef_conn.is_none());

        // The first pass schedules rather than dials, the way the others do.
        hl.maybe_spawn_secdef_reconnect();
        assert!(hl.secdef_next_attempt_at.is_some(), "no attempt was scheduled");
        assert!(hl.pending_secdef_reconnect.is_none(), "it dialled immediately");
    }

    /// A session the venue named no such farm for has none to rebuild, and
    /// does not sit trying to.
    #[test]
    fn a_session_without_that_farm_does_not_try() {
        let mut hl = HotLoop::new(Arc::new(SharedState::new()), None, None);
        hl.set_reconnect_auth(crate::gateway::ReconnectAuth {
            account_id: String::new(),
            trading_port: None,
            hmds_port: None,
            secdef_port: None,
            logged_in_at: String::new(),
            alternate_hosts: Vec::new(),
            settings: Default::default(),
            host: "gw.example".into(),
            username: "u".into(),
            password: zeroize::Zeroizing::new(String::new()),
            paper: true,
            code_provider: None,
            ib_key_timeout_secs: crate::auth::session::IB_KEY_DEFAULT_TIMEOUT_SECS,
            ib_key_token_sub_type: crate::auth::session::IB_KEY_DEFAULT_TOKEN_SUB_TYPE.into(),
            session_key: Default::default(),
            session_token: Default::default(),
            server_session_id: String::new(),
            hw_info: String::new(),
            encoded: String::new(),
            hmds_host: String::new(),
            hmds_farm: String::new(),
            trading_host: String::new(),
            trading_farm: String::new(),
            secdef_host: String::new(),
            secdef_farm: String::new(),
        });
        hl.maybe_spawn_secdef_reconnect();
        assert!(hl.secdef_next_attempt_at.is_none());
        assert!(hl.pending_secdef_reconnect.is_none());
    }
}

#[cfg(test)]
mod slot_reclamation_tests {
    use super::*;

    /// The slot an order held goes back to the table when the order ends.
    ///
    /// A slot is freed when the last thing referring to it goes, and only a
    /// cancelled subscription ever asked. An order — or a preview, which is
    /// tracked as one — held its slot for the life of the session, so a
    /// program working a chain a few thousand contracts wide ran the table out
    /// and was refused the next contract it named, with nothing referring to
    /// most of what it held.
    #[test]
    fn the_slot_an_order_held_goes_back_when_the_order_ends() {
        let mut hl = HotLoop::new(Arc::new(SharedState::new()), None, None);
        let instrument = hl.context.market.register(756733);
        hl.context.insert_order(crate::types::Order::new(
            42, instrument, crate::types::Side::Buy,
            100 * crate::types::QTY_SCALE, 150 * crate::types::PRICE_SCALE,
            b'2', b'0', 0,
        ));

        hl.reclaim_slots_no_order_holds();
        assert_eq!(
            hl.context.market.instrument_by_con_id(756733), Some(instrument),
            "a working order holds its slot",
        );

        // The copy a caller reads is a second one, and the reclaim is decided
        // on the engine's own book: a slot given back while this still said
        // 300 was read as holding 300 by whatever took it next.
        hl.shared.portfolio.set_position(instrument, 300.0);

        hl.context.retire_order(42);
        hl.reclaim_slots_no_order_holds();
        assert_eq!(
            hl.context.market.register(265598), instrument,
            "and the next contract that needs one is given it",
        );
        assert_eq!(
            hl.shared.market.take_released_slots(), vec![(instrument, 0)],
            "the surfaces are told, so they stop naming the slot it no longer holds",
        );
        assert_eq!(
            hl.shared.portfolio.position(instrument), 0.0,
            "and the holding a caller reads off the slot went with it",
        );
    }
}

#[cfg(test)]
mod slot_aliasing_tests {
    use super::*;

    /// The model the venue published for a slot goes back with the slot.
    ///
    /// It is kept by slot rather than by contract, so nothing that speaks in
    /// contract ids can drop it. The next contract handed the slot was solved
    /// against the previous contract's volatility, price and dividend, and the
    /// answer came back a finite number about a contract nobody asked about.
    #[test]
    fn the_model_a_slot_carried_goes_back_with_it() {
        let mut hl = HotLoop::new(Arc::new(SharedState::new()), None, None);
        let instrument = hl.context.market.register(756733);
        hl.shared.market.push_option_computation(crate::types::OptionComputation {
            instrument, implied_vol: 0.42, ..Default::default()
        });
        assert!(hl.shared.market.option_model(instrument).is_some(), "the venue stated one");

        hl.try_reclaim_instrument(instrument);

        assert_eq!(
            hl.context.market.register(265598), instrument,
            "the slot went to the next contract",
        );
        assert!(
            hl.shared.market.option_model(instrument).is_none(),
            "and it is not solved against the last contract's model",
        );
    }

    /// A lookup the venue never answers gives its slot back.
    ///
    /// A caller told the venue knows no such contract has no reason to
    /// withdraw a subscription that never opened, and nothing else asked. A
    /// chain naming a few dead strikes spent one slot on each until the table
    /// ran out — the failure reclaiming exists to prevent.
    ///
    /// Not before the reason has been read, though: it is queued under the
    /// slot, and giving the slot back is what drops it.
    #[test]
    fn a_lookup_the_venue_never_answers_gives_its_slot_back() {
        let mut hl = HotLoop::new(Arc::new(SharedState::new()), None, None);
        let instrument = hl.context.market.register(0);
        hl.ccp.pending_md_subscribe.push((
            1,
            crate::engine::hot_loop::ccp::PendingSubscribe {
                issued: 0,
                filters: Default::default(),
                con_id: 0, instrument,
                symbol: "SPY".into(), exchange: "SMART".into(), sec_type: "STK".into(),
                currency: "USD".into(),
                mode_9887: 0, regulatory_snapshot: false,
            },
            Instant::now() - std::time::Duration::from_secs(3600),
        ));

        hl.ccp.sweep_pending_subscribes(&mut hl.context, &hl.shared);
        hl.reclaim_slots_no_order_holds();

        assert_eq!(
            hl.shared.market.drain_subscription_failures().len(), 1,
            "the caller is told why before the slot goes anywhere",
        );
        hl.give_back_slots_their_word_has_left();

        assert_eq!(
            hl.context.market.register(265598), instrument,
            "the slot the abandoned lookup held is offered to the next contract",
        );
    }
}

#[cfg(test)]
mod queued_order_slot_tests {
    use super::*;

    /// An order waiting to be sent holds the slot it names.
    ///
    /// The book holds an order from the moment it is built, which is a lap
    /// after the caller was told it was accepted. A withdrawal of the quotes
    /// drained in the same pass found the slot unheld and gave it back, so the
    /// order was built a lap later from whatever contract had taken it — sent
    /// on the wrong contract, and recorded so its fill moved that contract's
    /// position.
    #[test]
    fn an_order_waiting_to_be_sent_holds_its_slot() {
        let mut hl = HotLoop::new(Arc::new(SharedState::new()), None, None);
        let instrument = hl.context.market.register(756733);
        hl.context.pending_orders.push(crate::types::OrderRequest::SubmitEx { con_id: 0,
            order_id: 42,
            instrument,
            side: crate::types::Side::Buy,
            qty: 100 * crate::types::QTY_SCALE,
            kind: crate::types::OrderKind::Limit { price: 150 * crate::types::PRICE_SCALE },
            tif: b'0',
            attrs: Default::default(),
        });

        hl.try_reclaim_instrument(instrument);

        assert_eq!(
            hl.context.market.instrument_by_con_id(756733), Some(instrument),
            "the slot the queued order names is still its contract's",
        );
        assert_ne!(
            hl.context.market.register(265598), instrument,
            "and is not handed to the next contract that asks",
        );
    }
}

#[cfg(test)]
mod queued_cancel_all_slot_tests {
    use super::*;

    /// A queued request holds the slot it names for as long as it is queued.
    ///
    /// A cancel-all names a slot and puts nothing in the book, so the reclaim
    /// cannot see it there. The slot is offered back when the request leaves
    /// the buffer — see the builder's own test — rather than asked about once
    /// per lap for as long as it sits there.
    #[test]
    fn a_queued_cancel_all_holds_its_slot() {
        let mut hl = HotLoop::new(Arc::new(SharedState::new()), None, None);
        let instrument = hl.context.market.register(756733);
        hl.context.pending_orders.push(crate::types::OrderRequest::CancelAll { instrument });

        hl.try_reclaim_instrument(instrument);

        assert_eq!(
            hl.context.market.instrument_by_con_id(756733), Some(instrument),
            "the queued request holds the slot while it is queued",
        );
        assert!(
            hl.context.slots_to_reconsider.is_empty(),
            "and the reclaim does not ask again every lap while it sits there",
        );
    }
}

#[cfg(test)]
mod withdrawal_tests {
    use super::*;

    /// An order that reached the wire is recorded as having, and one that did
    /// not is not.
    ///
    /// Silence has two causes and they are identical from outside: the venue
    /// answering nothing, and this client never having asked. A live phase
    /// skipped on that silence, so about fifty of them passed whether or not
    /// anything reached the socket.
    #[test]
    fn an_order_that_reached_the_wire_is_recorded_as_having() {
        let shared = Arc::new(SharedState::new());
        let mut hl = HotLoop::new(shared.clone(), None, None);
        let (conn, mut peer) = crate::protocol::connection::Connection::for_test();
        hl.ccp_conn = Some(conn);
        hl.ccp.disconnected = false;
        let instrument = hl.context_mut().register_instrument(756733);
        hl.context_mut().set_routing(instrument, "STK", "SMART");

        assert!(!shared.orders.the_order_went_out(41), "nothing has been sent yet");

        let (tx, rx) = std::sync::mpsc::sync_channel(4);
        hl.set_control_rx(rx);
        tx.send(crate::types::ControlCommand::Order(crate::types::OrderRequest::SubmitEx {
            order_id: 41,
            instrument, con_id: 756733,
            side: crate::types::Side::Buy,
            qty: crate::types::qty_from_f64(1.0),
            kind: crate::types::OrderKind::Limit {
                price: crate::types::price_from_f64(100.0),
            },
            tif: 0,
            attrs: crate::types::OrderAttrs::default(),
        })).unwrap();
        hl.poll_control_commands();
        order_builder::drain_and_send_orders(
            &mut hl.ccp_conn, &mut hl.context, &hl.account_id, &mut hl.hb,
            hl.ccp.disconnected, &hl.shared, false, &hl.event_tx,
        );

        use std::io::Read;
        let mut buf = [0u8; 8192];
        let n = peer.read(&mut buf).unwrap_or(0);
        assert!(n > 0, "the order reached the socket");
        assert!(
            shared.orders.the_order_went_out(41),
            "and this client records that it did, which is what tells silence \
             from never having asked",
        );
    }

    /// Every withdrawal in the client answers when it names nothing.
    ///
    /// Five said nothing at all, which reads exactly like a withdrawal that
    /// acted -- so a caller whose record disagrees with this client's had no
    /// way to learn it, and the inconsistency was inside one client: six
    /// request kinds answered and five did not.
    #[test]
    fn every_withdrawal_says_when_it_names_nothing() {
        for (what, cmd, code) in [
            ("bars", crate::types::ControlCommand::CancelHistorical { req_id: 9 }, 366),
            ("a head timestamp", crate::types::ControlCommand::CancelHeadTimestamp { req_id: 9 }, 300),
            ("corporate actions", crate::types::ControlCommand::CancelCorporateActions { req_id: 9 }, 300),
            ("a histogram", crate::types::ControlCommand::CancelHistogramData { req_id: 9 }, 300),
            ("live bars", crate::types::ControlCommand::CancelRealTimeBar { req_id: 9 }, 300),
        ] {
            let mut hl = HotLoop::new(Arc::new(SharedState::new()), None, None);
            let (tx, rx) = std::sync::mpsc::sync_channel(4);
            hl.set_control_rx(rx);

            tx.send(cmd).unwrap();
            hl.poll_control_commands();

            let told = hl.shared.reference.drain_historical_errors();
            assert_eq!(told.len(), 1, "{what}: the caller is told: {told:?}");
            assert_eq!(
                (told[0].0, told[0].1), (9, code),
                "{what}: under the number that names it",
            );
        }
    }

    /// And a withdrawal that does name something says nothing beside it.
    ///
    /// The bar withdrawal sweeps up the corporate-actions query an adjusted
    /// series may have out under the same number, and an ordinary series has
    /// none -- so reporting from inside that sweep would answer every
    /// ordinary bar withdrawal with a refusal it did not earn.
    #[test]
    fn withdrawing_bars_says_nothing_about_the_actions_query_it_sweeps() {
        let mut hl = HotLoop::new(Arc::new(SharedState::new()), None, None);
        let (tx, rx) = std::sync::mpsc::sync_channel(4);
        hl.set_control_rx(rx);
        hl.hmds.keep_up_to_date_reqs.insert(9);

        tx.send(crate::types::ControlCommand::CancelHistorical { req_id: 9 }).unwrap();
        hl.poll_control_commands();

        assert!(
            hl.shared.reference.drain_historical_errors().is_empty(),
            "a withdrawal that named something is answered with nothing",
        );
    }

    /// Withdrawing a news or fundamentals query this client is not waiting on
    /// is answered too, and under the number that says nothing was waiting
    /// rather than the data service's own.
    ///
    /// Both returned in silence, while the calendar withdrawal beside them
    /// reported -- three withdrawals in one area, two behaviours. And the
    /// calendar reported under the service's number, which says the service
    /// reported a difficulty with a request it answered. Nothing was waiting;
    /// that is a different thing.
    #[test]
    fn withdrawing_a_query_that_is_not_waiting_says_so() {
        for (what, cmd) in [
            ("news", crate::types::ControlCommand::CancelHistoricalNews { req_id: 9 }),
            ("fundamentals", crate::types::ControlCommand::CancelFundamentalData { req_id: 9 }),
            ("the calendar", crate::types::ControlCommand::CancelCalendar { req_id: 9 }),
        ] {
            let mut hl = HotLoop::new(Arc::new(SharedState::new()), None, None);
            let (tx, rx) = std::sync::mpsc::sync_channel(4);
            hl.set_control_rx(rx);

            tx.send(cmd).unwrap();
            hl.poll_control_commands();

            let told = hl.shared.reference.drain_historical_errors();
            assert_eq!(told.len(), 1, "{what}: the caller is told: {told:?}");
            assert_eq!(
                (told[0].0, told[0].1), (9, 300),
                "{what}: under the number that says nothing was waiting",
            );
        }
    }

    /// Withdrawing a scan this client is not running is answered, not passed
    /// over.
    ///
    /// Said nothing, the withdrawal is indistinguishable from one that worked,
    /// and a caller whose record disagrees with this client's has no way to
    /// learn it.
    #[test]
    fn withdrawing_a_scan_that_is_not_running_says_so() {
        let mut hl = HotLoop::new(Arc::new(SharedState::new()), None, None);
        let (tx, rx) = std::sync::mpsc::sync_channel(4);
        hl.set_control_rx(rx);

        tx.send(crate::types::ControlCommand::CancelScanner { req_id: 9 }).unwrap();
        hl.poll_control_commands();

        let errors = hl.shared.reference.drain_historical_errors();
        assert_eq!(errors.len(), 1, "the caller is told: {errors:?}");
        assert_eq!((errors[0].0, errors[0].1), (9, 365), "under the number that names it");
    }
    /// A farm-only recovery is bounded by the farm's own clock, not the
    /// trading connection's. Measured against the trading budget, a farm dial
    /// with no trading outage behind it was bounded by no deadline at all.
    #[test]
    fn a_farm_only_recovery_is_bounded_by_the_farms_own_clock() {
        let shared = Arc::new(SharedState::new());
        let mut hl = HotLoop::new(shared, None, None);
        hl.set_reconnect_config(
            crate::reliability::ReconnectConfig::default().with_max_elapsed(std::time::Duration::from_secs(30)),
        );
        let t = Instant::now();
        hl.farm.disconnected = true;
        hl.farm_budget.record_attempt(t);
        let (worker, rx) = std::sync::mpsc::sync_channel::<std::io::Result<Connection>>(1);
        drop(worker);
        hl.pending_farm_reconnect = Some(rx);

        hl.abandon_recovery_past_its_deadline(t + std::time::Duration::from_secs(29));
        assert!(hl.farm_halted.is_none(), "not past the farm's own deadline yet");
        hl.abandon_recovery_past_its_deadline(t + std::time::Duration::from_secs(31));
        assert!(hl.farm_halted.is_some(), "the farm's own deadline ends its recovery");
    }

    /// A trading connection that recovered and spent its clock does not end a
    /// farm recovery that started later and has not reached its own deadline.
    #[test]
    fn a_spent_trading_clock_does_not_end_a_fresh_farm_recovery() {
        let shared = Arc::new(SharedState::new());
        let mut hl = HotLoop::new(shared, None, None);
        hl.set_reconnect_config(
            crate::reliability::ReconnectConfig::default().with_max_elapsed(std::time::Duration::from_secs(30)),
        );
        let t = Instant::now();
        // The trading connection recovered earlier and spent its clock.
        hl.budget.record_attempt(t);
        // The farm drops later and starts its own attempt.
        hl.farm.disconnected = true;
        hl.farm_budget.record_attempt(t + std::time::Duration::from_secs(20));
        let (worker, rx) = std::sync::mpsc::sync_channel::<std::io::Result<Connection>>(1);
        drop(worker);
        hl.pending_farm_reconnect = Some(rx);

        // Past the trading clock's 30s, not the farm's.
        hl.abandon_recovery_past_its_deadline(t + std::time::Duration::from_secs(31));
        assert!(
            hl.farm_halted.is_none(),
            "the farm's recovery is not ended by the trading connection's spent clock",
        );
    }

    /// A historical liveness timeout takes the parked scan rows with it, as a
    /// drop seen by the poll does. The scans were failed when the connection
    /// was given up; releasing their rows on the sweep would answer a scan the
    /// caller was just told had failed.
    #[test]
    fn a_historical_liveness_timeout_takes_the_parked_scan_rows() {
        let shared = Arc::new(SharedState::new());
        let mut hl = HotLoop::new(shared, None, None);
        let (conn, peer) = crate::protocol::connection::Connection::for_test();
        drop(peer);
        hl.hmds_conn = Some(conn);
        // Silent well past the dead-after threshold, and not a stall in this
        // process (the check ran a moment ago).
        hl.hb.last_liveness_check = Instant::now();
        hl.hb.last_hmds_recv = Instant::now() - std::time::Duration::from_secs(600);
        hl.ccp.pending_scanner_enrichment.push(crate::engine::hot_loop::ccp::PendingScannerEnrichment {
            api_req_id: 9,
            result: crate::control::scanner::ScannerResult {
                con_ids: Vec::new(), entries: Vec::new(), scan_time: String::new(), error_text: String::new(),
            },
            awaiting: Default::default(),
            deadline: Instant::now() + std::time::Duration::from_secs(60),
        });

        hl.check_heartbeats();
        assert!(hl.hmds_conn.is_none(), "the silent connection is given up");
        assert!(
            hl.ccp.pending_scanner_enrichment.is_empty(),
            "and the parked rows go with it, not on to the sweep",
        );
    }

}
