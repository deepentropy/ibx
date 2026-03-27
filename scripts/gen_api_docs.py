#!/usr/bin/env python3
"""Generate API reference docs from Rust source files.

Parses pub fn signatures + doc comments from src/api/client/*.rs,
Wrapper trait from src/api/wrapper.rs, and Python pymethods from
src/python/compat/client/*.rs. Outputs docs/RUST_API.md and docs/PYTHON_API.md.

Usage: py scripts/gen_api_docs.py
"""

import re
import tomllib
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
RUST_CLIENT = ROOT / "src" / "api" / "client"
RUST_WRAPPER = ROOT / "src" / "api" / "wrapper.rs"
PY_CLIENT = ROOT / "src" / "python" / "compat" / "client"
PY_WRAPPER = ROOT / "src" / "python" / "compat" / "wrapper.rs"
DOCS = ROOT / "docs"


def version() -> str:
    with open(ROOT / "Cargo.toml", "rb") as f:
        return tomllib.load(f)["package"]["version"]


# ── Rust parser ──

def parse_rust_methods(path: Path) -> list[dict]:
    """Extract pub fn methods with preceding /// doc comments."""
    text = path.read_text(encoding="utf-8")
    results = []
    # Find all pub fn with optional preceding doc comments and attributes
    for m in re.finditer(
        r'((?:\s*///[^\n]*\n)*)(?:\s*#\[[^\]]*\]\s*\n)*\s*pub fn (\w+)\s*\(([^)]*(?:\([^)]*\)[^)]*)*)\)([^{;]*)',
        text,
    ):
        doc_block, name, _args, ret = m.group(1), m.group(2), m.group(3), m.group(4)
        # Parse doc
        doc_lines = []
        for line in doc_block.strip().splitlines():
            line = line.strip().removeprefix("///").strip()
            if line:
                doc_lines.append(line)
        doc = " ".join(doc_lines)
        # Clean doc: remove "Matches `xyz` in C++."
        doc = re.sub(r"\s*Matches `[^`]+` in C\+\+\.?", "", doc)
        # Build signature
        full_line = m.group(0).strip()
        sig = re.sub(r'\s*\{.*', '', full_line).strip()
        sig = re.sub(r'\s+', ' ', sig)
        # Remove preceding doc comments from sig
        sig = re.sub(r'///[^\n]*\n\s*', '', sig).strip()
        results.append({"name": name, "signature": sig, "doc": doc})
    return results


def parse_wrapper_trait(path: Path) -> list[dict]:
    """Extract fn methods from the Wrapper trait block."""
    text = path.read_text(encoding="utf-8")
    # Find the trait block
    trait_m = re.search(r'pub trait Wrapper\s*\{(.*?)\n\}', text, re.DOTALL)
    if not trait_m:
        return []
    body = trait_m.group(1)
    results = []
    for m in re.finditer(
        r'((?:\s*//[^\n]*\n)*)\s*fn (\w+)\s*\(([^)]*(?:\([^)]*\)[^)]*)*)\)([^{}]*)',
        body,
    ):
        comment_block, name, _args, _ret = m.group(1), m.group(2), m.group(3), m.group(4)
        doc_lines = []
        for line in comment_block.strip().splitlines():
            line = line.strip()
            if line.startswith("///"):
                doc_lines.append(line.removeprefix("///").strip())
            elif line.startswith("// "):
                # Section header like "// ── Market Data ──"
                sec = line.removeprefix("//").strip()
                sec = sec.strip("─ ").strip()
                if sec:
                    pass  # Skip section headers as doc
        doc = " ".join(doc_lines)
        results.append({"name": name, "signature": f"fn {name}(...)", "doc": doc})
    return results


# ── Python parser ──

