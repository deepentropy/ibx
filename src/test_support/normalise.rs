//! The one normaliser of session fields, for comparing a message ibx writes
//! with a message the reference wrote in another session.
//!
//! What changes from one session to the next, and how it is normalised:
//!
//! | Field | Rule |
//! |---|---|
//! | 8, 9, 34, 52, 10 (begin string, body length, sequence, sending time, checksum) | dropped |
//! | 8349 (signature) | dropped |
//! | 6205, 6531 (left out of the order comparisons on purpose, see the replace tests) | dropped by [`Normaliser::session`] |
//! | 11, 41 (order ids) | `{id}` and the version suffix (`1288736453.1` gives `{id}.1`), by [`Normaliser::session`] |
//! | 1 (account), and any account id in a value | `DUXXXXXXX` |
//! | 60, 122 (transaction and original sending times) | `{time}` |
//!
//! An order's own times (126 expiry, condition times) are order fields:
//! they are compared as they are.

use crate::protocol::fix::SOH;

/// A message as its fields, in wire order.
pub type Fields = Vec<(u32, String)>;

/// The masked account id of the fixtures and docs.
pub const ACCOUNT_MASK: &str = "DUXXXXXXX";

/// Header, sequence, sending time and trailer fields, with the signature.
pub const FRAMING: &[u32] = &[8, 9, 34, 52, 10, 8349];

/// Fields the order comparisons leave out on purpose.
pub const ORDER_UNSTABLE: &[u32] = &[6205, 6531];

/// Times the sender sets when it writes the message.
pub const SESSION_TIMES: &[u32] = &[60, 122];

/// The fields of a raw message (`tag=value` separated by SOH), in order.
/// Parts that are not `tag=value` (binary data) are skipped.
pub fn parse_fields(msg: &[u8]) -> Fields {
    msg.split(|&b| b == SOH)
        .filter_map(|part| {
            let text = std::str::from_utf8(part).ok()?;
            let (tag, value) = text.split_once('=')?;
            Some((tag.parse().ok()?, value.to_string()))
        })
        .collect()
}

/// The fields of a message written with `|` separators, as the captured
/// reference frames in the tests and fixtures are.
pub fn parse_pipe(text: &str) -> Fields {
    parse_fields(text.replace('|', "\x01").as_bytes())
}

/// The `|` separated form of fields.
pub fn to_pipe(fields: &[(u32, String)]) -> String {
    fields.iter().map(|(t, v)| format!("{t}={v}")).collect::<Vec<_>>().join("|")
}

/// What to normalise. [`Normaliser::framing`] keeps the order ids;
/// [`Normaliser::session`] masks them too.
#[derive(Debug, Clone)]
pub struct Normaliser {
    drop: Vec<u32>,
    mask_ids: bool,
}

impl Normaliser {
    /// Drops the framing fields; masks accounts and timestamps.
    pub fn framing() -> Self {
        Self { drop: FRAMING.to_vec(), mask_ids: false }
    }

    /// [`Normaliser::framing`], and drops 6205 and 6531 and masks the order
    /// ids 11 and 41.
    pub fn session() -> Self {
        let mut drop = FRAMING.to_vec();
        drop.extend_from_slice(ORDER_UNSTABLE);
        Self { drop, mask_ids: true }
    }

    /// Also drop these fields (ids a test does not control, for example).
    pub fn drop(mut self, tags: &[u32]) -> Self {
        self.drop.extend_from_slice(tags);
        self
    }

    /// Keep these fields (undo a drop of the preset).
    pub fn keep(mut self, tags: &[u32]) -> Self {
        self.drop.retain(|t| !tags.contains(t));
        self
    }

    /// Keep the order ids as they are.
    pub fn keep_ids(mut self) -> Self {
        self.mask_ids = false;
        self
    }

    /// The fields, normalised, in their order.
    pub fn apply(&self, fields: &[(u32, String)]) -> Fields {
        fields
            .iter()
            .filter(|(t, _)| !self.drop.contains(t))
            .map(|(t, v)| (*t, self.value(*t, v)))
            .collect()
    }

    /// A raw message, parsed and normalised.
    pub fn msg(&self, msg: &[u8]) -> Fields {
        self.apply(&parse_fields(msg))
    }

    /// A `|` separated message, parsed and normalised.
    pub fn pipe(&self, text: &str) -> Fields {
        self.apply(&parse_pipe(text))
    }

