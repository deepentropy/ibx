//! Pacing of the API requests the engine takes, as the reference paces the
//! requests it receives (ibx#555).
//!
//! Observed on the reference (run of 10/10/2026): a request that comes
//! while none waits is handled at once; of six requests made in the same
//! instant the first is handled at once and the others one every 0.1 s;
//! requests 2 ms or more apart are each handled at once.
//!
//! The rule that gives this: a request that finds the pacer at rest starts
//! an interval of one second, cut in ten steps of 0.1 s. Each step may
//! handle a share of the requests that waited when the interval started:
//! half of them in the first step, a quarter in the second, and so on,
//! rounded up, so at least one per step. When no request waits any more the
//! pacer is at rest again. An interval that ends with requests still
//! waiting is followed by a new one, counted from all of them.

use std::time::{Duration, Instant};

use crate::types::ControlCommand;

/// Requests per second the reference allows by default.
const MAX_PER_SECOND: usize = 50;
const STEP: Duration = Duration::from_millis(100);
const STEPS: u32 = 10;
/// How long the reference is busy with a request before it looks for the
/// next one: a request that comes within this time waits for the next
/// step. Requests 0.1 to 0.5 ms apart were always paced, requests 2 ms
/// apart never (run of 10/10/2026).
const HANDLING: Duration = Duration::from_millis(1);

/// Whether a command is an API request of its own. The others (the
/// registration of a contract ahead of its request, the engine's own
/// housekeeping, a further command of a request already counted) pass
/// without pacing, in their turn.
pub(crate) fn is_request(cmd: &ControlCommand) -> bool {
    !matches!(cmd, ControlCommand::Unpaced(_)) && !is_aside(cmd)
}

/// A command that belongs to no request's answer: the registration of a
/// contract ahead of its request (the caller waits for its reply), and
/// housekeeping. It is taken at once, also past requests that wait, so
/// that a call returns as fast as on the reference.
pub(crate) fn is_aside(cmd: &ControlCommand) -> bool {
    matches!(cmd,
        ControlCommand::RegisterInstrument { .. }
        | ControlCommand::MarketDataSlot { .. }
        | ControlCommand::RegisterOrderContract { .. }
        | ControlCommand::SetInstrumentCurrency { .. }
        | ControlCommand::UpdateParam { .. }
        | ControlCommand::DropSnapshot { .. }
        | ControlCommand::Ping
        | ControlCommand::Shutdown)
}

/// A command as the engine keeps it until it is taken: `channel` tells a
/// command of the channel from one the client gave aside of it, and
/// `ordinal` is its place among the commands of the channel (for the
/// second kind: the command of the channel it follows).
#[derive(Debug)]
pub(crate) struct Queued {
    pub(crate) ordinal: u64,
    pub(crate) channel: bool,
    pub(crate) cmd: ControlCommand,
}

#[derive(Debug)]
struct Interval {
    start: Instant,
    /// Requests that waited when the interval started, at most the limit.
    waiting_at_start: usize,
    step: u32,
    handled_in_step: usize,
    step_budget: usize,
    handled: usize,
    busy_until: Instant,
}

impl Interval {
    fn new(start: Instant, waiting_at_start: usize) -> Self {
        Self {
            start, waiting_at_start, step: u32::MAX, handled_in_step: 0, step_budget: 0, handled: 0,
            busy_until: start,
        }
    }
}

#[derive(Debug, Default)]
pub(crate) struct Pacer {
    interval: Option<Interval>,
    /// No request waited at the last look.
    was_empty: bool,
}

impl Pacer {
    /// Whether the request at the head of the queue is handled now;
    /// `waiting` counts it and the requests behind it.
    pub(crate) fn admit(&mut self, now: Instant, waiting: usize) -> bool {
        let waiting = waiting.max(1);
        // The queue was empty at the last look and the last request is
        // done: the pacer was at rest when this one came.
        if std::mem::take(&mut self.was_empty) && self.interval.as_ref().is_some_and(|iv| now >= iv.busy_until) {
            self.interval = None;
        }
        let over = self.interval.as_ref().is_some_and(|iv| now.duration_since(iv.start) >= STEP * STEPS);
        if over {
            // The interval ended with requests waiting: the next one is
            // counted from all of them.
            self.interval = Some(Interval::new(now, waiting.min(MAX_PER_SECOND)));
        }
        // At rest: the pacer wakes on the first request.
        let iv = self.interval.get_or_insert_with(|| Interval::new(now, 1));
        let step = (now.duration_since(iv.start).as_millis() / STEP.as_millis()) as u32;
        if step != iv.step {
            iv.step = step;
            iv.handled_in_step = 0;
            let share = 0.5f64.powi(step as i32 + 1);
            iv.step_budget = ((iv.waiting_at_start as f64 * share).ceil() as usize).min(waiting);
        }
        if iv.handled_in_step >= iv.step_budget || iv.handled >= MAX_PER_SECOND {
            return false;
        }
        iv.handled_in_step += 1;
        iv.handled += 1;
        iv.busy_until = now + HANDLING;
        true
    }

