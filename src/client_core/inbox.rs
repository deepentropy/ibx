//! The answers of the session in the order they were written (ibx#532).
//!
//! The reference gives its client the messages of one connection, in the
//! order they came. ibx keeps its answers in a queue per kind; each answer
//! takes the next count of the session's answer clock when it is written
//! (`bridge::SeqQueue`). A client takes every queue into its [`Inbox`] and
//! gives the answers in the order of their counts: a run of one kind at a
//! time, up to the first answer of another kind written after it.
//!
//! An answer is given only when every answer written before it is in the
//! inbox: the clock is read before the queues are taken, and an answer
//! with a count at or above it waits for the next pass (another one with a
//! smaller count may still be on its way into its queue).

use std::collections::VecDeque;

use crate::bridge::{FillExec, OrderReport, SharedState};
use crate::control::contracts::{ContractDefinition, SymbolMatch};
use crate::control::histogram::HistogramEntry;
use crate::control::historical::{HeadTimestampResponse, HistoricalBar, HistoricalResponse};
use crate::control::news::NewsHeadline;
use crate::control::scanner::ScannerResult;
use crate::types::*;

macro_rules! answer_kinds {
    ($($kind:ident: $ty:ty = $($path:ident).+;)*) => {
        /// The kinds of answers, one per queue.
        #[allow(non_camel_case_types)]
        #[derive(Clone, Copy, PartialEq, Eq, Debug)]
        pub(crate) enum Kind { $($kind),* }

        /// The answers taken from the queues and not given yet, with their
        /// counts.
        #[derive(Default)]
        pub(crate) struct Inbox {
            $(pub(crate) $kind: VecDeque<(u64, $ty)>,)*
            /// The clock read at the last take of every queue: while it
            /// has not moved, no queue holds an answer not taken yet.
            taken_at: u64,
        }

        impl Inbox {
            /// Take every queue of the session.
            fn take_queues(&mut self, shared: &SharedState) {
                $(shared.$($path).+.drain_into(&mut self.$kind);)*
            }

            /// Put back the answers of `other`, each kind in the order of
            /// its counts. When this one is empty (no pass ran meanwhile),
            /// `other` takes its place with its buffers.
            pub(crate) fn merge(&mut self, other: Inbox) {
                let taken_at = self.taken_at.min(other.taken_at);
                if $(self.$kind.is_empty())&&* {
                    *self = other;
                } else {
                    $(merge_queue(&mut self.$kind, other.$kind);)*
                }
                self.taken_at = taken_at;
            }

            /// The kind with the smallest first count and that count, and
            /// the smallest first count of the other kinds (`u64::MAX` when
            /// none), in one look at each queue (ibx#661).
            fn first_two(&self) -> (Option<(Kind, u64)>, u64) {
                let mut first: Option<(Kind, u64)> = None;
                let mut second = u64::MAX;
                $(
                    if let Some(&(at, _)) = self.$kind.front() {
                        match first {
                            Some((_, f)) if at >= f => second = second.min(at),
                            _ => {
                                second = first.map_or(second, |(_, f)| f);
                                first = Some((Kind::$kind, at));
                            }
                        }
                    }
                )*
                (first, second)
            }
        }
    };
}

answer_kinds! {
    fills: (Fill, FillExec) = orders.fills;
    untracked_executions: (crate::api::types::Contract, crate::api::types::Execution, FillExec) = orders.untracked_executions;
    forgotten_orders: OrderId = orders.forgotten_orders;
    commission_reports: crate::api::types::CommissionAndFeesReport = orders.commission_reports;
    order_errors: (i64, i64, String) = orders.order_errors;
    order_reports: OrderReport = orders.order_reports;
    what_if_responses: WhatIfResponse = orders.what_if_responses;
    completed_orders: crate::bridge::CompletedRow = orders.completed_rows;
    historical_ticks: (ReqId, HistoricalTickData, String, bool) = reference.historical_ticks;
    tbt_errors: (ReqId, i32, String) = market.tbt_errors;
    tbt_trades: TbtTrade = market.tbt_trades;
    tbt_quotes: TbtQuote = market.tbt_quotes;
    tbt_mid_points: TbtMidPoint = market.tbt_mid_points;
    depth_updates: DepthUpdate = market.depth_updates;
    tick_news: TickNews = market.tick_news;
    historical_errors: (ReqId, i32, String) = reference.historical_errors;
    historical_data: (ReqId, HistoricalResponse) = reference.historical_data;
    historical_updates: (ReqId, HistoricalBar) = reference.historical_updates;
    head_timestamps: (ReqId, HeadTimestampResponse) = reference.head_timestamps;
    contract_details: (ReqId, ContractDefinition) = reference.contract_details;
    contract_details_end: ReqId = reference.contract_details_end;
    matching_symbols: (ReqId, Vec<SymbolMatch>) = reference.matching_symbols;
    option_chains: (ReqId, Vec<crate::control::optparams::OptionChain>) = reference.option_chains;
    scanner_data: (ReqId, ScannerResult) = reference.scanner_data;
    historical_news: (ReqId, Vec<NewsHeadline>, bool) = reference.historical_news;
    news_articles: (ReqId, i32, String) = reference.news_articles;
    option_computations: crate::control::optcalc::OptionComputation = reference.option_computations;
    fundamental_data: (ReqId, String) = reference.fundamental_data;
    histogram_data: (ReqId, Vec<HistogramEntry>) = reference.histogram_data;
    real_time_bars: (ReqId, RealTimeBar) = market.real_time_bars;
    historical_schedules: (ReqId, HistoricalScheduleResponse) = reference.historical_schedules;
    wsh_answers: crate::client_core::WshOut = reference.wsh_answers;
    connection_notices: (i64, String) = connection_notices;
}

