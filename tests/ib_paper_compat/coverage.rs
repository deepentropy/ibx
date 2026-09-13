//! Endpoint coverage manifest and gap reporting for IB compatibility suite.
//!
//! This phase enforces that every `ControlCommand` variant is either:
//! - covered by an integration phase in this suite, or
//! - explicitly listed as a known gap with rationale.

use std::collections::BTreeSet;

use super::common::Conns;

const TESTED_CONTROL_COMMANDS: &[&str] = &[
    "Subscribe",
    "Unsubscribe",
    "SubscribeTbt",
    "UnsubscribeTbt",
    "SubscribeNews",
    "UnsubscribeNews",
    "UpdateParam",
    "Order",
    "RegisterInstrument",
    "FetchHistorical",
    "CancelHistorical",
    "FetchHeadTimestamp",
    "FetchContractDetails",
    "CancelHeadTimestamp",
    "FetchMatchingSymbols",
    "FetchScannerParams",
    "SubscribeScanner",
    "CancelScanner",
    "FetchHistoricalNews",
    "FetchNewsArticle",
    "FetchAdjustments",
    "FetchFundamentalData",
    "CancelFundamentalData",
    "CancelHistoricalNews",
    "FetchHistogramData",
    "CancelHistogramData",
    "FetchHistoricalTicks",
    "SubscribeRealTimeBar",
    "CancelRealTimeBar",
    "FetchHistoricalSchedule",
    "SubscribeDepth",
    "UnsubscribeDepth",
    "SubscribePnl",
    "CancelPnl",
    // Sent by the engine as it starts, and by `req_account_updates`. Every
    // phase that builds a loop exercises it.
    "RefreshAccount",
    // Covered live by `rtt_ping_phase_live`.
    "Ping",
    // Sent by the graceful shutdown phase, which stops the loop and hands the
    // connections back.
    "Shutdown",
];

const KNOWN_CONTROL_COMMAND_GAPS: &[(&str, &str)] = &[
    (
        "AlsoAskForSeries",
        "Sent where a second caller joins a contract already being watched and \
         names a series the first did not. A phase here watches each contract \
         once, so nothing joins: what the command does — asking for the \
         difference and nothing else, and recording it as an entry of the \
         subscription — is settled offline against the bytes it writes",
    ),
    (
        "MoveInstalled",
        "Said once a caller whose contract turned out to live in another slot \
         has been moved onto it. A phase here watches each contract once and \
         names it by id, so no lookup ever resolves onto a slot another caller \
         holds: what the command does — telling the engine who holds that slot \
         now, and withdrawing it where nobody arrived — is settled offline \
         against the state it changes",
    ),
    (
        "StopAskingForSeries",
        "The other half of the pair above, sent where such a joiner withdraws \
         and the subscription stays up for whoever opened it. Nothing joins in \
         a phase here, so nothing leaves one either: what the command does — \
         withdrawing the entries nobody asks for and leaving the rest — is \
         settled offline against the bytes it writes",
    ),
    (
        "CancelCorporateActions",
        "Sent where a caller stops waiting for corporate actions, which is a \
         deadline no phase waits out: the query is answered in well under it, \
         and a phase that sat through one to reach the withdrawal would spend \
         its whole run doing so",
    ),
    (
        "AdvisorConfig",
        "Not verifiable here: an \
         account that is not an advisor's holds no groups, profiles or models, \
         so the venue answers that it has none whatever is asked",
    ),
    (
        "Logout",
        "Ends the session rather than the loop. A phase that sent one would \
         take the session away from every phase after it, so the suite stops \
         the engine and leaves the socket to say the rest",
    ),
    (
        "ForceDisconnect",
        "Drops the session without telling the venue, which every phase after \
         it would then be running without",
    ),
    (
        "FetchCalendarMetaData",
        "No phase here asks for the calendar, and this harness names no \
         security-definition farm for one to be answered on: see the note in \
         `common.rs`",
    ),
    (
        "FetchCalendarEvents",
        "As above: no phase asks for the calendar",
    ),
    (
        "CancelCalendar",
        "As above: nothing is asked for, so there is nothing to withdraw",
    ),
    (
        "FetchOptionParams",
        "No phase asks for an option chain",
    ),
    (
        "FetchCompletedOrders",
        "Answered by a run of ordinary execution reports for orders placed before this \
         session, ending in the same sentinel the opening replay ends with. A phase \
         places and finishes its own orders within one session, so the window it opens \
         has nothing in it that the phase itself did not already watch finish",
    ),
    (
        "FetchMktDepthExchanges",
        "Exchange list is cached once from the 6040=102 init burst into a per-session \
         SharedState, consumed before any phase's hot loop runs — not re-requestable, \
         so a phase-model integration test cannot observe a non-empty result",
    ),
];

