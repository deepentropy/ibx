//! The coverage matrix (ibx#484): tests/coverage/matrix.csv lists what the
//! engine has to match and the test that checks each, or "no test".
//!
//! Rows (`kind,id,description,test`):
//! - `rule`: a numbered section of the gateway translation specs;
//! - `error`: an API error code of the gateway catalog;
//! - `writer`: a field of the new order (35=D), replace (35=G) and cancel
//!   (35=F) messages, as the gateway's writers emit them;
//! - `order_request`: a variant of `OrderRequest`;
//! - `eclient`: a public `EClient` method.
//!
//! The `test` column holds up to three `<path>::<test fn>` joined by ` ; `
//! (an ignored live test marked `live:`), or `no test`. It was filled from
//! the code: a test is named when it cites the spec section, asserts the
//! error code, holds a captured frame of that message with the field,
//! builds the request variant, or calls the client method.
//!
//! This test fails when a row has no entry, names a test that does not
//! exist, or when a request variant or client method has no row.

use std::collections::HashSet;
use std::path::Path;

const KINDS: &[&str] = &["rule", "error", "writer", "order_request", "eclient"];

/// The fields of one CSV line (quotes and doubled quotes handled).
fn csv_fields(line: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quoted = false;
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        match (c, quoted) {
            ('"', true) if chars.peek() == Some(&'"') => {
                cur.push('"');
                chars.next();
            }
            ('"', _) => quoted = !quoted,
            (',', false) => out.push(std::mem::take(&mut cur)),
            _ => cur.push(c),
        }
    }
    out.push(cur);
    out
}

fn rows() -> Vec<Vec<String>> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/coverage/matrix.csv");
    let text = std::fs::read_to_string(path).unwrap();
    let mut lines = text.lines();
    assert_eq!(lines.next(), Some("kind,id,description,test"));
    lines.filter(|l| !l.trim().is_empty()).map(csv_fields).collect()
}

/// Whether `file` (relative to the crate) has a test function `name`, and
/// whether it is ignored.
fn find_test(file: &str, name: &str) -> Option<bool> {
    let text = std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join(file)).ok()?;
    let lines: Vec<&str> = text.lines().collect();
    let at = lines.iter().position(|l| {
        let l = l.trim_start();
        let l = l.strip_prefix("pub ").unwrap_or(l);
        l.starts_with(&format!("fn {name}("))
    })?;
    let attrs: Vec<&str> = lines[..at].iter().rev().take_while(|l| l.trim_start().starts_with("#[")).copied().collect();
    attrs.iter().any(|a| a.trim() == "#[test]").then(|| attrs.iter().any(|a| a.contains("ignore")))
}

#[test]
fn every_row_names_its_test_or_says_no_test() {
    let rows = rows();
    assert!(rows.len() > 1000, "rows: {}", rows.len());
    let mut seen = HashSet::new();
    let mut problems = Vec::new();
    let mut tested = std::collections::BTreeMap::<&str, (usize, usize)>::new();
    for (n, row) in rows.iter().enumerate() {
        let line = n + 2;
        let [kind, id, _description, test] = &row[..] else {
            problems.push(format!("line {line}: {} fields", row.len()));
            continue;
        };
        if !KINDS.contains(&kind.as_str()) {
            problems.push(format!("line {line}: kind {kind:?}"));
        }
        if !seen.insert((kind.clone(), id.clone())) {
            problems.push(format!("line {line}: {kind} {id} twice"));
        }
        let entry = tested.entry(KINDS.iter().find(|k| *k == kind).copied().unwrap_or("?")).or_default();
        entry.1 += 1;
        if test.trim().is_empty() {
            problems.push(format!("line {line}: {kind} {id} has no entry (a test, or \"no test\")"));
            continue;
        }
        if test == "no test" {
            continue;
        }
        entry.0 += 1;
        for reference in test.split(" ; ") {
            let (live, reference) = match reference.strip_prefix("live:") {
                Some(r) => (true, r),
                None => (false, reference),
            };
            let Some((file, name)) = reference.split_once("::") else {
                problems.push(format!("line {line}: {reference:?} is not <path>::<test fn>"));
                continue;
            };
            match find_test(file, name) {
                None => problems.push(format!("line {line}: no test {name} in {file}")),
                Some(ignored) if ignored != live => problems.push(format!(
                    "line {line}: {reference} is {}ignored but {}marked live:", if ignored { "" } else { "not " },
                    if live { "" } else { "not " })),
                Some(_) => {}
            }
        }
    }
    for (kind, (with_test, total)) in &tested {
        println!("{kind}: {with_test} of {total} rows with a test");
    }
    assert!(problems.is_empty(), "matrix.csv:\n{}", problems.join("\n"));
}

fn matrix_ids(kind: &str) -> HashSet<String> {
    rows().into_iter().filter(|r| r[0] == kind).map(|r| r[1].clone()).collect()
}

#[test]
fn every_order_request_variant_has_a_row() {
    // A Windows checkout has CRLF line ends.
    let types = std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("src/types.rs")).unwrap()
        .replace("\r\n", "\n");
    let block = &types[types.find("pub enum OrderRequest {").unwrap()..];
    let block = &block[..block.find("\n}\n").unwrap()];
    let variants: Vec<&str> = block.lines()
        .filter_map(|l| l.strip_prefix("    "))
        .filter(|l| l.starts_with(|c: char| c.is_ascii_uppercase()))
        .map(|l| l.split(|c: char| !c.is_alphanumeric()).next().unwrap())
        .collect();
    assert!(variants.len() > 30, "{variants:?}");
    let rows = matrix_ids("order_request");
    let missing: Vec<&&str> = variants.iter().filter(|v| !rows.contains(**v)).collect();
    assert!(missing.is_empty(), "OrderRequest variants with no row in matrix.csv: {missing:?}");
}

#[test]
fn every_public_client_method_has_a_row() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/api/client");
    let rows = matrix_ids("eclient");
    let mut missing = Vec::new();
    let mut count = 0;
    for entry in std::fs::read_dir(dir).unwrap().flatten() {
        let path = entry.path();
        if path.file_name().unwrap() == "tests.rs" {
            continue;
        }
        let text = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        for (i, l) in lines.iter().enumerate() {
            let Some(rest) = l.trim_start().strip_prefix("pub fn ") else { continue };
            let name = rest.split('(').next().unwrap();
            let attrs = lines[..i].iter().rev()
                .take_while(|a| a.trim_start().starts_with("///") || a.trim_start().starts_with("#["))
                .any(|a| a.contains("doc(hidden)") || a.contains("cfg("));
            if attrs || name == "new" || name == "from_parts" || name.ends_with("_for_test") {
                continue;
            }
            count += 1;
            if !rows.contains(name) {
                missing.push(name.to_string());
            }
        }
    }
    assert!(count > 80, "client methods found: {count}");
    assert!(missing.is_empty(), "EClient methods with no row in matrix.csv: {missing:?}");
}
