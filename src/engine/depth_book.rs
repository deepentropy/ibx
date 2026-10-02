//! Depth books and the depth callbacks they give, as the reference builds
//! them (#451).
//!
//! - `DeepBook`: the book of one (contract, exchange). The farm's entries
//!   are applied by position; with the user book on (the session's logon
//!   says `demo`, as on paper), the book shown is diffed with the new one
//!   by index after each group, else each entry is a change at its own
//!   position.
//! - `SingleView`: the callbacks of a request on one book: the whole book
//!   (up to its rows) at its first data, then the changes inside the rows,
//!   with the row that leaves or enters the last row.
//! - `SmartMerge`: SmartDepth: the rows of all component books and tops of
//!   book, sorted, and the first rows compared before and after each
//!   change: deletes and inserts at the tail, updates in place.

use std::cmp::Ordering;
use std::sync::Arc;

use crate::protocol::depth_decoder::{DepthEntry, Side, OP_DELETE_BID, OP_DELETE_ASK, OP_INSERT, OP_UPDATE};
use crate::types::{DepthUpdate, ReqId};

/// Text of the reset error (317).
pub const RESET_TEXT: &str = "Market depth data has been RESET. Please empty deep book contents before applying any new entries.";

#[inline(always)]
fn idx(side: Side) -> usize {
    match side { Side::Bid => 0, Side::Ask => 1 }
}

/// Price and size units of a book or top-of-book entry: its minimum tick
/// (from the acknowledgement) and the size of one wire unit.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Scale {
    tick: f64,
    tick_num: i64,
    tick_den: i64,
    size_unit: f64,
}

impl Scale {
    /// `tick` the minimum price tick, `size_unit` the size increment times
    /// the round lot.
    pub(crate) fn new(tick: f64, size_unit: f64) -> Self {
        let tick = if tick.is_finite() && tick > 0.0 { tick } else { 0.01 };
        // The tick as a decimal fraction, for prices as the reference
        // writes them.
        let (mut num, mut den) = (tick, 1i64);
        while (num - num.round()).abs() > 1e-9 * num.max(1.0) && den < 10_000_000_000 {
            num *= 10.0;
            den *= 10;
        }
        Self { tick, tick_num: num.round() as i64, tick_den: den, size_unit }
    }

    /// The price of a book row as the reference holds it: ticks times the
    /// tick, the order of rows of equal written price follows it.
    #[inline]
    fn book_key(&self, ticks: i64) -> f64 {
        ticks as f64 * self.tick
    }

    /// A price as written: the nearest value to the exact decimal.
    #[inline]
    fn written(&self, ticks: i64) -> f64 {
        (ticks as f64 * self.tick_num as f64) / self.tick_den as f64
    }

    #[inline]
    fn size(&self, units: i64) -> f64 {
        units as f64 * self.size_unit
    }
}

/// One row of a book.
#[derive(Debug, Clone)]
pub(crate) struct Row {
    /// Price as the reference holds it (ordering).
    key: f64,
    /// Price as written.
    price: f64,
    size: f64,
    market_maker: String,
    /// The size value this row carries: a new one for each size the
    /// farm sends, kept when the row is copied. The shown book compares
    /// rows by it, as the reference compares its size objects.
    size_id: u64,
}

/// One change of a book, in the order the API gets it.
#[derive(Debug, Clone)]
pub(crate) struct Change {
    pub(crate) position: i32,
    /// 0 insert, 1 update, 2 delete.
    pub(crate) op: i32,
    pub(crate) side: Side,
    pub(crate) price: f64,
    pub(crate) size: f64,
    pub(crate) market_maker: String,
}

/// The farm sent an entry the book cannot take: the reference resets it.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Refused;

/// The book of one contract on one exchange.
#[derive(Debug, Clone)]
pub(crate) struct DeepBook {
    /// The user book is on (logon `6247=demo`): the shown book is diffed.
    user_book: bool,
    scale: Scale,
    /// Rows from the farm, bid then ask.
    market: [Vec<Row>; 2],
    /// Rows shown (with the user book on).
    shown: [Vec<Row>; 2],
    /// The side was marked empty by the farm.
    empty: [bool; 2],
    next_size_id: u64,
}

impl DeepBook {
    pub(crate) fn new(user_book: bool, scale: Scale) -> Self {
        Self {
            user_book, scale, market: [Vec::new(), Vec::new()], shown: [Vec::new(), Vec::new()],
            empty: [false; 2], next_size_id: 1,
        }
    }

