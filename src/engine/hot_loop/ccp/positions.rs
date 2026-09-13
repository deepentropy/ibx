use std::collections::HashMap;

use super::{adopt_position, CcpState};

use crate::bridge::{Event, SharedState};
use crate::engine::context::Context;
use crate::protocol::connection::Connection;
use crate::types::{
    MidnightSeed,
    PositionInfo, Price,
};

use super::{HeartbeatState, emit, EventSink};

/// Where to book a fill whose order this session does not track.
///
/// Both the contract and the side come off the report, and both are required:
/// a guessed side would move the position the wrong way, which is worse than
/// reporting that the fill could not be placed.
pub(crate) fn handle_account_update(msg: &[u8], context: &mut Context, shared: &SharedState) {
    let text = match std::str::from_utf8(msg) {
        Ok(t) => t,
        Err(_) => return,
    };
    let mut key: Option<&str> = None;
    // The venue states which currency a figure is in, and it is not always the
    // account's own. Read per group, and carried rather than assumed.
    let mut currency: &str = "";
    // Whether this frame stated anything at all. A frame that named no figure
    // must not publish the account: the flag that publishing raises is what
    // every "is this connection's data" gate reads, so raising it on an empty
    // frame marks a pre-drop snapshot current.
    let mut stated_a_figure = false;
    for part in text.split('\x01') {
        if let Some(val) = part.strip_prefix("15=") {
            currency = val;
        } else if let Some(val) = part.strip_prefix("8001=") {
            key = Some(val);
        } else if let Some(val) = part.strip_prefix("8004=")
            && let Some(k) = key {
                // Kept whether or not anything below names it. A figure nobody
                // named is still a figure about the account, and dropping it
                // left no trace that the venue had stated it.
                shared.portfolio.note_account_value(k, val, currency);
                stated_a_figure = true;
                match k {
                    "NetLiquidation" => { if let Ok(v) = val.parse::<f64>() { context.account.net_liquidation = crate::types::price_from_f64(v); } }
                    "BuyingPower" => { if let Ok(v) = val.parse::<f64>() { context.account.buying_power = crate::types::price_from_f64(v); } }
                    "MaintMarginReq" => {
                        if let Ok(v) = val.parse::<f64>() {
                            context.account.margin_used = crate::types::price_from_f64(v);
                            context.account.maint_margin_req = crate::types::price_from_f64(v);
                        }
                    }
                    "UnrealizedPnL" => { if let Ok(v) = val.parse::<f64>() { context.account.unrealized_pnl = crate::types::price_from_f64(v); } }
                    "RealizedPnL" => { if let Ok(v) = val.parse::<f64>() { context.account.realized_pnl = crate::types::price_from_f64(v); } }
                    "TotalCashValue" => { if let Ok(v) = val.parse::<f64>() { context.account.total_cash_value = crate::types::price_from_f64(v); } }
                    "SettledCash" => { if let Ok(v) = val.parse::<f64>() { context.account.settled_cash = crate::types::price_from_f64(v); } }
                    "AccruedCash" => { if let Ok(v) = val.parse::<f64>() { context.account.accrued_cash = crate::types::price_from_f64(v); } }
                    "EquityWithLoanValue" => { if let Ok(v) = val.parse::<f64>() { context.account.equity_with_loan = crate::types::price_from_f64(v); } }
                    "GrossPositionValue" => { if let Ok(v) = val.parse::<f64>() { context.account.gross_position_value = crate::types::price_from_f64(v); } }
                    // The plain spelling alone. The `Full` spellings are
                    // different figures — they diverge whenever intraday
                    // margin relief applies — and folded into one field the
                    // account read whichever the venue stated last. Each stays
                    // reachable by name among the stated values.
                    "InitMarginReq" => { if let Ok(v) = val.parse::<f64>() { context.account.init_margin_req = crate::types::price_from_f64(v); } }
                    "AvailableFunds" => { if let Ok(v) = val.parse::<f64>() { context.account.available_funds = crate::types::price_from_f64(v); } }
                    "ExcessLiquidity" => { if let Ok(v) = val.parse::<f64>() { context.account.excess_liquidity = crate::types::price_from_f64(v); } }
                    "Cushion" => { if let Ok(v) = val.parse::<f64>() { context.account.cushion = crate::types::price_from_f64(v); } }
                    "SMA" => { if let Ok(v) = val.parse::<f64>() { context.account.sma = crate::types::price_from_f64(v); } }
                    "DayTradesRemaining" => { if let Ok(v) = val.parse::<i64>() { context.account.day_trades_remaining = v; } }
                    "Leverage-S" | "Leverage" => { if let Ok(v) = val.parse::<f64>() { context.account.leverage = crate::types::price_from_f64(v); } }
                    "DailyPnL" => { if let Ok(v) = val.parse::<f64>() { context.account.daily_pnl = crate::types::price_from_f64(v); } }
                    _ => {}
                }
                key = None;
            }
    }
    if stated_a_figure {
        shared.portfolio.set_account(context.account());
    }
}

