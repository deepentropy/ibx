//! Every error code ibx can raise against the gateway's error table
//! (ibx#485 item 1): the code exists in the gateway, the text is the
//! gateway's message with its placeholders filled, and the code is sent
//! as itself or as the cause of 321 / 322 as the gateway sends it.
//!
//! The codes ibx raises are read from its source (the library code, not
//! its tests): a code literal next to a text (`(110, "The price ...")`,
//! `(201, format!("Order rejected - reason:{}", ..))`, `(200,
//! NO_SECURITY_DEFINITION)`), and the code argument of the error queues
//! (`push_order_error(id, 399, message)`).

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

use super::catalog::{self, matches_template};

/// Codes ibx raises that are not gateway codes: the official API client
/// raises them itself.
const CLIENT_CODES: &[(i64, &str)] = &[
    (504, "the API client's 'Not connected' (the client library's own code, not a gateway code)"),
];

/// One place where ibx raises an error.
#[derive(Debug, Clone)]
struct Raise {
    code: i64,
    /// The text as a template (`%s` for each value), when the source gives it.
    text: Option<String>,
    at: String,
}

/// The modules a file declares under `#[cfg(test)]` (`mod name;`).
fn test_modules(text: &str) -> Vec<String> {
    let lines: Vec<&str> = text.lines().map(str::trim).collect();
    lines.windows(2).filter(|w| w[0] == "#[cfg(test)]")
        .filter_map(|w| w[1].strip_prefix("pub(crate) ").unwrap_or(w[1]).strip_prefix("mod "))
        .filter_map(|m| m.strip_suffix(';'))
        .map(str::to_string)
        .collect()
}

fn library_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let mut skip: Vec<String> = Vec::new();
    for parent in ["mod.rs", "lib.rs"] {
        if let Ok(text) = std::fs::read_to_string(dir.join(parent)) {
            skip.extend(test_modules(&text));
        }
    }
    for entry in std::fs::read_dir(dir).unwrap().flatten() {
        let path = entry.path();
        let name = path.file_name().unwrap().to_string_lossy().to_string();
        if skip.iter().any(|m| *m == name.trim_end_matches(".rs")) {
            continue;
        }
        if path.is_dir() {
            // Test helpers and the benchmark binaries are not the library.
            if name != "test_support" && name != "bin" {
                library_files(&path, out);
            }
        } else if name.ends_with(".rs") && !name.contains("test") {
            out.push(path);
        }
    }
}

/// The source without its `#[cfg(test)]` items.
fn without_tests(text: &str) -> String {
    let lines: Vec<&str> = text.lines().collect();
    let mut out = Vec::new();
    let mut k = 0;
    while k < lines.len() {
        if lines[k].trim() == "#[cfg(test)]" {
            k += 1;
            let mut depth = 0i32;
            let mut opened = false;
            while k < lines.len() {
                let l = lines[k];
                depth += l.matches('{').count() as i32 - l.matches('}').count() as i32;
                opened |= l.contains('{');
                k += 1;
                if (opened && depth <= 0) || (!opened && l.trim_end().ends_with(';')) {
                    break;
                }
            }
            continue;
        }
        out.push(lines[k]);
        k += 1;
    }
    out.join("\n")
}

/// The string literal starting at `at` (a `"`), unescaped (a `\` at the
/// end of a line joins the next one without its indent), and the index
/// after it.
fn literal(src: &[u8], at: usize) -> Option<(String, usize)> {
    if src.get(at) != Some(&b'"') {
        return None;
    }
    let mut out = Vec::new();
    let mut i = at + 1;
    while i < src.len() {
        match src[i] {
            b'"' => return Some((String::from_utf8_lossy(&out).into_owned(), i + 1)),
            b'\\' => {
                i += 1;
                match src.get(i)? {
                    b'\n' | b'\r' => {
                        while i < src.len() && src[i].is_ascii_whitespace() {
                            i += 1;
                        }
                        continue;
                    }
                    b'n' => out.push(b'\n'),
                    b't' => out.push(b'\t'),
                    c => out.push(*c),
                }
            }
            c => out.push(c),
        }
        i += 1;
    }
    None
}

fn skip_ws(src: &[u8], mut i: usize) -> usize {
    while i < src.len() && src[i].is_ascii_whitespace() {
        i += 1;
    }
    i
}