    /// No request waits: the pacer is at rest once the last one is done.
    pub(crate) fn nothing_waits(&mut self, now: Instant) {
        self.was_empty = true;
        if self.interval.as_ref().is_some_and(|iv| now >= iv.busy_until) {
            self.interval = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Requests arriving at `arrivals` (ms): the time each is handled,
    /// with the engine looking every 0.25 ms.
    fn handled_at(arrivals: &[u64]) -> Vec<u64> {
        let t0 = Instant::now();
        let mut pacer = Pacer::default();
        let mut out = Vec::new();
        let mut next = 0usize;
        let mut quarter = 0u64;
        while out.len() < arrivals.len() {
            let now = t0 + Duration::from_micros(quarter * 250);
            let arrived = arrivals.iter().filter(|a| **a * 4 <= quarter).count();
            loop {
                let waiting = arrived - next;
                if waiting == 0 {
                    pacer.nothing_waits(now);
                    break;
                }
                if !pacer.admit(now, waiting) {
                    break;
                }
                out.push(quarter / 4);
                next += 1;
            }
            quarter += 1;
            assert!(quarter < 40_000, "a request is never handled: {out:?}");
        }
        out
    }

    // The rows of the reference run of 10/10/2026.
    #[test]
    fn a_request_alone_is_handled_at_once() {
        assert_eq!(handled_at(&[0]), [0]);
        assert_eq!(handled_at(&[0, 3000, 6000]), [0, 3000, 6000]);
    }

    #[test]
    fn six_at_once_leave_one_every_step() {
        assert_eq!(handled_at(&[0, 0, 0, 0, 0, 0]), [0, 100, 200, 300, 400, 500]);
    }

    #[test]
    fn requests_two_ms_or_more_apart_are_not_slowed() {
        assert_eq!(handled_at(&[0, 2, 4, 6, 8, 10]), [0, 2, 4, 6, 8, 10]);
        assert_eq!(handled_at(&[0, 10, 20, 30]), [0, 10, 20, 30]);
        assert_eq!(handled_at(&[0, 50, 100, 150]), [0, 50, 100, 150]);
    }

    // The engine looked only when each request came (it rests between):
    // a request 2 ms after the last one is still handled at once.
    #[test]
    fn a_rest_between_two_looks_is_seen() {
        let t0 = Instant::now();
        let at = |ms: u64| t0 + Duration::from_millis(ms);
        let mut pacer = Pacer::default();
        assert!(pacer.admit(at(0), 1));
        pacer.nothing_waits(at(0));
        assert!(pacer.admit(at(2), 1), "the pacer was at rest");
        pacer.nothing_waits(at(2));
        // Two at once: the second waits, and a third that comes while it
        // waits does too.
        assert!(pacer.admit(at(10), 2));
        assert!(!pacer.admit(at(10), 1));
        assert!(!pacer.admit(at(50), 2));
        assert!(pacer.admit(at(110), 2));
        assert!(!pacer.admit(at(110), 1));
        assert!(pacer.admit(at(210), 1));
    }

    // A cancel and a new request at once: the second a step later.
    #[test]
    fn two_at_once_are_a_step_apart() {
        assert_eq!(handled_at(&[0, 0]), [0, 100]);
    }

    // After the pacing ended, the next request is handled at once again.
    #[test]
    fn the_pacing_ends_with_the_queue() {
        assert_eq!(handled_at(&[0, 0, 0, 250]), [0, 100, 200, 250]);
        assert_eq!(handled_at(&[0, 0, 150]), [0, 100, 150]);
    }

    // More than an interval's worth: ten in the first second, one per
    // step; the next interval is counted from the fourteen left and takes
    // half of them at once, then a quarter, and so on.
    #[test]
    fn a_long_queue_goes_on_in_a_new_interval() {
        let got = handled_at(&[0; 24]);
        assert_eq!(&got[..10], [0, 100, 200, 300, 400, 500, 600, 700, 800, 900]);
        assert_eq!(&got[10..17], [1000; 7]);
        assert_eq!(&got[17..21], [1100; 4]);
        assert_eq!(&got[21..23], [1200; 2]);
        assert_eq!(got[23], 1300);
    }

    #[test]
    fn commands_that_are_not_requests() {
        assert!(!is_request(&ControlCommand::Ping) && is_aside(&ControlCommand::Ping));
        assert!(!is_aside(&ControlCommand::Unpaced(Box::new(ControlCommand::Ping))));
        assert!(!is_request(&ControlCommand::Shutdown));
        assert!(!is_request(&ControlCommand::Unpaced(Box::new(ControlCommand::CancelScanner { req_id: 1 }))));
        assert!(is_request(&ControlCommand::CancelScanner { req_id: 1 }));
        assert!(is_request(&ControlCommand::Unsubscribe { instrument: 0 }));
    }
}