const KNOWN_RUST_API_GAPS: &[(&str, &str)] = &[
    (
        "Options calculations endpoints",
        "Not implemented in Rust endpoint layer yet",
    ),
    (
        "WSH endpoints",
        "Not implemented in Rust endpoint layer yet",
    ),
];

pub(super) fn phase_endpoint_coverage(conns: Conns) -> Conns {
    phase!("--- Phase 132: Endpoint Coverage Manifest ---");

    let all_variants = enum_variants_from_types("ControlCommand");
    let tested: BTreeSet<&str> = TESTED_CONTROL_COMMANDS.iter().copied().collect();
    let gaps: BTreeSet<&str> = KNOWN_CONTROL_COMMAND_GAPS.iter().map(|(k, _)| *k).collect();

    let mut missing = Vec::new();
    for variant in &all_variants {
        let v = variant.as_str();
        if !tested.contains(v) && !gaps.contains(v) {
            missing.push(variant.clone());
        }
    }

    let covered = all_variants.len().saturating_sub(missing.len());
    println!(
        "  ControlCommand coverage: {}/{} variants mapped",
        covered,
        all_variants.len()
    );
    println!("  Tested variants: {}", TESTED_CONTROL_COMMANDS.len());
    println!("  Known command gaps: {}", KNOWN_CONTROL_COMMAND_GAPS.len());
    println!(
        "  Known API gaps (outside ControlCommand): {}",
        KNOWN_RUST_API_GAPS.len()
    );

    // Each is named above by its count, so the list under it says nothing
    // when there is nothing in it.
    for (name, why) in KNOWN_CONTROL_COMMAND_GAPS {
        println!("    - command gap {name}: {why}");
    }
    for (name, why) in KNOWN_RUST_API_GAPS {
        println!("    - API gap {name}: {why}");
    }

    assert!(
        missing.is_empty(),
        "Untracked ControlCommand variants in coverage manifest: {missing:?}"
    );
    println!("  PASS\n");
    conns
}

fn enum_variants_from_types(enum_name: &str) -> Vec<String> {
    let src = include_str!("../../src/types/commands.rs");
    let marker = format!("pub enum {enum_name} {{");
    let start = src
        .find(&marker)
        .expect("enum declaration not found in src/types/commands.rs");
    let body = &src[start + marker.len()..];

    let mut variants = Vec::new();
    let mut depth: i32 = 1;

    for raw_line in body.lines() {
        let line = raw_line.trim();
        if line.starts_with("///") || line.is_empty() {
            continue;
        }

        // Read at the depth the line starts at, not the depth it leaves
        // behind. Counted first, a variant that opens a brace on the same line
        // as its name — `Subscribe {`, and every other variant with fields —
        // was already at depth two by the time the name was looked for, so it
        // is not recorded. Only unit and single-line variants are visible that
        // way, which leaves the manifest below unchecked against the rest.
        if depth == 1 && !line.starts_with('#') && line != "}" {
            let token = line
                .split(['{', '(', ',', ' '])
                .next()
                .unwrap_or_default()
                .trim();
            if !token.is_empty() && token != "}" {
                variants.push(token.to_string());
            }
        }

        depth += line.matches('{').count() as i32;
        depth -= line.matches('}').count() as i32;

        if depth == 0 {
            break;
        }
    }

    variants.sort();
    variants.dedup();
    variants
}

#[test]
fn control_command_manifest_tracks_all_variants() {
    let variants: BTreeSet<String> =
        enum_variants_from_types("ControlCommand").into_iter().collect();
    let tested: BTreeSet<&str> = TESTED_CONTROL_COMMANDS.iter().copied().collect();
    let gaps: BTreeSet<&str> = KNOWN_CONTROL_COMMAND_GAPS.iter().map(|(k, _)| *k).collect();
    let missing: Vec<_> = variants
        .iter()
        .filter(|v| !tested.contains(v.as_str()) && !gaps.contains(v.as_str()))
        .cloned()
        .collect();
    assert!(
        missing.is_empty(),
        "Missing coverage mapping for variants: {missing:?}"
    );
    // And nothing is claimed for a command that is not there. This is what
    // establishes that the enum was read at all: a manifest naming commands the
    // reading above cannot see would otherwise pass. It is also what a renamed
    // variant meets, rather than keeping the coverage granted to its old name.
    let unknown: Vec<&&str> = tested
        .iter()
        .chain(gaps.iter())
        .filter(|named| !variants.contains(**named))
        .collect();
    assert!(
        unknown.is_empty(),
        "coverage is claimed for commands ControlCommand does not have: {unknown:?}"
    );
}
