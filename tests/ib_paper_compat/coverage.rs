//! Endpoint coverage manifest and gap reporting for IB compatibility suite.
//!
//! This phase enforces that every `ControlCommand` variant is either:
//! - covered by an integration phase in this suite, or
//! - explicitly listed as a known gap with rationale.

use std::collections::BTreeSet;

use super::common::Conns;

const TESTED_CONTROL_COMMANDS: &[&str] = &[
    "Subscribe",
    "Ping", // rtt_ping_phase_live (ibx#158)
    "Unsubscribe",
    "SubscribeTbt",
    "UnsubscribeTbt",
    "SubscribeNews",
    "UnsubscribeNews", // rust_api_gt api_shared_requests_live (ibx#444)
    "SubscribeGeneric", // rust_api_gt api_b2_regular_hours_live (ibx#450)
    "UnsubscribeGeneric", // rust_api_gt api_b2_regular_hours_live (ibx#450)
    "UpdateParam",
    "Order",
    "RegisterInstrument",
    "RegisterOrderContract", // order_paths_paper order_by_symbol (ibx#486)
    "HoldOrder", // order_paths_paper bracket_with_transmit_off (ibx#509)
    "OrderGroup", // order_paths_paper bracket_with_transmit_off (ibx#547)
    "FetchHistorical",
    "CancelHistorical",
    "FetchHeadTimestamp",
    "FetchContractDetails",
    "CancelHeadTimestamp",
    "FetchMatchingSymbols",
    "FetchSecDefOptParams", // rust_api_gt api_option_chains_live (ibx#440)
    "FetchScannerParams",
    "SubscribeScanner",
    "CancelScanner",
    "FetchHistoricalNews",
    "FetchNewsArticle",
    "FetchFundamentalData",
    "CancelFundamentalData",
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
    "Shutdown",
];

const KNOWN_CONTROL_COMMAND_GAPS: &[(&str, &str)] = &[
    (
        "MarketDataSlot",
        "The slot of a market data request, told ahead of the request (ibx#561); part of req_mkt_data, covered by the client and engine unit tests and the market data phases",
    ),
    (
        "LocalError",
        "An error of the client itself, given to the engine for its turn (ibx#561), not a request; covered by the engine unit tests",
    ),
    (
        "Turn",
        "The turn of a request the client answers itself (ibx#561), not a request of its own; covered by the engine unit tests",
    ),
    (
        "Unpaced",
        "A wrapper around another command for the pacing of requests (ibx#555), not a request; covered by the engine and sender unit tests",
    ),
    (
        "DisplayGroupLookup",
        "Checked on the paper account by examples/ex424_display_groups (ibx#424), not by a phase of this suite",
    ),
    (
        "FetchNewsProviders",
        "Gateway-local response path, no CCP round-trip in hot loop yet",
    ),
    (
        "FetchSmartComponents",
        "Gateway-local response path, no CCP round-trip in hot loop yet",
    ),
    (
        "FetchSoftDollarTiers",
        "Gateway-local response path, no CCP round-trip in hot loop yet",
    ),
    (
        "FetchUserInfo",
        "Gateway-local response path, no CCP round-trip in hot loop yet",
    ),
    (
        "FetchMktDepthExchanges",
        "Exchange list is cached once from the 6040=102 init burst into a per-session \
         SharedState, consumed before any phase's hot loop runs — not re-requestable, \
         so a phase-model integration test cannot observe a non-empty result",
    ),
    (
        "DropSnapshot",
        "Sent only by the regulatory snapshot fetcher of the clients (req_mkt_data with          regulatory_snapshot) when a fetch ends; the regulatory snapshot is billed on live          accounts, so no phase sends it. Exercised by unit tests",
    ),
    (
        "SubscribeAccountSummary",
        "Sent only by EClient::req_account_summary; no phase of this suite sends it \
         (phase 106 reads the account state). Exercised against the server by \
         tests/rust_api_gt.rs (req_account_summary) and by unit tests",
    ),
    (
        "CancelAccountSummary",
        "Sent only by EClient::cancel_account_summary (and by a reused request id); no \
         phase of this suite sends it. Exercised against the server by tests/rust_api_gt.rs \
         (cancel_account_summary) and by unit tests",
    ),
    (
        "SetInstrumentCurrency",
        "Sent only by EClient::place_order for a contract with a currency; the order phases \
         of this suite submit on the engine directly. Exercised against the server by \
         tests/rust_api_gt.rs (place_order on SPY USD) and by unit tests",
    ),
    (
        "SetMarketDataType",
        "Sent only by EClient::req_market_data_type; no phase of this suite and no Rust \
         server test sends it. Unit tests only",
    ),
];

const KNOWN_RUST_API_GAPS: &[(&str, &str)] = &[
    (
        "Options calculations endpoints",
        "Not implemented in Rust endpoint layer yet",
    ),
    (
        "WSH endpoints",
        "Only the permission check is implemented (answered locally, no control command); \
         the data request is not",
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

    if !KNOWN_CONTROL_COMMAND_GAPS.is_empty() {
        println!("  Known command gaps:");
        for (name, why) in KNOWN_CONTROL_COMMAND_GAPS {
            println!("    - {}: {}", name, why);
        }
    }
    if !KNOWN_RUST_API_GAPS.is_empty() {
        println!("  Known API gaps:");
        for (name, why) in KNOWN_RUST_API_GAPS {
            println!("    - {}: {}", name, why);
        }
    }

    assert!(
        missing.is_empty(),
        "Untracked ControlCommand variants in coverage manifest: {:?}",
        missing
    );
    pass!("  PASS\n");
    conns
}

fn enum_variants_from_types(enum_name: &str) -> Vec<String> {
    let src = include_str!("../../src/types.rs");
    let marker = format!("pub enum {} {{", enum_name);
    let start = src
        .find(&marker)
        .expect("enum declaration not found in src/types.rs");
    let body = &src[start + marker.len()..];

    let mut variants = Vec::new();
    let mut depth: i32 = 1;

    for raw_line in body.lines() {
        let line = raw_line.trim();
        if line.starts_with("///") || line.is_empty() {
            continue;
        }

        depth += line.matches('{').count() as i32;
        depth -= line.matches('}').count() as i32;

        if depth == 0 {
            break;
        }

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
    }

    variants.sort();
    variants.dedup();
    variants
}

#[test]
fn control_command_manifest_tracks_all_variants() {
    let variants = enum_variants_from_types("ControlCommand");
    let tested: BTreeSet<&str> = TESTED_CONTROL_COMMANDS.iter().copied().collect();
    let gaps: BTreeSet<&str> = KNOWN_CONTROL_COMMAND_GAPS.iter().map(|(k, _)| *k).collect();
    let missing: Vec<_> = variants
        .iter()
        .filter(|v| !tested.contains(v.as_str()) && !gaps.contains(v.as_str()))
        .cloned()
        .collect();
    assert!(
        missing.is_empty(),
        "Missing coverage mapping for variants: {:?}",
        missing
    );
}
