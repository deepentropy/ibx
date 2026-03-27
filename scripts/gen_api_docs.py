#!/usr/bin/env python3
"""Generate rich API reference docs from Rust source files.

Parses pub fn signatures, doc comments, and parameter types from
src/api/client/*.rs, Wrapper trait, and Python pymethods.
Outputs docs/RUST_API.md and docs/PYTHON_API.md with per-method
signature blocks and parameter tables.

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

FILE_ORDER = ["mod", "account", "orders", "market_data", "reference", "stubs"]
SECTION_NAMES = {
    "mod": "Connection",
    "account": "Account & Portfolio",
    "orders": "Orders",
    "market_data": "Market Data",
    "reference": "Reference Data",
    "stubs": "Gateway-Local & Stubs",
    "dispatch": None, "tests": None, "test_helpers": None,
}

# ── Well-known parameter descriptions ──

PARAM_DOCS: dict[str, str] = {
    "req_id": "Request identifier. Used to match responses to requests.",
    "order_id": "Order identifier. Must be unique per session.",
    "contract": "Contract specification (symbol, secType, exchange, currency, etc.).",
    "order": "Order parameters (action, quantity, type, price, TIF, etc.).",
    "wrapper": "Wrapper callback receiver for synchronous delivery.",
    "subscribe": "`true` to start updates, `false` to stop.",
    "snapshot": "If `true`, delivers one quote then auto-cancels.",
    "generic_tick_list": "Comma-separated generic tick IDs (e.g. `\"233\"` for RT volume).",
    "what_to_show": "Data type: `\"TRADES\"`, `\"MIDPOINT\"`, `\"BID\"`, `\"ASK\"`, `\"BID_ASK\"`, etc.",
    "use_rth": "If `true`, only return data from Regular Trading Hours.",
    "end_date_time": "End date/time in `\"YYYYMMDD HH:MM:SS\"` format, or empty for now.",
    "duration": "Duration string, e.g. `\"1 D\"`, `\"1 W\"`, `\"1 M\"`, `\"1 Y\"`.",
    "duration_str": "Duration string, e.g. `\"1 D\"`, `\"1 W\"`, `\"1 M\"`, `\"1 Y\"`.",
    "bar_size": "Bar size: `\"1 min\"`, `\"5 mins\"`, `\"1 hour\"`, `\"1 day\"`, etc.",
    "bar_size_setting": "Bar size: `\"1 min\"`, `\"5 mins\"`, `\"1 hour\"`, `\"1 day\"`, etc.",
    "tick_type": "Tick-by-tick type: `\"BidAsk\"` or `\"Last\"`.",
    "num_rows": "Number of order book rows to subscribe to.",
    "is_smart_depth": "If `true`, aggregate depth from multiple exchanges via SMART.",
    "market_data_type": "1=live, 2=frozen, 3=delayed, 4=delayed-frozen.",
    "market_rule_id": "Market rule ID (from contract details).",
    "con_id": "Contract ID. Unique per instrument.",
    "provider_codes": "Pipe-separated news provider codes (e.g. `\"BRFG+DJ-N\"`).",
    "provider_code": "News provider code (e.g. `\"BRFG\"`).",
    "article_id": "News article identifier.",
    "start_time": "Start date/time for news query.",
    "end_time": "End date/time for news query.",
    "start_date_time": "Start date/time for tick query.",
    "number_of_ticks": "Maximum number of ticks to return.",
    "max_results": "Maximum number of results.",
    "max_items": "Maximum number of scanner results.",
    "report_type": "Report type: `\"ReportSnapshot\"`, `\"ReportsFinSummary\"`, `\"RESC\"`, etc.",
    "instrument": "Instrument type for scanner (e.g. `\"STK\"`, `\"FUT\"`).",
    "instrument_id": "Internal instrument ID (dense, 0..256).",
    "location_code": "Scanner location (e.g. `\"STK.US.MAJOR\"`).",
    "scan_code": "Scanner code (e.g. `\"TOP_PERC_GAIN\"`, `\"HIGH_OPT_IMP_VOLAT\"`).",
    "pattern": "Symbol search pattern.",
    "period": "Histogram period, e.g. `\"1week\"`, `\"1month\"`.",
    "tags": "Comma-separated account tags: `\"NetLiquidation,BuyingPower,...\"`.",
    "all_msgs": "If `true`, receive all existing bulletins on subscribe.",
    "log_level": "Log level: 1=error, 2=warn, 3=info, 4=debug, 5=trace.",
    "filter": "Execution filter (client_id, acct_code, time, symbol, sec_type, exchange, side).",
    "b_auto_bind": "If `true`, auto-bind future orders to this client.",
    "bbo_exchange": "BBO exchange for smart component lookup (e.g. `\"SMART\"`).",
    "acct_code": "Account code (e.g. `\"DU1234567\"`).",
    "account": "Account ID.",
    "model_code": "Model portfolio code (empty for default).",
    "group_name": "Account group name (e.g. `\"All\"`).",
    "manual_order_cancel_time": "Manual cancel time (empty for immediate).",
    "config": "Connection configuration (username, password, host, paper, core_id).",
    "host": "Server hostname.",
    "port": "Port number (unused — ibx connects directly).",
    "client_id": "Client ID (unused — single-client engine).",
    "username": "Account username.",
    "password": "Account password.",
    "paper": "If `true`, connect to paper trading.",
    "core_id": "CPU core affinity for the hot loop thread.",
    "fa_data_type": "FA data type (1=Groups, 2=Profiles, 3=Aliases).",
    "cxml": "FA XML configuration data.",
    "group_id": "Display group ID.",
    "contract_info": "Display group contract info string.",
    "ledger_and_nlv": "If `true`, include ledger and NLV data.",
    "regulatory_snapshot": "If `true`, request a regulatory snapshot (additional fees may apply).",
    "format_date": "Date format: 1=`\"YYYYMMDD HH:MM:SS\"`, 2=Unix seconds.",
    "keep_up_to_date": "If `true`, continue receiving updates after initial history.",
    "option_price": "Option market price.",
    "under_price": "Underlying asset price.",
    "volatility": "Implied volatility.",
    "exercise_action": "1=exercise, 2=lapse.",
    "exercise_quantity": "Number of contracts to exercise.",
    "underlying_symbol": "Underlying symbol (e.g. `\"AAPL\"`).",
    "fut_fop_exchange": "Exchange for futures/FOP options.",
    "underlying_sec_type": "Underlying security type (e.g. `\"STK\"`).",
    "underlying_con_id": "Underlying contract ID.",
    # Wrapper callback params
    "status": "Order status string (`\"Submitted\"`, `\"Filled\"`, `\"Cancelled\"`, etc.).",
    "filled": "Cumulative filled quantity.",
    "remaining": "Remaining quantity.",
    "avg_fill_price": "Average fill price.",
    "perm_id": "Permanent order ID assigned by the server.",
    "parent_id": "Parent order ID (0 if no parent).",
    "last_fill_price": "Price of the last fill.",
    "why_held": "Reason the order is held (e.g. `\"locate\"`).",
    "mkt_cap_price": "Market cap price for the order.",
    "order_state": "Order state (status, margin, commission info).",
    "execution": "Execution details (exec_id, time, price, shares, etc.).",
    "report": "Commission report (exec_id, commission, currency, realized P&L).",
    "key": "Account value key (e.g. `\"NetLiquidation\"`, `\"BuyingPower\"`).",
    "value": "Account value.",
    "currency": "Currency code (e.g. `\"USD\"`).",
    "account_name": "Account identifier.",
    "market_price": "Current market price.",
    "market_value": "Current market value of position.",
    "average_cost": "Average cost basis.",
    "unrealized_pnl": "Unrealized profit/loss.",
    "realized_pnl": "Realized profit/loss.",
    "timestamp": "Timestamp string.",
    "pos": "Position size (decimal shares).",
    "avg_cost": "Average cost per share.",
    "daily_pnl": "Daily profit/loss.",
    "bar": "Bar data (date, open, high, low, close, volume, wap, bar_count).",
    "start": "Period start date/time.",
    "end": "Period end date/time.",
    "head_timestamp": "Earliest available data timestamp string.",
    "details": "Contract details object.",
    "descriptions": "Array of matching contract descriptions.",
    "time": "Tick timestamp (Unix seconds).",
    "price": "Tick price.",
    "size": "Tick size.",
    "attrib": "Tick attributes.",
    "exchange": "Exchange name.",
    "special_conditions": "Special trade conditions.",
    "bid_price": "Bid price.",
    "ask_price": "Ask price.",
    "bid_size": "Bid size.",
    "ask_size": "Ask size.",
    "mid_point": "Midpoint price.",
    "rank": "Scanner result rank (0-based).",
    "distance": "Scanner distance metric.",
    "benchmark": "Scanner benchmark.",
    "projection": "Scanner projection.",
    "legs_str": "Combo legs description.",
    "xml": "XML string.",
    "msg_id": "Bulletin message ID.",
    "msg_type": "Bulletin message type (1=regular, 2=exchange).",
    "message": "Bulletin message text.",
    "orig_exchange": "Originating exchange.",
    "ticker_id": "Ticker/request ID.",
    "provider_codes": "Pipe-separated news provider codes.",
    "headline": "News headline text.",
    "has_more": "If `true`, more results available.",
    "article_type": "Article type: 0=plain text, 1=HTML.",
    "article_text": "Full article body.",
    "date": "Bar date string.",
    "open": "Open price.",
    "high": "High price.",
    "low": "Low price.",
    "close": "Close price.",
    "volume": "Volume.",
    "wap": "Volume-weighted average price.",
    "count": "Trade count.",
    "items": "Histogram entries `[(price, count)]`.",
    "price_increments": "Price increment rules `[{low_edge, increment}]`.",
    "components": "Smart routing component exchanges.",
    "providers": "News provider list.",
    "tiers": "Soft dollar tier list.",
    "codes": "Family code list.",
    "white_branding_id": "White branding ID (empty for standard accounts).",
    "ticks": "Historical tick data.",
    "done": "If `true`, all ticks have been delivered.",
    "sessions": "Trading sessions `[(ref_date, open, close)]`.",
    "time_zone": "Timezone string (e.g. `\"US/Eastern\"`).",
    "data": "Raw data string (XML/JSON).",
    "min_tick": "Minimum tick size.",
    "snapshot_permissions": "Snapshot permissions bitmask.",
    "tick_type": "Tick type ID or tick-by-tick type string.",
    "error_code": "Error code.",
    "error_string": "Error message.",
    "advanced_order_reject_json": "JSON with advanced rejection details.",
    "accounts_list": "Comma-separated account IDs.",
    "position": "Book position (row index) or position size.",
    "operation": "Book operation: 0=insert, 1=update, 2=delete.",
    "side": "Book side: 0=ask, 1=bid. Or order side `\"BOT\"`/`\"SLD\"`.",
    "market_maker": "Market maker ID.",
    "market_rule_id": "Market rule ID.",
    "_descriptions": "Depth exchange descriptions.",
    # Misc params
    "group": "Account group name (e.g. `\"All\"`).",
    "tag": "Account tag name (e.g. `\"NetLiquidation\"`).",
    "strategy": "Algo strategy name (e.g. `\"Vwap\"`, `\"Twap\"`).",
    "params": "Algo parameter list.",
    "ignore_size": "If `true`, ignore size in tick-by-tick data.",
    "shared": "Shared state handle.",
    "handle": "Background thread handle.",
    "control_tx": "Control channel sender.",
    "extra_data": "Additional tick data.",
    "num_ids": "Number of IDs to reserve (unused).",
    "subscription": "Scanner subscription parameters.",
    "total_results": "Maximum number of news results.",
    "time_period": "Histogram time period.",
    "override": "Override flag for exercise.",
    "multiplier": "Contract multiplier.",
    "trading_class": "Trading class.",
    "delta": "Option delta.",
    "gamma": "Option gamma.",
    "theta": "Option theta.",
    "vega": "Option vega.",
    "implied_vol": "Implied volatility.",
    "opt_price": "Option theoretical price.",
    "und_price": "Underlying price.",
    "pv_dividend": "Present value of dividends.",
    "strikes": "Available strike prices.",
    "expirations": "Available expiration dates.",
    "text": "Informational text.",
    "groups": "FA group definitions.",
    "time_stamp": "Timestamp string.",
}

# Rust type → Python type display
RUST_TO_PY_TYPE = {
    "i64": "int", "i32": "int", "u32": "int", "u64": "int",
    "f64": "float", "bool": "bool",
    "&str": "str", "String": "str", "&String": "str",
    "&Contract": "Contract", "&Order": "Order",
    "&ExecutionFilter": "ExecutionFilter",
    "&mut impl Wrapper": "Wrapper",
    "InstrumentId": "int",
}

# ── Fallback method descriptions ──

KNOWN_DESCRIPTIONS: dict[str, str] = {
    "connect_ack": "Connection acknowledged.",
    "connection_closed": "Connection has been closed.",
    "next_valid_id": "Next valid order ID from the server.",
    "managed_accounts": "Comma-separated list of managed account IDs.",
    "error": "Error or informational message from the server.",
    "current_time": "Current server time (Unix seconds).",
    "is_connected": "Check if the client is connected.",
    "new": "Create a new EClient (or EWrapper) instance.",
    "tick_price": "Price tick update (bid, ask, last, etc.).",
    "tick_size": "Size tick update (bid size, ask size, volume, etc.).",
    "tick_string": "String tick (e.g. last trade timestamp).",
    "tick_generic": "Generic numeric tick value.",
    "tick_snapshot_end": "Snapshot delivery complete; subscription auto-cancelled.",
    "market_data_type": "Market data type changed (1=live, 2=frozen, 3=delayed, 4=delayed-frozen).",
    "tick_req_params": "Tick parameters: min tick size, BBO exchange, snapshot permissions.",
    "order_status": "Order status update (filled, remaining, avg price, etc.).",
    "open_order": "Open order details (contract, order, state).",
    "open_order_end": "End of open orders list.",
    "exec_details": "Execution fill details.",
    "exec_details_end": "End of execution details list.",
    "commission_report": "Commission report for an execution.",
    "completed_order": "Completed (filled/cancelled) order details.",
    "completed_orders_end": "End of completed orders list.",
    "order_bound": "Order bound to a perm ID.",
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
    "historical_data": "Historical OHLCV bar.",
    "historical_data_end": "End of historical data delivery.",
    "historical_data_update": "Real-time bar update (keep_up_to_date=true).",
    "head_timestamp": "Earliest available data timestamp.",
    "contract_details": "Contract definition details.",
    "contract_details_end": "End of contract details.",
    "symbol_samples": "Matching symbol search results.",
    "tick_by_tick_all_last": "Tick-by-tick last trade.",
    "tick_by_tick_bid_ask": "Tick-by-tick bid/ask quote.",
    "tick_by_tick_mid_point": "Tick-by-tick midpoint.",
    "scanner_data": "Scanner result entry (rank, contract, distance).",
    "scanner_data_end": "End of scanner results.",
    "scanner_parameters": "Scanner parameters XML.",
    "update_news_bulletin": "News bulletin message.",
    "tick_news": "Per-contract news tick.",
    "historical_news": "Historical news headline.",
    "historical_news_end": "End of historical news.",
    "news_article": "Full news article text.",
    "news_providers": "Available news providers list.",
    "real_time_bar": "Real-time 5-second OHLCV bar.",
    "historical_ticks": "Historical tick data (Last, BidAsk, or Midpoint).",
    "historical_ticks_bid_ask": "Historical bid/ask ticks.",
    "historical_ticks_last": "Historical last-trade ticks.",
    "histogram_data": "Price distribution histogram.",
    "market_rule": "Market rule: price increment schedule.",
    "historical_schedule": "Historical trading schedule (exchange hours).",
    "fundamental_data": "Fundamental data (XML/JSON).",
    "update_mkt_depth": "L2 book update (single exchange).",
    "update_mkt_depth_l2": "L2 book update (with market maker).",
    "mkt_depth_exchanges": "Available exchanges for market depth.",
    "smart_components": "SMART routing component exchanges.",
    "soft_dollar_tiers": "Soft dollar tier list.",
    "family_codes": "Family codes linking related accounts.",
    "user_info": "User info (white branding ID).",
    "tick_option_computation": "Option implied vol / greeks computation.",
    "security_definition_option_parameter": "Option chain parameters (strikes, expirations).",
    "security_definition_option_parameter_end": "End of option chain parameters.",
    "receive_fa": "Financial advisor data received.",
    "replace_fa_end": "Financial advisor replace complete.",
    "position_multi": "Multi-account position entry.",
    "position_multi_end": "End of multi-account positions.",
    "account_update_multi": "Multi-account value update.",
    "account_update_multi_end": "End of multi-account updates.",
    "display_group_list": "Display group list.",
    "display_group_updated": "Display group updated.",
    "wsh_meta_data": "Wall Street Horizon metadata.",
    "wsh_event_data": "Wall Street Horizon event data.",
    "bond_contract_details": "Bond contract details.",
    "delta_neutral_validation": "Delta-neutral validation response.",
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


def version() -> str:
    with open(ROOT / "Cargo.toml", "rb") as f:
        return tomllib.load(f)["package"]["version"]


# ── Parameter parsing ──

def parse_rust_params(args_str: str) -> list[dict]:
    """Parse Rust fn args into [{name, type}], skipping &self/&mut self."""
    params = []
    # Split carefully (handles nested generics/parens)
    depth = 0
    current = []
    for ch in args_str:
        if ch in ('(', '<', '['):
            depth += 1
            current.append(ch)
        elif ch in (')', '>', ']'):
            depth -= 1
            current.append(ch)
        elif ch == ',' and depth == 0:
            params.append("".join(current).strip())
            current = []
        else:
            current.append(ch)
    if current:
        params.append("".join(current).strip())

    result = []
    for p in params:
        p = p.strip()
        if not p or p in ("&self", "&mut self"):
            continue
        m = re.match(r'_?(\w+)\s*:\s*(.+)', p)
        if m:
            name = m.group(1)
            ty = m.group(2).strip()
            result.append({"name": name, "type": ty})
    return result


def param_description(name: str, ty: str = "") -> str:
    """Get description for a parameter by name, falling back to type-based inference."""
    # Strip leading underscore
    clean = name.lstrip("_")
    if clean in PARAM_DOCS:
        return PARAM_DOCS[clean]
    # Type-based inference
    if "Contract" in ty:
        return PARAM_DOCS.get("contract", "Contract specification.")
    if "Order" in ty and "order_id" not in clean:
        return PARAM_DOCS.get("order", "Order parameters.")
    if "Wrapper" in ty:
        return "Callback receiver."
    if "ExecutionFilter" in ty:
        return PARAM_DOCS.get("filter", "Execution filter.")
    return ""


def rust_type_to_py(ty: str) -> str:
    """Convert Rust type to Python display type."""
    ty = ty.strip()
    for rust, py in RUST_TO_PY_TYPE.items():
        if ty == rust:
            return py
    if ty.startswith("&[") or "Vec<" in ty:
        return "list"
    if ty.startswith("Option<"):
        inner = ty[7:-1]
        return f"{rust_type_to_py(inner)} or None"
    if ty.startswith("&"):
        return rust_type_to_py(ty[1:])
    if ty.startswith("impl "):
        return ty[5:]
    return ty


# ── Rust parser ──

def parse_rust_methods(path: Path) -> list[dict]:
    """Extract pub fn methods with doc comments and parsed parameters."""
    text = path.read_text(encoding="utf-8")
    results = []
    for m in re.finditer(
        r'((?:\s*///[^\n]*\n)*)(?:\s*#\[[^\]]*\]\s*\n)*\s*pub fn (\w+)\s*\(([^)]*(?:\([^)]*\)[^)]*)*)\)([^{;]*)',
        text,
    ):
        doc_block, name, args_str, ret_str = m.group(1), m.group(2), m.group(3), m.group(4)
        doc_lines = []
        for line in doc_block.strip().splitlines():
            line = line.strip().removeprefix("///").strip()
            if line:
                doc_lines.append(line)
        doc = " ".join(doc_lines)
        doc = re.sub(r"\s*Matches `[^`]+` in C\+\+\.?", "", doc)

        # Parse return type
        ret_m = re.search(r'->\s*(.+)', ret_str)
        ret_type = ret_m.group(1).strip() if ret_m else ""

        # Parse parameters
        params = parse_rust_params(args_str)

        # Build clean signature (remove doc comments, attributes, collapse whitespace)
        full_line = m.group(0).strip()
        sig = re.sub(r'\s*\{.*', '', full_line).strip()
        sig = re.sub(r'///[^\n]*\n?', '', sig)
        sig = re.sub(r'#\[[^\]]*\]\s*', '', sig)
        sig = re.sub(r'\s+', ' ', sig).strip()

        results.append({
            "name": name, "signature": sig, "doc": doc,
            "params": params, "return_type": ret_type,
        })
    return results


def parse_wrapper_trait(path: Path) -> list[dict]:
    """Extract fn methods from the Wrapper trait."""
    text = path.read_text(encoding="utf-8")
    trait_m = re.search(r'pub trait Wrapper\s*\{(.*?)\n\}', text, re.DOTALL)
    if not trait_m:
        return []
    body = trait_m.group(1)
    results = []
    for m in re.finditer(
        r'((?:\s*//[^\n]*\n)*)\s*fn (\w+)\s*\(([^)]*(?:\([^)]*\)[^)]*)*)\)([^{}]*)',
        body,
    ):
        comment_block, name, args_str, _ret = m.group(1), m.group(2), m.group(3), m.group(4)
        doc_lines = []
        for line in comment_block.strip().splitlines():
            line = line.strip()
            if line.startswith("///"):
                doc_lines.append(line.removeprefix("///").strip())
        doc = " ".join(doc_lines)
        params = parse_rust_params(args_str)
        results.append({"name": name, "doc": doc, "params": params, "return_type": "", "signature": ""})
    return results


# ── Python parser ──

def parse_pymethods(path: Path) -> list[dict]:
    """Extract fn methods from all #[pymethods] impl blocks."""
    text = path.read_text(encoding="utf-8")
    results = []
    blocks = re.split(r'#\[pymethods\]', text)
    for block in blocks[1:]:
        impl_m = re.match(r'\s*impl\s+\w+\s*\{', block)
        if not impl_m:
            continue
        start = impl_m.end() - 1
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
        for fm in re.finditer(
            r'((?:\s*(?:///|//)[^\n]*\n|\s*#\[pyo3[^\]]*\]\s*\n)*)\s*fn (\w+)\s*\(([^)]*(?:\([^)]*\)[^)]*)*)\)',
            impl_body,
        ):
            preamble, name, args_str = fm.group(1), fm.group(2), fm.group(3)
            if name.startswith("__"):
                continue
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
            params = parse_rust_params(args_str)
            # Filter out &self, py: Python
            params = [p for p in params if p["name"] != "py" and "Python" not in p.get("type", "")]
            py_sig = _build_py_sig(name, args_str, pyo3_sig)
            results.append({
                "name": name, "signature": py_sig, "doc": doc,
                "params": params, "return_type": "",
            })
    return results