impl Inbox {
    /// Take the answers written so far; returns the count below which
    /// every answer is in the inbox.
    pub(crate) fn take(&mut self, shared: &SharedState) -> u64 {
        // Read first: an answer counted before it is in its queue by then.
        let written = shared.answers_written();
        // Every answer takes its count under its queue's lock: a clock that
        // has not moved since the last take means no queue has anything new
        // (ibx#661), and the locks of the 31 queues are spared.
        if written != self.taken_at {
            self.take_queues(shared);
            self.taken_at = written;
        }
        written
    }

    /// The next run to give: its kind and the count it stops before (the
    /// first answer of another kind, or `written`). None when no answer
    /// below `written` waits.
    #[cfg(test)]
    pub(crate) fn next_run(&self, written: u64) -> Option<(Kind, u64)> {
        match self.next(written, None)? {
            Run::Queue(kind, until) => Some((kind, until)),
            Run::Client(_) | Run::State(_) => None,
        }
    }

    /// [`next_run`](Self::next_run) with the client's own answers, whose
    /// first count is `client`.
    #[cfg(test)]
    pub(crate) fn next(&self, written: u64, client: Option<u64>) -> Option<Run> {
        self.next_with_state(written, client, None)
    }

    /// [`next`](Self::next) with the answers read from the session's
    /// state, whose first count is `state` (ibx#662): a run of them when
    /// they come before a queued answer or one of the client's own. When
    /// nothing else waits they are left to the end of the pass.
    pub(crate) fn next_with_state(&self, written: u64, client: Option<u64>, state: Option<u64>) -> Option<Run> {
        let (queue, others) = self.first_two();
        let q = queue.map(|(_, at)| at);
        let first = match (q, client) {
            (Some(q), Some(c)) => q.min(c),
            (Some(q), None) => q,
            (None, Some(c)) => c,
            (None, None) => return None,
        };
        if first >= written {
            return None;
        }
        let state = state.unwrap_or(u64::MAX);
        if state < first {
            return Some(Run::State(first));
        }
        match queue {
            Some((kind, at)) if at == first => {
                let other = client.map_or(others, |c| c.min(others));
                Some(Run::Queue(kind, other.min(state).min(written)))
            }
            _ => Some(Run::Client(q.unwrap_or(u64::MAX).min(state).min(written))),
        }
    }
}

/// What to give next: a run of a queue, of the client's own answers, or of
/// the answers read from the session's state, up to the count it stops
/// before.
pub(crate) enum Run {
    Queue(Kind, u64),
    Client(u64),
    State(u64),
}

fn merge_queue<T>(into: &mut VecDeque<(u64, T)>, from: VecDeque<(u64, T)>) {
    if from.is_empty() {
        return;
    }
    into.extend(from);
    into.make_contiguous().sort_by_key(|(at, _)| *at);
}

/// The answers of a queue counted below `until`, in their order.
pub(crate) fn run<T>(queue: &mut VecDeque<(u64, T)>, until: u64) -> impl Iterator<Item = T> + '_ {
    run_counted(queue, until).map(|(_, item)| item)
}