fn ident(src: &[u8], i: usize) -> Option<(String, usize)> {
    let mut j = i;
    while j < src.len() && (src[j].is_ascii_alphanumeric() || src[j] == b'_' || src[j] == b':') {
        j += 1;
    }
    (j > i).then(|| (String::from_utf8_lossy(&src[i..j]).into_owned(), j))
}

/// A format string as a template: each `{...}` becomes `%s`.
fn template(format: &str) -> String {
    let mut out = String::new();
    let mut chars = format.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '{' if chars.peek() == Some(&'{') => {
                chars.next();
                out.push('{');
            }
            '{' => {
                for c in chars.by_ref() {
                    if c == '}' {
                        break;
                    }
                }
                out.push_str("%s");
            }
            '}' if chars.peek() == Some(&'}') => {
                chars.next();
                out.push('}');
            }
            c => out.push(c),
        }
    }
    out
}

/// The text of the expression at `i`: a literal, a `format!` (its leading
/// `{}` filled with a constant argument), or a constant (`NAME`,
/// `NAME.to_string()`, `NAME.1`). `None` when it is a variable.
fn text_at(src: &[u8], i: usize, consts: &HashMap<String, String>) -> Option<String> {
    let i = skip_ws(src, i);
    let i = if src.get(i) == Some(&b'&') { i + 1 } else { i };
    if let Some((lit, _)) = literal(src, i) {
        return Some(template(&lit));
    }
    let (name, end) = ident(src, i)?;
    if name == "format!" || (name == "format" && src.get(end) == Some(&b'!')) {
        let mut j = end;
        while j < src.len() && src[j] != b'"' {
            j += 1;
        }
        let (fmt, after) = literal(src, j)?;
        // Leading `{}` filled with constant arguments, in order.
        let mut text = template(&fmt);
        let mut k = after;
        while text.starts_with("%s") {
            k = skip_ws(src, k);
            if src.get(k) != Some(&b',') {
                break;
            }
            let Some((arg, next)) = ident(src, skip_ws(src, k + 1)) else { break };
            let Some(value) = consts.get(arg.rsplit("::").next().unwrap()) else { break };
            text = format!("{}{}", value, &text[2..]);
            k = next;
        }
        return Some(text);
    }
    let name = name.rsplit("::").next().unwrap().to_string();
    consts.get(&name).cloned()
}

/// `const NAME: &str = "..."` of the library, and the text of the
/// `(code, text)` constants (`NAME.1`).
fn constants(sources: &[(String, String)]) -> HashMap<String, String> {
    let mut out = HashMap::new();
    for (_, text) in sources {
        let src = text.as_bytes();
        let mut from = 0;
        while let Some(at) = text[from..].find("const ") {
            let i = from + at + 6;
            from = i;
            let Some((name, end)) = ident(src, i) else { continue };
            // The name without the `:` of its type.
            let name = name.trim_end_matches(':').to_string();
            let rest = &text[end..];
            let Some(eq) = rest.find('=') else { continue };
            let ty = &rest[..eq];
            if !(ty.contains("&str") || ty.contains("&'static str")) || ty.contains('(') {
                continue;
            }
            if let Some((lit, _)) = literal(src, skip_ws(src, end + eq + 1)) {
                out.insert(name, lit);
            }
        }
    }
    out
}

/// Whether the byte before `i` can end an identifier or a number.
fn word_before(src: &[u8], i: usize) -> bool {
    i > 0 && (src[i - 1].is_ascii_alphanumeric() || src[i - 1] == b'_' || src[i - 1] == b'.')
}

fn line_of(text: &str, i: usize) -> usize {
    text[..i].matches('\n').count() + 1
}

/// The error queues of the engine and the client, with the place of their
/// code argument.
const EMITTERS: &[(&str, usize)] = &[
    ("push_order_error(", 1), ("push_historical_error(", 1), ("push_tbt_error(", 1),
    ("push_connection_notice(", 0), ("wrapper.error(", 1),
];

fn is_error_code(code: i64) -> bool {
    (100..3000).contains(&code) || (10000..11000).contains(&code) || code == 2147483647
}

