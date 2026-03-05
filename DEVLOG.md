## 2026-03-04 - Architecture Research for Ultra-Low-Latency Rust Trading Engine

### Goal
Research production-grade HFT/low-latency trading engine architecture in Rust. Gather concrete, practical information on thread architecture, lock-free data structures, strategy APIs, Rust-specific optimizations, and open-source reference implementations.

### Approach Taken
Systematic web research across 5 topic areas, cross-referencing multiple sources including blog posts, GitHub repos, framework documentation, and industry articles.

### What Worked
- Found concrete architecture patterns from Botvana (thread-per-core), Barter-rs (event-driven), and HftBacktest (tick-by-tick)
- Found detailed SPSC ring buffer implementation guide with cache-line padding and memory ordering
- NautilusTrader and Barter-rs provided real strategy API patterns
- Multiple sources confirmed the same core principles (no allocation on hot path, CPU pinning, SPSC channels)

### Key Decisions
- Research output saved to `.tmp/rust-hft-architecture-research.md`
- This research will inform the architecture of the ib-engine project

### What Failed
- Some web pages (downgraf.com, siddharthqs.com) had JavaScript-heavy rendering that prevented content extraction
- Most "Rust HFT" articles are high-level overviews; real implementation details came from ring buffer articles and GitHub repos

### Current State
- Research complete, saved to `.tmp/rust-hft-architecture-research.md` (~750 lines)
- Covers: thread architecture, lock-free data structures, strategy APIs, Rust optimizations, open-source engines
- Includes code examples, benchmark data, crate recommendations, and a proposed ib-engine architecture
- No code written yet - this was a research-only session
- Next step: Define the concrete ib-engine architecture and start implementing

## 2026-03-04 - Strategy API Design Research (Deep Dive)

### Goal
Research how production trading frameworks design strategy interfaces while maintaining zero overhead. Find real code examples from open-source projects.

