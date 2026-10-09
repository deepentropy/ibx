//! Login-only framing with explicit owned-byte admission and retained carry.

use std::io::{self, Read};

const FIX_HEADER: &[u8] = b"8=FIX.4.1\x01";
const COMP_HEADER: &[u8] = b"8=FIXCOMP\x01";
const MAX_LENGTH_DIGITS: usize = 20;

fn invalid() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, "invalid bounded login frame")
}

/// Read one native FIX/FIXCOMP frame without discarding coalesced suffixes.
/// Timeout errors retain partial bytes for a subsequent call. The transport owns
/// cancellation and polling; this function neither retries errors nor logs bytes.
pub(super) fn read_frame<R: Read>(
    reader: &mut R,
    carry: &mut Vec<u8>,
    max_bytes: usize,
) -> io::Result<Vec<u8>> {
    if max_bytes == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "positive login frame bound required",
        ));
    }
    let mut chunk = [0u8; 4096];
    loop {
        if carry.len() > max_bytes {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "login frame carry bound exceeded",
            ));
        }
        if let Some(total) = frame_length(carry, max_bytes)? {
            let frame = carry[..total].to_vec();
            carry.drain(..total);
            return Ok(frame);
        }
        let capacity = chunk.len().min(max_bytes - carry.len());
        if capacity == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "login frame byte bound exceeded",
            ));
        }
        let count = reader.read(&mut chunk[..capacity])?;
        if count == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "truncated bounded login frame",
            ));
        }
        // The stack read is capped before owned accumulation; no peer length
        // determines an allocation. A declared length is checked on the next pass.
        carry.extend_from_slice(&chunk[..count]);
    }
}