/// Every place the library raises an error code.
fn raises() -> Vec<Raise> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut files = Vec::new();
    library_files(&root.join("src"), &mut files);
    files.sort();
    let sources: Vec<(String, String)> = files.iter().map(|p| {
        let rel = p.strip_prefix(root).unwrap().to_string_lossy().replace('\\', "/");
        (rel, without_tests(&std::fs::read_to_string(p).unwrap().replace("\r\n", "\n")))
    }).collect();
    let consts = constants(&sources);
    let mut out = Vec::new();
    for (file, text) in &sources {
        let src = text.as_bytes();
        // A code literal followed by a text: `(code, text)`.
        let mut i = 0;
        while i < src.len() {
            if !src[i].is_ascii_digit() || word_before(src, i) {
                i += 1;
                continue;
            }
            let start = i;
            while i < src.len() && src[i].is_ascii_digit() {
                i += 1;
            }
            let Ok(code) = text[start..i].parse::<i64>() else { continue };
            let mut j = i;
            for suffix in ["_i64", "i64", "_i32", "i32"] {
                if text[j..].starts_with(suffix) {
                    j += suffix.len();
                    break;
                }
            }
            if !is_error_code(code) || start == 0 || !matches!(src[start - 1], b'(' | b' ' | b'\n') {
                continue;
            }
            let j = skip_ws(src, j);
            if src.get(j) != Some(&b',') {
                continue;
            }
            // Only a text with a space (a tag value such as `(146, "1")`
            // is not an error), or a text constant.
            if let Some(t) = text_at(src, j + 1, &consts).filter(|t| t.contains(' ')) {
                out.push(Raise { code, text: Some(t), at: format!("{file}:{}", line_of(text, start)) });
            }
        }
        // The code argument of the error queues.
        for (name, arg) in EMITTERS {
            let mut from = 0;
            while let Some(at) = text[from..].find(name) {
                let open = from + at + name.len();
                from = open;
                // The arguments, split on the commas outside brackets.
                let mut depth = 0;
                let mut args = vec![open];
                let mut k = open;
                while k < src.len() {
                    match src[k] {
                        b'(' | b'[' | b'{' => depth += 1,
                        b')' | b']' | b'}' if depth == 0 => break,
                        b')' | b']' | b'}' => depth -= 1,
                        b'"' => {
                            k = literal(src, k).map(|(_, e)| e - 1).unwrap_or(k);
                        }
                        b',' if depth == 0 => args.push(k + 1),
                        _ => {}
                    }
                    k += 1;
                }
                let Some(&a) = args.get(*arg) else { continue };
                let a = skip_ws(src, a);
                let mut e = a;
                while e < src.len() && src[e].is_ascii_digit() {
                    e += 1;
                }
                let Ok(code) = text[a..e].parse::<i64>() else { continue };
                if !matches!(src.get(skip_ws(src, e)), Some(b',')) {
                    continue;
                }
                let t = args.get(arg + 1).and_then(|&b| text_at(src, b, &consts));
                out.push(Raise { code, text: t, at: format!("{file}:{}", line_of(text, a)) });
            }
        }
    }
    out
}

/// The cause of a 320 / 321 / 322 text, after its prefix.
fn cause(text: &str) -> Option<&str> {
    text.split_once(" : cause - ").map(|(_, c)| c)
        .or_else(|| text.strip_prefix("Error reading request:"))
        .or_else(|| text.strip_prefix("Error processing request: "))
}

/// A template made a sample text: each `%s` a value no catalog text holds.
fn sample(template: &str) -> String {
    template.replace("%s", "\u{1}")
}

/// The differences between what ibx raises and the gateway's table.
fn differences(raises: &[Raise], codes: &BTreeMap<i64, catalog::ErrorCode>) -> Vec<String> {
    let mut problems = Vec::new();
    for r in raises {
        if CLIENT_CODES.iter().any(|(c, _)| *c == r.code) {
            continue;
        }
        let Some(gw) = codes.get(&r.code) else {
            problems.push(format!("{}: ibx raises {}, a code the gateway does not have", r.at, r.code));
            continue;
        };
        let Some(text) = &r.text else { continue };
        let ibx = sample(text);
        // Only the values, or a code whose texts are written in the
        // gateway's code (no message in its table): nothing to compare.
        if ibx.chars().all(|c| c == '\u{1}') || gw.texts().is_empty() {
            continue;
        }
        if !gw.texts().iter().any(|t| matches_template(t, &ibx, true)) {
            problems.push(format!("{}: {} {:?} is not the gateway's text {:?}", r.at, r.code, text, gw.texts()));
        }
        if matches!(r.code, 320..=322) {
            continue;
        }
        if gw.wrapping == "321" || gw.wrapping == "322" {
            problems.push(format!("{}: ibx sends {} itself; the gateway sends it as the cause of {}",
                r.at, r.code, gw.wrapping));
        }
    }
    // A cause the gateway sends as its own code, never wrapped.
    for r in raises.iter().filter(|r| matches!(r.code, 321 | 322)) {
        let Some(c) = r.text.as_deref().and_then(cause) else { continue };
        let c = sample(c);
        let same: Vec<(&i64, &catalog::ErrorCode)> = codes.iter()
            .filter(|(code, _)| !matches!(**code, 320..=322))
            .filter(|(_, e)| e.texts().iter().any(|t| !t.trim().is_empty() && matches_template(t.trim_end(), &c, false)))
            .collect();
        if !same.is_empty() && same.iter().all(|(_, e)| e.wrapping == "as_is") {
            let list: Vec<&i64> = same.iter().map(|(c, _)| *c).collect();
            problems.push(format!("{}: {} with the cause {:?}: the gateway sends this text as {:?} itself",
                r.at, r.code, c, list));
        }
    }
    problems
}