def _build_py_sig(name: str, rust_args: str, pyo3_sig: str) -> str:
    if pyo3_sig:
        m = re.search(r'signature\s*=\s*\((.+)\)', pyo3_sig)
        if m:
            return f"{name}({m.group(1)})"
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
    return parse_pymethods(path)


# ── Enrichment ──

def enrich(m: dict) -> dict:
    if not m["doc"] and m["name"] in KNOWN_DESCRIPTIONS:
        m = {**m, "doc": KNOWN_DESCRIPTIONS[m["name"]]}
    return m


# ── Markdown rendering ──

def render_method_rust(m: dict) -> list[str]:
    """Render a single Rust method as markdown."""
    m = enrich(m)
    out = []
    out.append(f"#### `{m['name']}`")
    out.append("")
    if m["doc"]:
        out.append(m["doc"])
        out.append("")
    if m.get("signature"):
        out.append("```rust")
        out.append(m["signature"])
        out.append("```")
        out.append("")
    params = m.get("params", [])
    if params:
        out.append("| Parameter | Type | Description |")
        out.append("|-----------|------|-------------|")
        for p in params:
            desc = param_description(p["name"], p.get("type", ""))
            ty = f"`{p['type']}`" if p.get("type") else ""
            out.append(f"| `{p['name']}` | {ty} | {desc} |")
        out.append("")
    ret = m.get("return_type", "")
    if ret and ret not in ("()",):
        out.append(f"**Returns:** `{ret}`")
        out.append("")
    out.append("---")
    out.append("")
    return out