    /// The rows the API sees, of one side.
    #[inline]
    pub(crate) fn rows(&self, side: Side) -> &[Row] {
        if self.user_book { &self.shown[idx(side)] } else { &self.market[idx(side)] }
    }

    /// Drop every row (reset).
    pub(crate) fn clear(&mut self) {
        for side in 0..2 {
            self.market[side].clear();
            self.shown[side].clear();
            self.empty[side] = false;
        }
    }

    #[inline]
    fn size_id(&mut self) -> u64 {
        self.next_size_id += 1;
        self.next_size_id
    }

    /// Apply the entries of one group; the changes the API is told of.
    /// An update of a missing row (or an insert at a negative position)
    /// refuses the group: the caller resets the book.
    pub(crate) fn apply(&mut self, entries: &[DepthEntry], changes: &mut Vec<Change>) -> Result<(), Refused> {
        changes.clear();
        let mut touched = [false; 2];
        for e in entries {
            let side = e.book_side();
            touched[idx(side)] = true;
            let mut op = e.op;
            // The farm marks an empty side with a negative price and no size
            // at position 0; an update at 0 after it is an insert.
            if (op == OP_INSERT || op == OP_UPDATE) && e.position == 0 {
                if e.price.is_some_and(|p| p < 0) && e.size == Some(0) {
                    self.empty[idx(side)] = true;
                    if op == OP_INSERT { continue; }
                } else if op == OP_UPDATE && self.empty[idx(side)] {
                    op = OP_INSERT;
                    self.empty[idx(side)] = false;
                }
            }
            let row = self.apply_entry(e, op, side)?;
            if (op == OP_INSERT || op == OP_UPDATE) && e.position == 0 && e.price.is_some_and(|p| p < 0) && e.size == Some(0) {
                let rows = &mut self.market[idx(side)];
                if rows.len() == 1 && rows[0].size == 0.0 && rows[0].key == -1.0 {
                    rows.clear();
                    self.empty[idx(side)] = true;
                }
            }
            if !self.user_book {
                // Each entry is a change at its own position.
                let op = if op >= OP_DELETE_BID { 2 } else { i32::from(op) };
                let (price, size) = row.map_or((0.0, 0.0), |r| (r.price, r.size));
                changes.push(Change { position: e.position, op, side, price, size, market_maker: e.market_maker.clone() });
            }
        }
        if self.user_book {
            for side in [Side::Bid, Side::Ask] {
                if touched[idx(side)] {
                    self.diff_side(side, changes);
                }
            }
        }
        Ok(())
    }

    /// One entry on the farm rows; the row it leaves for an insert or an
    /// update.
    fn apply_entry(&mut self, e: &DepthEntry, op: u8, side: Side) -> Result<Option<Row>, Refused> {
        let pos = e.position;
        match op {
            OP_DELETE_BID | OP_DELETE_ASK => {
                let rows = &mut self.market[idx(side)];
                // A position out of the book is ignored.
                if pos >= 0 && (pos as usize) < rows.len() {
                    rows.remove(pos as usize);
                }
                Ok(None)
            }
            OP_INSERT => {
                if pos < 0 {
                    return Err(Refused);
                }
                let size_id = self.size_id();
                let row = Row {
                    key: e.price.map_or(0.0, |p| self.scale.book_key(p)),
                    price: e.price.map_or(0.0, |p| self.scale.written(p)),
                    size: e.size.map_or(0.0, |s| self.scale.size(s)),
                    market_maker: e.market_maker.clone(),
                    size_id,
                };
                let rows = &mut self.market[idx(side)];
                // Past the end, the row is dropped.
                if (pos as usize) <= rows.len() {
                    rows.insert(pos as usize, row.clone());
                }
                Ok(Some(row))
            }
            _ => {
                let size_id = if e.size.is_some() { self.size_id() } else { 0 };
                let scale = self.scale;
                let rows = &mut self.market[idx(side)];
                let Some(row) = usize::try_from(pos).ok().and_then(|p| rows.get_mut(p)) else {
                    return Err(Refused);
                };
                if let Some(s) = e.size {
                    row.size = scale.size(s);
                    row.size_id = size_id;
                }
                if let Some(p) = e.price {
                    row.key = scale.book_key(p);
                    row.price = scale.written(p);
                }
                Ok(Some(row.clone()))
            }
        }
    }