def parse_pymethods(path: Path) -> list[dict]:
    """Extract fn methods from all #[pymethods] impl blocks."""
    text = path.read_text(encoding="utf-8")
    results = []
    # Find all #[pymethods] blocks
    # Strategy: find each #[pymethods], then find the impl block, then extract methods
    blocks = re.split(r'#\[pymethods\]', text)
    for block in blocks[1:]:  # skip text before first #[pymethods]
        # Find the impl opening
        impl_m = re.match(r'\s*impl\s+\w+\s*\{', block)
        if not impl_m:
            continue
        # Track braces to find end of impl block
        start = impl_m.end() - 1  # position of opening {
        depth = 0
        end = start
        for i in range(start, len(block)):
            if block[i] == '{':
                depth += 1
            elif block[i] == '}':
                depth -= 1
                if depth == 0:
                    end = i
                    break
        impl_body = block[start + 1:end]
        # Extract methods from this impl body
        for fm in re.finditer(
            r'((?:\s*(?:///|//)[^\n]*\n|\s*#\[pyo3[^\]]*\]\s*\n)*)\s*fn (\w+)\s*\(([^)]*(?:\([^)]*\)[^)]*)*)\)',
            impl_body,
        ):
            preamble, name, args_str = fm.group(1), fm.group(2), fm.group(3)
            # Skip __new__ internal
            if name.startswith("__"):
                continue
            # Extract doc
            doc_lines = []
            pyo3_sig = ""
            for line in preamble.strip().splitlines():
                line = line.strip()
                if line.startswith("///"):
                    doc_lines.append(line.removeprefix("///").strip())
                elif line.startswith("#[pyo3(signature"):
                    pyo3_sig = line
            doc = " ".join(doc_lines)
            doc = re.sub(r"\s*Matches `[^`]+` in C\+\+\.?", "", doc)
            # Build Python signature
            py_sig = _build_py_sig(name, args_str, pyo3_sig)
            results.append({"name": name, "signature": py_sig, "doc": doc})
    return results


def _build_py_sig(name: str, rust_args: str, pyo3_sig: str) -> str:
    if pyo3_sig:
        m = re.search(r'signature\s*=\s*\((.+)\)', pyo3_sig)
        if m:
            return f"{name}({m.group(1)})"
    # Parse rust args, skip &self, py: Python
    args = [a.strip() for a in rust_args.split(',')]
    py_args = []
    for arg in args:
        if not arg or arg in ("&self", "&mut self"):
            continue
        if "Python" in arg:
            continue
        am = re.match(r'(\w+)\s*:', arg)
        if am:
            py_args.append(am.group(1))
    return f"{name}({', '.join(py_args)})"


def parse_py_wrapper(path: Path) -> list[dict]:
    """Extract callback methods from Python EWrapper pymethods."""
    return parse_pymethods(path)


# ── Fallback descriptions for well-known callbacks/methods ──

