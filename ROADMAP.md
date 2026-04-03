# IBX Roadmap: Definitive Gap Analysis and Phased Implementation Plan

**Version:** 2.1 (updated for v0.4.4)
**Date:** 2026-04-03
**Baseline commit:** `120d366` (IBX v0.4.4)
**Method:** Protocol analysis of IB Gateway behavior against IBX Rust implementation

---

## Table of Contents

1. [Executive Summary](#1-executive-summary)
2. [Current State](#2-current-state)
   - 2.1 [Verified Features](#21-verified-features)
   - 2.2 [Architecture Diagram](#22-architecture-diagram)
   - 2.3 [Coverage Dashboard](#23-coverage-dashboard)
3. [Non-Goals](#3-non-goals)
4. [Gap Summary](#4-gap-summary)
   - 4.1 [Order Lifecycle Gaps](#41-order-lifecycle-gaps)
   - 4.2 [Asset Class Gaps](#42-asset-class-gaps)
   - 4.3 [Authentication & Token Gaps](#43-authentication--token-gaps)
   - 4.4 [Connection Resilience Gaps](#44-connection-resilience-gaps)
   - 4.5 [Protocol Completeness Gaps](#45-protocol-completeness-gaps)
   - 4.6 [Advanced Feature Gaps](#46-advanced-feature-gaps)
5. [Phased Roadmap](#5-phased-roadmap)
   - Phase 0: [Paper Trading Robustness (NOW)](#phase-0-paper-trading-robustness-now)
   - Phase 1: [Protocol Completion](#phase-1-protocol-completion)
   - Phase 2: [Multi-Asset Expansion](#phase-2-multi-asset-expansion)
   - Phase 3: [Connection Resilience](#phase-3-connection-resilience)
   - Phase 4: [Production Hardening](#phase-4-production-hardening)
   - Phase 5: [Live Trading Enablement](#phase-5-live-trading-enablement)
6. [Testing Strategy](#6-testing-strategy)
7. [Decision Log](#7-decision-log)
8. [Contribution Guide](#8-contribution-guide)
9. [Appendices](#9-appendices)
   - A: [FIX Tag Coverage](#appendix-a-fix-tag-coverage)
   - B: [Order Type Coverage](#appendix-b-order-type-coverage)
   - C: [FIX Message Type Coverage](#appendix-c-fix-message-type-coverage)
   - D: [Security Type Coverage](#appendix-d-security-type-coverage)
   - E: [Order State Machine Coverage](#appendix-e-order-state-machine-coverage)
   - F: [Auth Token Type Coverage](#appendix-f-auth-token-type-coverage)
   - G: [NS Protocol Coverage](#appendix-g-ns-protocol-coverage)
   - H: [Disconnect Reason Coverage](#appendix-h-disconnect-reason-coverage)
   - I: [Connection State Coverage](#appendix-i-connection-state-coverage)
   - J: [Wrapper Callback Coverage](#appendix-j-wrapper-callback-coverage)
   - K: [ControlCommand Coverage](#appendix-k-controlcommand-coverage)
   - Z: [Glossary](#appendix-z-glossary)
10. [Expanded Task Specifications](#10-expanded-task-specifications)
    - 10.1 [Phase 0 Expanded](#101-phase-0-expanded-paper-trading-robustness)
    - 10.2 [Phase 1 Expanded](#102-phase-1-expanded-protocol-completion)
    - 10.3 [Phase 2 Expanded](#103-phase-2-expanded-multi-asset)
    - 10.4 [Phase 3 Expanded](#104-phase-3-expanded-connection-resilience)
    - 10.5 [Phase 4 Expanded](#105-phase-4-expanded-production-hardening)
    - 10.6 [Phase 5 Expanded](#106-phase-5-expanded-live-trading-enablement)
11. [Dependency Graph (Full)](#11-dependency-graph-full)
12. [Risk Register](#12-risk-register)
13. [Milestone Calendar](#13-milestone-calendar-estimated)
14. [Accuracy Verification Report](#14-accuracy-verification-report)
19. [Rust Module Architecture](#19-rust-module-architecture-current-state)
20. [Version History](#20-version-history)

---

## 1. Executive Summary

IBX is a Rust crate that connects directly to Interactive Brokers servers, bypassing the Java Gateway entirely. It speaks IB's proprietary FIX 4.1 protocol natively, authenticating via SRP-6 over a DH-encrypted channel, and decoding binary market data ticks in a pinned-core hot loop achieving 340ns tick decode latency.

At v0.4.4, IBX successfully:

- Authenticates via SRP-6 + DH key exchange + IB Key 2FA
- Receives real-time market data (L1 quotes, L2 depth, tick-by-tick) through binary tick decoding
- Tracks full 12-state order lifecycle (PreSubmitted, PendingCancel, PendingReplace, Inactive added in v0.4.4)
- Submits 37 order type variants via FIX `35=D` new order messages with bracket/OCA support
- Subscribes to options market data via dedicated UsOpt farm (v0.4.4)
- Streams real-time P&L via CCP subscription (6040=142/143, v0.4.4)
- Parses init-burst positions via 6040=75 position feed (v0.4.4)
- Fetches historical data with keepUpToDate via CCP route (v0.4.4)
- Provides ibapi-compatible Rust and Python APIs (63 Wrapper trait methods, 77+ EClient methods)
- Supports auto-reconnection for CCP, farm, and deferred secondary farm connections

Through systematic analysis of the IB Gateway's protocol behavior, the remaining gaps have been identified. Significant areas still needing work:

- **Asset class orders:** Options market data works, but options/futures/forex ORDER SUBMISSION not yet implemented
- **Connection resilience:** No structured disconnect reason handling, no hot backup failover, no session persistence
- **Feature flags:** Zero of 200+ IB user feature flags are parsed or respected
- **Auth breadth:** Only soft token (ST) and IB Key; no permanent token (PST), TWSRO, or TST flows

**Phase philosophy:** COMPLETE the protocol, BATTLE TEST on paper, THEN enable live.

| Phase | Goal | Effort | Priority |
|-------|------|--------|----------|
| **0** | Paper Trading Robustness | 11 person-days | **NOW** |
| **1** | Protocol Completion | 14 person-days | P0 |
| **2** | Multi-Asset Expansion | 7 person-days | P0 |
| **3** | Connection Resilience | 10 person-days | P1 |
| **4** | Production Hardening | 18 person-days | P2 |
| **5** | Live Trading Enablement | 10 person-days | **LAST** |
| | **Total** | **70 person-days** | |

---

## 2. Current State

### 2.1 Verified Features

**Version:** 0.4.3 (commit `7a4c7dd`, Cargo.toml edition 2024)

| Component | Status | Primary File |
|-----------|--------|-------------|
| SRP-6 Authentication | Working | `auth/srp.rs` |
| DH Key Exchange + Cipher | Working | `auth/dh.rs` |
| Auth Session Manager | Working | `auth/session.rs` |
| Crypto Primitives (AES-128-CBC, HMAC-SHA1, TLS 1.0 PRF) | Working | `auth/crypto.rs` |
| NS Protocol (#%#% framing) | Working | `protocol/ns.rs` |
| XYZ Binary Transport | Working | `protocol/xyz.rs` |
| FIX 4.1 Framing (HMAC sign/verify, checksum) | Working | `protocol/fix.rs` |
| FIX Compression (zlib, FIXCOMP) | Working | `protocol/fixcomp.rs` |
| Binary Tick Decoder (MSB bit-reader, VLQ, RTBAR) | Excellent | `protocol/tick_decoder.rs` |
| Connection Framing (TLS + raw TCP, non-blocking) | Working | `protocol/connection.rs` |
| Hot Loop (pinned-core, busy-poll, crossbeam SPSC) | Excellent | `engine/hot_loop/mod.rs` |
| CCP Message Handler (exec reports, positions, orders) | Good | `engine/hot_loop/ccp.rs` |
| Market Data Farm (ticks, depth, SmartDepth) | Good | `engine/hot_loop/farm.rs` |
| Historical Data Farm (HMDS bars, TBT, scanners) | Good | `engine/hot_loop/hmds.rs` |
| Order Builder (37 variants, FIX new/cancel/replace) | Good | `engine/hot_loop/order_builder.rs` |
| Engine Context (instrument registry, order tracking) | Good | `engine/context.rs` |
| Market State (SeqLock quotes, depth books) | Good | `engine/market_state.rs` |
| Gateway Orchestrator (CCP + farm + HMDS connect) | Working | `gateway.rs` |
| Bridge (SharedState, Event channel, SeqLock) | Good | `bridge.rs` |
| Contract Lookups + Market Rules | Good | `control/contracts.rs` |
| Historical Data Parsing | Good | `control/historical.rs` |
| News (bulletins, tick news, historical, articles) | Good | `control/news.rs` |
| Account Data (positions, PnL, summary) | Good | `control/account.rs` |
| Scanner (subscribe, params, results) | Basic | `control/scanner.rs` |
| Fundamental Data | Basic | `control/fundamental.rs` |
| Histogram Data | Basic | `control/histogram.rs` |
| ibapi-compatible API (EClient, Wrapper, types) | Good | `api/` |
| Python PyO3 Bindings | Good | `python/` |
| Type System (orders, quotes, fills, conditions) | Good | `types.rs` |
| Client Core (instrument registry, PnL, account) | Good | `client_core.rs` |
| Configuration + Constants | Good | `config.rs` |
| Benchmarks (11 binaries) | Good | `bin/` |
| IB Key 2FA (DSA 775, challenge-response) | Working | `auth/session.rs` |

**Performance baselines (from bench suite):**

| Benchmark | Result | Notes |
|-----------|--------|-------|
| Tick decode | 340ns | Binary bit-reader, zero-alloc hot path |
| First tick latency | < 5ms | Subscribe to first quote delivery |
| Limit order submission | < 1ms | FIX build + HMAC sign + TCP send |
| Order burst | 100 orders/sec | Batch drain from pending queue |
| Max instruments | 256 | `MAX_INSTRUMENTS` constant in types.rs |

### 2.2 Architecture Diagram

```
                    +-----------+
                    |  User App |
                    |  (Rust or |
                    |   Python) |
                    +-----+-----+
                          |
                   EClient / Wrapper API
                          |
                    +-----+-----+
                    | ClientCore|  client_core.rs -- instrument registry,
                    |  + Bridge |  bridge.rs      -- SharedState (SeqLock quotes,
                    +-----------+                    order queue, event channel)
                          |
                  ControlCommand SPSC
                          |
             +------------+------------+
             |     Engine Hot Loop     |  engine/hot_loop/mod.rs
             |  (pinned core, busy-    |  -- single thread, no locks
             |   poll CCP+Farm+HMDS)   |
             +---+--------+--------+--+
                 |        |        |
           +-----+  +----+---+ +--+-----+
           | CCP |  |  Farm  | |  HMDS  |
           +--+--+  +---+----+ +---+----+
              |          |         |
         TLS/FIX    TCP/FIX    TCP/FIX
              |          |         |
    +---------+--+ +-----+----+ +-+--------+
    | IB Auth    | | IB Market| | IB Hist  |
    | Server     | | Data Farm| | Data Farm|
    | (cdc1/ndc1)| | (usfarm) | | (hfarm)  |
    +------------+ +----------+ +----------+
```

### 2.3 Coverage Dashboard

File counts verified; coverage estimates labeled with `~` against commit `7a4c7dd`.

| Metric | IBX Rust | Coverage | Appendix |
|--------|----------|----------|----------|
| FIX tags defined | 21 | 100% of defined | A |
| FIX tags used (unique, all files) | ~134 | ~67% | A |
| FIX message types (35=X) | 13 | 100% of defined | C |
| NS message types | 16 | 94% | G |
| OrderRequest variants | 37 submit + 3 lifecycle | 37/37 common | B |
| Wrapper trait methods | 63 | -- | J |
| ControlCommand variants | 36 | -- | K |
| Security types (tag 167) | 8 | 27% | D |
| Order states (tag 39) | 7 | 58% | E |
| Auth token types | 2 (ST, IB Key) | 33% | F |
| Disconnect reasons | 0 (undifferentiated) | 0% | H |
| Connection states (LogonState) | 0 (implicit) | 0% | I |
| Hot backup strategies | 0 | 0% | -- |
| User feature flags | 0 | 0% | -- |
| Unit tests | 637 | -- | -- |
| Python integration tests | 16 files | -- | -- |
| Benchmark binaries | 11 | -- | -- |

---

## 3. Non-Goals

These items are explicitly out of scope. IBX is a headless trading engine, not a GUI application.

| # | Non-Goal | Rationale |
|---|----------|-----------|
| 1 | **GUI/UI components** | IBX is a library. GUI functionality is not applicable to a headless architecture. |
| 2 | **Login dialog / Welcome screen** | Headless auth -- no interactive login. |
| 3 | **TWS Workstation features** | IBX does not replicate TWS UI: charting, portfolio display, news reader UI. |
| 4 | **Legacy MD5 authentication** | Pre-XYZ protocol for dead servers. Even the Gateway marks it as legacy. |
| 5 | **Single Sign-On (SSO)** | Niche institutional feature. |
| 6 | **XOR key obfuscation** | Deprecated even in the Gateway. |
| 7 | **Log encryption** | Server-side diagnostic logging. No client value. |
| 8 | **Download/update manager** | IBX does not self-update. |
| 9 | **FIDO/WebAuthn** | Future IB feature. Implement when IB deploys it. |
| 10 | **Telemetry/diagnostics** | IBX uses Rust `log` crate. No server-side telemetry. |
| 11 | **Auto-restart manager** | Not applicable to a library. |
| 12 | **Certificate pinning UI** | IBX uses system trust store. Certificate prompts are a GUI feature. |

---

## 4. Gap Summary

This section identifies what IBX is missing, organized by functional domain. Each gap describes the protocol behavior that needs implementation.

### 4.1 Order Lifecycle Gaps

| Gap | Current State | Target State | Phase |
|-----|--------------|-------------|-------|
| Order state machine | ~~7 of 12 states~~ **DONE (v0.4.4)** — all 12 states tracked | ~~All 12~~ | ~~P1.1~~ |
| Execution restatements | Not handled | Busted/corrected trades reverse or update fills | P1.3 |
| Commission policy | Received but not parsed | Commission type (per-share, %, absolute) tracked | P1.5 |
| OCA type differentiation | ~~Group ID only~~ Bracket linking fixed (v0.4.4) | 3 types: cancel-with-block, reduce-with-block, reduce-without-block | P1.6 |
| Open/Close indicator | Not sent | Tag 77 on options orders | P1.7 |
| Manual order indicator | Not sent | Tag 1028 for MiFID II / CAT | P1.8 |
| Order state checkpointing | Not persisted | Open orders written to file, reconciled on restart | P4.2 |

### 4.2 Asset Class Gaps

| Gap | Current State | Target State | Phase |
|-----|--------------|-------------|-------|
| Security type routing | ~~Hardcoded STK/SMART/USD~~ Dynamic sec_type in subscribe (v0.4.4) | ~~Dynamic from Contract~~ Order builder still hardcoded STK/SMART/USD | P1.11 |
| Options market data | ~~Not supported~~ **UsOpt farm + option fields working (v0.4.4)** | ~~Calls/puts with strike, right, expiry~~ Greeks and exercise still needed | P2.1 |
| Options order submission | Not supported | Option orders with strike, right, expiry in order builder | P2.1 |
| Futures / FOP | Not supported | FUT and futures options with multiplier, expiry | P2.2 |
| Forex | Not supported | CASH pairs via cashfarm connection | P2.3 |
| Combos/spreads | Not supported | Multi-leg orders with per-leg ratios | P2.4 |
| Structured products | Not supported | Warrants, certificates (7 types) | P2.5 |
| Security ID sources | conId only | ISIN, CUSIP, SEDOL lookup | P2.6 |
| Volatility orders | ~~Not supported~~ IV delivery via keepUpToDate (v0.4.4) | Volatility order types still needed | P4.5 |
| Scale/iceberg orders | Not supported | Auto-replenish with price increment | P4.6 |
| Cross orders | Not supported | Buyer/seller matching | P4.7 |

### 4.3 Authentication & Token Gaps

| Gap | Current State | Target State | Phase |
|-----|--------------|-------------|-------|
| Token types | 2 (Soft Token + IB Key) | 5 (add Permanent, TWSRO, TST) | P5.2-P5.4 |
| SMS/Email 2FA | Not supported | Accept OTP via callback | P5.1 |
| Permanent session token | Not implemented | Skip re-auth on reconnect | P5.2 |
| Read-only token (TWSRO) | Not supported | Monitoring without order capability | P5.3 |
| Auth retry with recovery | No retry | Exponential backoff, host redirect, mid-auth recovery | P5.5 |
| Session persistence | Not persisted | Encrypted session file for token reuse | P3.2 |

### 4.4 Connection Resilience Gaps

| Gap | Current State | Target State | Phase |
|-----|--------------|-------------|-------|
| Disconnect reasons | Undifferentiated (retry 3x) | 22 classified reasons with appropriate responses | P1.9 |
| Connection states | Implicit in code flow | 6 explicit states (PRELOGON -> LOGGEDON) | P1.10 |
| Hot backup / failover | 3 retries to single host | Multi-host failover with pre-connected backups | P3.1 |
| Login competition | Not detected | 4 types: none, live, read-only, self. Yield/kick/degrade. | P3.3 |
| Farm reconnect replay | ~~Basic reconnect~~ Position staleness fixed on CCP reconnect (v0.4.4) | Per-farm reconnect replays all subscriptions | P3.4 |
| HMDS reconnect retry | Pending requests lost | Retry queue for all pending historical requests | P3.5 |
| Network pre-check | No pre-check | DNS + TCP connectivity check before auth | P3.6 |
| Configurable heartbeat | Hardcoded 10s | Configurable, default 5s (matching Gateway) | P3.7 |
| Read-only mode | Not supported | Market data only, no order submission | P3.8 |

### 4.5 Protocol Completeness Gaps

| Gap | Current State | Target State | Phase |
|-----|--------------|-------------|-------|
| FIX message validation | No inbound validation | Checksum, required tags, type consistency | P1.4 |
| Settlement types | Not supported | Regular, cash, next-day (tag 63) | P1.12 |
| User feature flags | 0 of 200+ parsed | Safety-critical subset parsed from logon response | P1.2 |
| FIX audit log | No logging | Every message logged with rotation and configurable verbosity | P4.1 |
| SSL certificate pinning | System trust store | Pin IB certificates, reject unexpected | P4.11 |

### 4.6 Advanced Feature Gaps

| Gap | Current State | Target State | Phase |
|-----|--------------|-------------|-------|
| Financial Advisor (FA) | reqManagedAccts works | Full: allocation profiles, groups, model portfolios | P4.8 |
| Additional algo strategies | 6 algos | Add: Accumulate/Distribute, Balance Impact Risk, MinImpact, CSFB | P4.9 |
| Order attribution | Not implemented | MiFID II / CAT regulatory tags per jurisdiction | P4.10 |
| Benchmark regression | Manual | Automated CI regression detection | P4.12 |

---

## 5. Phased Roadmap

### Phase 0: Paper Trading Robustness (NOW)

**Goal:** Stress-test every existing feature on paper trading. Find and fix bugs before expanding scope.

**Context:** The maintainer has stated: "Build everything, test everything on paper trading, ensure absolute robustness." This phase directly addresses that priority.

| ID | Task | Person-Days | Rust Target | Acceptance Criteria | Dependencies | Labels |
|----|------|-------------|-------------|-------------------|--------------|--------|
| P0.1 | Concurrent order stress test (100+ orders) | 2 | `engine/hot_loop/order_builder.rs` | 100 orders submitted in <1s, all receive exec reports, zero panics | Paper account | |
| P0.2 | CCP reconnect under load (50 symbols + 10 orders) | 2 | `engine/hot_loop/mod.rs`, `gateway.rs` | All orders reconciled via mass status, all subscriptions resume, no duplicate fills | P0.1 | |
| P0.3 | Farm disconnect/reconnect per-farm | 1 | `engine/hot_loop/farm.rs` | Disconnected farm sets stale flag, other farms unaffected, reconnect resumes subs | None | **Good First Issue** |
| P0.4 | Order modify/cancel race conditions | 1.5 | `engine/hot_loop/ccp.rs` | No panics, deterministic final state, CancelReject surfaced correctly | P0.1 | |
| P0.5 | HMDS reconnect + historical data recovery | 1 | `engine/hot_loop/hmds.rs` | Pending historical requests retried after reconnect | None | **Good First Issue** |
| P0.6 | L2 depth stress test (20 symbols, SmartDepth) | 1 | `engine/hot_loop/farm.rs` | All 20 depth books update correctly, no stale data, cancel/resub works | None | |
| P0.7 | Bracket order stress test (SL/TP with fills) | 1.5 | `engine/hot_loop/order_builder.rs` | Parent fills -> TP+SL become active, one child triggers -> other cancels via OCA | P0.1 | |
| P0.8 | Overnight session stability (8+ hours) | 1 | All | No memory leaks, heartbeats maintained, reconnects successful across IB maintenance | P0.2 | |

**Phase 0 total: 11 person-days (ongoing)**

#### P0.1 Detail: Concurrent Order Stress Test

Submit 100+ orders concurrently on paper trading. This exercises:
- `order_builder.rs`: FIX message generation for all order types
- `ccp.rs`: Execution report parsing under load
- `context.rs`: Order tracking with concurrent insert/update
- `bridge.rs`: SharedState queue capacity under burst

**Test harness requirements:**
- Submit 100 limit orders across 10 symbols (SPY, AAPL, MSFT, AMZN, GOOG, TSLA, META, NVDA, AMD, QQQ)
- Randomize: buy/sell, limit prices within 0.5% of market, qty 1-100
- Measure: time to submit all 100, time to receive all 100 execution reports
- Assert: no `OrderStatus::Uncertain`, no panics, no duplicate fills

#### P0.2 Detail: Reconnect Under Load

While actively streaming and trading, forcibly terminate the CCP connection (via TCP RST or kill the stream). This tests:
- `engine/hot_loop/mod.rs`: CCP disconnect detection within heartbeat timeout + grace
- `gateway.rs`: `reconnect_ccp()` with exponential backoff
- `ccp.rs`: Mass status request (`35=H`) after reconnect to reconcile all open orders
- `farm.rs`: Market data farms should be unaffected by CCP disconnect

**Expected behavior:** Orders transition to `Uncertain` on disconnect, then reconcile to correct state within 30 seconds of reconnect. Market data ticks continue uninterrupted on farm connections.

#### P0.7 Detail: Bracket Order Stress Test

Submit 20 bracket orders (parent + take-profit + stop-loss). For each:
- Parent is a limit buy slightly below market
- Take-profit is a limit sell 2% above entry
- Stop-loss is a stop sell 1% below entry

When a parent fills:
- Both children should become active (PreSubmitted -> Submitted)
- If TP triggers, SL should auto-cancel via OCA
- If SL triggers, TP should auto-cancel via OCA

This tests OCA group integrity under the full order lifecycle.

```
P0.1 ──> P0.2 ──> P0.8
  |          |
  +──> P0.4  +──> P0.7
P0.3 (independent)
P0.5 (independent)
P0.6 (independent)
```

**Critical path: P0.1 -> P0.2 -> P0.8**

---

### Phase 1: Protocol Completion

**Goal:** Complete the FIX message handling to achieve full order lifecycle visibility and remove hardcoded assumptions.

| ID | Task | Person-Days | Rust Target | Acceptance Criteria | Dependencies | Labels |
|----|------|-------------|-------------|-------------------|--------------|--------|
| P1.1 | Complete order state machine (add 5 missing states) | 2 | `types.rs` OrderStatus, `engine/hot_loop/ccp.rs` | All 12 states tracked, transitions match Gateway, Wrapper receives accurate status strings | None | **Good First Issue** |
| P1.2 | Parse CCP user feature flags | 2 | New `types.rs` FeatureFlags, `engine/hot_loop/ccp.rs` | Feature flags parsed from CCP logon response, stored in SharedState, accessible via API | None | |
| P1.3 | Handle execution restatements (busted/corrected trades) | 2 | `engine/hot_loop/ccp.rs` | Busted trades reverse fills, corrected trades update fill records, Wrapper notified | P1.1 | |
| P1.4 | Implement FIX message validation | 1 | New `protocol/validate.rs` | Inbound messages validated (checksum, required tags, type consistency) before processing | None | **Good First Issue** |
| P1.5 | Parse and track commission policy | 0.5 | `types.rs`, `engine/hot_loop/ccp.rs` | Commission type (per-share, percentage, absolute) parsed from exec reports | None | |
| P1.6 | Implement OCA type differentiation | 0.5 | `types.rs` OrderAttrs | OCA groups distinguish: cancel-with-block (1), reduce-with-block (2), reduce-non-block (3) | None | **Good First Issue** |
| P1.7 | Implement open/close position indicator | 0.5 | `types.rs` OrderAttrs, `order_builder.rs` | Open/Close tag (tag 77) sent on orders, especially needed for options | None | |
| P1.8 | Implement manual order indicator | 0.5 | `order_builder.rs` | ManualOrderIndicator (tag 1028) sent per MiFID II / CAT requirements | None | |
| P1.9 | Add disconnect reason classification | 1.5 | New `types.rs` DisconnectReason, `engine/hot_loop/*.rs` | Disconnects classified (broken socket, inactivity, auth failure, redirect, etc.), surfaced to Wrapper | None | |
| P1.10 | Implement LogonState tracking | 1 | New `types.rs` LogonState, `gateway.rs` | Formal state machine: PRELOGON -> LOGON -> FIX_SESSION_REQUEST -> FIX_SESSION_OPEN -> LOGGEDON | None | |
| P1.11 | Dynamic security type in order builder | 2 | `order_builder.rs` | Remove hardcoded STK/SMART/USD; derive tag 167/100/15 from Contract | Phase 2 prep | |
| P1.12 | Add settlement type support | 0.5 | `types.rs`, `order_builder.rs` | Settlement type tag (tag 63) for regular/cash/next-day settlement | None | |

**Phase 1 total: 14 person-days** (2 + 2 + 2 + 1 + 0.5 + 0.5 + 0.5 + 0.5 + 1.5 + 1 + 2 + 0.5 = 14)

#### P1.1 Detail: Complete Order State Machine

The Gateway defines 12 distinct order states with explicit transitions. IBX currently tracks 7. The missing 5 states cause the caller to miss important intermediate lifecycle events:

**Missing states and their significance:**

| State | When it occurs | Why it matters |
|-------|---------------|----------------|
| **Acked** (FIX 39=A PendingNew) | Server received order, routing to exchange | Confirms server receipt before exchange acceptance |
| **CancelSubmitted** (FIX 39=6 PendingCancel) | Cancel request sent, awaiting exchange response | Caller knows cancel is in progress |
| **CancelAcked** | Cancel acknowledged by IB | Intermediate cancel state |
| **CancelConfirmed** | Cancel confirmed by exchange | Currently merged with Cancelled |
| **PendingModifyAck** (FIX 39=E PendingReplace) | Modify request sent, awaiting response | Caller knows modify is in progress |

**Implementation:**
1. Add 5 new variants to `OrderStatus` enum in `types.rs`
2. Update the FIX tag 39 (OrdStatus) match in `ccp.rs` to route to correct states
3. Update `order_status_str()` in `client_core.rs` to return correct status strings
4. Add transition validation: e.g., cannot go from Filled to CancelSubmitted

#### P1.2 Detail: Parse CCP User Feature Flags

When IBX logs into the CCP server, the logon response contains a bitmap of 200+ user feature flags. These flags control:
- Which order types the account is permitted to use
- Which security types are tradeable
- Which exchanges are accessible
- Whether margin trading is enabled
- Whether short selling is permitted

Currently IBX ignores all flags, which means it may attempt operations the account is not permitted to perform, resulting in mysterious rejects.

**Implementation:**
1. Parse feature flag field from CCP logon response (tag 6XXX in `gateway.rs`)
2. Define `FeatureFlags` struct in `types.rs` with bitfield for each flag
3. Store in `SharedState::reference` for API access
4. Add `feature_flags()` method to EClient
5. Optionally: check flags before order submission to fail fast

#### P1.11 Detail: Dynamic Security Type in Order Builder

Currently, `order_builder.rs` hardcodes these tags for every order:
```
(167, "STK")    // SecurityType = Common Stock
(100, "SMART")  // ExDestination
(15, "USD")     // Currency
```

This restricts IBX to US equities only. Phase 2 (multi-asset) cannot proceed until this is parameterized.

**Implementation:**
1. Add `sec_type`, `exchange`, `currency`, `multiplier`, `right`, `strike`, `expiry` to all `OrderRequest` variants that need them (or add a `ContractSpec` struct)
2. Derive these from the `Contract` passed to `place_order()`
3. Update all ~37 order builder match arms to use dynamic values
4. For backwards compatibility, default to STK/SMART/USD when not specified

```
P1.1 ──> P1.3
P1.2
P1.4
P1.5
P1.6
P1.7
P1.8
P1.9
P1.10
P1.11
P1.12
```

**Critical path: P1.1 -> P1.3 (order state machine must be complete before restatement handling)**

---

### Phase 2: Multi-Asset Expansion

**Goal:** Extend IBX beyond US equities to support options, futures, forex, and international markets.

| ID | Task | Person-Days | Rust Target | Acceptance Criteria | Dependencies | Labels |
|----|------|-------------|-------------|-------------------|--------------|--------|
| P2.1 | Options support (calls, puts, exercise, assignment) | 2 | `types.rs`, `order_builder.rs`, `api/types.rs` Contract | Options orders with strike, right (C/P), expiry. Exercise/assignment notifications parsed. | P1.11 | |
| P2.2 | Futures support (FUT + FOP) | 1 | `order_builder.rs`, `types.rs` | Futures and futures options orders with multiplier, expiry | P1.11 | |
| P2.3 | Forex support (CASH) | 1 | `order_builder.rs`, cashfarm connection | Forex pairs (e.g., EUR.USD) via cashfarm, correct lot sizes | P1.11 | **Good First Issue** |
| P2.4 | Combo/spread orders | 1.5 | `order_builder.rs`, `api/types.rs` ComboLeg | Multi-leg combo orders with per-leg ratios and actions | P2.1 | |
| P2.5 | Structured products (warrants, certificates) | 0.5 | `types.rs`, `order_builder.rs` | Warrant and structured product order support | P1.11 | |
| P2.6 | Security ID sources (ISIN, CUSIP, SEDOL) | 0.5 | `api/types.rs` Contract | Lookup by ISIN/CUSIP/SEDOL in addition to conId | None | **Good First Issue** |
| P2.7 | International exchanges (EU, Japan, HK) | 0.5 | `gateway.rs` (eufarm, jfarm connections) | Connect to EU and Japan farm slots, route orders to non-US exchanges | P1.11 | |

**Phase 2 total: 7 person-days**

#### P2.1 Detail: Options Support

Options are the highest-demand missing asset class. Implementation requires:

1. **Contract specification:** `api/types.rs` Contract already has `strike`, `right`, `last_trade_date_or_contract_month` fields. These need to be wired to the order builder.

2. **Order builder changes:** Tags to add for options orders:
   - Tag 167 = "OPT" (SecurityType)
   - Tag 201 = "0" (PutOrCall: 0=Put, 1=Call)
   - Tag 200 = "202606" (MaturityMonthYear)
   - Tag 202 = "450.00" (StrikePrice)
   - Tag 231 = "100" (ContractMultiplier)
   - Tag 77 = "O" or "C" (OpenClose)

3. **Exercise/assignment:** Parse tag 6091 for exercise notifications. Types include:
   1. Automatic Exercise
   2. Voluntary Exercise
   3. Against (assignment)
   4. Lapse
   5. Dividend Reinvestment
   6. Tender
   7. Rights Issue
   8. Bond Exchange Offer
   9. Stock Split
   10. Reverse Stock Split

   Only types 1-4 are critical for options trading. Types 5-10 are corporate actions.

4. **Greeks:** Parse tag-delivered greeks (delta, gamma, theta, vega, implied vol) from market data. Add to Quote or separate GreeksSnapshot struct.

#### P2.3 Detail: Forex Support (Good First Issue)

Forex (CASH) is simpler than options because there are no strikes/expiries. Requirements:

1. Set tag 167 = "CASH" in order builder
2. Set tag 55 = "EUR" (base currency), tag 15 = "USD" (quote currency)
3. Use cashfarm connection slot (`HotLoop::cashfarm_conn`) instead of usfarm
4. Handle fractional lot sizes (forex trades in lots of 20,000+)

```
P1.11 ──> P2.1 ──> P2.4
       |──> P2.2
       |──> P2.3
       |──> P2.5
       |──> P2.7
P2.6 (independent)
```

**Critical path: P1.11 -> P2.1 -> P2.4**

---

### Phase 3: Connection Resilience

**Goal:** Survive IB maintenance windows, network flaps, and concurrent login competition without data loss.

| ID | Task | Person-Days | Rust Target | Acceptance Criteria | Dependencies | Labels |
|----|------|-------------|-------------|-------------------|--------------|--------|
| P3.1 | Hot backup strategy (multi-host failover) | 2.5 | `gateway.rs`, `engine/hot_loop/mod.rs` | On primary CCP failure, automatically fail to secondary host. 3 strategies: OFF, ON (2 backups), SINGLE_HOST. | None | |
| P3.2 | Session persistence (save/restore session token) | 2 | `auth/session.rs` | Session token saved to file on clean shutdown, restored on restart. Avoids full re-auth. | None | |
| P3.3 | Concurrent login competition handling | 1.5 | `gateway.rs`, new `types.rs` CompetitionEvent | Detect "another client logged in" event. Options: yield, kick, read-only mode. Surface to Wrapper. | None | |
| P3.4 | Farm-specific reconnect with subscription replay | 1.5 | `engine/hot_loop/farm.rs` | Per-farm reconnect replays all market data subscriptions, depth subscriptions, and TBT subscriptions. | P0.3 | |
| P3.5 | HMDS reconnect with request retry | 1 | `engine/hot_loop/hmds.rs` | Pending historical, scanner, and fundamental requests retried after HMDS reconnect. | P0.5 | |
| P3.6 | Network connectivity pre-check | 0.5 | New `protocol/connectivity.rs` | DNS resolution and TCP connectivity check before auth attempt. | None | **Good First Issue** |
| P3.7 | Configurable heartbeat intervals | 0.5 | `config.rs`, `engine/hot_loop/mod.rs` | CCP and farm heartbeat intervals configurable via env var or API. | None | **Good First Issue** |
| P3.8 | Read-only connection mode | 0.5 | `gateway.rs`, `config.rs` | Connect in read-only mode: market data only, no order submission. | None | |

**Phase 3 total: 10 person-days**

```
P3.1 (independent)
P3.2 (independent)
P3.3 (independent)
P0.3 ──> P3.4
P0.5 ──> P3.5
P3.6 (independent)
P3.7 (independent)
P3.8 (independent)
```

**Critical path: P3.1 (longest single task, no external dependency)**

---

### Phase 4: Production Hardening

**Goal:** Audit trail, chaos testing, soak testing, and operational tooling for production deployment confidence.

| ID | Task | Person-Days | Rust Target | Acceptance Criteria | Dependencies | Labels |
|----|------|-------------|-------------|-------------------|--------------|--------|
| P4.1 | FIX message audit log | 2 | New `protocol/audit.rs` | Every FIX message sent/received is logged with timestamp to rotated file. Configurable: off/summary/full. | None | |
| P4.2 | Order state checkpointing | 2 | New `engine/checkpoint.rs` | Open orders periodically written to file. On restart, last known state is loaded for reconciliation. | P1.1 | |
| P4.3 | Chaos testing framework | 3 | New test infrastructure | Kill CCP mid-order, kill farm during tick burst, inject corrupt FIX messages, simulate 500ms latency spikes. Automated. | P0.2 | |
| P4.4 | Soak test (72-hour continuous run) | 2 | Test script + monitoring | Run IBX for 72 hours on paper: track memory (RSS), file descriptors, reconnect count, tick rate. All stable. | P4.3 | |
| P4.5 | Volatility order support | 2 | `types.rs`, `order_builder.rs` | Volatility orders (implied vol, delta-neutral) with reference price types. | P2.1 | |
| P4.6 | Scale order support | 1 | `types.rs`, `order_builder.rs` | Scale/iceberg orders with auto-replenish and price increment. | None | |
| P4.7 | Cross order support | 0.5 | `types.rs`, `order_builder.rs` | Cross orders matching buyer and seller. | None | |
| P4.8 | FA (Financial Advisor) support | 2 | `api/client/stubs.rs` -> full impl | FA allocations, groups, profiles. request_fa and replace_fa fully implemented. | None | |
| P4.9 | Additional algo strategies | 1 | `types.rs` AlgoParams | Add: Accumulate/Distribute, Balance Impact Risk, MinImpact, CSFB algos. | None | |
| P4.10 | Order attribution (MiFID II / CAT) | 1 | `order_builder.rs` | Regulatory order attribution tags per jurisdiction. | P1.8 | |
| P4.11 | SSL certificate pinning | 0.5 | `gateway.rs` TLS config | Pin IB's TLS certificates. Reject unexpected certificates. | None | |
| P4.12 | Benchmark regression suite | 1 | CI configuration | Benchmarks run on every PR. Regressions flagged. Results stored. | None | |

**Phase 4 total: 18 person-days**

```
P1.1 ──> P4.2
P0.2 ──> P4.3 ──> P4.4
P2.1 ──> P4.5
P1.8 ──> P4.10
P4.1  (independent)
P4.6  (independent)
P4.7  (independent)
P4.8  (independent)
P4.9  (independent)
P4.11 (independent)
P4.12 (independent)
```

**Critical path: P0.2 -> P4.3 -> P4.4**

---

### Phase 5: Live Trading Enablement

**Goal:** Enable live trading with full auth support. This phase is EXPLICITLY LAST -- paper must be solid first.

| ID | Task | Person-Days | Rust Target | Acceptance Criteria | Dependencies | Labels |
|----|------|-------------|-------------|-------------------|--------------|--------|
| P5.1 | SMS/Email 2FA challenge | 2 | `auth/session.rs` | Receive 2FA challenge via SMS or email. Accept OTP code from caller (CLI prompt or API callback). | None | |
| P5.2 | Permanent session token (PST) flow | 2 | `auth/session.rs` | Save persistent token after first auth. Reuse on subsequent connections without full SRP. | P3.2 | |
| P5.3 | TWSRO read-only token | 1 | `auth/session.rs` | Connect with read-only token for monitoring without order capability. | None | |
| P5.4 | TST token support | 0.5 | `auth/session.rs` | Handle TST token type if encountered. | None | |
| P5.5 | Auth retry with state recovery | 1.5 | `auth/session.rs`, `gateway.rs` | On auth failure: retry with exponential backoff, handle redirect to different host, recover mid-auth state. | P3.1 | |
| P5.6 | Live account integration test suite | 2 | Test scripts | Run full test suite on live paper-to-live migration path. Document all differences between paper and live. | P5.1, P5.2 | |
| P5.7 | Security audit of auth flow | 1 | Review | Third-party review of SRP-6, DH, AES-CBC, HMAC-SHA1 implementation. No hardcoded credentials, no token leaks. | P5.2 | |

**Phase 5 total: 10 person-days**

**GATE: Phase 5 CANNOT begin until the following prerequisites are complete:**
- P0.8 (Overnight stability) -- proves paper trading is robust
- P1.1 (Order state machine) -- full lifecycle visibility required for live
- P3.1 (Hot backup) -- failover required for live trading safety
- P4.4 (72h soak test) -- stability proven under sustained operation

```
GATE: P0.8 + P1.1 + P3.1 + P4.4 must all be complete before ANY P5 task begins

P3.2 ──> P5.2 ──> P5.6
P3.1 ──> P5.5
P5.1 (independent after gate) ──> P5.6
P5.3 (independent after gate)
P5.4 (independent after gate)
P5.7 (independent, but after P5.2)
```

**Critical path: P3.2 -> P5.2 -> P5.6**

---

### Phase Summary

| Phase | Goal | Tasks | Person-Days | Good First Issues |
|-------|------|-------|-------------|-------------------|
| 0 | Paper Trading Robustness | 8 | 11 | 2 |
| 1 | Protocol Completion | 12 | 14 | 4 |
| 2 | Multi-Asset Expansion | 7 | 7 | 2 |
| 3 | Connection Resilience | 8 | 10 | 2 |
| 4 | Production Hardening | 12 | 18 | 0 |
| 5 | Live Trading Enablement | 7 | 10 | 0 |
| **Total** | | **54** | **70** | **10** |

---

## 6. Testing Strategy

### 6.1 Test Layers

| Layer | Current State | Target | Description |
|-------|--------------|--------|-------------|
| **Unit tests** | 637 tests (Rust) | 1,000+ | Cover all FIX message parse/build, state transitions, crypto, type conversions |
| **Integration tests** | 16 Python files | 30+ files | End-to-end: connect, subscribe, order, fill, disconnect |
| **Paper trading tests** | Ad-hoc | 15+ structured scenarios | Documented scenarios with acceptance criteria |
| **Chaos tests** | None | 10+ scenarios | Network partitions, corrupt messages, latency injection |
| **Soak tests** | None | 72-hour minimum | Continuous operation stability |
| **Benchmark tests** | 11 binaries | Regression CI | Automated performance regression detection |

### 6.2 Mock Gateway Architecture

For offline testing without IB connectivity:

```
+-------------+       +------------------+       +------------------+
| Test Suite  | ----> | Mock CCP Server  | ----> | Scenario Engine  |
| (Rust/Py)   | <---- | (TLS, FIX 4.1)   | <---- | (replay, inject) |
+-------------+       +------------------+       +------------------+
                      | Mock Farm Server |
                      | (binary ticks)   |
                      +------------------+
```

The mock gateway would:
- Accept TLS connections and perform SRP-6 handshake
- Respond to FIX logon with configurable CCP response (account, feature flags)
- Replay captured tick data for market data subscriptions
- Accept 35=D orders and generate configurable execution reports
- Support scenario injection: delays, rejects, partial fills, disconnect mid-order

### 6.3 Paper Trading Scenarios (15+)

| # | Scenario | Tests |
|---|----------|-------|
| 1 | Submit 1 limit order, verify fill | Order lifecycle: PendingSubmit -> Submitted -> Filled |
| 2 | Submit market order, verify immediate fill | Market order fill speed |
| 3 | Submit limit order, cancel before fill | Cancel lifecycle: Submitted -> CancelSubmitted -> Cancelled |
| 4 | Submit limit order, modify price | Modify lifecycle: Submitted -> PendingModifyAck -> Submitted |
| 5 | Submit bracket order (entry + TP + SL) | OCA group, parent-child linking |
| 6 | Fill parent, verify TP+SL activation | Child order activation on parent fill |
| 7 | Subscribe 50 symbols, verify all receive ticks | Bulk subscription stability |
| 8 | Subscribe L2 depth for 5 symbols | Depth book accuracy |
| 9 | Request historical data (1 year daily bars) | HMDS data integrity |
| 10 | Submit trailing stop, verify trail updates | Trailing stop price adjustment |
| 11 | Submit conditional order (price condition) | Condition evaluation |
| 12 | Submit VWAP algo order | Algo parameter passing |
| 13 | Disconnect CCP, reconnect, verify order recovery | Reconnect + mass status |
| 14 | Disconnect farm, reconnect, verify data recovery | Farm reconnect + re-subscribe |
| 15 | Run 8 hours overnight, verify stability | Soak test |
| 16 | Submit 100 orders in burst | Burst capacity |
| 17 | Request contract details for SPY options chain | Options contract discovery |

### 6.4 Coverage Targets

| Metric | Current | Phase 1 Target | Phase 4 Target |
|--------|---------|---------------|----------------|
| Rust unit tests | 637 | 800 | 1,200 |
| Python integration tests | 16 files | 25 files | 35 files |
| Code coverage (tarpaulin) | Not measured | 60% | 80% |
| Paper trading scenarios passed | Ad-hoc | 10/17 | 17/17 |
| Chaos test scenarios | 0 | 0 | 10 |
| Soak test hours | 0 | 8 | 72 |

### 6.5 Chaos Test Scenarios (Phase 4)

| # | Scenario | Injection Method | Expected Behavior |
|---|----------|-----------------|-------------------|
| 1 | Kill CCP mid-order-submit | TCP RST during send_fix() | Order retried after reconnect |
| 2 | Kill farm during tick burst | Drop farm TCP socket | Farm reconnects, subscriptions replayed |
| 3 | Corrupt FIX checksum | Flip random byte in inbound message | Message rejected, no crash |
| 4 | Corrupt HMAC signature | Alter signed data | Message rejected, connection continues |
| 5 | Inject 500ms latency on CCP | tc qdisc netem | Orders delayed but succeed, no timeout |
| 6 | Inject 50% packet loss on farm | tc qdisc netem | Some ticks lost, no crash, reconnect on timeout |
| 7 | Fill then disconnect before ack | Send exec report, then RST | Fill recorded, order state reconciled on reconnect |
| 8 | Simultaneous CCP + farm disconnect | Kill both connections | Both reconnect independently, state recovered |
| 9 | Out-of-order FIX messages | Reorder seq numbers | Messages processed correctly (no strict ordering) |
| 10 | Memory pressure (OOM) | cgroups memory limit | Graceful degradation, no data corruption |

### 6.6 Soak Test Metrics

During the 72-hour soak test (Phase 4.4), the following metrics must be monitored:

| Metric | Acceptable Range | Alert Threshold |
|--------|-----------------|-----------------|
| RSS memory | Stable +/- 10% | Growing > 20% over 24h |
| File descriptors | < 50 | > 100 |
| Heartbeat success rate | > 99% | < 95% |
| Tick processing latency (p99) | < 10us | > 100us |
| Order round-trip (submit to ack) | < 100ms | > 500ms |
| Reconnect count per 24h | 0-2 (IB maintenance) | > 5 |
| Duplicate fill count | 0 | > 0 |
| Panic count | 0 | > 0 |
| CPU usage (hot loop core) | 100% (busy-poll) | N/A |
| CPU usage (other cores) | < 5% | > 20% |

### 6.7 Regression Test Strategy

Before each release:

```
Step 1: cargo fmt --all -- --check
Step 2: cargo clippy --workspace -- -D warnings
Step 3: cargo test --workspace
Step 4: cd tests/python && python -m pytest -v
Step 5: cargo bench (if benchmark regression CI exists)
Step 6: Manual paper trading smoke test (5 minutes):
        - Subscribe 5 symbols
        - Submit 1 limit order
        - Modify price
        - Cancel order
        - Verify fills/updates via Wrapper
```

---

## 7. Decision Log

Unresolved questions for the maintainer:

| # | Question | Context | Options | Recommendation |
|---|----------|---------|---------|----------------|
| D1 | Should IBX parse feature flags from CCP logon? | The Gateway supports 200+ flags. IBX ignores them all. Some may restrict order types. | (a) Parse all (b) Parse safety-critical subset (c) Ignore | (b) Parse subset: order type restrictions, trading permissions, margin requirements |
| D2 | Should IBX support read-only connection mode? | The Gateway supports TWSRO token for monitoring. | (a) Implement (b) Defer (c) Never | (a) Implement in Phase 3 -- useful for monitoring dashboards |
| D3 | Should order state checkpointing use file or database? | Phase 4.2 needs persistent order state. | (a) JSON file (b) SQLite (c) mmap'd struct | (a) JSON file -- simple, debuggable, no deps |
| D4 | Should CCP heartbeat match Gateway's 5s or stay at 10s? | IBX uses 10s, the Gateway uses 5s. Longer interval risks server timeout. | (a) Match Gateway at 5s (b) Keep 10s (c) Configurable | (c) Configurable with 5s default |
| D5 | Should IBX support Financial Advisor (FA) accounts? | FA requires allocation profiles, groups, model portfolios. | (a) Full support (b) Basic (c) Defer | (b) Basic in Phase 4 -- reqManagedAccts works, add allocation |
| D6 | Should the mock gateway be part of the IBX repo or separate? | Testing infrastructure. | (a) In-repo test fixture (b) Separate crate (c) External | (a) In-repo -- keeps test infra close to code |
| D7 | What is the MAX_INSTRUMENTS ceiling? | Currently 256. Some strategies need 1000+. | (a) 256 (b) 1024 (c) Dynamic | (b) 1024 -- pre-allocated arrays grow but stay cache-friendly |
| D8 | Should IBX validate FIX messages before processing? | Currently no validation. Corrupt messages could cause panics. | (a) Full validation (b) Checksum only (c) None | (a) Full validation -- Phase 1.4 |

---

## 8. Contribution Guide

### 8.1 How to Claim Tasks

1. Check the [Phase Summary](#phase-summary) for tasks labeled **Good First Issue** (10 total across all phases)
2. Open an issue on `deepentropy/ibx` referencing the task ID (e.g., "Implement P1.1: Complete order state machine")
3. Fork to your staging repo, create a branch named `feat/<task-id>-description`
4. Implement against the acceptance criteria listed in the task table
5. Run the pre-commit checklist (Section 8.2) before opening a PR

### 8.2 PR Standards

- **Title format:** `feat: <short description> (#issue)` or `fix: <short description> (#issue)`
- **Body:** Reference the task ID, list acceptance criteria met, include test evidence
- **Tests required:** Every PR must include tests that cover the new code
- **No formatting violations:** Run `cargo fmt --all -- --check` before pushing
- **No clippy warnings:** Run `cargo clippy --workspace -- -D warnings` before pushing
- **No test failures:** Run `cargo test --workspace` before pushing
- **Full CI checklist:**
  ```sh
  cargo fmt --all -- --check
  cargo test --workspace
  cargo clippy --workspace -- -D warnings
  ```

### 8.3 Good First Issues

| Task ID | Description | Difficulty | Phase |
|---------|-------------|------------|-------|
| P0.3 | Farm disconnect/reconnect per-farm | Easy | 0 |
| P0.5 | HMDS reconnect + historical data recovery | Easy | 0 |
| P1.1 | Complete order state machine (5 new states) | Medium | 1 |
| P1.4 | FIX message validation | Easy | 1 |
| P1.6 | OCA type differentiation | Easy | 1 |
| P1.7 | Open/Close position indicator | Easy | 1 |
| P2.3 | Forex support (CASH security type) | Medium | 2 |
| P2.6 | Security ID sources (ISIN/CUSIP/SEDOL) | Easy | 2 |
| P3.6 | Network connectivity pre-check | Easy | 3 |
| P3.7 | Configurable heartbeat intervals | Easy | 3 |

---

## 9. Appendices

### Appendix A: FIX Tag Coverage

**Verification command:**
```sh
grep -rhoP 'TAG_\w+' src/protocol/fix.rs | sort -u | wc -l  # => 21 defined
grep -rhoP '\b[4-9]\d{3}\b' src/ | sort -u | wc -l           # => ~134 unique IB custom tags used
```

**Defined FIX tags in `protocol/fix.rs`:**

| Tag | Name | Defined | Used |
|-----|------|---------|------|
| 8 | BeginString | Yes | Yes |
| 9 | BodyLength | Yes | Yes |
| 10 | Checksum | Yes | Yes |
| 34 | MsgSeqNum | Yes | Yes |
| 35 | MsgType | Yes | Yes |
| 49 | SenderCompID | Yes | Yes |
| 52 | SendingTime | Yes | Yes |
| 56 | TargetCompID | Yes | Yes |
| 58 | Text | Yes | Yes |
| 61 | Urgency | Yes | Yes |
| 98 | EncryptMethod | Yes | Yes |
| 108 | HeartBtInt | Yes | Yes |
| 112 | TestReqID | Yes | Yes |
| 141 | ResetSeqNumFlag | Yes | Yes |
| 148 | Headline | Yes | Yes |
| 207 | SecurityExchange | Yes | Yes |
| 6034 | IB Build | Yes | Yes |
| 6040 | IB CommType | Yes | Yes |
| 6143 | IB CompVersion | Yes | Yes |
| 6968 | IB Version | Yes | Yes |
| 8349 | HMAC Signature | Yes | Yes |

**Commonly used IB custom tags in `order_builder.rs` (sample):**

| Tag | Purpose | Used |
|-----|---------|------|
| 11 | ClOrdID | Yes |
| 1 | Account | Yes |
| 21 | HandlInst | Yes |
| 38 | OrderQty | Yes |
| 40 | OrdType | Yes |
| 44 | Price | Yes |
| 54 | Side | Yes |
| 55 | Symbol | Yes |
| 59 | TimeInForce | Yes |
| 60 | TransactTime | Yes |
| 99 | StopPx | Yes |
| 100 | ExDestination | Yes |
| 110 | MinQty | Yes |
| 111 | MaxFloor (DisplaySize) | Yes |
| 126 | ExpireTime (GTD) | Yes |
| 167 | SecurityType | Yes (hardcoded STK) |
| 168 | EffectiveTime (GAT) | Yes |
| 204 | CustomerOrFirm | Yes |
| 583 | OCA Group | Yes |
| 847 | AlgoStrategy | Yes |
| 6102 | SweepToFill | Yes |
| 6107 | ParentId | Yes |
| 6135 | Hidden | Yes |
| 6261 | AdjustedOrderType | Yes |
| 6268 | TrailPercent | Yes |
| 6433 | OutsideRTH | Yes |
| 9813 | DiscretionaryAmt | Yes |

### Appendix B: Order Type Coverage

**Verification command:**
```sh
grep -c 'Submit\|Modify\|Cancel' src/types.rs  # => 122 lines (37 submit + lifecycle)
```

| Order Type | FIX OrdType (tag 40) | IBX Status |
|-----------|---------------------|------------|
| Market | 1 | Complete |
| Limit | 2 | Complete |
| Stop | 3 | Complete |
| Stop Limit | 4 | Complete |
| Market on Close (MOC) | 5 | Complete |
| Limit on Close (LOC) | 2 + TIF=7 | Complete |
| Market if Touched (MIT) | J | Complete |
| Limit if Touched (LIT) | K | Complete |
| Market to Limit (MTL) | K | Complete |
| Market with Protection | U | Complete |
| Stop with Protection | SP | Complete |
| Trailing Stop | P | Complete |
| Trailing Stop Limit | P + price | Complete |
| Trailing Stop Pct | P + tag 6268 | Complete |
| Mid-Price | MIDPX | Complete |
| Snap to Market | SMKT | Complete |
| Snap to Midpoint | SMID | Complete |
| Snap to Primary | SREL | Complete |
| Pegged to Market | E + ExecInst P | Complete |
| Pegged to Midpoint | E + ExecInst M | Complete |
| Pegged to Benchmark | PB | Complete |
| Relative (Pegged to Primary) | R | Complete |
| Limit OPG (Opening) | 2 + TIF=2 | Complete |
| Limit Auction | 2 + TIF=A | Complete |
| MTL Auction | K + TIF=A | Complete |
| Adaptive (IB algo overlay) | 2 + algo=Adaptive | Complete |
| Bracket (parent + TP + SL) | 3x 35=D + OCA | Complete |
| What-If (margin preview) | any + tag 6091=1 | Complete |
| Fractional Shares | 2 + decimal qty | Complete |
| Adjustable Stop | 3 + tag 6261 | Complete |
| Limit GTC | 2 + TIF=1 | Complete |
| Stop GTC | 3 + TIF=1 | Complete |
| Stop Limit GTC | 4 + TIF=1 | Complete |
| Limit IOC | 2 + TIF=3 | Complete |
| Limit FOK | 2 + TIF=4 | Complete |
| VWAP algo | tag 847=Vwap | Complete |
| TWAP algo | tag 847=Twap | Complete |
| Arrival Price algo | tag 847=ArrivalPx | Complete |
| Close Price algo | tag 847=ClosePx | Complete |
| Dark Ice algo | tag 847=DarkIce | Complete |
| PctVol algo | tag 847=PctVol | Complete |
| **Volatility orders** | -- | **Missing** |
| **Scale/iceberg (auto-replenish)** | -- | **Missing** |
| **Cross orders** | -- | **Missing** |
| **Combo/spread orders** | -- | **Missing** |
| **Accumulate/Distribute algo** | -- | **Missing** |
| **Balance Impact Risk algo** | -- | **Missing** |
| **MinImpact algo** | -- | **Missing** |

### Appendix C: FIX Message Type Coverage

**Verification command:**
```sh
grep -c 'pub const MSG_' src/protocol/fix.rs  # => 13
```

| MsgType (tag 35) | Name | IBX Send | IBX Receive | Notes |
|-------------------|------|----------|-------------|-------|
| 0 | Heartbeat | Yes | Yes | CCP + Farm + HMDS |
| 1 | TestRequest | Yes | Yes | CCP + Farm + HMDS |
| 3 | Reject | No | Yes | CCP |
| 5 | Logout | No | Yes | CCP |
| 8 | ExecutionReport | No | Yes | CCP (orders, fills, positions) |
| 9 | OrderCancelReject | No | Yes | CCP |
| A | Logon | Yes | Yes | CCP + Farm + HMDS |
| B | News | No | Yes | CCP (bulletins, articles) |
| D | NewOrderSingle | Yes | No | CCP |
| E | TBT Data | No | Yes | HMDS (tick-by-tick) |
| F | OrderCancelRequest | Yes | No | CCP |
| G | OrderCancelReplaceRequest / HMDS bar data | Yes (CCP) | Yes (Farm, HMDS) | Dual use |
| L | Ticker Setup | No | Yes | Farm |
| P | Binary Tick | No | Yes | Farm (8=O binary protocol) |
| U | Custom IB message | Yes | Yes | CCP (account, contract details) |
| V | MarketDataRequest | Yes | No | Farm |
| W | MarketDataSnapshot / HMDS response | No | Yes | HMDS (bars, head timestamp, histogram) |
| Y | Market Depth / QuoteRequest | No | Yes | Farm (NASDAQ TotalView) |
| RL | Market Rule | No | Yes | Farm |
| UM | User Message | No | Yes | Farm |
| UP | User Property | No | Yes | Farm |
| UT | User Tick | No | Yes | Farm |

### Appendix D: Security Type Coverage

**Verification command:**
```sh
grep -c 'STK\|OPT\|FUT\|CASH\|IND\|WAR\|BOND\|FUND' src/types.rs
```

| Security Type (tag 167) | Name | IBX Status |
|-------------------------|------|------------|
| STK | Common Stock | Complete (hardcoded default) |
| OPT | Option | Missing (Phase 2.1) |
| FUT | Future | Missing (Phase 2.2) |
| FOP | Future Option | Missing (Phase 2.2) |
| CASH | Forex | Missing (Phase 2.3) |
| IND | Index | Partial (market data only) |
| WAR | Warrant | Missing (Phase 2.5) |
| BOND | Bond | Missing |
| FUND | Fund | Missing |
| CFD | Contract for Difference | Missing |
| CRYPTO | Cryptocurrency | Missing |
| CMDTY | Commodity | Missing |
| NEWS | News | N/A |
| BAG | Combo | Missing (Phase 2.4) |
| IOPT | Structured Product | Missing (Phase 2.5) |

### Appendix E: Order State Machine Coverage

**Verification command:**
```sh
grep -A1 'pub enum OrderStatus' src/types.rs
```

| Gateway State | FIX 39 Value | IBX Status | Notes |
|--------------|-------------|------------|-------|
| Unknown | -- | Complete | Initial state |
| Inactive | -- | Missing | Pre-submitted, waiting for conditions |
| Submitted | 0 (New) | Complete | Order accepted by exchange |
| Acked | A (PendingNew) | Missing | Acknowledged by IB, not yet on exchange |
| Pending | -- | Complete (as PendingSubmit) | Locally queued |
| Confirmed | -- | Missing | Server confirmed receipt |
| Filled | 2 (Filled) | Complete | |
| PartiallyFilled | 1 (PartiallyFilled) | Complete | |
| CancelSubmitted | 6 (PendingCancel) | Missing | Cancel request sent |
| CancelAcked | -- | Missing | Cancel acknowledged |
| CancelConfirmed | 4 (Cancelled) | Complete (as Cancelled) | But intermediate states lost |
| PendingModifyAck | E (PendingReplace) | Missing | Modify request acknowledged |
| WarnState | -- | Missing | Warning state (margin, pattern day trader) |

### Appendix F: Auth Token Type Coverage

**Verification command:**
```sh
grep -i 'token\|SOFT\|PERMANENT\|TWSRO\|TST' src/auth/session.rs | head -20
```

| Token Type | Value | IBX Status | Notes |
|-----------|-------|------------|-------|
| SOFT (ST) | 0 / flag=2 | Complete | Soft token challenge via XYZ |
| PERMANENT (PST) | 1 / flag=4 | Missing | Persistent session cookie |
| PWD | MAX_VALUE | Complete | Password-only auth (paper accounts) |
| TWSRO (RST) | 3 / flag=16 | Missing | Read-only token |
| TST | 7 / flag=256 | Missing | Unknown purpose |
| IB Key (via DSA 775) | -- | Complete | IB Key 2FA challenge-response |

### Appendix G: NS Protocol Coverage

**Verification command:**
```sh
grep -c 'pub const NS_' src/protocol/ns.rs  # => 17
```

| NS Type | Value | IBX Defined | IBX Handled |
|---------|-------|-------------|-------------|
| NS_ERROR_RESPONSE | 519 | Yes | Yes |
| NS_AUTH_START | 520 | Yes | Yes |
| NS_CONNECT_REQUEST | 521 | Yes | Yes (sent) |
| NS_DEMO_REQUEST | 522 | No | No (non-goal) |
| NS_CONNECT_RESPONSE | 523 | Yes | Yes |
| NS_REDIRECT | 524 | Yes | Yes |
| NS_FIX_START | 525 | Yes | Yes |
| NS_NEWCOMMPORTTYPE | 526 | Yes | Yes |
| NS_BACKUP_HOST | 527 | Yes | Partial (received, not used for failover) |
| NS_MISC_URLS_REQUEST | 528 | Yes | Yes |
| NS_MISC_URLS_RESPONSE | 529 | Yes | Yes |
| NS_TEST_REQUEST | 530 | Yes | Yes |
| NS_HEART_BEAT | 531 | Yes | Yes |
| NS_SECURE_CONNECT | 532 | Yes | Yes |
| NS_SECURE_CONNECTION_START | 533 | Yes | Yes |
| NS_SECURE_MESSAGE | 534 | Yes | Yes |
| NS_SECURE_ERROR | 535 | Yes | Yes |

### Appendix H: Disconnect Reason Coverage

**Verification command:**
```sh
grep 'DisconnectReason\|disconnect' src/engine/hot_loop/*.rs | wc -l
```

| Gateway Disconnect Reason | IBX Handling |
|--------------------------|--------------|
| DISCONNECT_NO_DISCONNECT_STATUS | Not differentiated |
| DISCONNECT_ON_BROKEN_SOCKET | Detected (io::Error), not classified |
| DISCONNECT_ON_CONNECTION_FAILURE | Detected (connect error), not classified |
| DISCONNECT_ON_INACTIVITY | Detected (heartbeat timeout), not classified |
| DISCONNECT_ON_EXIT_SESSION | Not handled |
| DISCONNECT_NATIVE_CONNECTION_RESTORED | Not handled |
| DISCONNECT_AUTHORIZATION_FAILED | Detected (auth error), not classified |
| DISCONNECT_ON_REDIRECTION | Detected (NS_REDIRECT), not classified |
| DISCONNECT_BY_DESIGN | Not handled |
| DISCONNECT_ON_WORKSPACE | Not applicable (no GUI) |
| DISCONNECT_ON_RO_TO_RW | Not handled |
| NO_AUTH_RESPONSE | Detected (timeout), not classified |
| NO_PING_RESPONSE | Detected (heartbeat timeout), not classified |
| DISCONNECT_ON_USER_COMPETITION | Not handled |
| CONNECTING | Not tracked as disconnect |
| DISCONNECT_ON_SETTINGS | Not applicable |
| DISCONNECT_ON_SITE_DOWN | Not differentiated |
| DISCONNECT_ON_SITE_NOT_READY | Not differentiated |
| NEED_SSL | Not handled (always uses TLS) |
| DISCONNECT_CCP_CONNECTION_REQUIRED | Not handled |
| FIX_VALIDATION_FAILURE | Not handled (no FIX validation) |
| SSL_CERTIFICATE_NOT_ACCEPTED | Not handled (system trust store) |

### Appendix I: Connection State Coverage

| Gateway LogonState | IBX Equivalent |
|-------------------|----------------|
| PRELOGON | Implicit (before TLS connect) |
| LOGON | Implicit (during NS handshake) |
| FIX_SESSION_REQUEST | Implicit (after NS_FIX_START) |
| FIX_SESSION_OPEN | Implicit (after FIX logon response) |
| LOGGEDON | Implicit (hot loop running) |
| ACCT_NOT_SUPPORTED | Not handled |

### Appendix J: Wrapper Callback Coverage

**Verification command:**
```sh
awk '/pub trait Wrapper/,/^}/' src/api/wrapper.rs | grep -c 'fn '  # => 63
```

All 63 Wrapper trait methods have default no-op implementations. Key categories:

| Category | Methods | Implementation Status |
|----------|---------|---------------------|
| Connection (connect_ack, connection_closed, etc.) | 6 | Complete |
| Market Data (tick_price, tick_size, etc.) | 6 | Complete |
| Orders (order_status, open_order, exec_details, etc.) | 6 | Complete |
| Account (update_account_value, position, pnl, etc.) | 12 | Complete |
| Historical Data (historical_data, head_timestamp, etc.) | 4 | Complete |
| Contract Details | 3 | Complete |
| Tick-by-Tick | 3 | Complete |
| Scanner | 3 | Complete |
| News | 5 | Complete |
| Real-Time Bars | 1 | Complete |
| Historical Ticks | 1 | Complete |
| Histogram | 1 | Complete |
| Market Rules | 1 | Complete |
| Completed Orders | 2 | Complete |
| Historical Schedule | 1 | Complete |
| Fundamental Data | 1 | Complete |
| Market Depth (L2) | 3 | Complete |
| Smart Components | 1 | Complete |
| News Providers | 1 | Complete |
| Soft Dollar Tiers | 1 | Complete |
| Family Codes | 1 | Complete |
| User Info | 1 | Complete |

### Appendix K: ControlCommand Coverage

**Verification command:**
```sh
awk '/pub enum ControlCommand/,/^}/' src/types.rs | grep -cE '^\s+[A-Z]'  # => 36
```

Key ControlCommand variants (36 total):

| Category | Commands | Count |
|----------|----------|-------|
| Market Data | Subscribe, Unsubscribe, SubscribeTbt, UnsubscribeTbt, SubscribeNews, UnsubscribeNews, SubscribeDepth, UnsubscribeDepth | 8 |
| Orders | Order(OrderRequest) -- 37 submit + Cancel + CancelAll + Modify | 3 wrappers |
| Historical | FetchHistorical, CancelHistorical, FetchHeadTimestamp, CancelHeadTimestamp, FetchHistoricalTicks, FetchHistoricalSchedule | 6 |
| Reference | FetchContractDetails, FetchMatchingSymbols, FetchMktDepthExchanges, FetchScannerParams, SubscribeScanner, CancelScanner | 6 |
| News | FetchHistoricalNews, FetchNewsArticle | 2 |
| Fundamental | FetchFundamentalData, CancelFundamentalData | 2 |
| Histogram | FetchHistogramData, CancelHistogramData | 2 |
| Real-Time Bars | SubscribeRealTimeBar, CancelRealTimeBar | 2 |
| Instrument | RegisterInstrument | 1 |
| Config | UpdateParam | 1 |
| Lifecycle | Shutdown | 1 |

### Appendix Z: Glossary

| Term | Definition |
|------|-----------|
| **CCP** | Central Connection Point -- IB's auth server (cdc1/ndc1.ibllc.com:4001). Handles login, orders, account data. |
| **Farm** | Market data server (e.g., usfarm). Delivers real-time ticks, depth, and market data. |
| **HMDS** | Historical Market Data Server. Delivers historical bars, tick-by-tick data, scanner results. |
| **FIX 4.1** | Financial Information eXchange protocol version 4.1. IB's variant uses custom tags (4000+, 6000+, 8000+). |
| **NS** | IB's proprietary framing protocol. `#%#%` magic + 4-byte length + semicolon-delimited payload. |
| **XYZ** | IB's binary protocol for SRP auth. `#%#%` magic + length-prefixed binary fields. |
| **FIXCOMP** | Compressed FIX message container. Zlib-compressed inner messages in `8=FIXCOMP` framing. |
| **SRP-6** | Secure Remote Password protocol v6. Zero-knowledge password proof without sending password over wire. |
| **DH** | Diffie-Hellman key exchange. Establishes shared secret for AES-128-CBC encrypted channel. |
| **SeqLock** | Sequence lock. Lock-free data structure for single-writer multiple-reader. Used for hot-path quote storage. |
| **Hot Loop** | The pinned-core busy-poll engine thread. Non-blocking, no locks, drains all connections in a tight loop. |
| **OCA** | One-Cancels-All. Order group where filling one cancels the others (used in bracket TP/SL). |
| **TBT** | Tick-by-Tick. Individual trade/quote events (vs aggregated ticks). |
| **RTBAR** | Real-Time Bar. 5-second OHLCV bars from HMDS. |
| **conId** | IB's unique contract identifier. Integer assigned by IB to each instrument. |
| **InstrumentId** | IBX's internal dense identifier. Maps from conId at subscription time for array indexing. |
| **SMART** | IB's smart order routing. Automatically routes to best exchange. |
| **FA** | Financial Advisor. Multi-account management with allocation profiles. |
| **PST** | Permanent Session Token. Survives restarts, avoids full re-authentication. |
| **SWTK** | Soft Token. Time-based one-time password for 2FA. |
| **IB Key** | IB's mobile 2FA app. Uses DSA challenge-response (notification code 775). |

---

---

## 10. Expanded Task Specifications

This section provides implementation-level detail for every task across all phases. Each specification includes the exact files to modify, the function signatures to add or change, the FIX tags involved, and step-by-step implementation guidance.

### 10.1 Phase 0 Expanded: Paper Trading Robustness

#### P0.1 Expanded: Concurrent Order Stress Test

**Implementation as a new binary: `src/bin/stress_orders.rs`**

The test binary should:
1. Connect to IB paper trading via `gateway::connect()`
2. Subscribe to 10 instruments: SPY, AAPL, MSFT, AMZN, GOOG, TSLA, META, NVDA, AMD, QQQ
3. Wait for first tick on all 10 (confirms market data flowing)
4. Submit 100 limit orders in rapid succession:
   - 10 orders per symbol
   - Alternating buy/sell
   - Limit price = last price +/- 0.5% (to avoid immediate fills for pure stress testing)
   - Qty = random 1-100
5. Collect all 100 execution reports (OrderUpdate events)
6. Assert: all 100 orders received an Acked/Submitted status
7. Cancel all 100 orders
8. Assert: all 100 cancellation confirmations received
9. Print timing: total submit time, total ack time, total cancel time

**Key assertion: no `OrderStatus::Uncertain`, no panics, no duplicate order IDs.**

**Metrics to track:**
- Submit throughput: orders/second
- Ack latency (p50, p95, p99): time from send_fix() to OrderUpdate
- Cancel latency: time from cancel request to Cancelled status
- Memory delta: RSS before and after the 100-order cycle

#### P0.2 Expanded: CCP Reconnect Under Load

**Implementation approach:**

This test must be manual or require a network control mechanism. Two approaches:

**(a) iptables-based:** Add a firewall rule to drop traffic to cdc1.ibllc.com:4001 for 30 seconds, then remove it. The hot loop should detect the CCP heartbeat timeout (10s + 1s grace), trigger `reconnect_ccp()`, and re-establish the session.

```sh
# Drop CCP traffic
sudo iptables -A OUTPUT -d cdc1.ibllc.com -j DROP
sleep 30
sudo iptables -D OUTPUT -d cdc1.ibllc.com -j DROP
```

**(b) Code-based:** Add a `ControlCommand::SimulateDisconnect(ConnectionType)` for testing. This would close the CCP socket, triggering the reconnect path without network manipulation.

**Expected behavior sequence:**
1. Hot loop detects CCP heartbeat timeout (11 seconds after last recv)
2. `ccp.disconnected` set to `true`
3. All orders transition to `OrderStatus::Uncertain`
4. `Event::Disconnected` emitted
5. Background reconnect thread spawned (`reconnect_ccp()`)
6. New CCP connection established (SRP re-auth not needed -- session token reuse)
7. Mass status request (35=H) sent on new CCP connection
8. All open orders reconciled: Uncertain -> Submitted, Filled, Cancelled
9. Market data on farm connections should be UNINTERRUPTED during CCP disconnect

**Rust files involved:**
- `engine/hot_loop/mod.rs` lines 66-70: reconnect state machine
- `gateway.rs`: `reconnect_ccp()` function
- `engine/hot_loop/ccp.rs`: `handle_disconnect()` and mass status on reconnect
- `bridge.rs`: `OrderState` for order status updates

#### P0.3 Expanded: Farm Disconnect/Reconnect Per-Farm (Good First Issue)

**Context:** IBX currently supports multiple farm connections (`farm_conn`, `cashfarm_conn`, `eufarm_conn`, `jfarm_conn`, `usfuture_conn`). When one farm disconnects, only that farm's subscriptions should be affected. Other farms must continue operating.

**Implementation steps:**
1. In `engine/hot_loop/farm.rs`, the `handle_disconnect()` method already sets `self.disconnected = true`. Verify this only affects the specific farm, not all farms.
2. Add per-farm reconnect tracking in `HotLoop`:
   - `pending_cashfarm_reconnect: Option<Receiver<io::Result<Connection>>>`
   - Same for eufarm, jfarm, usfuture
3. In the main hot loop iteration, check each secondary farm for pending reconnect results
4. On successful reconnect, replay that farm's market data subscriptions

**Test procedure:**
1. Subscribe to a US stock (via usfarm) and a forex pair (via cashfarm)
2. Disconnect cashfarm (via iptables or code injection)
3. Verify: US stock ticks continue uninterrupted
4. Verify: Forex pair ticks stop (cashfarm disconnected)
5. Reconnect cashfarm
6. Verify: Forex pair ticks resume

**This is a Good First Issue because:**
- The per-farm architecture already exists in `HotLoop`
- The primary farm reconnect logic already works and can be replicated
- The change is localized to `engine/hot_loop/mod.rs` and `farm.rs`
- No protocol changes needed

#### P0.4 Expanded: Order Modify/Cancel Race Conditions

**Race conditions to test:**

| Race | Sequence | Expected Outcome |
|------|----------|-----------------|
| Cancel after fill | Submit -> Fill arrives -> Cancel sent before ack | CancelReject (too late, already filled) |
| Modify during cancel | Submit -> Cancel sent -> Modify sent | CancelReject or ModifyReject |
| Double cancel | Submit -> Cancel -> Cancel again | First cancels, second gets CancelReject |
| Modify unfilled | Submit -> Modify price -> New fill at new price | Fill at modified price |
| Cancel+Resubmit | Submit -> Cancel -> Immediately resubmit | Two separate orders, no confusion |
| Fill during modify | Submit -> Fill arrives -> Modify sent | Modify rejected (already filled) or partial modify |

**Implementation as `src/bin/stress_modify.rs`:**
1. Submit a limit order far from market (will not fill)
2. Immediately send a modify (change price)
3. Verify: PendingModifyAck -> Submitted at new price
4. Send a cancel
5. Verify: CancelSubmitted -> Cancelled
6. Repeat 50 times, assert zero panics and deterministic final states

**Key Rust files:**
- `engine/hot_loop/order_builder.rs`: Modify variant (35=G)
- `engine/hot_loop/ccp.rs`: ExecType handling for modify acks
- `types.rs`: OrderStatus transitions

#### P0.5 Expanded: HMDS Reconnect + Historical Data Recovery (Good First Issue)

**Current behavior:** When the HMDS connection drops, pending historical requests are lost. The caller never receives a response or error.

**Fix:**
1. In `engine/hot_loop/hmds.rs`, `handle_disconnect()` should mark `self.disconnected = true`
2. Store all pending request metadata (request type, parameters, req_id) in a `retry_queue`
3. On successful HMDS reconnect, drain the retry queue and resend all pending requests
4. If a request was already partially received (e.g., historical bars streaming), discard the partial data and start fresh

**Pending request types to retry:**
- `pending_historical`: Historical bar requests
- `pending_head_ts`: Head timestamp requests
- `pending_scanner`: Scanner subscription requests
- `pending_news`: Historical news requests
- `pending_articles`: News article requests
- `pending_fundamental`: Fundamental data requests
- `pending_histogram`: Histogram data requests
- `pending_schedule`: Trading schedule requests
- `pending_ticks`: Historical tick requests

**This is a Good First Issue because:**
- The HMDS state machine in `hmds.rs` already tracks all pending requests
- The disconnect detection already works
- The fix is: on reconnect, re-send each pending request
- Localized to `engine/hot_loop/hmds.rs`

#### P0.6 Expanded: L2 Depth Stress Test

**Test procedure:**
1. Subscribe to L2 depth for 20 symbols (10 SMART + 10 individual exchanges)
2. Enable SmartDepth for 10 of them
3. Run for 30 minutes during market hours
4. Verify: all 20 depth books update continuously
5. Cancel and resubscribe 5 depth streams mid-test
6. Verify: no stale data in the re-subscribed books

**Key metrics:**
- Depth update rate per symbol
- SmartDepth fan-out correctness (multiple exchanges consolidated)
- Memory stability (depth book sizes should plateau)

#### P0.7 Expanded: Bracket Order Stress Test

**20 bracket orders, each with:**
- Parent: Limit Buy at (last_price - $0.10) -- slightly below market to control fill timing
- Take-Profit: Limit Sell at (entry + 2%)
- Stop-Loss: Stop at (entry - 1%)

**OCA group naming:** `BRACKET_{order_id}` linking TP and SL

**Test sequence:**
1. Submit all 20 brackets (60 FIX messages total: 20 parents + 20 TPs + 20 SLs)
2. For 10 brackets: lower parent price to market to trigger fill
3. On fill: verify TP and SL both transition from PreSubmitted -> Submitted
4. For 5 of the 10 filled brackets: the TP should be at a fillable price. When TP fills, verify SL auto-cancels via OCA.
5. For the other 5: manually cancel TP. Verify SL remains active.
6. Cancel remaining 10 unfilled parent orders. Verify: TP and SL also cancel.

The Gateway defines three OCA types:
- Type 1: Cancel with block (fills cancel others, block new orders)
- Type 2: Reduce with block (fills reduce others, block new orders)
- Type 3: Reduce without block (fills reduce others, allow new orders)

IBX currently tracks OCA group ID but does not differentiate types. This test will reveal if the default behavior is sufficient or if type differentiation (P1.6) is needed first.

#### P0.8 Expanded: Overnight Session Stability

**8-hour soak test during non-market hours:**
1. Connect and subscribe to 10 instruments at 6 PM ET
2. Market closes at 8 PM ET. Verify: ticks stop gracefully, no errors
3. IB maintenance window typically 11:30 PM - 12:15 AM ET. Verify: reconnect succeeds after maintenance
4. Pre-market opens 4 AM ET. Verify: ticks resume
5. Regular market opens 9:30 AM ET. Verify: full tick flow

**Metrics to monitor every 5 minutes:**
- RSS memory (via /proc/self/status)
- Open file descriptors (via /proc/self/fd)
- Heartbeat success count (CCP + Farm + HMDS)
- Reconnect events
- Tick count (should be near-zero during non-market, non-zero during pre-market)

### 10.2 Phase 1 Expanded: Protocol Completion

#### P1.1 Deep Dive: Order State Machine

**Current `OrderStatus` enum (7 states):**
```rust
pub enum OrderStatus {
    PendingSubmit,   // Locally queued
    Submitted,       // FIX 39=0 (New) or 39=A (PendingNew)
    Filled,          // FIX 39=2
    PartiallyFilled, // FIX 39=1
    Cancelled,       // FIX 39=4
    Rejected,        // FIX 39=8
    Uncertain,       // Disconnect-induced
}
```

**Target `OrderStatus` enum (12 states):**
```rust
pub enum OrderStatus {
    PendingSubmit,     // Locally queued, not sent
    Acked,             // FIX 39=A (PendingNew) -- server received, routing to exchange
    Submitted,         // FIX 39=0 (New) -- accepted by exchange
    PartiallyFilled,   // FIX 39=1
    Filled,            // FIX 39=2
    CancelSubmitted,   // FIX 39=6 (PendingCancel) -- cancel request in progress
    CancelAcked,       // IB-specific intermediate cancel state
    Cancelled,         // FIX 39=4 -- confirmed cancelled
    PendingModifyAck,  // FIX 39=E (PendingReplace) -- modify in progress
    Rejected,          // FIX 39=8
    Inactive,          // Pre-submitted, waiting for conditions
    Uncertain,         // Disconnect-induced
}
```

**Transition rules from the Gateway's order state machine:**
```
PendingSubmit -> Acked (server receipt)
Acked -> Submitted (exchange acceptance)
Acked -> Rejected (server reject)
Submitted -> PartiallyFilled (partial fill)
Submitted -> Filled (complete fill)
Submitted -> CancelSubmitted (cancel request sent)
Submitted -> PendingModifyAck (modify request sent)
PartiallyFilled -> Filled (remaining fills)
PartiallyFilled -> CancelSubmitted (cancel remaining)
CancelSubmitted -> CancelAcked (IB intermediate)
CancelSubmitted -> Cancelled (direct cancel)
CancelAcked -> Cancelled (exchange confirms)
PendingModifyAck -> Submitted (modify accepted, new price)
PendingModifyAck -> Rejected (modify rejected, revert to original)
Inactive -> Submitted (condition triggered)
Inactive -> Cancelled (cancelled before trigger)
```

**FIX tag 39 (OrdStatus) mapping update in `ccp.rs`:**
```rust
match ord_status {
    b'A' => OrderStatus::Acked,           // PendingNew
    b'0' => OrderStatus::Submitted,        // New
    b'1' => OrderStatus::PartiallyFilled,  // Partially Filled
    b'2' => OrderStatus::Filled,           // Filled
    b'4' => OrderStatus::Cancelled,        // Cancelled
    b'6' => OrderStatus::CancelSubmitted,  // PendingCancel
    b'8' => OrderStatus::Rejected,         // Rejected
    b'E' => OrderStatus::PendingModifyAck, // PendingReplace
    _    => OrderStatus::Submitted,        // Default fallback
}
```

#### P1.2 Deep Dive: User Feature Flags

The Gateway supports 200+ user feature flags. This is the largest single configuration structure in the protocol.

**Key features to parse (safety-critical subset):**

| Feature Flag | Purpose | Safety Impact |
|-------------|---------|---------------|
| `CRORDER` | Cross orders permitted | Order would be rejected if not permitted |
| `NONEGLAST` | Non-eligible last sale | Affects order routing |
| `BLOTTERCROSS` | Blotter cross permitted | Cross order capability |
| `FALOT` | FA lot allocation | FA order placement |
| `MARGIN` | Margin trading enabled | Margin orders would fail |
| `SHORTSEL` | Short selling permitted | Short sells would be rejected |
| `CRYPTO` | Crypto trading enabled | Crypto orders would fail |
| `OPTIONS` | Options trading enabled | Options orders would fail |
| `FUTURES` | Futures trading enabled | Futures orders would fail |
| `FOREX` | Forex trading enabled | Forex orders would fail |

**Implementation:**
1. In `gateway.rs`, after CCP FIX logon response is received, extract feature flag fields
2. Feature flags are sent as a semicolon-separated list in a custom IB tag (likely 6XXX range)
3. Define `FeatureFlags` in `types.rs`:
```rust
pub struct FeatureFlags {
    pub raw_flags: Vec<String>,
    pub cross_orders: bool,
    pub margin: bool,
    pub short_sell: bool,
    pub crypto: bool,
    pub options: bool,
    pub futures: bool,
    pub forex: bool,
    // ... additional flags
}
```
4. Store in `SharedState::reference` (new field)
5. Expose via `EClient::feature_flags() -> &FeatureFlags`

#### P1.9 Deep Dive: Disconnect Reason Classification

The Gateway defines 22 disconnect reasons:

```rust
pub enum DisconnectReason {
    /// No disconnect occurred.
    None,
    /// TCP socket broken (read/write returned error).
    BrokenSocket,
    /// Initial connection attempt failed (DNS, TCP connect, TLS handshake).
    ConnectionFailure,
    /// Heartbeat timeout exceeded (no response to TestRequest).
    Inactivity,
    /// User explicitly requested session end.
    ExitSession,
    /// Native connection restored after temporary failure.
    NativeConnectionRestored,
    /// Authentication failed (wrong password, expired token, 2FA failed).
    AuthorizationFailed,
    /// Server redirected to different host (NS_REDIRECT received).
    Redirection,
    /// Planned disconnect (server shutdown, maintenance).
    ByDesign,
    /// Read-only to read-write mode switch.
    ReadOnlyToReadWrite,
    /// No authentication response within timeout.
    NoAuthResponse,
    /// No heartbeat response within timeout.
    NoPingResponse,
    /// Another session logged in with same credentials.
    UserCompetition,
    /// Server-side settings change.
    Settings,
    /// Server site is down.
    SiteDown,
    /// Server site not ready.
    SiteNotReady,
    /// SSL required but not established.
    NeedSsl,
    /// CCP connection required (farm-only session not permitted).
    CcpConnectionRequired,
    /// FIX message validation failure.
    FixValidationFailure,
    /// SSL certificate not accepted.
    SslCertificateNotAccepted,
}
```

**Detection mapping in the hot loop:**

| Disconnect Source | Detection Method | Reason |
|------------------|-----------------|--------|
| `conn.try_recv()` returns `Err(io::Error)` with `ConnectionReset` | TCP RST | `BrokenSocket` |
| `conn.try_recv()` returns `Err(io::Error)` with `BrokenPipe` | TCP broken | `BrokenSocket` |
| `conn.try_recv()` returns `Ok(0)` and no buffered data after timeout | EOF | `BrokenSocket` |
| Heartbeat test request sent, no response within grace period | Timeout | `Inactivity` |
| FIX 35=5 (Logout) received with tag 58 containing "authorization" | Auth failure | `AuthorizationFailed` |
| FIX 35=5 (Logout) received with tag 58 containing "competing" | Competition | `UserCompetition` |
| NS_REDIRECT (524) received | Redirect | `Redirection` |
| NS_ERROR_RESPONSE (519) with "maintenance" | Server maintenance | `ByDesign` |

### 10.3 Phase 2 Expanded: Multi-Asset

#### P2.1 Deep Dive: Options Support

**FIX tags required for options orders:**

| Tag | Name | Example |
|-----|------|---------|
| 167 | SecurityType | "OPT" |
| 200 | MaturityMonthYear | "202606" |
| 201 | PutOrCall | "0" (Put) / "1" (Call) |
| 202 | StrikePrice | "450.00" |
| 207 | SecurityExchange | "CBOE" |
| 231 | ContractMultiplier | "100" |
| 77 | OpenClose | "O" (Open) / "C" (Close) |

**Greeks integration:**

IB sends greeks via FIX tag 6005 (model option computation) in market data ticks. The tick decoder in `protocol/tick_decoder.rs` needs to handle a new tick type containing:

| Field | Type | Description |
|-------|------|-------------|
| impliedVol | f64 | Implied volatility |
| delta | f64 | Delta |
| optPrice | f64 | Option model price |
| pvDividend | f64 | Present value of dividends |
| gamma | f64 | Gamma |
| vega | f64 | Vega |
| theta | f64 | Theta |
| undPrice | f64 | Underlying price |

**New struct in `types.rs`:**
```rust
pub struct OptionGreeks {
    pub implied_vol: f64,
    pub delta: f64,
    pub gamma: f64,
    pub vega: f64,
    pub theta: f64,
    pub model_price: f64,
    pub pv_dividend: f64,
    pub und_price: f64,
}
```

**Exercise/Assignment notifications:**

IB sends exercise events via CCP (tag 6091). The Gateway defines 10 exercise types:
1. Automatic Exercise
2. Voluntary Exercise
3. Against (assignment)
4. Lapse
5. Dividend Reinvestment
6. Tender
7. Rights Issue
8. Bond Exchange Offer
9. Stock Split
10. Reverse Stock Split

Only types 1-4 are critical for options trading. Types 5-10 are corporate actions.

#### P2.2 Deep Dive: Futures Support

**FIX tags for futures:**

| Tag | Name | Example |
|-----|------|---------|
| 167 | SecurityType | "FUT" |
| 200 | MaturityMonthYear | "202606" |
| 207 | SecurityExchange | "CME" |
| 231 | ContractMultiplier | "50" (ES) |
| 15 | Currency | "USD" |

**Futures options (FOP) are a sub-category:**

| Tag | Name | Example |
|-----|------|---------|
| 167 | SecurityType | "FOP" |
| 200 | MaturityMonthYear | "202606" |
| 201 | PutOrCall | "0" / "1" |
| 202 | StrikePrice | "5800.00" |
| 231 | ContractMultiplier | "50" |

**Farm connection:** US futures use the `usfuture_conn` farm slot (already defined in HotLoop). This connection needs to be activated via `connect_farm()` with the appropriate farm name.

#### P2.3 Deep Dive: Forex Support (Good First Issue)

**Forex is the simplest multi-asset extension because:**
- No strike, no expiry, no multiplier
- Only needs: `tag 167 = "CASH"`, `tag 55 = "EUR"`, `tag 15 = "USD"`
- Uses cashfarm connection (already defined in HotLoop)
- Standard lot sizes (no fractional shares complexity)

**Implementation checklist:**
1. In `order_builder.rs`, check Contract.sec_type. If "CASH":
   - Set tag 167 = "CASH"
   - Set tag 55 = base currency (e.g., "EUR")
   - Set tag 15 = quote currency (e.g., "USD")
   - Do NOT send tag 100 (ExDestination) -- forex uses "IDEALPRO" routing implicitly
2. Activate cashfarm connection in `gateway.rs`:
   - After CCP logon, if forex is needed, connect to cashfarm
   - Cashfarm name is returned in the CCP logon response (tag 6XXX)
3. Handle cashfarm market data in the hot loop:
   - Forex ticks have the same binary format as stock ticks
   - Prices are to 5 decimal places (e.g., 1.08525)

### 10.4 Phase 3 Expanded: Connection Resilience

#### P3.1 Deep Dive: Hot Backup Strategy

The Gateway supports 3 hot backup strategies:

| Strategy | Behavior |
|----------|----------|
| OFF | Single connection, no backup. Reconnect to same host on failure. |
| ON | Maintain 2 backup connections to hot-backup hosts (e.g., `cdc1-hb.ibllc.com`). On primary failure, instantly switch to backup. |
| SINGLE_HOST | Maintain 1 backup to same host different port. |

**Implementation in IBX:**

The Gateway generates backup host names by inserting `-hb` into the primary host URL:
- Primary: `cdc1.ibllc.com` -> Backup 1: `cdc1-hb.ibllc.com` -> Backup 2: `cdc1-hb2.ibllc.com`

**IBX implementation:**
1. Add `HotBackupStrategy` enum to `config.rs`
2. On CCP logon response, parse NS_BACKUP_HOST (527) messages to discover backup hosts
3. If strategy is ON, spawn background threads to pre-connect to backup hosts (SRP auth + DH)
4. Maintain backup sessions in a `Vec<BackupSession>` in the hot loop
5. On primary disconnect, immediately switch `ccp_conn` to the first ready backup
6. The old primary becomes a failed connection; attempt re-establishment in background

**Estimated latency improvement:**
- Without hot backup: reconnect = re-auth (2-5 seconds)
- With hot backup: failover = socket swap (~1ms)

#### P3.2 Deep Dive: Session Persistence

**Problem:** IBX currently performs full SRP-6 + DH authentication on every start. This takes 2-5 seconds and requires 2FA interaction.

**Solution:** Save the session token to an encrypted file after successful auth. On restart, attempt token-based reconnect first. Only fall back to full SRP if the token is expired.

**Implementation:**
1. After successful auth, serialize `AuthResult` to JSON
2. Encrypt with AES-256-GCM using a key derived from: `PBKDF2(username + machine_id)`
3. Write to `~/.ibx/session.enc`
4. On startup, attempt to read and decrypt the session file
5. If valid: use the session token with `FLAG_PERMANENT_TOKEN` in the connect request
6. If expired/invalid: fall back to full SRP auth

**Security considerations:**
- Token file must be mode 0600 (user-read-only)
- Token has a server-side TTL (typically 24 hours)
- Machine ID must match (prevents token theft via file copy)
- Never log the token value

#### P3.3 Deep Dive: Concurrent Login Competition

The Gateway defines 4 competition types:

| Type | Description | Recommended Action |
|------|-------------|-------------------|
| `NOT_COMPETING` | No competition | Normal operation |
| `COMPETING_LIVE` | Another live session logged in | Yield or kick |
| `COMPETING_RO` | Another read-only session logged in | Continue (can coexist) |
| `COMPETING_SELF` | Same user on different machine | Yield or kick |

**IB behavior:** When a second session logs in with the same credentials, IB sends a FIX logout message (35=5) with tag 58 containing "(RO)" for read-only competition or a specific competition text.

**IBX implementation:**
1. In `ccp.rs`, when receiving 35=5 (Logout), check tag 58 for competition indicators
2. Surface `Event::CompetitionDetected(CompetitionType)` to the caller
3. Caller can respond via `ControlCommand::CompetitionResponse(CompetitionAction)`:
   - `Yield`: Gracefully disconnect and let the other session take over
   - `Kick`: Send competition kick message (re-authenticate aggressively)
   - `ReadOnly`: Downgrade to read-only mode

### 10.5 Phase 4 Expanded: Production Hardening

#### P4.1 Deep Dive: FIX Message Audit Log

**Architecture:**

```
Hot Loop                    Audit Thread
  |                            |
  +-- FIX send/recv ------>  ring buffer (lock-free)
  |                            |
  |                         flush to disk every 1s
  |                            |
  |                         rotated log files:
  |                         ~/.ibx/audit/fix-20260401.log
  |                         ~/.ibx/audit/fix-20260402.log
```

**Log format (one line per message):**
```
2026-04-01T10:30:15.123456789Z SEND CCP 35=D|11=12345|54=1|55=SPY|38=100|44=450.25|...
2026-04-01T10:30:15.234567890Z RECV CCP 35=8|39=0|11=12345|150=0|...
2026-04-01T10:30:16.000000000Z RECV FARM 35=P|BINARY:256bytes|...
```

**Configuration levels:**
- `off`: No audit logging
- `summary`: Log message type and key fields only (35, 11, 39, 150)
- `full`: Log complete FIX message (sanitize passwords/tokens)

**Implementation:**
1. New file `protocol/audit.rs` with `AuditLogger` struct
2. `AuditLogger::log_send(conn_type: &str, msg: &[u8])`
3. `AuditLogger::log_recv(conn_type: &str, msg: &[u8])`
4. Backed by a lock-free SPSC ring buffer (crossbeam)
5. Background thread drains buffer to file every 1 second
6. File rotation: new file every day, keep 30 days

#### P4.2 Deep Dive: Order State Checkpointing

**Problem:** When IBX restarts, all knowledge of open orders is lost. The only recovery mechanism is the mass status request, which has a 2-5 second delay.

**Solution:** Periodically write open order state to a checkpoint file.

**Checkpoint format (JSON, human-readable for debugging):**
```json
{
  "timestamp": "2026-04-01T10:30:00Z",
  "orders": [
    {
      "order_id": 12345,
      "con_id": 756733,
      "symbol": "SPY",
      "side": "Buy",
      "qty": 100,
      "price": 450.25,
      "status": "Submitted",
      "perm_id": "abc123",
      "fill_qty": 0,
      "avg_fill_price": 0.0
    }
  ],
  "positions": [
    {
      "con_id": 756733,
      "symbol": "SPY",
      "position": 500,
      "avg_cost": 448.50
    }
  ]
}
```

**Checkpoint frequency:** Every 5 seconds, or after any order status change.

**Recovery on restart:**
1. Read checkpoint file
2. Pre-populate `Context::open_orders` and `Context::positions`
3. After CCP logon, send mass status request
4. Reconcile: merge checkpoint state with server state
5. Alert on discrepancies (checkpoint says Submitted, server says Filled)

#### P4.3 Deep Dive: Chaos Testing Framework

**Architecture: Mock Gateway**

A local mock gateway that speaks IB's protocol is needed for deterministic chaos testing:

```
+------------------+       +-------------------+
| IBX (system      | TLS   | Mock Gateway      |
| under test)      |------>| - TLS termination |
|                  |<------| - NS handshake    |
+------------------+       | - FIX logon       |
                           | - Scenario engine |
                           +-------------------+
```

**Mock gateway components:**
1. **TLS acceptor:** Accept TLS connections on localhost:4001
2. **NS handshake:** Perform simplified NS exchange (skip SRP for testing)
3. **FIX session:** Accept FIX logon, respond with configurable account/features
4. **Market data replay:** Replay captured tick data (binary 8=O 35=P format)
5. **Order engine:** Accept 35=D, generate configurable 35=8 responses
6. **Chaos injector:**
   - `delay(ms)`: Add latency to responses
   - `drop()`: Drop the connection
   - `corrupt(field)`: Flip bits in a specific FIX field
   - `reorder(n)`: Deliver messages out of sequence
   - `partial_fill(pct)`: Fill only a percentage of order qty
   - `reject(reason)`: Reject with specific reason code

**Test DSL example:**
```rust
scenario("kill_ccp_mid_order")
    .connect()
    .subscribe("SPY")
    .wait_ticks(10)
    .place_order(Order::limit_buy("SPY", 100, 450.0))
    .inject(Chaos::drop_ccp_after_ms(50))  // Kill CCP 50ms after order
    .expect_event(Event::Disconnected)
    .wait_reconnect()
    .expect_order_reconciled(OrderStatus::Submitted)
```

### 10.6 Phase 5 Expanded: Live Trading Enablement

#### P5.1 Deep Dive: SMS/Email 2FA

**Current state:** IBX handles IB Key 2FA (DSA challenge code 775). SMS and Email 2FA use a different mechanism.

Delivery methods:
- `SMS`: IB sends SMS to registered phone
- `EMAIL`: IB sends email to registered address
- `IB_KEY`: IB Key app notification (already implemented)
- `PUSH`: Push notification to IB app

**Implementation for SMS/Email:**
1. During auth, if server requests SWTK challenge with delivery_method SMS or EMAIL:
   - Emit `Event::TwoFactorRequired { method: TwoFactorMethod, prompt: String }`
   - Wait for `ControlCommand::TwoFactorResponse(code: String)` from caller
2. The caller must provide a mechanism to accept user input:
   - CLI: `stdin` prompt
   - Python: Wrapper callback `two_factor_required(method, prompt)` -> return code
   - API: `submit_two_factor_code(code: &str)`
3. Send the OTP code back via XYZ protocol (same as IB Key challenge-response, different delivery method flag)

**Timeline:** Server sends SMS/Email, gives 120 seconds for response. After 120s, auth fails.

#### P5.2 Deep Dive: Permanent Session Token

**Token type flow:**

| Token Type | Value | Flag Bit | Persistence | When Used |
|-----------|-------|----------|-------------|-----------|
| SOFT (ST) | 0 | flag=2 | In-memory | Every session |
| PERMANENT (PST) | 1 | flag=4 | File | Subsequent sessions after first auth |
| PWD | MAX | -- | None | Paper accounts only |
| TWSRO | 3 | flag=16 | File | Read-only monitoring |
| TST | 7 | flag=256 | Unknown | Reserved |

**PST flow:**
1. First auth: Full SRP + 2FA -> receive session token
2. Server may include a "permanent token" field in the auth response
3. Save permanent token to encrypted file (P3.2)
4. Subsequent auth: Send permanent token via `FLAG_PERMANENT_TOKEN` in connect request
5. Server validates token without requiring password or 2FA
6. If token expired: server responds with auth challenge, fall back to full SRP

**Security:**
- PST has a longer TTL than session tokens (days vs hours)
- PST is bound to machine ID (same as session persistence)
- PST can be revoked server-side (e.g., password change)

#### P5.7 Deep Dive: Security Audit Checklist

**Items to verify in third-party review:**

| # | Area | What to Check |
|---|------|--------------|
| 1 | SRP-6 | Verify `srp_compute_x()` matches RFC 2945. Salt handling correct. |
| 2 | SRP-6 | Verify `srp_compute_s()` handles BigUint underflow correctly. |
| 3 | DH | Verify private key generation uses CSPRNG (rand crate). |
| 4 | DH | Verify key derivation uses TLS 1.0 PRF correctly. |
| 5 | AES-128-CBC | Verify PKCS7 padding is correct. Verify IV chaining. |
| 6 | HMAC-SHA1 | Verify FIX message signing matches Gateway behavior. |
| 7 | Token storage | Verify encrypted file uses authenticated encryption (AES-GCM). |
| 8 | Token storage | Verify file permissions are 0600. |
| 9 | Logging | Verify no passwords, tokens, or keys appear in logs. |
| 10 | Memory | Verify sensitive data is zeroized on drop (zeroize crate). |
| 11 | Network | Verify TLS 1.2+ is enforced (no TLS 1.0/1.1). |
| 12 | Network | Verify certificate validation is not disabled. |

---

## 11. Dependency Graph (Full)

This section shows the complete inter-phase dependency graph across all 54 tasks.

```
PHASE 0 (Paper Trading Robustness)
========================================
P0.1 (Concurrent order stress)
  |
  +---> P0.2 (CCP reconnect under load) ----> P0.8 (Overnight stability)
  |       |
  |       +---> P0.7 (Bracket order stress)
  |
  +---> P0.4 (Modify/cancel race conditions)
  
P0.3 (Farm disconnect/reconnect)  [independent]
P0.5 (HMDS reconnect)             [independent]
P0.6 (L2 depth stress)            [independent]


PHASE 1 (Protocol Completion) -- can start in parallel with Phase 0
========================================
P1.1 (Order state machine) ----> P1.3 (Execution restatements)
P1.2 (Feature flags)        [independent]
P1.4 (FIX validation)       [independent]
P1.5 (Commission policy)    [independent]
P1.6 (OCA types)            [independent]
P1.7 (Open/Close)           [independent]
P1.8 (Manual order)         [independent]
P1.9 (Disconnect reasons)   [independent]
P1.10 (LogonState)          [independent]
P1.11 (Dynamic sec type)    [independent, but BLOCKS Phase 2]
P1.12 (Settlement type)     [independent]


PHASE 2 (Multi-Asset) -- requires P1.11
========================================
P1.11 ----> P2.1 (Options) ----> P2.4 (Combos)
       |---> P2.2 (Futures)
       |---> P2.3 (Forex)         [Good First Issue]
       |---> P2.5 (Structured products)
       |---> P2.7 (International exchanges)
P2.6 (Security ID sources)        [independent, no P1.11 dep]


PHASE 3 (Connection Resilience)
========================================
P3.1 (Hot backup)           [independent]
P3.2 (Session persistence)  [independent]
P3.3 (Login competition)    [independent]
P0.3 ----> P3.4 (Farm reconnect with replay)
P0.5 ----> P3.5 (HMDS reconnect with retry)
P3.6 (Network pre-check)    [independent, Good First Issue]
P3.7 (Configurable heartbeat) [independent, Good First Issue]
P3.8 (Read-only mode)       [independent]


PHASE 4 (Production Hardening)
========================================
P4.1 (Audit log)            [independent]
P1.1 ----> P4.2 (Order checkpointing)
P0.2 ----> P4.3 (Chaos testing) ----> P4.4 (72h soak test)
P2.1 ----> P4.5 (Volatility orders)
P4.6 (Scale orders)         [independent]
P4.7 (Cross orders)         [independent]
P4.8 (FA support)           [independent]
P4.9 (Additional algos)     [independent]
P1.8 ----> P4.10 (Order attribution)
P4.11 (SSL pinning)         [independent]
P4.12 (Benchmark regression) [independent]


PHASE 5 (Live Trading -- LAST)
========================================
P5.1 (SMS/Email 2FA)        [independent]
P3.2 ----> P5.2 (Permanent token) ----> P5.6 (Live integration tests)
P3.1 ----> P5.5 (Auth retry with recovery)
P5.3 (TWSRO token)          [independent]
P5.4 (TST token)            [independent]
P5.1 ----> P5.6 (Live integration tests)
P5.2 ----> P5.7 (Security audit)


CROSS-PHASE CRITICAL PATHS
========================================
Path 1 (longest): P0.1 -> P0.2 -> P4.3 -> P4.4
  Total: 2 + 2 + 3 + 2 = 9 person-days

Path 2 (multi-asset): P1.11 -> P2.1 -> P4.5
  Total: 2 + 2 + 2 = 6 person-days

Path 3 (live trading): P3.2 -> P5.2 -> P5.6
  Total: 2 + 2 + 2 = 6 person-days

Path 4 (order lifecycle): P1.1 -> P1.3
  Total: 2 + 2 = 4 person-days
```

---

## 12. Risk Register

| # | Risk | Probability | Impact | Mitigation |
|---|------|------------|--------|------------|
| R1 | IB changes auth protocol in Gateway update | Medium | High | Monitor IB release notes. Analyze protocol changes. Version-gate auth code. |
| R2 | Binary tick format changes without notice | Low | High | Tick decoder has fallback for unknown tick types (skip). Add format version detection. |
| R3 | Paper trading behavior differs from live | Medium | Medium | Document all known paper/live differences. Run same tests on both. |
| R4 | CCP heartbeat interval reduced by IB | Low | Medium | Configurable heartbeat (P3.7). Monitor for server-initiated timeout disconnects. |
| R5 | New FIX tags added for regulatory compliance | High | Low | Unknown tags are ignored, not errored. Add tag discovery logging. |
| R6 | IB deprecates SRP-6 for a newer auth method | Low | Critical | Monitor auth protocol changes. XYZ protocol has version negotiation. |
| R7 | SSL certificate rotation breaks pinning | Medium | Medium | Use certificate chain pinning (pin CA, not leaf). Auto-update mechanism. |
| R8 | Rate limiting on order submission | Low | Medium | Implement request throttling (P3.1 related). Exponential backoff. |
| R9 | Hot loop thread stalls on syscall | Low | High | All hot-path I/O is non-blocking. Monitor loop iteration rate. Alert on stalls > 100ms. |
| R10 | Memory leak in long-running session | Medium | Medium | Soak test (P4.4). RSS monitoring. Use `HashSet::shrink_to_fit()` periodically. |

---

## 13. Milestone Calendar (Estimated)

Assuming a single dedicated contributor working full-time (5 days/week):

| Milestone | Phase | Target Week | Cumulative PD | Deliverable |
|-----------|-------|-------------|---------------|-------------|
| M0: Paper trading confidence | 0 | Week 2.2 | 11 | All P0 tasks pass |
| M1: Protocol complete | 1 | Week 5.0 | 25 | All 12 order states, feature flags, validation |
| M2: Options + Futures | 2 | Week 6.4 | 32 | Options and futures orders working on paper |
| M3: Resilient connections | 3 | Week 8.4 | 42 | Hot backup, session persistence, competition handling |
| M4: Production ready | 4 | Week 12.0 | 60 | Audit log, chaos tested, 72h soak passed |
| M5: Live trading ready | 5 | Week 14.0 | 70 | All auth types, security audited |
| **Total** | **0-5** | **~14 weeks** | **70 pd** | **Full production-ready IBX** |

**Parallel execution:** Phases 0 and 1 can be worked in parallel by different contributors. P2 and P3 can also overlap (different codepaths). With 2 contributors, the calendar compresses to ~8-9 weeks. The milestone targets above assume strictly sequential, single-contributor execution.

---

## 14. Accuracy Verification Report

All quantitative claims derived from file counts and enum variant counts have been verified against the actual codebase at commit `7a4c7dd`. Functional coverage percentages (e.g., "~40% coverage") are engineering estimates and are labeled with `~` to distinguish them from exact counts. Performance baselines are from manual benchmark runs and are not CI-reproducible. Below is the verification matrix for all exact claims.

| Claim | Section | Verified Value | Command | Match |
|-------|---------|---------------|---------|-------|
| 72 Rust files | 1, 2.3 | 72 | `find src/ -name "*.rs" -type f \| wc -l` | YES |
| 637 unit tests | 1, 2.3, 6.4 | 637 | `grep -rn '#\[test\]' src/ \| wc -l` | YES |
| 16 Python test files | 2.3 | 16 | `find tests/ -name "*.py" -type f \| wc -l` | YES |
| 11 benchmark binaries | 2.3 | 11 | `ls src/bin/*.rs \| wc -l` | YES |
| 21 FIX tags defined | 2.3, A | 21 | `grep -c 'pub const TAG_' src/protocol/fix.rs` | YES |
| 13 FIX msg types | 2.3, C | 13 | `grep -c 'pub const MSG_' src/protocol/fix.rs` | YES |
| 17 NS constants | 2.3, G | 17 | `grep -c 'pub const NS_' src/protocol/ns.rs` | YES |
| 63 Wrapper trait methods | 2.3, J | 63 | `awk '/pub trait Wrapper/,/^}/' src/api/wrapper.rs \| grep -c 'fn '` | YES |
| 70 order builder tags | -- | 70 | `grep -oP '\(\d+,' src/engine/hot_loop/order_builder.rs \| sort -u \| wc -l` | YES |
| Phase 0: 11 pd | 5.0 | 2+2+1+1.5+1+1+1.5+1 = 11 | Sum of P0.1-P0.8 | YES |
| Phase 1: 14 pd | 5.1 | 2+2+2+1+0.5+0.5+0.5+0.5+1.5+1+2+0.5 = 14 | Sum of P1.1-P1.12 | YES |
| Phase 2: 7 pd | 5.2 | 2+1+1+1.5+0.5+0.5+0.5 = 7 | Sum of P2.1-P2.7 | YES |
| Phase 3: 10 pd | 5.3 | 2.5+2+1.5+1.5+1+0.5+0.5+0.5 = 10 | Sum of P3.1-P3.8 | YES |
| Phase 4: 18 pd | 5.4 | 2+2+3+2+2+1+0.5+2+1+1+0.5+1 = 18 | Sum of P4.1-P4.12 | YES |
| Phase 5: 10 pd | 5.5 | 2+2+1+0.5+1.5+2+1 = 10 | Sum of P5.1-P5.7 | YES |
| Total: 70 pd | 5.6 | 11+14+7+10+18+10 = 70 | Sum of phases | YES |
| 54 tasks | 5.6 | 8+12+7+8+12+7 = 54 | Count of P*.* | YES |
| 10 GFIs | 8.3 | 2+4+2+2+0+0 = 10 | Count per phase | YES |

**All exact (non-estimated) claims verified. Zero discrepancies in reproducible counts or phase arithmetic.**

**Note:** Functional coverage percentages (e.g., "~40%", "~43%") are engineering estimates. Performance baselines (tick decode 340ns, etc.) are from manual benchmark runs on specific hardware and are not CI-reproducible.

---

## 19. Rust Module Architecture (Current State)

```
src/ (72 files)
|
+-- auth/ (5 files)
|   +-- mod.rs         -- re-exports
|   +-- crypto.rs      -- AES-128-CBC, HMAC-SHA1, TLS 1.0 PRF
|   +-- dh.rs          -- Diffie-Hellman key exchange + SecureChannel
|   +-- session.rs     -- Auth lifecycle: SRP, DH, token, IB Key 2FA
|   +-- srp.rs         -- SRP-6 implementation (2048-bit prime)
|
+-- protocol/ (7 files)
|   +-- mod.rs         -- re-exports
|   +-- connection.rs  -- Non-blocking TLS/TCP with HMAC sign/verify
|   +-- fix.rs         -- FIX 4.1 framing: build, parse, checksum, HMAC
|   +-- fixcomp.rs     -- Compressed FIX (zlib) framing
|   +-- ns.rs          -- NS protocol (#%#% framing, semicolon payload)
|   +-- tick_decoder.rs  -- Binary tick decoder (MSB bit-reader, VLQ)
|   +-- xyz.rs         -- XYZ binary protocol for SRP auth
|
+-- engine/ (8 files)
|   +-- mod.rs         -- re-exports
|   +-- context.rs     -- Strategy context: market data, orders, positions
|   +-- market_state.rs  -- Pre-allocated quote storage, server_tag mapping
|   +-- hot_loop/
|       +-- mod.rs     -- Pinned-core busy-poll loop, heartbeat, reconnect
|       +-- ccp.rs     -- CCP message handler: exec reports, positions, PnL
|       +-- farm.rs    -- Farm message handler: ticks, depth, market data
|       +-- hmds.rs    -- HMDS handler: historical, TBT, scanners, bars
|       +-- order_builder.rs  -- FIX order construction (37 types)
|
+-- api/ (11 files)
|   +-- mod.rs
|   +-- types.rs       -- ibapi-compatible types (Contract, Order, etc.)
|   +-- wrapper.rs     -- Wrapper trait (63 methods)
|   +-- client/
|       +-- mod.rs     -- EClient struct
|       +-- dispatch.rs  -- Event dispatch loop
|       +-- account.rs   -- Account methods
|       +-- market_data.rs  -- Market data methods
|       +-- orders.rs    -- Order methods
|       +-- reference.rs  -- Contract/scanner/news methods
|       +-- stubs.rs     -- Stub methods (not yet implemented)
|       +-- tests.rs     -- Unit tests
|
+-- control/ (8 files)
|   +-- mod.rs
|   +-- account.rs     -- Account summary, positions
|   +-- contracts.rs   -- Contract details, market rules
|   +-- fundamental.rs -- Fundamental data parsing
|   +-- histogram.rs   -- Histogram data
|   +-- historical.rs  -- Historical bar parsing
|   +-- news.rs        -- News bulletins, articles
|   +-- scanner.rs     -- Scanner subscriptions
|
+-- python/ (14 files)
|   +-- mod.rs
|   +-- types.rs
|   +-- compat/
|       +-- mod.rs
|       +-- contract.rs  -- Python Contract class
|       +-- tick_types.rs  -- Tick type enums
|       +-- wrapper.rs    -- Python Wrapper adapter
|       +-- client/
|           +-- mod.rs    -- Python EClient class
|           +-- dispatch.rs  -- Python event dispatch
|           +-- account.rs   -- Python account methods
|           +-- market_data.rs  -- Python market data
|           +-- orders.rs    -- Python order methods
|           +-- reference.rs  -- Python reference methods
|           +-- stubs.rs     -- Python stubs
|           +-- test_helpers.rs  -- Test utilities
|
+-- bin/ (11 files)
|   +-- benchmark.rs   -- General benchmark harness
|   +-- bench_decode.rs  -- Tick decode benchmark
|   +-- bench_first_tick.rs  -- First tick latency
|   +-- bench_limit_order.rs  -- Limit order latency
|   +-- bench_market_order.rs  -- Market order latency
|   +-- bench_multi_instrument.rs  -- Multi-instrument stress
|   +-- bench_order_burst.rs  -- Order burst throughput
|   +-- bench_order_modify.rs  -- Order modify latency
|   +-- bench_replay.rs  -- Market data replay
|   +-- bench_tbt.rs   -- Tick-by-tick benchmark
|   +-- bench_ticks.rs -- Tick processing benchmark
|
+-- (top-level files: 8 files)
    +-- bridge.rs      -- SharedState, Event channel, SeqLock
    +-- client_core.rs -- Shared dispatch for Rust/Python
    +-- config.rs      -- Constants, timestamps, timeouts
    +-- gateway.rs     -- Auth + farm orchestration
    +-- lib.rs         -- Crate root
    +-- logging.rs     -- Tracing setup
    +-- main.rs        -- CLI entry point
    +-- types.rs       -- Core types: Price, Quote, Order, Fill, etc.
```

---

## 20. Version History

| Version | Date | Changes |
|---------|------|---------|
| 1.0 | 2026-03-31 | Initial gap analysis |
| 2.0 | 2026-04-02 | Complete rewrite: domain-focused gap analysis, phased implementation plan, expanded task specifications |
| 2.1 | 2026-04-03 | Updated for v0.4.4: P1.1 order state machine complete, options market data infrastructure (UsOpt farm), P&L subscription (6040=142/143), position feed (6040=75), bracket order linking, keepUpToDate via CCP, scanner enrichment |

---

*This document was produced by systematic analysis of the IB Gateway protocol against IBX's Rust implementation. Baseline updated to v0.4.4 (commit `120d366`). Structural metrics are verifiable against the stated commit.*
