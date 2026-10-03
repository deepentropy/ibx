//! The coverage matrix (ibx#484): tests/coverage/matrix.csv lists what the
//! engine has to match and the test that checks each, or "no test".
//!
//! Rows (`kind,id,description,test`):
//! - `rule`: a numbered section of the gateway translation specs;
//! - `error`: an API error code of the gateway catalog;
//! - `writer`: a field of the new order (35=D), replace (35=G) and cancel
//!   (35=F) messages, as the gateway's writers emit them;
//! - `order_request`: a variant of `OrderRequest`;
//! - `eclient`: a public `EClient` method;
//! - `local_rule`: a local order refusal of the gateway catalog
//!   (`tests/fixtures/gw1040/catalog/order_local_rules.csv`, ibx#485);
//! - `attribute`: an order attribute of the gateway catalog
//!   (`order_attributes.csv`, ibx#485).
//! - `decoder`: a decoder of network input, with its property test
//!   (ibx#488): every decoder of src/protocol has one.
//!
//! The `test` column holds up to three `<path>::<test fn>` joined by ` ; `
//! (an ignored live test marked `live:`), `no test`, or `open #N` for a
//! gateway rule ibx does not implement yet, with its issue. It was filled
//! from the code: a test is named when it cites the spec section or the
//! rule, asserts the error code, holds a captured frame of that message
//! with the field, builds the request variant, or calls the client method;
//! the layer A table tests (ibx#485) cover every error code ibx raises,
//! every writer row, every request variant and the attributes ibx writes.
//!
//! This test fails when a row has no entry, names a test that does not
//! exist, or when a request variant or client method has no row.

use std::collections::HashSet;
use std::path::Path;

const KINDS: &[&str] = &["rule", "error", "writer", "order_request", "eclient", "local_rule", "attribute", "decoder"];

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
        if let Some(issue) = test.strip_prefix("open #") {
            if kind != "local_rule" || issue.is_empty() || !issue.bytes().all(|b| b.is_ascii_digit()) {
                problems.push(format!("line {line}: {test:?}: \"open #N\" is for a local rule, with its issue"));
            }
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

/// The rows of a gateway catalog file (`tests/fixtures/gw1040/catalog/`)
/// as maps of column name to value.
fn catalog(name: &str) -> Vec<std::collections::HashMap<String, String>> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/gw1040/catalog").join(name);
    let text = std::fs::read_to_string(path).unwrap();
    let mut lines = text.lines().filter(|l| !l.trim().is_empty());
    let header = csv_fields(lines.next().unwrap());
    lines.map(|l| header.iter().cloned().zip(csv_fields(l)).collect()).collect()
}

/// Every row of the four gateway catalogs (ibx#485) has its row: each
/// error code, each local rule, each tag of the three order writers, each
/// order attribute.
#[test]
fn every_catalog_row_has_a_row() {
    let mut missing = Vec::new();
    let errors = matrix_ids("error");
    missing.extend(catalog("error_codes.csv").iter().map(|r| r["code"].clone())
        .filter(|c| !errors.contains(c)).map(|c| format!("error {c}")));
    let rules = matrix_ids("local_rule");
    missing.extend(catalog("order_local_rules.csv").iter().map(|r| r["id"].clone())
        .filter(|id| !rules.contains(id)).map(|id| format!("local_rule {id}")));
    let writers = matrix_ids("writer");
    missing.extend(catalog("order_writers.csv").iter()
        .filter(|r| r["block"].is_empty() && !r["tag"].is_empty())
        .map(|r| format!("35={} {}", r["msg"], r["tag"]))
        .filter(|id| !writers.contains(id)).map(|id| format!("writer {id}")));
    let attributes = matrix_ids("attribute");
    missing.extend(catalog("order_attributes.csv").iter().map(|r| format!("{} {}", r["tag"], r["attribute"]))
        .filter(|id| !attributes.contains(id)).map(|id| format!("attribute {id}")));
    missing.sort();
    missing.dedup();
    assert!(missing.is_empty(), "catalog rows with no row in matrix.csv: {missing:?}");
}

/// The public functions of src/protocol that are no decoder of network
/// input: builders, signing, and the plumbing of a link.
const NOT_DECODERS: &[&str] = &[
    "connection::new", "connection::new_raw", "connection::new_mem", "connection::set_mem_read_timeout",
    "connection::set_keys", "connection::seed_buffer", "connection::has_buffered_data", "connection::try_recv",
    "connection::shutdown", "connection::set_queued_writes", "connection::has_queued_output",
    "connection::write_error", "connection::flush_queued", "connection::send_fix", "connection::send_fix_unsequenced",
    "connection::send_fixcomp", "connection::send_raw", "connection::buffered", "connection::inject_buf",
    "connection::mem_pair", "connection::set_read_timeout", "connection::set_nonblocking",
    "connection::set_write_capacity", "connection::unread_output",
    "depth_decoder::api", "depth_decoder::book_side",
    "fix::fmt_pipe", "fix::fix_checksum", "fix::fix_build", "fix::xor_fold", "fix::fix_sign",
    "fixcomp::fixcomp_build",
    "ns::ns_build", "ns::ns_build_heart_beat",
    "tick_decoder::new", "tick_decoder::remaining", "tick_decoder::tbt_field_count",
    "xyz::xyz_build", "xyz::xyz_wrap", "xyz::xyz_build_srp_v20", "xyz::xyz_build_soft_token",
    "xyz::xyz_build_swcr_token_init", "xyz::xyz_build_swcr_token_code_submission", "xyz::xyz_write_string",
];

/// Every public function of src/protocol is a decoder with its row (and so
/// its property test, ibx#488) or one of [`NOT_DECODERS`].
#[test]
fn every_protocol_decoder_has_a_property_test() {
    let rows = matrix_ids("decoder");
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/protocol");
    let mut missing = Vec::new();
    let mut count = 0;
    for entry in std::fs::read_dir(dir).unwrap().flatten() {
        let path = entry.path();
        let module = path.file_stem().unwrap().to_string_lossy().to_string();
        if module == "mod" {
            continue;
        }
        let text = std::fs::read_to_string(&path).unwrap();
        // A Windows checkout has CRLF line ends; the tests end the code.
        let text = text.replace("\r\n", "\n");
        let code = text.split("#[cfg(test)]\nmod tests").next().unwrap();
        for line in code.lines() {
            let Some(rest) = line.trim_start().strip_prefix("pub fn ") else { continue };
            let name = rest.split(['(', '<']).next().unwrap();
            let key = format!("{module}::{name}");
            if NOT_DECODERS.contains(&key.as_str()) {
                continue;
            }
            count += 1;
            if !rows.iter().any(|r| r.starts_with("protocol::") && r.ends_with(&format!("::{name}")) && r.contains(&format!("::{module}::"))) {
                missing.push(key);
            }
        }
    }
    assert!(count > 20, "decoders found: {count}");
    assert!(missing.is_empty(), "src/protocol decoders with no decoder row in matrix.csv: {missing:?}");
}