KNOWN_DESCRIPTIONS: dict[str, str] = {
    # Connection
    "connect_ack": "Connection acknowledged.",
    "connection_closed": "Connection has been closed.",
    "next_valid_id": "Next valid order ID from the server.",
    "managed_accounts": "Comma-separated list of managed account IDs.",
    "error": "Error or informational message from the server.",
    "current_time": "Current server time (Unix seconds).",
    "is_connected": "Check if the client is connected.",
    "new": "Create a new EClient (or EWrapper) instance.",
    # Market Data
    "tick_price": "Price tick update (bid, ask, last, etc.).",
    "tick_size": "Size tick update (bid size, ask size, volume, etc.).",
    "tick_string": "String tick (e.g. last trade timestamp).",
    "tick_generic": "Generic numeric tick value.",
    "tick_snapshot_end": "Snapshot delivery complete; subscription auto-cancelled.",
    "market_data_type": "Market data type changed (1=live, 2=frozen, 3=delayed, 4=delayed-frozen).",
    "tick_req_params": "Tick parameters: min tick size, BBO exchange, snapshot permissions.",
    # Orders
    "order_status": "Order status update (filled, remaining, avg price, etc.).",
    "open_order": "Open order details (contract, order, state).",
    "open_order_end": "End of open orders list.",
    "exec_details": "Execution fill details.",
    "exec_details_end": "End of execution details list.",
    "commission_report": "Commission report for an execution.",
    "completed_order": "Completed (filled/cancelled) order details.",
    "completed_orders_end": "End of completed orders list.",
    "order_bound": "Order bound to a perm ID.",
    # Account
    "update_account_value": "Account value update (key/value/currency).",
    "update_portfolio": "Portfolio position update.",
    "update_account_time": "Account update timestamp.",
    "account_download_end": "Account data delivery complete.",
    "account_summary": "Account summary tag/value entry.",
    "account_summary_end": "End of account summary.",
    "position": "Position entry (account, contract, size, avg cost).",
    "position_end": "End of positions list.",
    "pnl": "Account P&L update (daily, unrealized, realized).",
    "pnl_single": "Single-position P&L update.",
    # Historical Data
    "historical_data": "Historical OHLCV bar.",
    "historical_data_end": "End of historical data delivery.",
    "historical_data_update": "Real-time bar update (keep_up_to_date=true).",
    "head_timestamp": "Earliest available data timestamp.",
    # Contract Details
    "contract_details": "Contract definition details.",
    "contract_details_end": "End of contract details.",
    "symbol_samples": "Matching symbol search results.",
    # Tick-by-Tick
    "tick_by_tick_all_last": "Tick-by-tick last trade.",
    "tick_by_tick_bid_ask": "Tick-by-tick bid/ask quote.",
    "tick_by_tick_mid_point": "Tick-by-tick midpoint.",
    # Scanner
    "scanner_data": "Scanner result entry (rank, contract, distance).",
    "scanner_data_end": "End of scanner results.",
    "scanner_parameters": "Scanner parameters XML.",
    # News
    "update_news_bulletin": "News bulletin message.",
    "tick_news": "Per-contract news tick.",
    "historical_news": "Historical news headline.",
    "historical_news_end": "End of historical news.",
    "news_article": "Full news article text.",
    "news_providers": "Available news providers list.",
    # Real-Time Bars
    "real_time_bar": "Real-time 5-second OHLCV bar.",
    # Historical Ticks
    "historical_ticks": "Historical tick data (Last, BidAsk, or Midpoint).",
    "historical_ticks_bid_ask": "Historical bid/ask ticks.",
    "historical_ticks_last": "Historical last-trade ticks.",
    # Histogram
    "histogram_data": "Price distribution histogram.",
    # Market Rules
    "market_rule": "Market rule: price increment schedule.",
    # Historical Schedule
    "historical_schedule": "Historical trading schedule (exchange hours).",
    # Fundamental Data
    "fundamental_data": "Fundamental data (XML/JSON).",
    # Market Depth
    "update_mkt_depth": "L2 book update (single exchange).",
    "update_mkt_depth_l2": "L2 book update (with market maker).",
    "mkt_depth_exchanges": "Available exchanges for market depth.",
    # Gateway-local
    "smart_components": "SMART routing component exchanges.",
    "soft_dollar_tiers": "Soft dollar tier list.",
    "family_codes": "Family codes linking related accounts.",
    "user_info": "User info (white branding ID).",
    # Options
    "tick_option_computation": "Option implied vol / greeks computation.",
    "security_definition_option_parameter": "Option chain parameters (strikes, expirations).",
    "security_definition_option_parameter_end": "End of option chain parameters.",
    # FA
    "receive_fa": "Financial advisor data received.",
    "replace_fa_end": "Financial advisor replace complete.",
    # Multi-account
    "position_multi": "Multi-account position entry.",
    "position_multi_end": "End of multi-account positions.",
    "account_update_multi": "Multi-account value update.",
    "account_update_multi_end": "End of multi-account updates.",
    # Display Groups
    "display_group_list": "Display group list.",
    "display_group_updated": "Display group updated.",
    # WSH
    "wsh_meta_data": "Wall Street Horizon metadata.",
    "wsh_event_data": "Wall Street Horizon event data.",
    # Bond
    "bond_contract_details": "Bond contract details.",
    "delta_neutral_validation": "Delta-neutral validation response.",
    # Python stubs
    "calculate_implied_volatility": "Calculate option implied volatility. Not yet implemented.",
    "calculate_option_price": "Calculate option theoretical price. Not yet implemented.",
    "cancel_calculate_implied_volatility": "Cancel implied volatility calculation.",
    "cancel_calculate_option_price": "Cancel option price calculation.",
    "exercise_options": "Exercise options. Not yet implemented.",
    "req_sec_def_opt_params": "Request option chain parameters. Not yet implemented.",
    "cancel_mkt_data": "Cancel market data subscription.",
    "req_news_bulletins": "Subscribe to news bulletins.",
    "cancel_news_bulletins": "Cancel news bulletin subscription.",
    "req_current_time": "Request current server time.",
    "request_fa": "Request FA data. Not yet implemented.",
    "replace_fa": "Replace FA data. Not yet implemented.",
    "query_display_groups": "Query display groups.",
    "subscribe_to_group_events": "Subscribe to display group events.",
    "unsubscribe_from_group_events": "Unsubscribe from display group events.",
    "update_display_group": "Update display group.",
    "req_smart_components": "Request SMART routing component exchanges.",
    "req_news_providers": "Request available news providers.",
    "req_soft_dollar_tiers": "Request soft dollar tiers.",
    "req_family_codes": "Request family codes.",
    "set_server_log_level": "Set server log level (1=error..5=trace).",
    "req_user_info": "Request user info (white branding ID).",
    "req_wsh_meta_data": "Request Wall Street Horizon metadata. Not yet implemented.",
    "req_wsh_event_data": "Request Wall Street Horizon event data. Not yet implemented.",
}


def enrich_doc(m: dict) -> dict:
    """Fill empty doc from KNOWN_DESCRIPTIONS."""
    if not m["doc"] and m["name"] in KNOWN_DESCRIPTIONS:
        m = {**m, "doc": KNOWN_DESCRIPTIONS[m["name"]]}
    return m


# ── Markdown generation ──