### Approach Taken
Extracted actual source code from GitHub repos using `gh api` to get raw file contents. Analyzed 5 frameworks: Barter-rs (Rust), NautilusTrader (Rust/Python), QuantConnect Lean (C#), rust_bt (Rust), MMB/purefinance (Rust).

### What Worked
- `gh api repos/.../contents/path --jq '.content' | base64 -d` gave exact source code from GitHub
- Barter-rs has the most complete and well-designed Rust strategy API - extracted all 4 strategy traits + engine struct + full example
- NautilusTrader's Rust Strategy trait is massive (~1300 lines) with full order management
- Found two fundamentally different patterns: functional (Barter-rs) vs event-driven (NautilusTrader)

### What Failed
- Medium articles blocked (403 errors)
- docs.rs pages often don't show actual trait method signatures inline
- General web search for "trait Strategy" returned too many generic Rust trait tutorials

### Key Decisions
- **Barter-rs functional pattern is recommended for HFT**: strategy as pure function `(&self, &State) -> Orders`
- **Static dispatch via generics** for the hot path, not trait objects
- **Engine owns all state**, strategy borrows immutably - eliminates borrow conflicts
- **Strategies return iterators** (lazy, zero-allocation), not vectors
- **Position tracking is engine-managed**, not strategy-managed

### Current State
- Research saved to `.tmp/strategy-api-research.md`
- Ready to start designing our own strategy API based on findings

## 2026-03-04 - Architecture Scaffold Implementation

### Goal
Implement the full module structure, core types, Strategy trait, Context, and hot loop skeleton from the architecture plan.

### Approach Taken
Created all modules in one pass: types → market_state → context → hot_loop → protocol/auth/control stubs → main.rs. Chose imperative context API over Barter-rs functional style for accessibility.

### What Worked
- Fixed-point Price (i64 × 10^8) and Qty (i64 × 10^4) — no floating point on hot path
- Cache-aligned Quote struct (`#[repr(C, align(64))]`, 88 bytes → 2 cache lines)
- Pre-allocated arrays for quotes `[Quote; 256]` and positions `[i64; 256]` indexed by InstrumentId
- OrderBuffer uses `Vec::with_capacity(64)` — allocates once at startup, push/clear cycle is zero-alloc
- Strategy trait is generic type param (monomorphized, no vtable)
- HotLoop<S: Strategy> runs everything inline: poll → decode → strategy.on_tick → drain orders → send
- core_affinity for CPU pinning, std::time::Instant for clock (quanta ready as dep)

### Key Decisions
- **Imperative context** (`ctx.submit_limit()`) over functional (`fn(&State) -> Orders`) — more intuitive for quant devs
- **HashMap for open_orders** (keyed by OrderId) — fills are less frequent than ticks, hash lookup is fine
- **Vec<OrderRequest> for pending_orders** — pre-allocated capacity, never grows, drained each tick
- **std::time::Instant for Clock** initially — swap to quanta/RDTSC when profiling shows it matters
- **Protocol/auth/control as empty modules** — doc comments describe what goes there, no premature implementation

### Current State
- Full module structure compiles and runs
- 20 files created across src/engine, src/protocol, src/auth, src/control
- Example SimpleMomentum strategy in main.rs demonstrates the API
- All dead-code warnings are expected (protocol layer not wired yet)
- Next step: Implement the IB protocol layer (FIX parser, FIXCOMP, tick decoder) using ibgw-headless as reference

## 2026-03-04 - Test Suite (Unit + Integration)

### Goal
Add comprehensive tests matching ibgw-headless testing patterns: unit tests per module, integration tests for strategy lifecycle, and protocol test vectors from captured binary data.

### Approach Taken
- Unit tests as `#[cfg(test)]` modules in types.rs, market_state.rs, context.rs, hot_loop.rs
- Integration tests in `tests/` directory: strategy_lifecycle.rs and protocol_vectors.rs
- Extracted binary test vectors from ibgw-headless/tests/test_fix.py and test_historical.py
- Added lib.rs to expose public API for integration tests

### What Worked
- 73 tests all passing (50 unit + 21 protocol vectors + 2 integration)
- VLQ decoder, hibit string decoder, tick decoder (AllLast/BidAsk) all verified against ibgw-headless test vectors
- FIX checksum and XOR fold validated against Python reference implementation
- 6 RTBAR binary captures from ibgw-headless issue #37 included as test constants
- Strategy lifecycle test covers: tick → order → fill → position → take-profit → idle

### Key Decisions
- **lib.rs added** to separate library from binary — integration tests need `ib_engine::` paths
- **inject_tick/inject_fill made public** (not `#[cfg(test)]`) — integration tests can't see `#[cfg(test)]` items
- **Context got public methods**: `register_instrument()`, `quote_mut()`, `drain_pending_orders()` — needed for both integration tests and future engine consumers
- **Protocol decoders written standalone in test file** — will move to `src/protocol/` when implementing the real protocol layer

### Current State
- 73/73 tests passing
- Protocol test vectors ready to validate real decoder implementations
- Next step: Implement protocol layer (move VLQ/hibit/tick decoders from tests into src/protocol/)

## 2026-03-04 - Control Plane: Contracts + Historical + Integration Tests

### Goal
Complete the two remaining stub modules (contracts.rs, historical.rs) and add integration tests.

### Approach Taken
- Researched ibgw-headless source for exact FIX tag layouts (35=c/d for contracts, 35=W with XML tag 6118 for historical)
- Implemented contracts.rs: FIX request builders, response parser, ContractStore cache, SecurityType/OptionRight enums
- Implemented historical.rs: XML query builder, bar response parser, ticker ID parser, cancel request builder
- Integration tests cover cross-module workflows: FIX roundtrips, FIXCOMP compression, HMAC signing, account tracking

### What Worked
- Pure string-based XML builder/parser (no external XML dependency needed)
- FIX tag mapping matches ibgw-headless exactly (SMART→BEST, STK→CS)
- ContractStore dual-index (by conId + by symbol:secType:currency)
- 14 integration tests covering realistic multi-step workflows

### Key Decisions
- **No XML parser dependency**: XML structure is well-defined, simple string building/extraction suffices
- **HistoricalRequest.exchange is &'static str**: avoids lifetime complexity, exchange strings are known constants
- **ContractStore keyed by composite string**: "AAPL:CS:USD" for symbol lookups, HashMap<u32> for conId

### Current State
- 172 total tests passing (135 unit + 21 protocol vectors + 14 control plane integration + 2 strategy lifecycle)
- All control plane modules complete: config, account, contracts, historical
- All protocol modules complete: fix, fixcomp, ns, xyz, tick_decoder
- All auth modules complete: crypto, dh, srp, session
- Next step: Wire protocol layer into hot loop (connect decoders to market state updates)

## 2026-03-05 - Wire Protocol Layer into Hot Loop

### Goal
Implement the 5 TODO methods in hot_loop.rs:59-93 that connect the protocol decoders/encoders to the hot loop: poll_market_data, drain_and_send_orders, poll_executions, poll_control_commands, check_heartbeats.

### Approach Taken
Created 6 GitHub issues (#11-#16) for each step, implemented sequentially. Used ibgw-headless (D:\PycharmProjects\ibgw-headless) as reference for connection management, message framing, heartbeat logic, and FIX field mappings.

### What Worked
- **Connection struct** (protocol/connection.rs): non-blocking TLS stream wrapper with frame extraction for FIX/FIXCOMP/8=O protocols, HMAC sign/unsign with IV chaining
- **poll_market_data**: farm recv → unsign → FIXCOMP decompress → 35=P bit-packed tick decode → quote delta-accumulation → strategy on_tick (once per instrument per batch via bitmask dedup)
- **drain_and_send_orders**: all OrderRequest variants mapped to FIX 35=D/F/G with proper tag mapping (side→54, ordType→40, price→44), format_price for fixed-point→decimal
- **poll_executions**: CCP recv → FIX decode → 35=8 exec report → position update → on_fill/on_order_update, 35=9 cancel reject handling
- **SPSC control channel**: crossbeam-channel bounded, ControlCommand enum (Subscribe/Unsubscribe/UpdateParam/Shutdown), run() changed from -> ! to -> ()
- **Heartbeat tracking**: CCP 10s / farm 30s intervals, TestRequest escalation, auto-respond to 35=1, timestamp tracking on send/recv

### Key Decisions
- **Collect-then-process pattern** to avoid borrow conflicts: extract all frames from conn into Vec, then process without holding conn borrow
- **Price ticks are deltas** (magnitude * minTick * PRICE_SCALE), size ticks are absolute values
- **crossbeam-channel** over tokio channels for SPSC (no async runtime needed on hot path)
- **HeartbeatState** as separate struct to keep HotLoop fields clean

### What Failed
- Previous session hung because the TODOs required a network layer that didn't exist — the Connection struct had to be designed first

### Current State
- 198 total tests passing (161 unit + 21 protocol vectors + 14 control plane integration + 2 strategy lifecycle)
- All 5 hot loop TODOs implemented
- All protocol modules complete: fix, fixcomp, ns, xyz, tick_decoder, connection
- Next step: Connection establishment/auth handshake (SRP flow), then real integration testing against IB gateway