/// The figures the venue keeps per currency, by the tag each arrives on.
///
/// A ledger reply is not the name-and-value stream the other account messages
/// are: it opens a bucket, names the currency it is in, and then states each
/// figure on a tag of its own. Read as name-and-value it matched nothing, and
/// the standard way to read per-currency cash came back empty — which a caller
/// cannot tell from an account holding no cash at all.
///
/// The names are the venue's own. Five tags it states — 6925, 6926, 8007, 8208
/// and 8398 — have a place in its own formatter and no name in the map it
/// publishes from, so they are left out rather than given one here. A figure
/// with a name invented for it is worse than a figure withheld: a caller reads
/// it as the venue's.
const LEDGER_FIGURES: [(u32, &str); 21] = [
    (9807, "StockMarketValue"),
    (9808, "OptionMarketValue"),
    (9809, "FutureOptionValue"),
    (9810, "FuturesPNL"),
    (9818, "TotalCashBalance"),
    (9819, "NetLiquidationByCurrency"),
    (9820, "ExchangeRate"),
    (6681, "NetDividend"),
    (6682, "MutualFundValue"),
    (6683, "MoneyMarketFundValue"),
    (6684, "CorporateBondValue"),
    (6685, "TBondValue"),
    (6686, "TBillValue"),
    (6687, "WarrantValue"),
    (6924, "IssuerOptionValue"),
    (6099, "RealizedPnL"),
    (6100, "UnrealizedPnL"),
    (6242, "AccruedCash"),
    (6483, "FundValue"),
    (6711, "FxCashBalance"),
    (8406, "Cryptocurrency"),
];

/// A ledger figure as the venue writes it: at least two places and at most
/// seven, without grouping.
fn ledger_figure(value: f64) -> String {
    let mut text = format!("{value:.7}");
    while text.contains('.')
        && text.ends_with('0')
        && text.split('.').nth(1).is_some_and(|frac| frac.len() > 2)
    {
        text.pop();
    }
    text
}

/// One per-currency bucket of the account's figures.
///
/// The venue opens a bucket, names its currency, and states each figure on its
/// own tag. An insured-deposit balance is stated apart and belongs to the cash
/// balance unless the session is set to split the two, which this one is not —
/// so it is added in rather than published as a figure of its own.
pub(crate) fn handle_ledger_update(msg: &[u8], shared: &SharedState) {
    let Ok(text) = std::str::from_utf8(msg) else { return };
    let mut currency = String::new();
    let mut opened = false;
    let mut cash: Option<f64> = None;
    let mut insured: Option<f64> = None;
    let mut stated: Vec<(&str, String)> = Vec::new();
    let publish = |currency: &str,
                   cash: &Option<f64>,
                   insured: &Option<f64>,
                   stated: &mut Vec<(&str, String)>| {
        if currency.is_empty() {
            stated.clear();
            return;
        }
        // The bucket's currency is itself one of the figures the venue keeps
        // per currency, and it is the first of them. A caller that asked for
        // the ledger and reads the rows by name has no other way to be told
        // which currency a bucket is in when it asked for all of them.
        shared.portfolio.note_account_value("Currency", currency, currency);
        if let Some(balance) = cash {
            // The insured deposit is part of what is held in cash unless the
            // session splits them, and it is stated apart either way.
            let held = balance + insured.filter(|d| d.is_finite()).unwrap_or(0.0);
            shared.portfolio.note_account_value("CashBalance", &ledger_figure(held), currency);
        }
        for (name, value) in stated.drain(..) {
            shared.portfolio.note_account_value(name, &value, currency);
        }
    };
    for part in text.split('\x01') {
        let Some((tag, value)) = part.split_once('=') else { continue };
        let Ok(tag) = tag.parse::<u32>() else { continue };
        match tag {
            // A new bucket: whatever the last one stated goes out under its own
            // currency before this one starts.
            8001 => {
                if opened {
                    publish(&currency, &cash, &insured, &mut stated);
                }
                opened = true;
                currency.clear();
                cash = None;
                insured = None;
            }
            15 => currency = value.to_string(),
            9806 => cash = value.parse().ok(),
            8174 => insured = value.parse().ok(),
            _ => {
                if let Some((_, name)) = LEDGER_FIGURES.iter().find(|(t, _)| *t == tag)
                    && let Ok(figure) = value.parse::<f64>()
                {
                    stated.push((name, ledger_figure(figure)));
                }
            }
        }
    }
    if opened {
        publish(&currency, &cash, &insured, &mut stated);
    }
}