SECTION_NAMES = {
    "mod": "Connection",
    "account": "Account & Portfolio",
    "orders": "Orders",
    "market_data": "Market Data",
    "reference": "Reference Data",
    "stubs": "Gateway-Local & Stubs",
    "dispatch": None,
    "tests": None,
    "test_helpers": None,
}

FILE_ORDER = ["mod", "account", "orders", "market_data", "reference", "stubs"]


def fmt_row(m: dict) -> str:
    return f"| `{m['name']}` | {m['doc']} |"


def generate_rust_md(ver: str) -> str:
    out = [
        f"# Rust API Reference (v{ver})",
        "",
        "*Auto-generated from source — do not edit.*",
        "",
        "## EClient Methods",
        "",
    ]
    for stem in FILE_ORDER:
        fname = RUST_CLIENT / f"{stem}.rs"
        if not fname.exists():
            continue
        section = SECTION_NAMES.get(stem, stem.replace("_", " ").title())
        if section is None:
            continue
        methods = parse_rust_methods(fname)
        if not methods:
            continue
        out.append(f"### {section}")
        out.append("")
        out.append("| Method | Description |")
        out.append("|--------|-------------|")
        for m in methods:
            out.append(fmt_row(enrich_doc(m)))
        out.append("")

    # Wrapper
    out.append("## Wrapper Callbacks")
    out.append("")
    wm = parse_wrapper_trait(RUST_WRAPPER)
    out.append("| Callback | Description |")
    out.append("|----------|-------------|")
    for m in wm:
        out.append(fmt_row(enrich_doc(m)))
    out.append("")

    # Signatures appendix
    out.append("## Full Signatures")
    out.append("")
    out.append("<details>")
    out.append("<summary>EClient</summary>")
    out.append("")
    out.append("```rust")
    for stem in FILE_ORDER:
        fname = RUST_CLIENT / f"{stem}.rs"
        if not fname.exists():
            continue
        if SECTION_NAMES.get(stem) is None:
            continue
        for m in parse_rust_methods(fname):
            out.append(m["signature"])
    out.append("```")
    out.append("</details>")
    out.append("")
    out.append("<details>")
    out.append("<summary>Wrapper</summary>")
    out.append("")
    out.append("```rust")
    for m in wm:
        out.append(m["signature"])
    out.append("```")
    out.append("</details>")
    out.append("")

    return "\n".join(out)


def generate_python_md(ver: str) -> str:
    out = [
        f"# Python API Reference (v{ver})",
        "",
        "*Auto-generated from source — do not edit.*",
        "",
        "## EClient Methods",
        "",
    ]
    for stem in FILE_ORDER:
        fname = PY_CLIENT / f"{stem}.rs"
        if not fname.exists():
            continue
        section = SECTION_NAMES.get(stem, stem.replace("_", " ").title())
        if section is None:
            continue
        methods = parse_pymethods(fname)
        if not methods:
            continue
        out.append(f"### {section}")
        out.append("")
        out.append("| Method | Description |")
        out.append("|--------|-------------|")
        for m in methods:
            out.append(fmt_row(enrich_doc(m)))
        out.append("")

    # Wrapper
    out.append("## EWrapper Callbacks")
    out.append("")
    wm = parse_py_wrapper(PY_WRAPPER)
    out.append("| Callback | Description |")
    out.append("|----------|-------------|")
    for m in wm:
        out.append(fmt_row(enrich_doc(m)))
    out.append("")

    # Signatures
    out.append("## Full Signatures")
    out.append("")
    out.append("<details>")
    out.append("<summary>EClient</summary>")
    out.append("")
    out.append("```python")
    for stem in FILE_ORDER:
        fname = PY_CLIENT / f"{stem}.rs"
        if not fname.exists():
            continue
        if SECTION_NAMES.get(stem) is None:
            continue
        for m in parse_pymethods(fname):
            out.append(f"def {m['signature']}")
    out.append("```")
    out.append("</details>")
    out.append("")
    out.append("<details>")
    out.append("<summary>EWrapper</summary>")
    out.append("")
    out.append("```python")
    for m in wm:
        out.append(f"def {m['signature']}")
    out.append("```")
    out.append("</details>")
    out.append("")

    return "\n".join(out)


def main():
    DOCS.mkdir(exist_ok=True)
    ver = version()
    rust = generate_rust_md(ver)
    py = generate_python_md(ver)
    (DOCS / "RUST_API.md").write_text(rust, encoding="utf-8")
    (DOCS / "PYTHON_API.md").write_text(py, encoding="utf-8")
    rc = rust.count("| `")
    pc = py.count("| `")
    print(f"docs/RUST_API.md  — {rc} methods")
    print(f"docs/PYTHON_API.md — {pc} methods")


if __name__ == "__main__":
    main()