    /// Compare the shown side with the farm side by index: changed rows
    /// are updates, extra rows inserts, missing rows deletes at their old
    /// index; the farm side is then shown.
    fn diff_side(&mut self, side: Side, changes: &mut Vec<Change>) {
        let i = idx(side);
        let old = &self.shown[i];
        let new = &self.market[i];
        let mut o = 0;
        let mut n = 0;
        loop {
            match (old.get(o), new.get(n)) {
                (None, None) => break,
                (Some(row), None) => {
                    changes.push(Change {
                        position: o as i32, op: 2, side, price: 0.0, size: 0.0,
                        market_maker: row.market_maker.clone(),
                    });
                    o += 1;
                }
                (None, Some(row)) => {
                    changes.push(change(n, 0, side, row));
                    n += 1;
                }
                (Some(a), Some(b)) => {
                    if a.key.total_cmp(&b.key) != Ordering::Equal || a.size_id != b.size_id || a.market_maker != b.market_maker {
                        changes.push(change(n, 1, side, b));
                    }
                    o += 1;
                    n += 1;
                }
            }
        }
        self.shown[i].clone_from(&self.market[i]);
    }
}

#[inline]
fn change(position: usize, op: i32, side: Side, row: &Row) -> Change {
    Change { position: position as i32, op, side, price: row.price, size: row.size, market_maker: row.market_maker.clone() }
}

/// The callbacks of a request on one book.
#[derive(Debug, Clone)]
pub(crate) struct SingleView {
    req_id: ReqId,
    num_rows: i32,
    /// updateMktDepthL2 (the book has the market-maker service), else
    /// updateMktDepth.
    l2: bool,
    /// The whole book was sent.
    started: bool,
}

impl SingleView {
    pub(crate) fn new(req_id: ReqId, num_rows: i32, l2: bool) -> Self {
        Self { req_id, num_rows, l2, started: false }
    }

    /// The book was reset: the next data sends the whole book again.
    pub(crate) fn reset(&mut self) {
        self.started = false;
    }

    fn push(&self, out: &mut Vec<DepthUpdate>, position: i32, op: i32, side: Side, (price, size): (f64, f64), mm: &str) {
        // A delete is written without price and size.
        let (price, size) = if op == 2 { (0.0, 0.0) } else { (price, size) };
        out.push(DepthUpdate {
            req_id: self.req_id, position, market_maker: if self.l2 { mm.to_string() } else { String::new() },
            operation: op, side: side.api(), price, size, is_smart_depth: false, l2: self.l2,
        });
    }

    fn push_row(&self, out: &mut Vec<DepthUpdate>, position: i32, op: i32, side: Side, row: &Row) {
        self.push(out, position, op, side, (row.price, row.size), &row.market_maker);
    }

    /// The book changed with `changes`: the whole book the first time,
    /// else the changes inside the rows asked for. An insert inside the
    /// book first deletes the row that leaves the last row; a delete then
    /// inserts the row that enters it.
    pub(crate) fn on_change(&mut self, book: &DeepBook, changes: &[Change], out: &mut Vec<DepthUpdate>) {
        if !self.started {
            self.started = true;
            for side in [Side::Bid, Side::Ask] {
                for (k, row) in book.rows(side).iter().take(self.num_rows.max(0) as usize).enumerate() {
                    self.push_row(out, k as i32, 0, side, row);
                }
            }
            return;
        }
        let last = self.num_rows - 1;
        for (i, c) in changes.iter().enumerate() {
            if c.position >= self.num_rows {
                continue;
            }
            let rows = book.rows(c.side);
            if c.op == 0 && (c.position as usize) < rows.len()
                && let Some(row) = row_at_last(changes, rows, c.side, last, i)
            {
                self.push_row(out, last, 2, c.side, row);
            }
            self.push(out, c.position, c.op, c.side, (c.price, c.size), &c.market_maker);
            if c.op == 2 && let Some(row) = row_at_last(changes, rows, c.side, last, i) {
                self.push_row(out, last, 0, c.side, row);
            }
        }
    }
}

