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
    "Shutdown",
];

const KNOWN_CONTROL_COMMAND_GAPS: &[(&str, &str)] = &[
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
    println!("--- Phase 132: Endpoint Coverage Manifest ---");

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
    println!("  PASS\n");
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