/// Known differences, with the open issue: (source file, code, why).
const OPEN: &[(&str, i64, &str)] = &[
    ("src/engine/hot_loop/ccp.rs", 200,
     "ibx#485: a contract details request with no reply in time ends with 200 and an ibx text; no such \
      timeout was found in the gateway's contract details path"),
    ("src/engine/hot_loop/hmds.rs", 162,
     "ibx#485: a historical request with no reply in time ends with 162 and an ibx text; the gateway's \
      text for this case is not known"),
];

fn split_open(problems: Vec<String>) -> (Vec<String>, Vec<String>) {
    problems.into_iter().partition(|p| {
        !OPEN.iter().any(|(file, code, _)| p.starts_with(file) && p.contains(&format!(": {code} ")))
    })
}

/// The raise sites, the differences and the known differences.
fn checked() -> (Vec<Raise>, Vec<String>, Vec<String>) {
    let codes = catalog::error_codes();
    let mut raises = raises();
    raises.sort_by(|a, b| (&a.at, a.code).cmp(&(&b.at, b.code)));
    raises.dedup_by(|a, b| a.at == b.at && a.code == b.code && a.text == b.text);
    let (problems, open) = split_open(differences(&raises, &codes));
    (raises, problems, open)
}

#[test]
fn every_error_ibx_raises_is_the_gateways() {
    let (raises, problems, open) = checked();
    let distinct: std::collections::BTreeSet<i64> = raises.iter().map(|r| r.code).collect();
    println!("{} raise sites, {} codes: {:?}", raises.len(), distinct.len(), distinct);
    println!("{} known differences (OPEN):\n{}", open.len(), open.join("\n"));
    assert!(raises.len() > 150 && distinct.len() > 80, "the scan finds the raise sites");
    assert!(problems.is_empty(), "{} differences:\n{}", problems.len(), problems.join("\n"));
}

#[test]
#[ignore = "ibx#485: the 200 and 162 timeouts of contract details and historical requests use ibx texts"]
fn the_request_timeouts_use_the_gateways_texts() {
    let (_, _, open) = checked();
    assert!(open.is_empty(), "{}", open.join("\n"));
}

/// The check fails on a code the gateway does not have, on a text that
/// is not the gateway's, and on a wrapped code sent as itself.
#[test]
fn the_error_check_finds_each_kind_of_difference() {
    let codes = catalog::error_codes();
    let raise = |code, text: &str| Raise { code, text: Some(text.to_string()), at: "x".into() };
    let ok = [
        raise(201, "Order rejected - reason:%s"),
        raise(10147, "OrderId %s that needs to be cancelled is not found."),
        raise(321, "Error validating request.-'bH' : cause - Invalid trigger method"),
        raise(104, "Cannot modify a filled order."),
    ];
    assert_eq!(differences(&ok, &codes), Vec::<String>::new());
    let unknown = differences(&[raise(9999, "No such code")], &codes);
    assert!(unknown[0].contains("does not have"), "{unknown:?}");
    let text = differences(&[raise(202, "Order cancelled")], &codes);
    assert!(text[0].contains("is not the gateway's text"), "{text:?}");
    let wrapped = differences(&[raise(146, "Invalid trigger method")], &codes);
    assert!(wrapped[0].contains("as the cause of 321"), "{wrapped:?}");
    let own = differences(&[raise(321, "Error validating request.-'bH' : cause - Duplicate order id")], &codes);
    assert!(own[0].contains("itself"), "{own:?}");
}

