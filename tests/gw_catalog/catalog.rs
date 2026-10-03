//! The gateway 1040 catalogs of `tests/fixtures/gw1040/catalog/` (see its
//! README): error codes, order local refusals, order writers, order
//! attributes. Every expected value of the layer A tests comes from them.

use std::collections::{BTreeMap, HashMap};
use std::path::Path;

/// The fields of one CSV line (quotes and doubled quotes handled).
pub fn csv_fields(line: &str) -> Vec<String> {
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

/// The rows of a catalog file as maps column name to value.
pub fn rows(name: &str) -> Vec<HashMap<String, String>> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/gw1040/catalog").join(name);
    let text = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    let mut lines = text.lines().filter(|l| !l.trim().is_empty());
    let header = csv_fields(lines.next().unwrap());
    lines.map(|l| {
        let fields = csv_fields(l);
        assert_eq!(fields.len(), header.len(), "{name}: {l}");
        header.iter().cloned().zip(fields).collect()
    }).collect()
}

/// One gateway error code.
pub struct ErrorCode {
    /// The message texts (`%s` and `%TM%` placeholders), most often one.
    pub messages: Vec<String>,
    /// `as_is`, `321`, `322` or `321|as_is`: whether the code reaches the
    /// API client itself or as the cause of 321 / 322.
    pub wrapping: String,
    /// The text captured from the gateway where it differs from its
    /// message table.
    pub captured: Option<String>,
}

impl ErrorCode {
    /// Every text the gateway sends for this code: the captured one when
    /// the capture differs from the message table.
    pub fn texts(&self) -> Vec<&str> {
        match &self.captured {
            Some(text) => vec![text.as_str()],
            None => self.messages.iter().map(String::as_str).collect(),
        }
    }
}

pub fn error_codes() -> BTreeMap<i64, ErrorCode> {
    rows("error_codes.csv").into_iter().map(|r| {
        let messages = if r["message"].is_empty() {
            Vec::new()
        } else {
            r["message"].split(" || ").map(|m| m.replace("\\n", "\n")).collect()
        };
        let captured = Some(r["captured_text"].replace("\\n", "\n")).filter(|t| !t.is_empty());
        (r["code"].parse().unwrap(), ErrorCode { messages, wrapping: r["wrapping"].clone(), captured })
    }).collect()
}

/// One local refusal rule of the gateway's placeOrder path.
#[derive(Clone)]
pub struct LocalRule {
    pub id: String,
    pub code: i64,
    /// The code the API client gets (320 / 321 for the read and check
    /// stages, else the code itself).
    pub api_code: i64,
    /// The text the API client gets (`%s` placeholders).
    pub text: String,
}

pub fn local_rules() -> Vec<LocalRule> {
    rows("order_local_rules.csv").into_iter().map(|r| LocalRule {
        id: r["id"].clone(),
        code: r["code"].parse().unwrap(),
        api_code: r["api_code"].parse().unwrap(),
        text: r["text"].replace("\\n", "\n"),
    }).collect()
}

pub fn local_rule(id: &str) -> LocalRule {
    local_rules().into_iter().find(|r| r.id == id).unwrap_or_else(|| panic!("no local rule {id}"))
}

/// One emission of a writer: a tag at a position, alone or in a block.
#[derive(Clone, Debug)]
pub struct WriterRow {
    pub pos: u32,
    pub tag: u32,
    /// The block name (attributes, algo, conditions, combo...), empty for
    /// a single tag.
    pub block: String,
    /// When the gateway writes it: `always`, `never`, `attribute`,
    /// `other`, `flag:<fact>`, `type_in:<types>`, `type_not_in:<types>`,
    /// joined by ` & `.
    pub when: String,
}

/// The rows of the writer of message `msg` (`D`, `G` or `F`), in order.
pub fn writer(msg: &str) -> Vec<WriterRow> {
    rows("order_writers.csv").into_iter()
        .filter(|r| r["msg"] == msg && !r["tag"].is_empty())
        .map(|r| WriterRow {
            pos: r["pos"].parse().unwrap(),
            tag: r["tag"].parse().unwrap(),
            block: r["block"].clone(),
            when: r["when"].clone(),
        })
        .collect()
}

/// One order attribute: its tag and the kind of value it writes.
pub struct Attribute {
    pub name: String,
    pub kind: String,
}

/// The attributes by tag (a tag can have several).
pub fn attributes() -> HashMap<u32, Vec<Attribute>> {
    let mut out: HashMap<u32, Vec<Attribute>> = HashMap::new();
    for r in rows("order_attributes.csv") {
        out.entry(r["tag"].parse().unwrap()).or_default()
            .push(Attribute { name: r["attribute"].clone(), kind: r["kind"].clone() });
    }
    out
}

/// Whether `text` is an instance of the catalog `template`: `%s` and the
/// `%NAME%` placeholders stand for any text (the trademark placeholder
/// `%TM%` included). With `prefix`, `text` may go on after the template.
pub fn matches_template(template: &str, text: &str, prefix: bool) -> bool {
    let parts = template_parts(template);
    fn go(parts: &[&str], text: &str, prefix: bool) -> bool {
        match parts.split_first() {
            None => prefix || text.is_empty(),
            Some((&"\0", rest)) => (0..=text.len()).filter(|&i| text.is_char_boundary(i)).any(|i| go(rest, &text[i..], prefix)),
            Some((lit, rest)) => text.strip_prefix(lit).is_some_and(|t| go(rest, t, prefix)),
        }
    }
    go(&parts, text, prefix)
}

/// The literal parts of a template, with "\0" for each placeholder.
fn template_parts(template: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut rest = template;
    loop {
        let next = [rest.find("%s"), placeholder_at(rest)].into_iter().flatten().min();
        let Some(i) = next else { break };
        if i > 0 {
            out.push(&rest[..i]);
        }
        out.push("\0");
        let len = if rest[i..].starts_with("%s") { 2 } else { rest[i + 1..].find('%').unwrap() + 2 };
        rest = &rest[i + len..];
    }
    if !rest.is_empty() {
        out.push(rest);
    }
    out
}

/// The start of a `%NAME%` placeholder (capitals and underscores).
fn placeholder_at(text: &str) -> Option<usize> {
    let bytes = text.as_bytes();
    (0..bytes.len()).find(|&i| {
        bytes[i] == b'%' && {
            let end = bytes[i + 1..].iter().position(|&b| !(b.is_ascii_uppercase() || b == b'_'));
            matches!(end, Some(n) if n > 0 && bytes[i + 1 + n] == b'%')
        }
    })
}

#[test]
fn templates_match_their_instances() {
    assert!(matches_template("OrderId %s that needs to be cancelled is not found.",
        "OrderId 7 that needs to be cancelled is not found.", false));
    assert!(matches_template("Order rejected - reason:", "Order rejected - reason:no", true));
    assert!(!matches_template("Order rejected - reason:", "Order rejected - reason:no", false));
    assert!(matches_template("Connectivity between %SHORT_COMPNAME% and %SHORT_PRODNAME% has been lost.",
        "Connectivity between client and server has been lost.", false));
    assert!(matches_template("Max number (%s) of market depth requests", "Max number (3) of market depth requests", false));
    assert!(!matches_template("Duplicate ticker id", "Duplicate order id", true));
    assert!(matches_template("Unable to fetch %: %s", "Unable to fetch %: x", false));
}