/// Handle 6040=143, the venue's daily P&L seeds.
/// Repeating group: 146={count} × (6008=conId, 6064=qtyMidnight, 8223=qtyTraded,
/// 8233=costMidnight, 6822=moneyTraded, 6099=realizedPnl), then 8058 combo
/// buckets repeating the same five fields under an 8020 label. Both counts are
/// hints and the scan delimits itself on the tags, so neither is read.
/// Only the realized figure is stated outright; the rest are what a daily
/// figure is computed from. Values are taken as sent, unscaled.
pub(crate) fn handle_pnl_response(msg: &[u8], shared: &SharedState) {
    let text = match std::str::from_utf8(msg) {
        Ok(t) => t,
        Err(_) => return,
    };
    let mut seeds: Vec<MidnightSeed> = Vec::new();
    // The entry the scan is currently filling. `None` between entries and for
    // the duration of a combo bucket, so figures that belong to neither a
    // contract nor this client land nowhere rather than on the last contract.
    let mut current: Option<MidnightSeed> = None;
    let mut request_id = String::new();
    let mut reference_id = String::new();
    for part in text.split('\x01') {
        if let Some(v) = part.strip_prefix("58=") {
            // The body states its own status here. A body that has something to
            // say went wrong, so nothing in it is a figure and the whole thing
            // is abandoned rather than half-read.
            if !v.is_empty() {
                log::warn!("P&L seeds not usable: {v}");
                return;
            }
        } else if let Some(v) = part.strip_prefix("6529=") {
            request_id = v.to_string();
        } else if let Some(v) = part.strip_prefix("8292=") {
            reference_id = v.to_string();
        } else if let Some(v) = part.strip_prefix("6008=") {
            seeds.extend(current.take());
            current = v.parse::<i64>().ok().filter(|&id| id != 0)
                .map(|con_id| MidnightSeed { con_id, ..Default::default() });
        } else if part.starts_with("8020=") {
            // A combo bucket states the same five figures against a label
            // instead of a contract id. Nothing downstream is keyed by a label,
            // so the entry in hand is closed and the bucket's figures are read
            // past rather than folded into the contract that came before it.
            seeds.extend(current.take());
        } else if let Some(seed) = current.as_mut() {
            if let Some(v) = part.strip_prefix("6064=") {
                // Same rule as the position feed above: a quantity that is absent
                // or unparseable is not a flat. Reading it as zero here makes the
                // day's P&L look as though the position were opened intraday. The
                // row is still kept — dropping it says the same thing, because a
                // position with no seed row *is* the intraday case, and it would
                // discard the cash and realized figures the row does carry.
                seed.qty_midnight = v.parse::<f64>().ok().filter(|q| q.is_finite());
            } else if let Some(v) = part.strip_prefix("8223=") {
                seed.qty_traded = v.parse::<f64>().ok().filter(|q| q.is_finite());
            } else if let Some(v) = part.strip_prefix("8233=") {
                // What the venue says the position was worth at midnight, which
                // is the figure the day's change is measured from.
                seed.cost_midnight = v.parse::<f64>().ok().filter(|c| c.is_finite());
            } else if let Some(v) = part.strip_prefix("6822=") {
                // moneyTradedSinceMidnight: signed net cash, SELL positive / BUY
                // negative. Stored with the wire sign; poll_pnl adds it.
                // Filtered to finite like the figures above: `"NaN".parse()`
                // succeeds, and one such value folded into the daily and
                // realized totals poisons the whole position.
                seed.money_traded = v.parse::<f64>().ok().filter(|m| m.is_finite()).unwrap_or(0.0);
            } else if let Some(v) = part.strip_prefix("6099=") {
                seed.realized_pnl = v.parse::<f64>().ok().filter(|r| r.is_finite()).unwrap_or(0.0);
            }
        }
    }
    seeds.extend(current);
    // The venue answers against the reference it was given and falls back to
    // its own request id when it has none.
    let key = if reference_id.is_empty() { request_id } else { reference_id };
    shared.portfolio.set_midnight_seeds(key, seeds);
}