/// The row at index `last` of the book as it is after change `i`: the
/// later changes of the side move it (an insert at or above it +1, a
/// delete above it -1), as the reference walks them.
fn row_at_last<'a>(changes: &[Change], rows: &'a [Row], side: Side, last: i32, i: usize) -> Option<&'a Row> {
    let mut n = last;
    for c in changes[i + 1..].iter().filter(|c| c.side == side) {
        if c.op == 0 {
            if c.position <= n { n += 1; }
        } else if c.op == 2 && c.position < n {
            n -= 1;
        }
    }
    usize::try_from(n).ok().and_then(|n| rows.get(n))
}

/// The bid and ask of an exchange's top of book, in wire units.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct TopQuote {
    pub(crate) bid: Option<i64>,
    pub(crate) bid_size: Option<i64>,
    pub(crate) ask: Option<i64>,
    pub(crate) ask_size: Option<i64>,
}

impl TopQuote {
    /// The row of one side, when its price and a size above zero are
    /// known.
    fn row(&self, side: Side) -> Option<(i64, i64)> {
        let (p, s) = match side {
            Side::Bid => (self.bid, self.bid_size),
            Side::Ask => (self.ask, self.ask_size),
        };
        match (p, s) {
            (Some(p), Some(s)) if s > 0 => Some((p, s)),
            _ => None,
        }
    }
}

/// One row of the SmartDepth book.
#[derive(Debug, Clone)]
struct SmartRow {
    key: f64,
    price: f64,
    size: f64,
    exchange: Arc<str>,
    market_maker: String,
}

impl SmartRow {
    /// The name the row is sorted and written with: its market maker,
    /// else its exchange.
    #[inline]
    fn name(&self) -> &str {
        if self.market_maker.is_empty() { &self.exchange } else { &self.market_maker }
    }

    #[inline]
    fn same(&self, o: &SmartRow) -> bool {
        self.key.total_cmp(&o.key) == Ordering::Equal && self.size == o.size
            && self.exchange.eq_ignore_ascii_case(&o.exchange) && self.name().eq_ignore_ascii_case(o.name())
    }
}

/// Case-insensitive order of names, as the reference compares them.
fn cmp_names(a: &str, b: &str) -> Ordering {
    a.bytes().map(|c| c.to_ascii_lowercase()).cmp(b.bytes().map(|c| c.to_ascii_lowercase()))
}

/// The SmartDepth book of a request.
#[derive(Debug, Clone)]
pub(crate) struct SmartMerge {
    req_id: ReqId,
    num_rows: usize,
    rows: [Vec<SmartRow>; 2],
    before: Vec<SmartRow>,
}

impl SmartMerge {
    pub(crate) fn new(req_id: ReqId, num_rows: i32) -> Self {
        Self { req_id, num_rows: num_rows.max(0) as usize, rows: [Vec::new(), Vec::new()], before: Vec::new() }
    }

    /// Insert a row in its place: bids by price down, asks by price up,
    /// equal prices by name; a row equal to one there is not added.
    fn add(&mut self, side: Side, row: SmartRow) {
        let rows = &mut self.rows[idx(side)];
        if rows.iter().any(|r| r.same(&row)) {
            return;
        }
        let mut n = 0;
        while n < rows.len() {
            let e = &rows[n];
            let c = e.key.total_cmp(&row.key);
            if side == Side::Bid && c == Ordering::Less { break; }
            if side == Side::Ask && c == Ordering::Greater { break; }
            if c == Ordering::Equal && cmp_names(e.name(), row.name()) == Ordering::Greater { break; }
            n += 1;
        }
        rows.insert(n, row);
    }

    /// The rows of an exchange's book replace its earlier rows.
    pub(crate) fn set_book(&mut self, exchange: &Arc<str>, book: &DeepBook, out: &mut Vec<DepthUpdate>) {
        self.change(out, |m| {
            for side in [Side::Bid, Side::Ask] {
                m.rows[idx(side)].retain(|r| *r.exchange != **exchange);
                for r in book.rows(side) {
                    m.add(side, SmartRow {
                        key: r.key, price: r.price, size: r.size, exchange: exchange.clone(), market_maker: r.market_maker.clone(),
                    });
                }
            }
        });
    }

    /// The bid and ask of an exchange's top of book replace its rows.
    pub(crate) fn set_top(&mut self, exchange: &Arc<str>, quote: &TopQuote, scale: &Scale, out: &mut Vec<DepthUpdate>) {
        self.change(out, |m| {
            for side in [Side::Bid, Side::Ask] {
                m.rows[idx(side)].retain(|r| *r.exchange != **exchange);
                if let Some((p, s)) = quote.row(side) {
                    let price = scale.written(p);
                    m.add(side, SmartRow { key: price, price, size: scale.size(s), exchange: exchange.clone(), market_maker: String::new() });
                }
            }
        });
    }