def render_method_python(m: dict) -> list[str]:
    """Render a single Python method as markdown."""
    m = enrich(m)
    out = []
    out.append(f"#### `{m['name']}`")
    out.append("")
    if m["doc"]:
        out.append(m["doc"])
        out.append("")
    if m.get("signature"):
        out.append("```python")
        out.append(f"def {m['signature']}")
        out.append("```")
        out.append("")
    params = m.get("params", [])
    if params:
        out.append("| Parameter | Type | Description |")
        out.append("|-----------|------|-------------|")
        for p in params:
            desc = param_description(p["name"], p.get("type", ""))
            py_type = rust_type_to_py(p.get("type", "")) if p.get("type") else ""
            ty = f"`{py_type}`" if py_type else ""
            out.append(f"| `{p['name']}` | {ty} | {desc} |")
        out.append("")
    out.append("---")
    out.append("")
    return out


def render_callback(m: dict, python: bool = False) -> list[str]:
    """Render a wrapper callback."""
    m = enrich(m)
    out = []
    out.append(f"#### `{m['name']}`")
    out.append("")
    if m["doc"]:
        out.append(m["doc"])
        out.append("")
    params = m.get("params", [])
    if params:
        out.append("| Parameter | Type | Description |")
        out.append("|-----------|------|-------------|")
        for p in params:
            desc = param_description(p["name"], p.get("type", ""))
            if python:
                ty = rust_type_to_py(p.get("type", ""))
                ty = f"`{ty}`" if ty else ""
            else:
                ty = f"`{p['type']}`" if p.get("type") else ""
            out.append(f"| `{p['name']}` | {ty} | {desc} |")
        out.append("")
    out.append("---")
    out.append("")
    return out