/// The basis to publish for a position row.
///
/// A row that states one states it. A row that does not leaves the one on file
/// standing, since an absent cost is not a cost of zero — but only while the
/// holding is open: a row closing it takes the basis with it, or the next
/// position in the same contract would inherit the last one's.
pub(crate) fn basis_for(shared: &SharedState, con_id: i64, stated: Option<Price>, qty: f64) -> Price {
    // A closed holding has no basis, and a row closing one has been seen to
    // carry the cost it was closed at. Keeping that leaves the next position
    // in the contract opening against the last one's price.
    // The quantity decides this, and a fractional holding is a holding: taking
    // it from the whole-number field would read half a share as flat and throw
    // away the basis of a position that is open.
    if qty == 0.0 {
        return 0;
    }
    if let Some(c) = stated {
        return c;
    }
    shared.portfolio.position_info(con_id).map(|p| p.avg_cost).unwrap_or(0)
}

/// Handle position update messages (cross-cutting, called from CCP message processing).
/// A holding the venue reports apart from the account's own.
///
/// The same fields in the same tags as a holding of the account's own, so it
/// is read the same way — and kept apart, because a caller asking what the
/// account holds does not mean what it holds somewhere else.
pub(crate) fn handle_position_elsewhere(
    parsed: &std::collections::HashMap<u32, String>,
    shared: &SharedState,
    held: crate::types::HeldElsewhere,
) {
    let Some(con_id) = parsed.get(&6008).and_then(|s| s.parse::<i64>().ok()).filter(|id| *id != 0)
    else {
        return;
    };
    // An absent quantity means this frame carries no quantity, not that the
    // holding is gone. Defaulting to zero publishes a real holding as flat, and
    // `"NaN".parse()` succeeds, so a non-finite value does the same by another
    // route. Both are how the two sibling paths went wrong;
    // this one kept the defect after they were fixed.
    let Some(position) = parsed.get(&6064)
        .and_then(|s| s.parse::<f64>().ok())
        .filter(|v| v.is_finite())
    else {
        return;
    };
    // The basis this holding was already carrying, where the frame states
    // none. Written into a row that persists, an absent tag read as zero, so a
    // later frame about the same holding replaced a real cost with nothing —
    // the rule the account's own holdings already follow.
    let avg_cost = parsed.get(&6101)
        .and_then(|s| s.parse::<f64>().ok())
        .filter(|v| v.is_finite())
        .map(crate::types::price_from_f64)
        .unwrap_or_else(|| {
            shared.portfolio.positions_elsewhere()
                .iter()
                // On the contract alone, which is what the map is keyed on:
                // a second set restates the row rather than adding one, so
                // matching the set as well found nothing and wrote the basis
                // away as zero -- and the next frame without one then found
                // the zero and kept it, which is the loss this carry exists
                // to prevent.
                .find(|row| row.con_id == con_id)
                .map_or(0, |row| row.avg_cost)
        });
    let row = crate::types::PositionElsewhere {
        con_id,
        symbol: parsed.get(&6068).map(|s| s.trim_end().to_string()).unwrap_or_default(),
        sec_type: parsed.get(&167).cloned().unwrap_or_default(),
        currency: parsed.get(&15).cloned().unwrap_or_default(),
        position,
        avg_cost,
        held,
    };
    log::info!("Held elsewhere: {} {:?} x{position}", row.symbol, held);
    shared.portfolio.set_position_elsewhere(row);
}