fn frame_length(bytes: &[u8], max_bytes: usize) -> io::Result<Option<usize>> {
    let prefix_len = bytes.len().min(FIX_HEADER.len());
    let fix = FIX_HEADER.starts_with(&bytes[..prefix_len]);
    let comp = COMP_HEADER.starts_with(&bytes[..prefix_len]);
    if !fix && !comp {
        return Err(invalid());
    }
    if bytes.len() < FIX_HEADER.len() {
        return Ok(None);
    }
    let tag9 = &bytes[FIX_HEADER.len()..];
    if !tag9.starts_with(b"9=") {
        if b"9=".starts_with(tag9) {
            return Ok(None);
        }
        return Err(invalid());
    }
    let mut body_len = 0usize;
    let mut digits = 0usize;
    let mut body_start = None;
    for (offset, &byte) in tag9[2..].iter().enumerate() {
        if byte == 1 {
            if digits == 0 {
                return Err(invalid());
            }
            body_start = Some(FIX_HEADER.len() + 2 + offset + 1);
            break;
        }
        if !byte.is_ascii_digit() || digits == MAX_LENGTH_DIGITS {
            return Err(invalid());
        }
        body_len = body_len
            .checked_mul(10)
            .and_then(|n| n.checked_add((byte - b'0') as usize))
            .ok_or_else(invalid)?;
        digits += 1;
        if body_len > max_bytes {
            return Err(invalid());
        }
    }
    let Some(body_start) = body_start else {
        return Ok(None);
    };
    let body_end = body_start.checked_add(body_len).ok_or_else(invalid)?;
    let total = body_end
        .checked_add(if fix { 7 } else { 0 })
        .ok_or_else(invalid)?;
    if body_len == 0 || total > max_bytes {
        return Err(invalid());
    }
    if bytes.len() < total {
        return Ok(None);
    }
    if bytes[body_end - 1] != 1 {
        return Err(invalid());
    }
    if fix {
        let trailer = &bytes[body_end..total];
        if !trailer.starts_with(b"10=")
            || trailer[6] != 1
            || !trailer[3..6].iter().all(u8::is_ascii_digit)
        {
            return Err(invalid());
        }
        let checksum = (trailer[3] - b'0') as u16 * 100
            + (trailer[4] - b'0') as u16 * 10
            + (trailer[5] - b'0') as u16;
        let expected = bytes[..body_end]
            .iter()
            .fold(0u8, |sum, byte| sum.wrapping_add(*byte));
        if checksum != expected as u16 {
            return Err(invalid());
        }
    }
    Ok(Some(total))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::fix::fix_build;

    struct Segments {
        chunks: std::collections::VecDeque<io::Result<Vec<u8>>>,
    }
    impl Read for Segments {
        fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
            let Some(chunk) = self.chunks.pop_front() else {
                return Ok(0);
            };
            let mut chunk = chunk?;
            let count = chunk.len().min(output.len());
            output[..count].copy_from_slice(&chunk[..count]);
            if count < chunk.len() {
                self.chunks.push_front(Ok(chunk.split_off(count)));
            }
            Ok(count)
        }
    }

    #[test]
    fn bounded_login_frames_preserve_coalesced_suffix() {
        let first = fix_build(&[(35, "A")], 1);
        let second = fix_build(&[(35, "U"), (1, "account")], 2);
        let mut input = first.clone();
        input.extend_from_slice(&second);
        let mut reader = io::Cursor::new(input);
        let mut carry = Vec::new();
        assert_eq!(read_frame(&mut reader, &mut carry, 4096).unwrap(), first);
        assert_eq!(carry, second);
        let position = reader.position();
        assert_eq!(read_frame(&mut reader, &mut carry, 4096).unwrap(), second);
        assert_eq!(reader.position(), position);
        assert!(carry.is_empty());
    }

    #[test]
    fn bounded_login_frames_preserve_partial_frame_across_poll_error() {
        let frame = fix_build(&[(35, "A")], 1);
        let mut reader = Segments {
            chunks: [
                Ok(frame[..5].to_vec()),
                Err(io::ErrorKind::TimedOut.into()),
                Ok(frame[5..17].to_vec()),
                Ok(frame[17..].to_vec()),
            ]
            .into(),
        };
        let mut carry = Vec::new();
        assert_eq!(
            read_frame(&mut reader, &mut carry, 4096)
                .unwrap_err()
                .kind(),
            io::ErrorKind::TimedOut
        );
        assert_eq!(carry, frame[..5]);
        assert_eq!(read_frame(&mut reader, &mut carry, 4096).unwrap(), frame);
    }

    #[test]
    fn bounded_login_frames_do_not_scan_compressed_payload_for_checksum() {
        let body = b"95=12\x0196=\x0110=999\x01x\x01";
        let mut frame = format!("8=FIXCOMP\x019={}\x01", body.len()).into_bytes();
        frame.extend_from_slice(body);
        let next = fix_build(&[(35, "A")], 1);
        let mut input = frame.clone();
        input.extend_from_slice(&next);
        let mut carry = Vec::new();
        assert_eq!(
            read_frame(&mut io::Cursor::new(input), &mut carry, 4096).unwrap(),
            frame
        );
        assert_eq!(carry, next);
    }

    #[test]
    fn bounded_login_frames_reject_bad_lengths_before_reading_body() {
        for length in ["", "-1", "+1", "x", "999999999999999999999999", "99999"] {
            let header = format!("8=FIX.4.1\x019={}\x01", length).into_bytes();
            let mut carry = header.clone();
            let mut reader = io::Cursor::new(b"private-body".to_vec());
            let error = read_frame(&mut reader, &mut carry, 4096).unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::InvalidData);
            assert_eq!(reader.position(), 0);
            assert_eq!(carry, header);
            assert!(!error.to_string().contains("private"));
        }
        let header = format!("8=FIX.4.1\x019={}\x01", usize::MAX).into_bytes();
        assert!(frame_length(&header, usize::MAX).is_err());
    }

    #[test]
    fn bounded_login_frames_reject_truncation_bad_content_and_caps() {
        let frame = fix_build(&[(35, "A")], 1);
        let mut carry = Vec::new();
        assert_eq!(
            read_frame(
                &mut io::Cursor::new(&frame[..frame.len() - 1]),
                &mut carry,
                4096
            )
            .unwrap_err()
            .kind(),
            io::ErrorKind::UnexpectedEof
        );
        assert_eq!(carry, frame[..frame.len() - 1]);
        assert_eq!(
            read_frame(
                &mut io::Cursor::new(&frame[frame.len() - 1..]),
                &mut carry,
                4096
            )
            .unwrap(),
            frame
        );
        for bad in [b"private-body".to_vec(), b"8=FIX.4.1\x0135=A\x01".to_vec()] {
            let mut carry = bad.clone();
            assert_eq!(
                read_frame(&mut io::empty(), &mut carry, 4096)
                    .unwrap_err()
                    .kind(),
                io::ErrorKind::InvalidData
            );
            assert_eq!(carry, bad);
        }
        let mut corrupt = frame.clone();
        let last = corrupt.len() - 2;
        corrupt[last] = if corrupt[last] == b'9' { b'0' } else { b'9' };
        assert!(read_frame(&mut io::empty(), &mut corrupt, 4096).is_err());
        assert!(
            read_frame(
                &mut io::Cursor::new(&frame),
                &mut Vec::new(),
                frame.len() - 1
            )
            .is_err()
        );
        assert_eq!(
            read_frame(&mut io::Cursor::new(&frame), &mut Vec::new(), frame.len()).unwrap(),
            frame
        );
        assert!(read_frame(&mut io::empty(), &mut vec![0; 4097], 4096).is_err());
    }
}
