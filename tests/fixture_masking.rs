//! Every committed fixture is masked (ibx#484): no account id, no username,
//! no MAC address or machine fingerprint, in the text and in the raw bytes
//! of the recorded frames (`raw_b64`). The rules are those of
//! tests/fixtures/gw1040/README.md: account `DUXXXXXXX`, username `{user}`,
//! machine fingerprint `{hwid}|XX:XX:XX:XX:XX:XX`.

use std::path::{Path, PathBuf};

use base64::Engine as _;

/// Committed data trees checked by this test.
const ROOTS: &[&str] = &["tests/fixtures", "tests/coverage"];

/// `DU`, `DF`, `U` or `F` followed by 6 to 8 digits, as a whole word, and
/// not an 8349 signature (8 hex characters that may look like an account).
fn account_ids(b: &[u8]) -> Vec<String> {
    let word = |i: usize| b.get(i).is_some_and(|c| c.is_ascii_alphanumeric());
    let mut out = Vec::new();
    for i in 0..b.len() {
        if i > 0 && word(i - 1) {
            continue;
        }
        let prefix = if b[i..].starts_with(b"DU") || b[i..].starts_with(b"DF") {
            2
        } else if b[i] == b'U' || b[i] == b'F' {
            1
        } else {
            continue;
        };
        let digits = b[i + prefix..].iter().take_while(|c| c.is_ascii_digit()).count();
        let end = i + prefix + digits;
        if (6..=8).contains(&digits) && !word(end) && !b[..i].ends_with(b"8349=") {
            out.push(String::from_utf8_lossy(&b[i..end]).into_owned());
        }
    }
    out
}

/// Six hex pairs joined by `:`.
fn mac_addresses(b: &[u8]) -> Vec<String> {
    let hex = |c: u8| c.is_ascii_hexdigit();
    let mut out = Vec::new();
    for i in 0..b.len().saturating_sub(16) {
        let w = &b[i..i + 17];
        if (0..6).all(|k| hex(w[3 * k]) && hex(w[3 * k + 1])) && (0..5).all(|k| w[3 * k + 2] == b':') {
            out.push(String::from_utf8_lossy(w).into_owned());
        }
    }
    out
}

/// Usernames in the login fields: `;521;S<user>;` (connect request) and
/// `96=S<user>/` (farm logon), unless masked.
fn usernames(b: &[u8]) -> Vec<String> {
    let mut out = Vec::new();
    for (start, stops) in [(&b";521;S"[..], &b";|\x00"[..]), (&b"96=S"[..], &b"/|\x00\x01;"[..])] {
        let mut from = 0;
        while let Some(p) = b[from..].windows(start.len()).position(|w| w == start) {
            let at = from + p + start.len();
            let len = b[at..].iter().position(|c| stops.contains(c)).unwrap_or(b.len() - at);
            let name = &b[at..at + len];
            if !name.is_empty() && !name.starts_with(b"{user}") {
                out.push(String::from_utf8_lossy(name).into_owned());
            }
            from = at;
        }
    }
    out
}

/// What is not masked in `b`, by kind.
fn leaks(b: &[u8], username: Option<&str>) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    out.extend(account_ids(b).into_iter().map(|v| format!("account id {v}")));
    out.extend(mac_addresses(b).into_iter().filter(|v| v != "XX:XX:XX:XX:XX:XX").map(|v| format!("MAC {v}")));
    out.extend(usernames(b).into_iter().map(|v| format!("username {v}")));
    if let Some(user) = username {
        let lower = String::from_utf8_lossy(b).to_lowercase();
        if lower.contains(&user.to_lowercase()) {
            out.push("the IB_USERNAME of this machine".to_string());
        }
    }
    out
}

fn files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            files(&path, out);
        } else {
            out.push(path);
        }
    }
}

#[test]
fn committed_fixtures_are_masked() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    // The username of this machine, when the environment has one: it must
    // not appear anywhere (a CI run has none and checks the patterns only).
    let username = std::env::var("IB_USERNAME").ok().filter(|u| u.trim().len() >= 3);
    let mut paths = Vec::new();
    for dir in ROOTS {
        files(&root.join(dir), &mut paths);
    }
    assert!(paths.len() > 10, "fixture files found: {}", paths.len());
    let mut raw_records = 0;
    let mut found = Vec::new();
    for path in &paths {
        let bytes = std::fs::read(path).unwrap();
        let name = path.strip_prefix(root).unwrap().display().to_string();
        for leak in leaks(&bytes, username.as_deref()) {
            found.push(format!("{name}: {leak}"));
        }
        if name.ends_with(".jsonl") {
            for (n, line) in String::from_utf8_lossy(&bytes).lines().enumerate() {
                let Ok(rec) = serde_json::from_str::<serde_json::Value>(line) else { continue };
                let Some(b64) = rec["raw_b64"].as_str() else { continue };
                let raw = base64::engine::general_purpose::STANDARD.decode(b64).unwrap();
                raw_records += 1;
                for leak in leaks(&raw, username.as_deref()) {
                    found.push(format!("{name}:{} raw: {leak}", n + 1));
                }
            }
        }
    }
    assert!(raw_records > 1000, "raw records checked: {raw_records}");
    assert!(found.is_empty(), "unmasked values in committed fixtures:\n{}", found.join("\n"));
}

#[test]
fn the_check_finds_unmasked_values() {
    let sample = b"1=DU1234567\x01x U7654321 F12345678;521;Ssomeone;96=Sfarmuser/ 0a:1b:2c:3d:4e:5f";
    let got = leaks(sample, Some("SOMEONE"));
    for want in ["account id DU1234567", "account id U7654321", "account id F12345678", "username someone",
                 "username farmuser", "MAC 0a:1b:2c:3d:4e:5f", "the IB_USERNAME of this machine"] {
        assert!(got.iter().any(|g| g == want), "{want} not found in {got:?}");
    }
    let masked = b"1=DUXXXXXXX\x018349=F1234567\x01;521;S{user};96=S{user}xx/ {hwid}|XX:XX:XX:XX:XX:XX USD FUT";
    assert!(leaks(masked, None).is_empty(), "{:?}", leaks(masked, None));
}