pub(crate) fn handle_position_update(
    parsed: &std::collections::HashMap<u32, String>,
    context: &mut Context,
    shared: &SharedState,
    event_tx: &Option<EventSink>,
) {
    let con_id: i64 = match parsed.get(&6008).and_then(|s| s.parse::<i64>().ok()).filter(|id| *id != 0) {
        Some(v) => v,
        None => return,
    };
    // Named by a download in progress, whatever else this frame carries.
    shared.portfolio.note_restated(con_id);
    // An absent quantity means this frame carries no quantity, not that the
    // account is flat. Defaulting to 0 reconciled the engine's position to zero
    // off a marks-only frame and published a flat book to reqPositions and both
    // P&L paths until the next frame that did carry 6064.
    let position_raw: Option<f64> = parsed.get(&6064)
        .and_then(|s| s.parse::<f64>().ok())
        // `"NaN".parse()` succeeds and `NaN as i64` is 0, so a non-finite
        // value would flatten a live position by the same route the absent
        // tag did. Route it to no-data instead.
        .filter(|v| v.is_finite());
    let position: Option<f64> = position_raw;
    // Tag map verified against the updatePortfolio callback:
    // 6101 = averageCost, 6065 = marketPrice (per share), 6067 = marketValue,
    // 6100 = unrealizedPNL, 6099 = realizedPNL. Earlier code read 6065 as the
    // average cost, which is actually the market price.
    // What the frame states, and nothing where it states nothing: an absent
    // tag written as nought overwrites a real figure with zero, which the
    // caller reads as a holding worth nothing.
    let price_tag = |tag: u32| parsed.get(&tag)
        .and_then(|s| s.parse::<f64>().ok())
        .filter(|v| v.is_finite())
        .map(crate::types::price_from_f64);
    // The average cost is written into a row that persists, so an absent tag
    // must not overwrite a real one with zero — the same rule the quantity
    // above follows. Marks are refreshed every frame and are handled apart.
    let avg_cost: Option<Price> = parsed.get(&6101)
        .and_then(|s| s.parse::<f64>().ok())
        .filter(|v| v.is_finite())
        .map(crate::types::price_from_f64);
    let market_price = price_tag(6065);
    let market_value = price_tag(6067);
    let unrealized_pnl = price_tag(6100);
    let realized_pnl = price_tag(6099);
    // Symbol arrives space-padded; trim trailing whitespace.
    let symbol = parsed.get(&6068).map(|s| s.trim_end().to_string()).unwrap_or_default();
    let sec_type = parsed.get(&167).cloned().unwrap_or_default();
    let currency = parsed.get(&15).cloned().unwrap_or_default();
    // Not taken off this frame, which carries no multiplier at all. The tag
    // that was being read for it is the
    // row's second key — the name the venue files the holding under — and read
    // as a multiplier it reached a caller as the contract's own, and made a
    // plain share read as multiplied: the holding was then counted unpriceable
    // and dropped out of the account's totals altogether. The contract's
    // definition is where a multiplier comes from, and fills it in when it
    // arrives.
    let multiplier = String::new();

    let Some(position) = position else {
        // Marks-only frame. Apply the marks to a row that already exists, but do
        // not create one: set_position_marks inserts a default PositionInfo, and
        // a row conjured here would carry position 0 and read as flat — the very
        // thing this is fixing. Ordering matters for the same reason, so the
        // quantity-bearing path below still writes the info row first.
        if shared.portfolio.position_info(con_id).is_some() {
            shared.portfolio.set_position_marks(con_id, market_price, market_value, unrealized_pnl, realized_pnl);
        }
        return;
    };

    // Always store position info for reqPositions/pnlSingle, regardless of instrument
    // registry.
    let avg_cost = basis_for(shared, con_id, avg_cost, position_raw.unwrap_or(0.0));
    shared.portfolio.set_position_info(PositionInfo {
        con_id, position, avg_cost,
        symbol, sec_type, currency, multiplier,
        ..Default::default()
    });
    shared.portfolio.set_position_marks(con_id, market_price, market_value, unrealized_pnl, realized_pnl);

    if let Some(instrument) = context.market.instrument_by_con_id(con_id) {
        let current = context.position(instrument);
        let delta = position - current;
        if delta != 0.0 {
            context.update_position(instrument, delta);
        }
        shared.portfolio.set_position(instrument, position);
        emit(event_tx, Event::PositionUpdate { instrument, con_id, position, avg_cost });
    }
}

