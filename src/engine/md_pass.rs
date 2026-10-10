//! The passes in which market data requests and cancels leave, as the
//! reference sends them (ibx#560).
//!
//! Observed on the reference (run of 10/10/2026): a request or a cancel
//! alone leaves at once; six cancels 2 ms apart left as three messages,
//! the first contract at once, the next three together 6 ms later, the
//! last two together 5 ms after that.
//!
//! The rule that gives this: the changes are sent in passes, at most one
//! every 5 ms. A change that comes less than 5 ms after the last pass
//! waits for the end of those 5 ms; one pass sends everything that changed
//! since the last one, the entries of one kind for one farm in one
//! message. After 100 passes started in a second the passes are 500 ms
//! apart for the rest of that second.

use std::time::{Duration, Instant};

use super::hot_loop::pool::{FarmId, FixSink};

const PASS: Duration = Duration::from_millis(5);
const SLOW_PASS: Duration = Duration::from_millis(500);
const QUICK_PASSES_PER_SECOND: u32 = 100;

pub(crate) type FarmMessage = (FarmId, Vec<(u32, String)>);

#[derive(Debug, Default)]
pub(crate) struct MdPass {
    /// Off: every message leaves at once, one per request (engines built
    /// by hand, tests).
    pub(crate) on: bool,
    held: Vec<FarmMessage>,
    last: Option<Instant>,
    due: Option<Instant>,
    /// The second being counted, and the passes started in it.
    counted: Option<(Instant, u32)>,
}

impl MdPass {
    /// Keep a message for the next pass.
    pub(crate) fn hold(&mut self, now: Instant, farm: FarmId, msg: Vec<(u32, String)>) {
        self.held.push((farm, msg));
        if self.due.is_some() {
            return;
        }
        let count = match &mut self.counted {
            Some((start, count)) if now.duration_since(*start) < Duration::from_secs(1) => {
                *count += 1;
                *count
            }
            counted => {
                *counted = Some((now, 1));
                1
            }
        };
        let period = if count > QUICK_PASSES_PER_SECOND { SLOW_PASS } else { PASS };
        self.due = Some(match self.last {
            Some(last) if now < last + period => last + period,
            _ => now,
        });
    }

    /// A pass is due within a few ms: the engine does not rest until then
    /// (its rest is far coarser than 5 ms on some systems).
    pub(crate) fn waits(&self) -> bool {
        self.due.is_some()
    }

    /// The messages of the pass that is due, grouped; none when no pass is
    /// due. `all` takes what waits whatever the time (the engine stops).
    pub(crate) fn take_due(&mut self, now: Instant, all: bool) -> Vec<FarmMessage> {
        if !self.due.is_some_and(|due| all || due <= now) {
            return Vec::new();
        }
        self.due = None;
        self.last = Some(now);
        group(std::mem::take(&mut self.held))
    }
}

/// What makes two market data messages one: the farm, the action, the
/// kind of their entries (top of book or not) and their data mode.
fn key(farm: FarmId, msg: &[(u32, String)]) -> Option<(FarmId, String, bool, String)> {
    if msg.first().is_none_or(|(tag, v)| *tag != 35 || v != "V") {
        return None;
    }
    let action = msg.iter().find(|(tag, _)| *tag == 263)?.1.clone();
    let mut kinds = msg.iter().filter(|(tag, _)| *tag == 264).map(|(_, v)| v.as_str()).peekable();
    kinds.peek()?;
    let top = kinds.all(|k| k == "442" || k == "443");
    let mode = msg.iter().find(|(tag, _)| *tag == 9887).map(|(_, v)| v.clone()).unwrap_or_default();
    Some((farm, action, top, mode))
}

/// The messages of a pass: those of one key become one message, with the
/// entries in the order they came and their count; the others as they are.
fn group(held: Vec<FarmMessage>) -> Vec<FarmMessage> {
    let mut out: Vec<(Option<(FarmId, String, bool, String)>, FarmMessage)> = Vec::new();
    for (farm, msg) in held {
        let k = key(farm, &msg);
        let first_entry = msg.iter().position(|(tag, _)| *tag == 262);
        match (k.as_ref().and_then(|k| out.iter_mut().find(|(o, _)| o.as_ref() == Some(k))), first_entry) {
            (Some((_, (_, into))), Some(at)) => {
                let added = msg.iter().filter(|(tag, _)| *tag == 262).count();
                into.extend(msg.into_iter().skip(at));
                if let Some((_, n)) = into.iter_mut().find(|(tag, _)| *tag == 146) {
                    *n = (n.parse::<usize>().unwrap_or(0) + added).to_string();
                }
            }
            _ => out.push((k, (farm, msg))),
        }
    }
    out.into_iter().map(|(_, m)| m).collect()
}

/// A sink that keeps the compressed messages written to it for the next
/// pass of a farm.
pub(crate) struct HoldSink<'a> {
    pub(crate) farm: FarmId,
    pub(crate) pass: &'a mut MdPass,
}