def generate_rust_md(ver: str) -> str:
    out = [
        f"# Rust API Reference (v{ver})",
        "",
        "*Auto-generated from source — do not edit.*",
        "",
        "## Table of Contents",
        "",
    ]
    # Build TOC
    toc_items = []
    for stem in FILE_ORDER:
        section = SECTION_NAMES.get(stem)
        if section is None:
            continue
        anchor = section.lower().replace(" & ", "--").replace(" ", "-")
        toc_items.append(f"- [EClient: {section}](#{anchor})")
    toc_items.append("- [Wrapper Callbacks](#wrapper-callbacks)")
    out.extend(toc_items)
    out.append("")

    # EClient methods
    method_count = 0
    for stem in FILE_ORDER:
        fname = RUST_CLIENT / f"{stem}.rs"
        if not fname.exists():
            continue
        section = SECTION_NAMES.get(stem)
        if section is None:
            continue
        methods = parse_rust_methods(fname)
        if not methods:
            continue
        out.append(f"## {section}")
        out.append("")
        for m in methods:
            out.extend(render_method_rust(m))
            method_count += 1

    # Wrapper
    out.append("## Wrapper Callbacks")
    out.append("")
    wm = parse_wrapper_trait(RUST_WRAPPER)
    for m in wm:
        out.extend(render_callback(m))
        method_count += 1

    return "\n".join(out), method_count


