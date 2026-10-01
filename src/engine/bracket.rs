//! The bracket key of a parent order and its children (ibx#248).
//!
//! The reference gives a bracket a key `{group}/{child}/{colour}`: a
//! group number counted per session, the child index (0 for the parent,
//! then 1, 2, ... in the order the children are attached) and a random
//! light colour as a signed RGB value. Every order of the bracket carries
//! it on the new order and restates it on each replace. A parent sent
//! before any child has no key on its new order; it gets one when its
//! first child is attached, and restates it on its replaces. A key seen
//! on a report moves the group counter past it.

use std::collections::HashMap;

use crate::types::OrderId;

/// One order's bracket key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct BracketKey {
    pub group: u32,
    pub child: u32,
    pub rgb: i32,
}

impl std::fmt::Display for BracketKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}/{}/{}", self.group, self.child, self.rgb)
    }
}

impl BracketKey {
    /// Parse a key as reported, `group/child/rgb`.
    pub(crate) fn parse(text: &str) -> Option<Self> {
        let mut parts = text.split('/');
        let group = parts.next()?.trim().parse().ok()?;
        let child = parts.next()?.trim().parse().ok()?;
        let rgb = parts.next()?.trim().parse().ok()?;
        Some(BracketKey { group, child, rgb })
    }
}

/// A seed for the colour generator.
pub(crate) fn seed() -> u64 {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0x9E37_79B9_7F4A_7C15);
    nanos | 1
}

/// A light colour as the reference draws it: each component from 100 to
/// 254, opaque, as a signed RGB value.
fn colour(state: &mut u64) -> i32 {
    let mut component = || {
        // xorshift64
        let mut x = *state;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        *state = x;
        (100 + (x >> 33) % 155) as u32
    };
    let (r, g, b) = (component(), component(), component());
    (0xFF00_0000u32 | (r << 16) | (g << 8) | b) as i32
}

/// The keys and counters a session holds.
pub(crate) struct Brackets<'a> {
    pub keys: &'a mut HashMap<OrderId, BracketKey>,
    pub next_child: &'a mut HashMap<OrderId, u32>,
    pub groups: &'a mut u32,
    pub rng: &'a mut u64,
}

impl Brackets<'_> {
    /// The key of a parent that has none yet: the next group, child 0.
    fn parent_key(&mut self, parent_id: OrderId) -> BracketKey {
        if let Some(key) = self.keys.get(&parent_id) {
            return *key;
        }
        *self.groups += 1;
        let key = BracketKey { group: *self.groups, child: 0, rgb: colour(self.rng) };
        self.keys.insert(parent_id, key);
        key
    }

    /// The key of a child attached to `parent_id`: the parent's group and
    /// colour with the parent's next child index. The parent gets its key
    /// here when it has none.
    pub(crate) fn attach_child(&mut self, parent_id: OrderId, child_id: OrderId) -> BracketKey {
        if let Some(key) = self.keys.get(&child_id).filter(|k| k.child > 0) {
            return *key;
        }
        let parent = self.parent_key(parent_id);
        let next = self.next_child.entry(parent_id).or_insert(1);
        let key = BracketKey { child: *next, ..parent };
        *next += 1;
        self.keys.insert(child_id, key);
        key
    }

    /// A key on a report: kept for the order when it has none, and the
    /// group counter moves past it.
    pub(crate) fn reported(&mut self, order_id: OrderId, parent_id: Option<OrderId>, key: BracketKey) {
        *self.groups = (*self.groups).max(key.group);
        self.keys.entry(order_id).or_insert(key);
        if let Some(parent) = parent_id.filter(|_| key.child > 0) {
            let next = self.next_child.entry(parent).or_insert(1);
            *next = (*next).max(key.child + 1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Held {
        keys: HashMap<OrderId, BracketKey>,
        next_child: HashMap<OrderId, u32>,
        groups: u32,
        rng: u64,
    }

    impl Held {
        fn new() -> Self { Held { keys: HashMap::new(), next_child: HashMap::new(), groups: 0, rng: 7 } }
        fn brackets(&mut self) -> Brackets<'_> {
            Brackets { keys: &mut self.keys, next_child: &mut self.next_child, groups: &mut self.groups, rng: &mut self.rng }
        }
    }

    #[test]
    fn the_key_reads_and_writes_as_reported() {
        let key = BracketKey::parse("4/2/-6183061").unwrap();
        assert_eq!(key, BracketKey { group: 4, child: 2, rgb: -6183061 });
        assert_eq!(key.to_string(), "4/2/-6183061");
        assert!(BracketKey::parse("4/2").is_none());
    }

    #[test]
    fn colours_are_light_and_opaque() {
        let mut state = seed();
        for _ in 0..1000 {
            let rgb = colour(&mut state) as u32;
            assert_eq!(rgb >> 24, 0xFF);
            for shift in [16, 8, 0] {
                let c = (rgb >> shift) & 0xFF;
                assert!((100..=254).contains(&c), "{c}");
            }
        }
    }

    // Captured 01/10/2026 (ib-agent captures/pd-orders): a parent sent
    // alone has no key; its first child gets the next group with index 1,
    // the second index 2, and the parent restates group/0 on its replace.
    #[test]
    fn children_of_a_parent_sent_alone_open_a_new_group() {
        let mut held = Held::new();
        let mut b = held.brackets();
        let c1 = b.attach_child(10, 11);
        let c2 = b.attach_child(10, 12);
        assert_eq!((c1.group, c1.child), (1, 1));
        assert_eq!((c2.group, c2.child), (1, 2));
        assert_eq!(c1.rgb, c2.rgb);
        let parent = held.keys[&10];
        assert_eq!((parent.group, parent.child, parent.rgb), (1, 0, c1.rgb));
        // A second bracket gets the next group.
        let mut b = held.brackets();
        assert_eq!(b.attach_child(20, 21).group, 2);
        // The same child asked again keeps its key.
        assert_eq!(b.attach_child(10, 11), c1);
    }

    // ib-agent captures/192 A5 and pd-orders: a child added to a working
    // bracket of three gets index 3.
    #[test]
    fn a_later_child_gets_the_next_index() {
        let mut held = Held::new();
        let mut b = held.brackets();
        b.attach_child(1, 2);
        b.attach_child(1, 3);
        assert_eq!(b.attach_child(1, 4).child, 3);
    }

    #[test]
    fn a_reported_key_moves_the_group_counter() {
        let mut held = Held::new();
        let mut b = held.brackets();
        b.reported(5, None, BracketKey { group: 7, child: 0, rgb: -1 });
        b.reported(6, Some(5), BracketKey { group: 7, child: 2, rgb: -1 });
        assert_eq!(b.attach_child(5, 8).child, 3, "the recovered parent's next child");
        assert_eq!(b.attach_child(30, 31).group, 8);
    }
}