/// [`run`], each answer with its count.
pub(crate) fn run_counted<T>(queue: &mut VecDeque<(u64, T)>, until: u64) -> impl Iterator<Item = (u64, T)> + '_ {
    let n = queue.iter().take_while(|(at, _)| *at < until).count();
    queue.drain(..n)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runs_follow_the_counts_across_the_queues() {
        let shared = SharedState::new();
        shared.reference.push_contract_details_end(1);
        shared.orders.push_order_error(2, 200, "a".into());
        shared.orders.push_order_error(3, 200, "b".into());
        shared.reference.push_contract_details_end(4);
        let mut inbox = Inbox::default();
        let written = inbox.take(&shared);
        let mut got = Vec::new();
        while let Some((kind, until)) = inbox.next_run(written) {
            match kind {
                Kind::contract_details_end => got.extend(run(&mut inbox.contract_details_end, until).into_iter().map(|r| format!("end {r}"))),
                Kind::order_errors => got.extend(run(&mut inbox.order_errors, until).into_iter().map(|(id, ..)| format!("error {id}"))),
                other => panic!("{other:?}"),
            }
        }
        assert_eq!(got, ["end 1", "error 2", "error 3", "end 4"]);
    }

    // ibx#661: the next run is found in one look at the queues; three kinds
    // and the client's own answers interleaved keep their order, and a pass
    // with no answer written since the last one takes nothing.
    #[test]
    fn three_kinds_and_the_clients_answers_keep_their_order() {
        let shared = SharedState::new();
        shared.reference.push_contract_details_end(1);
        shared.orders.push_order_error(2, 200, "a".into());
        let client = shared.answer_stamp();
        shared.orders.push_forgotten_order(3);
        shared.reference.push_contract_details_end(4);
        shared.orders.push_order_error(5, 200, "b".into());
        let mut inbox = Inbox::default();
        let written = inbox.take(&shared);
        let mut got = Vec::new();
        let mut client_left = Some(client);
        while let Some(next) = inbox.next(written, client_left) {
            match next {
                Run::State(_) => unreachable!(),
                Run::Client(until) => {
                    assert!(client < until);
                    got.push("client".to_string());
                    client_left = None;
                }
                Run::Queue(Kind::contract_details_end, until) => got.extend(run(&mut inbox.contract_details_end, until).map(|r| format!("end {r}"))),
                Run::Queue(Kind::order_errors, until) => got.extend(run(&mut inbox.order_errors, until).map(|(id, ..)| format!("error {id}"))),
                Run::Queue(Kind::forgotten_orders, until) => got.extend(run(&mut inbox.forgotten_orders, until).map(|id| format!("forgotten {id}"))),
                Run::Queue(other, _) => panic!("{other:?}"),
            }
        }
        assert_eq!(got, ["end 1", "error 2", "client", "forgotten 3", "end 4", "error 5"]);
        assert_eq!(inbox.take(&shared), written, "nothing written since");
        assert!(inbox.next(written, None).is_none());
    }

    // ibx#662: the answers read from the state take their place at their
    // count, between the queued answers; alone, they wait for the end of
    // the pass.
    #[test]
    fn the_state_comes_at_its_count() {
        let shared = SharedState::new();
        shared.reference.push_contract_details_end(1);
        shared.market.push_md_resolved(0, 1);
        shared.reference.push_contract_details_end(2);
        let state = shared.state_mark().first();
        let mut inbox = Inbox::default();
        let written = inbox.take(&shared);
        let mut got = Vec::new();
        let mut state_left = state;
        while let Some(next) = inbox.next_with_state(written, None, state_left) {
            match next {
                Run::State(until) => {
                    got.push(format!("state until {}", until - state.unwrap()));
                    state_left = None;
                }
                Run::Queue(Kind::contract_details_end, until) => got.extend(run(&mut inbox.contract_details_end, until).map(|r| format!("end {r}"))),
                _ => unreachable!(),
            }
        }
        assert_eq!(got, ["end 1", "state until 1", "end 2"]);
        assert!(inbox.next_with_state(written, None, state).is_none(), "alone: at the end of the pass");
    }

    #[test]
    fn an_answer_counted_after_the_reading_waits_for_the_next_pass() {
        let shared = SharedState::new();
        shared.reference.push_contract_details_end(1);
        let mut inbox = Inbox::default();
        let written = inbox.take(&shared);
        // Written after the clock was read, before the queues were taken.
        shared.orders.push_order_error(2, 200, "a".into());
        inbox.take_queues(&shared);
        let (kind, until) = inbox.next_run(written).unwrap();
        assert_eq!(kind, Kind::contract_details_end);
        assert_eq!(run(&mut inbox.contract_details_end, until).collect::<Vec<_>>(), [1]);
        assert!(inbox.next_run(written).is_none());
        let written = inbox.take(&shared);
        assert_eq!(inbox.next_run(written).map(|(k, _)| k), Some(Kind::order_errors));
    }
}