impl CcpState {
    pub(crate) fn handle_position_feed(
        &mut self,
        msg: &[u8],
        ccp_conn: &mut Option<Connection>,
        context: &mut Context,
        shared: &SharedState,
        event_tx: &Option<EventSink>,
        hb: &mut HeartbeatState,
    ) {
    let text = match std::str::from_utf8(msg) {
        Ok(t) => t,
        Err(_) => return,
    };
    // Parse repeating group by scanning for 6008= boundaries
    let mut con_id: i64 = 0;
    // `None` until this entry carries a parseable, finite quantity. A zero
    // default meant an entry without one flattened a live position, published
    // it to reqPositions and both P&L paths, and emitted a PositionUpdate
    // saying flat — the same defect fixed on the account-update path. A genuine flat
    // still arrives as an explicit `6064=0`.
    let mut qty: Option<f64> = None;
    // `None` where the row states no cost. Folding that into a zero made an
    // absent cost indistinguishable from a real one, and publishing it erased
    // the basis of a live holding.
    let mut avg_cost_raw: Option<f64> = None;
    // The contract as this entry names it. The venue states both beside the
    // quantity; the definition lookup is what fills them in when it does not.
    let mut symbol = String::new();
    let mut sec_type = String::new();
    let mut count = 0;
    for part in text.split('\x01') {
        if let Some(v) = part.strip_prefix("6008=") {
            // Flush previous position if any
            if count > 0 && con_id != 0 {
                if let Some(qty) = qty {
                    let avg_cost = basis_for(
                        shared, con_id,
                        avg_cost_raw.map(crate::types::price_from_f64), qty,
                    );
                    shared.portfolio.set_position_info(PositionInfo {
                        con_id,
                        position: qty,
                        avg_cost,
                        symbol: std::mem::take(&mut symbol),
                        sec_type: std::mem::take(&mut sec_type),
                        ..Default::default()
                    });
                    if let Some(instrument) = context.market.instrument_by_con_id(con_id) {
                        adopt_position(context, instrument, qty);
                        shared.portfolio.set_position(instrument, qty);
                        emit(event_tx, Event::PositionUpdate { instrument, con_id, position: qty, avg_cost });
                    }
                }
                shared.portfolio.note_restated(con_id);
                self.auto_fetch_secdef_if_cold(con_id, ccp_conn, shared, hb);
            }
            con_id = v.parse().unwrap_or(0);
            qty = None;
            avg_cost_raw = None;
            symbol.clear();
            sec_type.clear();
            count += 1;
        } else if let Some(v) = part.strip_prefix("6068=") {
            // The entry names its own contract. Dropped here, a holding reached
            // the caller carrying a contract id and nothing else, and stayed
            // that way until a definition lookup answered.
            symbol = v.trim_end().to_string();
        } else if let Some(v) = part.strip_prefix("167=") {
            sec_type = v.to_string();
        } else if let Some(v) = part.strip_prefix("6064=") {
            // Filtered to finite: `"NaN".parse()` succeeds and `NaN as i64`
            // is 0, which would flatten by the same route.
            qty = v.parse::<f64>().ok().filter(|f| f.is_finite());
        } else if let Some(v) = part.strip_prefix("6101=") {
            avg_cost_raw = v.parse::<f64>().ok().filter(|f| f.is_finite());
        }
    }
    // Flush last position
    if count > 0 && con_id != 0 {
        if let Some(qty) = qty {
            let avg_cost = basis_for(
                shared, con_id,
                avg_cost_raw.map(crate::types::price_from_f64), qty,
            );
            // With what the frame said it is, as every holding before it in
            // the same frame is published. Left off, the last one is published
            // unnamed and stays that way until the venue's own description of
            // the contract arrives.
            shared.portfolio.set_position_info(PositionInfo {
                con_id,
                position: qty,
                avg_cost,
                symbol: std::mem::take(&mut symbol),
                sec_type: std::mem::take(&mut sec_type),
                ..Default::default()
            });
            if let Some(instrument) = context.market.instrument_by_con_id(con_id) {
                adopt_position(context, instrument, qty);
                shared.portfolio.set_position(instrument, qty);
                emit(event_tx, Event::PositionUpdate { instrument, con_id, position: qty, avg_cost });
            }
        }
        shared.portfolio.note_restated(con_id);
        self.auto_fetch_secdef_if_cold(con_id, ccp_conn, shared, hb);
    }
    }
}

/// One message per holding a position frame carries.
///
/// A position frame is a repeating group: one message states as many holdings
/// as the account has, and a holding begins at its symbol. A flat parse keeps
/// only the last value of each tag, so only the holding the venue lists last
/// is reported and a fill on any other leaves that position looking frozen.
pub(crate) fn split_position_entries(msg: &[u8]) -> Vec<HashMap<u32, String>> {
    /// A holding begins where it names itself.
    const SYMBOL: u32 = 6068;
    crate::protocol::fix::fix_parse_repeating(msg, SYMBOL)
}