impl FixSink for HoldSink<'_> {
    fn send_plain(&mut self, _fields: &[(u32, &str)]) -> bool {
        false
    }

    fn send_comp(&mut self, fields: &[(u32, &str)]) -> bool {
        let msg = fields.iter().map(|(tag, v)| (*tag, v.to_string())).collect();
        self.pass.hold(Instant::now(), self.farm, msg);
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FARM: FarmId = super::super::hot_loop::pool::PRIMARY_MD;

    fn top(action: &str, con_id: &str, first_id: u32) -> Vec<(u32, String)> {
        let mut msg = vec![(35, "V".to_string()), (52, "t".into()), (263, action.into()), (146, "2".into())];
        for (k, kind) in ["442", "443"].iter().enumerate() {
            msg.extend([(262, (first_id + k as u32).to_string()), (6008, con_id.to_string()), (207, "BEST".into()),
                (167, "CS".into()), (264, kind.to_string()), (9830, "1".into())]);
        }
        msg
    }

    fn news(action: &str, con_id: &str, id: u32) -> Vec<(u32, String)> {
        vec![(35, "V".into()), (52, "t".into()), (263, action.into()), (146, "1".into()), (262, id.to_string()),
            (6008, con_id.into()), (207, "NEWS".into()), (167, "CS".into()), (264, "292".into()), (9830, "1".into())]
    }

    fn text(msg: &[(u32, String)]) -> String {
        msg.iter().filter(|(tag, _)| *tag != 52).map(|(tag, v)| format!("{tag}={v}")).collect::<Vec<_>>().join("|")
    }

    // The recording of 02/10/2026: the cancels of two contracts, top of
    // book and news, leave as one message of four entries and one of two.
    #[test]
    fn one_pass_sends_one_message_per_kind() {
        let held = vec![(FARM, top("2", "272093", 36)), (FARM, news("2", "272093", 38)),
            (FARM, top("2", "265598", 33)), (FARM, news("2", "265598", 35))];
        let out = group(held);
        assert_eq!(out.len(), 2);
        assert_eq!(text(&out[0].1), "35=V|263=2|146=4|262=36|6008=272093|207=BEST|167=CS|264=442|9830=1|\
            262=37|6008=272093|207=BEST|167=CS|264=443|9830=1|262=33|6008=265598|207=BEST|167=CS|264=442|9830=1|\
            262=34|6008=265598|207=BEST|167=CS|264=443|9830=1");
        assert_eq!(text(&out[1].1), "35=V|263=2|146=2|262=38|6008=272093|207=NEWS|167=CS|264=292|9830=1|\
            262=35|6008=265598|207=NEWS|167=CS|264=292|9830=1");
    }

    // A request and a cancel, another farm, another data mode: not merged.
    #[test]
    fn different_actions_farms_and_modes_stay_apart() {
        let mut delayed = top("1", "8314", 9);
        delayed.insert(9, (9887, "1".into()));
        let other_farm: FarmId = FARM + 1;
        let out = group(vec![(FARM, top("1", "265598", 1)), (FARM, top("2", "272093", 3)), (FARM, delayed),
            (other_farm, top("1", "4391", 5)), (FARM, top("1", "270639", 7)),
            (FARM, vec![(35, "U".into()), (6040, "1".into())])]);
        let counts: Vec<String> = out.iter().map(|(f, m)| {
            format!("{f}:{}", m.iter().find(|(t, _)| *t == 146).map_or("-", |(_, v)| v.as_str()))
        }).collect();
        assert_eq!(counts, [format!("{FARM}:4"), format!("{FARM}:2"), format!("{FARM}:2"), format!("{other_farm}:2"), format!("{FARM}:-")]);
    }

    // The times of the reference run: alone at once; within 5 ms of the
    // last pass, at the end of those 5 ms, with what came meanwhile.
    #[test]
    fn passes_are_five_ms_apart() {
        let t0 = Instant::now();
        let at = |micros: u64| t0 + Duration::from_micros(micros);
        let mut pass = MdPass { on: true, ..Default::default() };
        pass.hold(at(0), FARM, top("2", "1", 1));
        assert_eq!(pass.take_due(at(0), false).len(), 1, "alone: at once");
        pass.hold(at(1_000), FARM, top("2", "2", 3));
        pass.hold(at(3_000), FARM, top("2", "3", 5));
        assert!(pass.take_due(at(4_900), false).is_empty(), "not before 5 ms after the last pass");
        let out = pass.take_due(at(5_000), false);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].1.iter().find(|(t, _)| *t == 146).unwrap().1, "4", "the two contracts in one message");
        // 10 ms later: at once again.
        pass.hold(at(15_000), FARM, top("2", "4", 7));
        assert_eq!(pass.take_due(at(15_000), false).len(), 1);
        // What waits is taken when the engine stops.
        pass.hold(at(16_000), FARM, top("2", "5", 9));
        assert!(pass.take_due(at(16_000), false).is_empty());
        assert_eq!(pass.take_due(at(16_000), true).len(), 1);
    }

    // Past 100 passes started in a second, the next waits 500 ms.
    #[test]
    fn many_passes_in_a_second_slow_down() {
        let t0 = Instant::now();
        let mut pass = MdPass { on: true, ..Default::default() };
        for k in 0..100u64 {
            let now = t0 + Duration::from_millis(k * 6);
            pass.hold(now, FARM, top("2", "1", 1));
            assert_eq!(pass.take_due(now, false).len(), 1, "pass {k}");
        }
        let now = t0 + Duration::from_millis(600);
        pass.hold(now, FARM, top("2", "1", 1));
        assert!(pass.take_due(now + Duration::from_millis(400), false).is_empty());
        assert_eq!(pass.take_due(t0 + Duration::from_millis(594 + 500), false).len(), 1);
    }
}