    /// Apply `f`, then compare the first rows of each side before and
    /// after it: deletes from the tail, inserts at the tail, then updates
    /// of the rows that changed; bids first.
    fn change(&mut self, out: &mut Vec<DepthUpdate>, f: impl FnOnce(&mut Self)) {
        let mut before = std::mem::take(&mut self.before);
        before.clear();
        let k = self.num_rows;
        let bid_len = self.rows[0].len().min(k);
        before.extend_from_slice(&self.rows[0][..bid_len]);
        before.extend_from_slice(&self.rows[1][..self.rows[1].len().min(k)]);
        f(self);
        for side in [Side::Bid, Side::Ask] {
            let old = if side == Side::Bid { &before[..bid_len] } else { &before[bid_len..] };
            let new = &self.rows[idx(side)][..self.rows[idx(side)].len().min(k)];
            for n in (new.len()..old.len()).rev() {
                self.push(out, n, 2, side, &old[n]);
            }
            for (n, row) in new.iter().enumerate().skip(old.len()) {
                self.push(out, n, 0, side, row);
            }
            for n in 0..old.len().min(new.len()) {
                if !new[n].same(&old[n]) {
                    self.push(out, n, 1, side, &new[n]);
                }
            }
        }
        self.before = before;
    }

    fn push(&self, out: &mut Vec<DepthUpdate>, position: usize, op: i32, side: Side, row: &SmartRow) {
        // A delete is written without price and size.
        let (price, size) = if op == 2 { (0.0, 0.0) } else { (row.price, row.size) };
        out.push(DepthUpdate {
            req_id: self.req_id, position: position as i32, market_maker: row.name().to_string(),
            operation: op, side: side.api(), price, size, is_smart_depth: true, l2: true,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(op: u8, side: Side, pos: i32, price: Option<i64>, size: Option<i64>) -> DepthEntry {
        DepthEntry { op, position: pos, market_maker: String::new(), side: Some(side), price, size }
    }

    fn short(u: &DepthUpdate) -> (i32, i32, i32, f64, f64) {
        (u.position, u.operation, u.side, u.price, u.size)
    }

    // #451: with the user book on (paper), an insert in the middle reaches
    // the API as updates of the rows below and an insert at the tail; the
    // rows past the ones asked for are not sent.
    #[test]
    fn user_book_diffs_by_index() {
        let mut book = DeepBook::new(true, Scale::new(0.01, 40.0));
        let mut view = SingleView::new(7, 3, false);
        let mut changes = Vec::new();
        let mut out = Vec::new();
        let rows: Vec<DepthEntry> = (0..3).map(|i| entry(0, Side::Bid, i, Some(100 - i as i64), Some(1))).collect();
        book.apply(&rows, &mut changes).unwrap();
        view.on_change(&book, &changes, &mut out);
        assert_eq!(out.iter().map(short).collect::<Vec<_>>(), [(0, 0, 1, 1.0, 40.0), (1, 0, 1, 0.99, 40.0), (2, 0, 1, 0.98, 40.0)]);
        out.clear();
        book.apply(&[entry(0, Side::Bid, 0, Some(101), Some(2))], &mut changes).unwrap();
        view.on_change(&book, &changes, &mut out);
        assert_eq!(out.iter().map(short).collect::<Vec<_>>(), [(0, 1, 1, 1.01, 80.0), (1, 1, 1, 1.0, 40.0), (2, 1, 1, 0.99, 40.0)]);
    }

    // #451: rows are compared by their size value, not the number: a row
    // whose size the farm sent again is an update, a price update keeps
    // the size of the row.
    #[test]
    fn user_book_size_resent_is_an_update() {
        let mut book = DeepBook::new(true, Scale::new(0.01, 1.0));
        let mut changes = Vec::new();
        book.apply(&[entry(0, Side::Ask, 0, Some(5), Some(3))], &mut changes).unwrap();
        book.apply(&[entry(1, Side::Ask, 0, None, Some(3))], &mut changes).unwrap();
        assert_eq!(changes.len(), 1);
        book.apply(&[entry(1, Side::Ask, 0, Some(5), None)], &mut changes).unwrap();
        assert!(changes.is_empty());
    }

    // #451: an update of a missing row refuses the group.
    #[test]
    fn update_of_a_missing_row_is_refused() {
        let mut book = DeepBook::new(true, Scale::new(0.01, 1.0));
        let mut changes = Vec::new();
        assert_eq!(book.apply(&[entry(1, Side::Bid, 0, None, Some(1))], &mut changes), Err(Refused));
    }

    // #451: without the user book, each entry is a change at its server
    // position; an insert inside the rows first deletes the row that
    // leaves the last row, a delete then inserts the row that enters it.
    #[test]
    fn server_positions_with_rows_at_the_last_row() {
        let mut book = DeepBook::new(false, Scale::new(1.0, 1.0));
        let mut view = SingleView::new(1, 2, true);
        let mut changes = Vec::new();
        let mut out = Vec::new();
        book.apply(&[entry(0, Side::Bid, 0, Some(10), Some(1)), entry(0, Side::Bid, 1, Some(9), Some(1)), entry(0, Side::Bid, 2, Some(8), Some(1))], &mut changes).unwrap();
        view.on_change(&book, &changes, &mut out);
        out.clear();
        book.apply(&[entry(0, Side::Bid, 0, Some(11), Some(1))], &mut changes).unwrap();
        view.on_change(&book, &changes, &mut out);
        assert_eq!(out.iter().map(short).collect::<Vec<_>>(), [(1, 2, 1, 0.0, 0.0), (0, 0, 1, 11.0, 1.0)]);
        out.clear();
        book.apply(&[entry(2, Side::Bid, 0, None, None)], &mut changes).unwrap();
        view.on_change(&book, &changes, &mut out);
        assert_eq!(out.iter().map(short).collect::<Vec<_>>(), [(0, 2, 1, 0.0, 0.0), (1, 0, 1, 9.0, 1.0)]);
        assert!(out.iter().all(|u| u.l2));
    }

    // #451: SmartDepth: a better price from another exchange is an insert
    // at the tail and updates in place; equal prices sort by name.
    #[test]
    fn smart_merge_tail_insert_and_updates() {
        let mut m = SmartMerge::new(3, 5);
        let scale = Scale::new(0.01, 1.0);
        let mut out = Vec::new();
        let memx: Arc<str> = Arc::from("MEMX");
        let edge: Arc<str> = Arc::from("DRCTEDGE");
        m.set_top(&memx, &TopQuote { bid: Some(33900), bid_size: Some(1), ..Default::default() }, &scale, &mut out);
        assert_eq!(out.iter().map(|u| (u.position, u.operation, u.market_maker.as_str())).collect::<Vec<_>>(), [(0, 0, "MEMX")]);
        out.clear();
        m.set_top(&edge, &TopQuote { bid: Some(33989), bid_size: Some(2), ..Default::default() }, &scale, &mut out);
        assert_eq!(out.iter().map(|u| (u.position, u.operation, u.market_maker.as_str(), u.price)).collect::<Vec<_>>(),
            [(1, 0, "MEMX", 339.0), (0, 1, "DRCTEDGE", 339.89)]);
        out.clear();
        m.set_top(&edge, &TopQuote { bid: Some(33900), bid_size: Some(2), ..Default::default() }, &scale, &mut out);
        assert_eq!(out.iter().map(|u| (u.position, u.operation, u.market_maker.as_str())).collect::<Vec<_>>(), [(0, 1, "DRCTEDGE")]);
        out.clear();
        m.set_top(&edge, &TopQuote { bid: Some(33900), bid_size: Some(0), ..Default::default() }, &scale, &mut out);
        assert_eq!(out.iter().map(|u| (u.position, u.operation, u.market_maker.as_str(), u.price)).collect::<Vec<_>>(),
            [(1, 2, "MEMX", 0.0), (0, 1, "MEMX", 339.0)]);
        assert!(out.iter().all(|u| u.is_smart_depth && u.l2));
    }

    #[test]
    fn scale_writes_decimal_prices() {
        let s = Scale::new(0.01, 40.0);
        assert_eq!(s.written(34165), 341.65);
        assert_eq!(s.book_key(34165), 34165.0 * 0.01);
        assert_eq!(s.size(3), 120.0);
        assert_eq!(Scale::new(0.005, 1.0).written(3), 0.015);
    }
}
