# IBX

**Direct IB connection engine. No Java Gateway. No middleman.**

IBX connects directly to Interactive Brokers servers — without requiring the official Java Gateway. Built in Rust for ultra-low-latency, available as both a Rust library and a Python library via PyO3. Both expose an ibapi-compatible `EClient` / `Wrapper` API.

## Why IBX

- **Latency.** Tick read path is ~340 ns vs ~2 ms through the Java Gateway (≈5,900× faster). Order send is ~460 ns vs 76–125 µs (≈170–320× faster).
- **No JVM.** No localhost hop, no garbage-collector pauses, no separate process to babysit.
- **Same API.** Drop-in compatible callback shape — port existing strategies without rewriting.
- **Two languages, one engine.** Rust crate and Python wheel ship from the same core.

## Where to go next

- New here? → [Getting Started](./getting-started.md)
- Want to see real code? → [Recipes](./recipes/streaming-l2.md)
- Looking up a specific call? → [Rust API](./api/rust.md) · [Python API](./api/python.md)
- Wondering what's wired up? → [Endpoint Coverage](./reference/coverage.md)

## Project status

IBX is under active development. The Rust and Python APIs track the official IB API surface; gaps are tracked in [Endpoint Coverage](./reference/coverage.md).