    fn value(&self, tag: u32, v: &str) -> String {
        if tag == 1 && !v.is_empty() {
            return ACCOUNT_MASK.to_string();
        }
        if self.mask_ids && matches!(tag, 11 | 41) {
            return match v.rsplit_once('.') {
                Some((_, version)) if version.bytes().all(|b| b.is_ascii_digit()) => format!("{{id}}.{version}"),
                _ => "{id}".to_string(),
            };
        }
        if SESSION_TIMES.contains(&tag) && !v.is_empty() {
            return "{time}".to_string();
        }
        mask_accounts(v)
    }
}

/// Every account id in a text masked: `DU` or `DF` and digits, or `U` or
/// `F` and 6 to 8 digits, as a whole word.
pub fn mask_accounts(text: &str) -> String {
    let b = text.as_bytes();
    let word = |i: usize| i < b.len() && b[i].is_ascii_alphanumeric();
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while i < b.len() {
        if i == 0 || !word(i - 1) {
            let prefix = if b[i..].starts_with(b"DU") || b[i..].starts_with(b"DF") {
                2
            } else if b[i] == b'U' || b[i] == b'F' {
                1
            } else {
                0
            };
            if prefix > 0 {
                let n = b[i + prefix..].iter().take_while(|c| c.is_ascii_digit()).count();
                let end = i + prefix + n;
                let ok = if prefix == 2 { n >= 1 } else { (6..=8).contains(&n) };
                if ok && !word(end) {
                    out.push_str(ACCOUNT_MASK);
                    i = end;
                    continue;
                }
            }
        }
        let ch = text[i..].chars().next().unwrap();
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

/// The same fields in the same order with the same values; on a
/// difference, the first field that differs and both messages.
#[track_caller]
pub fn assert_same_fields(ours: &[(u32, String)], reference: &[(u32, String)]) {
    let ours_tags: Vec<u32> = ours.iter().map(|(t, _)| *t).collect();
    let want_tags: Vec<u32> = reference.iter().map(|(t, _)| *t).collect();
    assert_eq!(ours_tags, want_tags, "field order differs\n ours: {}\n want: {}", to_pipe(ours), to_pipe(reference));
    for ((t, a), (_, b)) in ours.iter().zip(reference.iter()) {
        assert_eq!(a, b, "field {t}: ours {a:?}, reference {b:?}\n ours: {}\n want: {}", to_pipe(ours), to_pipe(reference));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_rules() {
        let n = Normaliser::session();
        let got = n.pipe("8=FIX.4.1|9=120|35=G|34=7|52=20261002-13:30:01|11=1288736453.1|41=1288736453.0|1=DU1234567|6205=1|6531=a/1/2|60=20261002-13:30:01.123|58=account DU1234567 and U1234567|8349=ABCDEF01|10=123");
        assert_eq!(to_pipe(&got), "35=G|11={id}.1|41={id}.0|1=DUXXXXXXX|60={time}|58=account DUXXXXXXX and DUXXXXXXX");
    }

    #[test]
    fn framing_keeps_ids_and_order_fields() {
        let n = Normaliser::framing();
        let got = n.pipe("8=FIX.4.1|9=10|35=D|11=5.0|1=DU1|6205=1|10=001");
        assert_eq!(to_pipe(&got), "35=D|11=5.0|1=DUXXXXXXX|6205=1");
    }

    #[test]
    fn account_masking_leaves_other_words() {
        assert_eq!(mask_accounts("DU1 DUXXXXXXX U12345 F123456 USD FUT XU1234567"), "DUXXXXXXX DUXXXXXXX U12345 DUXXXXXXX USD FUT XU1234567");
    }

    #[test]
    fn order_times_are_kept() {
        let got = Normaliser::framing().pipe("35=G|126=20260930-20:00:00|60=20261002-13:30:01");
        assert_eq!(to_pipe(&got), "35=G|126=20260930-20:00:00|60={time}");
    }

    #[test]
    #[should_panic(expected = "field 44")]
    fn a_value_difference_fails() {
        assert_same_fields(&parse_pipe("35=D|44=1.00"), &parse_pipe("35=D|44=1.01"));
    }

    #[test]
    #[should_panic(expected = "field order differs")]
    fn an_order_difference_fails() {
        assert_same_fields(&parse_pipe("35=D|44=1.00|38=1"), &parse_pipe("35=D|38=1|44=1.00"));
    }
}