def generate_python_md(ver: str) -> str:
    out = [
        f"# Python API Reference (v{ver})",
        "",
        "*Auto-generated from source — do not edit.*",
        "",
        "## Table of Contents",
        "",
    ]
    toc_items = []
    for stem in FILE_ORDER:
        section = SECTION_NAMES.get(stem)
        if section is None:
            continue
        anchor = section.lower().replace(" & ", "--").replace(" ", "-")
        toc_items.append(f"- [EClient: {section}](#{anchor})")
    toc_items.append("- [EWrapper Callbacks](#ewrapper-callbacks)")
    out.extend(toc_items)
    out.append("")

    method_count = 0
    for stem in FILE_ORDER:
        fname = PY_CLIENT / f"{stem}.rs"
        if not fname.exists():
            continue
        section = SECTION_NAMES.get(stem)
        if section is None:
            continue
        methods = parse_pymethods(fname)
        if not methods:
            continue
        out.append(f"## {section}")
        out.append("")
        for m in methods:
            out.extend(render_method_python(m))
            method_count += 1

    # Wrapper
    out.append("## EWrapper Callbacks")
    out.append("")
    wm = parse_py_wrapper(PY_WRAPPER)
    for m in wm:
        out.extend(render_callback(m, python=True))
        method_count += 1

    return "\n".join(out), method_count


def main():
    DOCS.mkdir(exist_ok=True)
    ver = version()
    rust, rc = generate_rust_md(ver)
    py, pc = generate_python_md(ver)
    (DOCS / "RUST_API.md").write_text(rust, encoding="utf-8")
    (DOCS / "PYTHON_API.md").write_text(py, encoding="utf-8")
    print(f"docs/RUST_API.md  — {rc} methods")
    print(f"docs/PYTHON_API.md — {pc} methods")


if __name__ == "__main__":
    main()
