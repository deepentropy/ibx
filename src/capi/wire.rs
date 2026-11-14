//! The encoding of a structure across the C entry layer: its fields one
//! after the other, numbers as 8 bytes little-endian, a flag as one byte,
//! a text or a list as a 4-byte count and its content. Both sides are
//! generated from one table, so the form is private to a build.

/// Writes into a buffer the caller keeps.
pub(crate) struct W<'a>(pub &'a mut Vec<u8>);

impl W<'_> {
    #[inline]
    pub fn i(&mut self, v: i64) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }

    #[inline]
    pub fn f(&mut self, v: f64) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }

    #[inline]
    pub fn b(&mut self, v: bool) {
        self.0.push(v as u8);
    }

    #[inline]
    pub fn n(&mut self, count: usize) {
        self.0.extend_from_slice(&(count as u32).to_le_bytes());
    }

    #[inline]
    pub fn s(&mut self, v: &str) {
        self.n(v.len());
        self.0.extend_from_slice(v.as_bytes());
    }
}

/// Reads what the other side wrote. A buffer that ends early reads as
/// zeros and empty texts: nothing panics on the boundary.
pub(crate) struct R<'a> {
    buf: &'a [u8],
    /// Why the structure read cannot be sent: an error code and its text.
    pub refused: Option<(i64, &'static str)>,
}

impl<'a> R<'a> {
    pub fn new(buf: &'a [u8]) -> Self {
        Self { buf, refused: None }
    }

    #[inline]
    fn take(&mut self, n: usize) -> &'a [u8] {
        let n = n.min(self.buf.len());
        let (head, tail) = self.buf.split_at(n);
        self.buf = tail;
        head
    }

    #[inline]
    fn eight(&mut self) -> [u8; 8] {
        let mut out = [0u8; 8];
        let got = self.take(8);
        out[..got.len()].copy_from_slice(got);
        out
    }

    #[inline]
    pub fn i(&mut self) -> i64 {
        i64::from_le_bytes(self.eight())
    }

    #[inline]
    pub fn f(&mut self) -> f64 {
        f64::from_le_bytes(self.eight())
    }

    #[inline]
    pub fn b(&mut self) -> bool {
        self.take(1).first().is_some_and(|b| *b != 0)
    }

    /// A count, never more than the bytes left: a list cannot be longer
    /// than its content.
    #[inline]
    pub fn n(&mut self) -> usize {
        let mut four = [0u8; 4];
        let got = self.take(4);
        four[..got.len()].copy_from_slice(got);
        (u32::from_le_bytes(four) as usize).min(self.buf.len())
    }

    #[inline]
    pub fn s(&mut self) -> String {
        let n = self.n();
        String::from_utf8_lossy(self.take(n)).into_owned()
    }

    /// The structure cannot be sent as read; the first reason is kept.
    pub fn refuse(&mut self, code: i64, text: &'static str) {
        self.refused.get_or_insert((code, text));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn values_come_back_as_written() {
        let mut buf = Vec::new();
        let mut w = W(&mut buf);
        w.i(-7);
        w.f(1.5);
        w.b(true);
        w.s("héllo");
        w.n(3);
        let mut r = R::new(&buf);
        assert_eq!((r.i(), r.f(), r.b(), r.s(), r.n()), (-7, 1.5, true, "héllo".to_string(), 0));
    }

    #[test]
    fn a_short_buffer_reads_as_empty() {
        let mut r = R::new(&[1, 2, 3]);
        assert_eq!(r.i(), 0x030201);
        assert_eq!((r.i(), r.f(), r.b(), r.s()), (0, 0.0, false, String::new()));
        // A count larger than what is left is cut to it.
        let mut r = R::new(&[255, 255, 255, 255, b'a']);
        assert_eq!(r.s(), "a");
    }

    #[test]
    fn the_first_refusal_is_kept() {
        let mut r = R::new(&[]);
        r.refuse(1, "first");
        r.refuse(2, "second");
        assert_eq!(r.refused, Some((1, "first")));
    }
}
