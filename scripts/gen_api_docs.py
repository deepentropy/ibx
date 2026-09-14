#!/usr/bin/env python3
"""Generate rich API reference docs from Rust source files.

Parses pub fn signatures, doc comments, and parameter types from
src/api/client/*.rs, Wrapper trait, and Python pymethods.
Outputs the per-method signature blocks and parameter tables that
the mdBook chapters in docs/book/ include via {{#include}}.

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
BOOK_SRC = ROOT / "docs" / "book" / "src"
RUST_REF = BOOK_SRC / "api" / "rust-reference.md"
PYTHON_REF = BOOK_SRC / "api" / "python-reference.md"
COVERAGE_REF = BOOK_SRC / "reference" / "coverage-data.md"

FILE_ORDER = ["mod", "ask", "account", "orders", "market_data", "reference", "stubs"]
SECTION_NAMES = {
    "mod": "Connection",
    "ask": "Calls That Answer",
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
    "clientId": "`client_id` under the reference client's spelling. Give one or the other.",
    "opts": "Connect options, as the reference client takes them. Not applied.",
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
    "filters": "Scanner filter tags from `req_scanner_parameters`, e.g. `priceAbove` = `\"10\"`.",
    "scanner_subscription_filter_options": "Scanner filter tags from `req_scanner_parameters`, e.g. `priceAbove` = `\"10\"`.",
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
    "ib_key_timeout_secs": "Live second-factor approval timeout in seconds (default ~18 min). Lower it to fail fast on unattended live logins; ignored for paper.",
    "ib_key_token_sub_type": "Fallback second-factor token sub-type (default `\"2a\"`), used only when the server states none for the session; ignored for paper.",
    "code_provider": "Callable `(factor, display_id, avth_url) -> str` returning the second-factor code. `factor` is `\"ibkey\"` (the code shown for `display_id`) or `\"authenticator\"` (the account's current code). Required for authenticator accounts, which have no push to fall back to; ignored for paper.",
    "host": "Server hostname.",
    "port": "Port number (unused — ibx connects directly).",
    "client_id": "Client ID (unused — single-client engine).",
    "username": "Account username.",
    "password": "Account password.",
    "paper": "If `true`, connect to paper trading. If `false`, connect blocks on the live second-factor approval window (see method note).",
    "core_id": "CPU core affinity for the hot loop thread. Use a distinct value per engine when running several in one process.",
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
    "current_time_in_millis": "Current server time (Unix milliseconds).",
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
    "commission_and_fees_report": "Commission and fees report for an execution.",
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
    "exercise_options": "Exercise or lapse a long option position.",
    "req_sec_def_opt_params": "Request the expirations and strikes an underlying's options list.",
    "cancel_mkt_data": "Cancel market data subscription.",
    "req_news_bulletins": "Subscribe to news bulletins.",
    "cancel_news_bulletins": "Cancel news bulletin subscription.",
    "req_current_time": "Request current server time.",
    "req_current_time_in_millis": "Request current server time in milliseconds.",
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

def plain_intra_doc_links(doc: str) -> str:
    """Drop the link, keep the words.

    A doc comment links to a neighbouring item by its path, which the tool that
    renders the crate's own documentation resolves. Markdown does not: the same
    text becomes a link to a relative path that is not there, and a reader who
    clicks it lands on nothing. The name is already the link text, and this page
    lists every item, so the words alone say the same thing and go nowhere
    wrong.
    """
    return re.sub(r"\[([^\]]+)\]\((?:Self|crate|[A-Z][A-Za-z0-9_]*)(?:::[A-Za-z0-9_]+)+\)", r"\1", doc)


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
        p = re.sub(r"^#\[[^\]]*\]\s*", "", p.strip())
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

def eclient_impls(text: str) -> str:
    """Just the `impl EClient` blocks, where a file holds more than its own.

    A file may define a type of its own beside the client — a report with an
    `is_done` on it — and those methods belong to that type, not to `EClient`.
    Scanning the file whole would file them under the client, which is where a
    reader would then look for them and not find them.
    """
    if "impl EClient" not in text:
        return text
    out = []
    i = 0
    while (i := text.find("impl EClient", i)) != -1:
        j = text.find("{", i)
        if j == -1:
            break
        depth = 0
        for k in range(j, len(text)):
            if text[k] == "{":
                depth += 1
            elif text[k] == "}":
                depth -= 1
                if depth == 0:
                    out.append(text[i:k + 1])
                    i = k + 1
                    break
        else:
            break
    return "\n".join(out)


def parse_rust_methods(path: Path) -> list[dict]:
    """Extract pub fn methods with doc comments and parsed parameters."""
    text = eclient_impls(path.read_text(encoding="utf-8"))
    results = []
    for m in re.finditer(
        r'((?:\s*///[^\n]*\n)*)((?:\s*#\[[^\]]*\]\s*\n)*)\s*pub fn (\w+)\s*\(([^)]*(?:\([^)]*\)[^)]*)*)\)([^{;]*)',
        text,
    ):
        doc_block, attrs = m.group(1), m.group(2)
        name, args_str, ret_str = m.group(3), m.group(4), m.group(5)
        # What the compiler's own documentation leaves out. These are hooks a
        # test reaches for, callable and not part of the surface — published
        # here, they read as capabilities this client offers and a page built
        # from this one listed them beside the calls a program is written
        # against.
        if "doc(hidden)" in attrs:
            continue
        doc_lines = []
        for line in doc_block.strip().splitlines():
            line = line.strip().removeprefix("///").strip()
            if line:
                doc_lines.append(line)
        doc = " ".join(doc_lines)
        doc = re.sub(r"\s*Matches `[^`]+` in C\+\+\.?", "", doc)
        doc = plain_intra_doc_links(doc)

        # Parse return type. A signature wide enough to wrap carries its return
        # across lines, so read it as one before looking for the arrow.
        ret_m = re.search(r'->\s*(.+)', re.sub(r'\s+', ' ', ret_str))
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
            # Any attribute, not only pyo3's. A lint allowance between the
            # signature and the function used to end the match, so a method
            # carrying one was published under whatever the page already said
            # rather than under what it takes.
            # An attribute runs to the end of its line. Matched up to the
            # first `]` instead, a signature naming an indexed constant —
            # `CCP_HOSTS[0]` — ended the attribute early, the signature was
            # never captured, and the method was published under the names of
            # its Rust arguments instead of the ones a caller passes.
            r'((?:\s*(?:///|//)[^\n]*\n|\s*#\[[^\n]*\n)*)\s*fn (\w+)\s*\(([^)]*(?:\([^)]*\)[^)]*)*)\)',
            impl_body,
        ):
            preamble, name, args_str = fm.group(1), fm.group(2), fm.group(3)
            # `__init__` is the constructor a caller reaches as `EClient(wrapper)`.
            if name.startswith("__") and name != "__init__":
                continue
            doc_lines = []
            pyo3_sig = ""
            attribute = False
            for line in preamble.strip().splitlines():
                line = line.strip()
                if line.startswith("///"):
                    doc_lines.append(line.removeprefix("///").strip())
                elif line.startswith("#[pyo3(signature"):
                    pyo3_sig = line
                elif line.startswith("#[getter]"):
                    attribute = True
            doc = " ".join(doc_lines)
            doc = re.sub(r"\s*Matches `[^`]+` in C\+\+\.?", "", doc)
            doc = plain_intra_doc_links(doc)
            params = parse_rust_params(args_str)
            # Filter out &self, py: Python
            params = [p for p in params if p["name"] != "py" and "Python" not in p.get("type", "")]
            py_sig = _build_py_sig(name, args_str, pyo3_sig)
            results.append({
                "name": name, "signature": py_sig, "doc": doc,
                "params": params, "return_type": "", "attribute": attribute,
            })
    return results


#: Rust spellings of a default, and what a caller writing Python passes.
_DEFAULTS_IN_PYTHON = {
    "true": "True",
    "false": "False",
    "None": "None",
    "Vec::new()": "[]",
    '"".to_string()': '""',
}


def _default_in_python(value: str) -> str | None:
    """The default as a caller would write it, or nothing where it is not a
    value a caller can write.

    A default built by calling something — a constant looked up, a string
    allocated — has no Python spelling, and printing the Rust for it in a
    Python signature tells a reader to type something that is not Python.
    Those are shown as taking a default without saying what it is.
    """
    value = value.strip()
    if value in _DEFAULTS_IN_PYTHON:
        return _DEFAULTS_IN_PYTHON[value]
    if value.endswith('.to_string()') and value.startswith('"'):
        return value[: -len(".to_string()")]
    if re.fullmatch(r'-?\d+(\.\d+)?', value) or re.fullmatch(r'"[^"]*"', value):
        return value
    return None


def _build_py_sig(name: str, rust_args: str, pyo3_sig: str) -> str:
    if pyo3_sig:
        m = re.search(r'signature\s*=\s*\((.+)\)', pyo3_sig)
        if m:
            # The capture is greedy to the last paren on the line, which is the
            # attribute's own. Trimmed until the parens balance, so the final
            # parameter keeps its default instead of carrying a bracket into it.
            raw = m.group(1)
            while raw.count("(") < raw.count(")") and ")" in raw:
                raw = raw[: raw.rfind(")")]
            shown = []
            for arg in _split_top_level(raw):
                if "=" not in arg:
                    shown.append(arg.strip())
                    continue
                param, default = arg.split("=", 1)
                as_python = _default_in_python(default)
                shown.append(
                    f"{param.strip()}={as_python}" if as_python is not None
                    else param.strip()
                )
            return f"{name}({', '.join(shown)})"
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


def _split_top_level(args: str) -> list[str]:
    """Split on the commas between parameters, not the ones inside a default."""
    out, depth, current = [], 0, ""
    for ch in args:
        if ch in "([<":
            depth += 1
        elif ch in ")]>":
            depth -= 1
        if ch == "," and depth == 0:
            out.append(current)
            current = ""
            continue
        current += ch
    if current.strip():
        out.append(current)
    return out


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
    if m.get("attribute"):
        out.append("```python")
        out.append(f"client.{m['name']}  # read-only attribute")
        out.append("```")
        out.append("")
    elif m.get("signature"):
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
        methods += reexported_into(stem)
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


# ── IB API Coverage Matrix ──

# Canonical ibapi EClient methods (source of truth).
# Category, ibapi method name, ibapi C++ name (for reference).
IBAPI_ECLIENT: list[tuple[str, str, str]] = [
    # Connection
    ("Connection", "connect", "eConnect"),
    ("Connection", "disconnect", "eDisconnect"),
    ("Connection", "is_connected", "isConnected"),
    ("Connection", "set_server_log_level", "setServerLogLevel"),
    ("Connection", "req_current_time", "reqCurrentTime"),
    ("Connection", "req_current_time_in_millis", "reqCurrentTimeInMillis"),
    # Market Data
    ("Market Data", "req_mkt_data", "reqMktData"),
    ("Market Data", "cancel_mkt_data", "cancelMktData"),
    ("Market Data", "req_market_data_type", "reqMarketDataType"),
    ("Market Data", "req_tick_by_tick_data", "reqTickByTickData"),
    ("Market Data", "cancel_tick_by_tick_data", "cancelTickByTickData"),
    ("Market Data", "req_mkt_depth", "reqMktDepth"),
    ("Market Data", "cancel_mkt_depth", "cancelMktDepth"),
    ("Market Data", "req_mkt_depth_exchanges", "reqMktDepthExchanges"),
    ("Market Data", "req_smart_components", "reqSmartComponents"),
    ("Market Data", "req_real_time_bars", "reqRealTimeBars"),
    ("Market Data", "cancel_real_time_bars", "cancelRealTimeBars"),
    # Historical Data
    ("Historical Data", "req_historical_data", "reqHistoricalData"),
    ("Historical Data", "cancel_historical_data", "cancelHistoricalData"),
    ("Historical Data", "req_head_time_stamp", "reqHeadTimeStamp"),
    ("Historical Data", "cancel_head_time_stamp", "cancelHeadTimestamp"),
    ("Historical Data", "req_historical_ticks", "reqHistoricalTicks"),
    ("Historical Data", "req_histogram_data", "reqHistogramData"),
    ("Historical Data", "cancel_histogram_data", "cancelHistogramData"),
    ("Historical Data", "req_historical_schedule", "reqHistoricalSchedule"),
    # Orders
    ("Orders", "place_order", "placeOrder"),
    ("Orders", "cancel_order", "cancelOrder"),
    ("Orders", "req_open_orders", "reqOpenOrders"),
    ("Orders", "req_all_open_orders", "reqAllOpenOrders"),
    ("Orders", "req_auto_open_orders", "reqAutoOpenOrders"),
    ("Orders", "req_ids", "reqIds"),
    ("Orders", "req_global_cancel", "reqGlobalCancel"),
    ("Orders", "req_completed_orders", "reqCompletedOrders"),
    # Executions
    ("Executions", "req_executions", "reqExecutions"),
    # Account
    ("Account", "req_account_updates", "reqAccountUpdates"),
    ("Account", "req_account_summary", "reqAccountSummary"),
    ("Account", "cancel_account_summary", "cancelAccountSummary"),
    ("Account", "req_positions", "reqPositions"),
    ("Account", "cancel_positions", "cancelPositions"),
    ("Account", "req_pnl", "reqPnL"),
    ("Account", "cancel_pnl", "cancelPnL"),
    ("Account", "req_pnl_single", "reqPnLSingle"),
    ("Account", "cancel_pnl_single", "cancelPnLSingle"),
    ("Account", "req_managed_accts", "reqManagedAccts"),
    ("Account", "req_account_updates_multi", "reqAccountUpdatesMulti"),
    ("Account", "cancel_account_updates_multi", "cancelAccountUpdatesMulti"),
    ("Account", "req_positions_multi", "reqPositionsMulti"),
    ("Account", "cancel_positions_multi", "cancelPositionsMulti"),
    # Contract
    ("Contract", "req_contract_details", "reqContractDetails"),
    ("Contract", "req_matching_symbols", "reqMatchingSymbols"),
    ("Contract", "req_market_rule", "reqMarketRule"),
    # Scanner
    ("Scanner", "req_scanner_parameters", "reqScannerParameters"),
    ("Scanner", "req_scanner_subscription", "reqScannerSubscription"),
    ("Scanner", "cancel_scanner_subscription", "cancelScannerSubscription"),
    # News
    ("News", "req_news_providers", "reqNewsProviders"),
    ("News", "req_news_article", "reqNewsArticle"),
    ("News", "req_historical_news", "reqHistoricalNews"),
    ("News", "req_news_bulletins", "reqNewsBulletins"),
    ("News", "cancel_news_bulletins", "cancelNewsBulletins"),
    # Fundamental
    ("Fundamental", "req_fundamental_data", "reqFundamentalData"),
    ("Fundamental", "cancel_fundamental_data", "cancelFundamentalData"),
    # Options
    ("Options", "calculate_implied_volatility", "calculateImpliedVolatility"),
    ("Options", "cancel_calculate_implied_volatility", "cancelCalculateImpliedVolatility"),
    ("Options", "calculate_option_price", "calculateOptionPrice"),
    ("Options", "cancel_calculate_option_price", "cancelCalculateOptionPrice"),
    ("Options", "exercise_options", "exerciseOptions"),
    ("Options", "req_sec_def_opt_params", "reqSecDefOptParams"),
    # Reference
    ("Reference", "req_soft_dollar_tiers", "reqSoftDollarTiers"),
    ("Reference", "req_family_codes", "reqFamilyCodes"),
    ("Reference", "req_user_info", "reqUserInfo"),
    # FA
    ("Financial Advisor", "request_fa", "requestFA"),
    ("Financial Advisor", "replace_fa", "replaceFA"),
    # Display Groups
    ("Display Groups", "query_display_groups", "queryDisplayGroups"),
    ("Display Groups", "subscribe_to_group_events", "subscribeToGroupEvents"),
    ("Display Groups", "unsubscribe_from_group_events", "unsubscribeFromGroupEvents"),
    ("Display Groups", "update_display_group", "updateDisplayGroup"),
    # WSH
    ("WSH", "req_wsh_meta_data", "reqWshMetaData"),
    ("WSH", "req_wsh_event_data", "reqWshEventData"),
]

IBAPI_EWRAPPER: list[tuple[str, str]] = [
    ("Connection", "connect_ack"),
    ("Connection", "connection_closed"),
    ("Connection", "next_valid_id"),
    ("Connection", "managed_accounts"),
    ("Connection", "error"),
    ("Connection", "current_time"),
    ("Connection", "current_time_in_millis"),
    ("Market Data", "tick_price"),
    ("Market Data", "tick_size"),
    ("Market Data", "tick_string"),
    ("Market Data", "tick_generic"),
    ("Market Data", "tick_snapshot_end"),
    ("Market Data", "market_data_type"),
    ("Market Data", "tick_req_params"),
    ("Orders", "order_status"),
    ("Orders", "open_order"),
    ("Orders", "open_order_end"),
    ("Orders", "order_bound"),
    ("Executions", "exec_details"),
    ("Executions", "exec_details_end"),
    ("Executions", "commission_and_fees_report"),
    ("Account", "update_account_value"),
    ("Account", "update_portfolio"),
    ("Account", "update_account_time"),
    ("Account", "account_download_end"),
    ("Account", "account_summary"),
    ("Account", "account_summary_end"),
    ("Account", "position"),
    ("Account", "position_end"),
    ("Account", "pnl"),
    ("Account", "pnl_single"),
    ("Account", "position_multi"),
    ("Account", "position_multi_end"),
    ("Account", "account_update_multi"),
    ("Account", "account_update_multi_end"),
    ("Contract", "contract_details"),
    ("Contract", "contract_details_end"),
    ("Contract", "bond_contract_details"),
    ("Contract", "symbol_samples"),
    ("Historical Data", "historical_data"),
    ("Historical Data", "historical_data_end"),
    ("Historical Data", "historical_data_update"),
    ("Historical Data", "head_timestamp"),
    ("Historical Data", "historical_ticks"),
    ("Historical Data", "historical_ticks_bid_ask"),
    ("Historical Data", "historical_ticks_last"),
    ("Historical Data", "histogram_data"),
    ("Historical Data", "historical_schedule"),
    ("Market Depth", "update_mkt_depth"),
    ("Market Depth", "update_mkt_depth_l2"),
    ("Market Depth", "mkt_depth_exchanges"),
    ("Tick-by-Tick", "tick_by_tick_all_last"),
    ("Tick-by-Tick", "tick_by_tick_bid_ask"),
    ("Tick-by-Tick", "tick_by_tick_mid_point"),
    ("Scanner", "scanner_data"),
    ("Scanner", "scanner_data_end"),
    ("Scanner", "scanner_parameters"),
    ("News", "news_providers"),
    ("News", "news_article"),
    ("News", "historical_news"),
    ("News", "historical_news_end"),
    ("News", "tick_news"),
    ("News", "update_news_bulletin"),
    ("Real-Time Bars", "real_time_bar"),
    ("Fundamental", "fundamental_data"),
    ("Market Rules", "market_rule"),
    ("Completed Orders", "completed_order"),
    ("Completed Orders", "completed_orders_end"),
    ("Options", "tick_option_computation"),
    ("Options", "security_definition_option_parameter"),
    ("Options", "security_definition_option_parameter_end"),
    ("Reference", "smart_components"),
    ("Reference", "soft_dollar_tiers"),
    ("Reference", "family_codes"),
    ("Reference", "user_info"),
    ("FA", "receive_fa"),
    ("FA", "replace_fa_end"),
    ("Display Groups", "display_group_list"),
    ("Display Groups", "display_group_updated"),
    ("Other", "delta_neutral_validation"),
    ("WSH", "wsh_meta_data"),
    ("WSH", "wsh_event_data"),
    # Defined by the reference API and not fired here, which is a difference
    # worth counting rather than one worth leaving out of the denominator: the
    # notes state each and why, and a total that omitted them would say this
    # client answers everything there is.
    #
    # `config` used to sit here and does not belong: it is this client's own
    # connection record, and no reference client has a callback of that name.
    # Counted as one the documented API names, it put a row in the comparison
    # that nothing was measured against.
    ("Market Data", "reroute_mkt_data_req"),
    ("Market Data", "reroute_mkt_depth_req"),
    # An exchange-for-physical quote, which the reference client reports on a
    # callback of its own. This client reads the tick types it is carried
    # under and fires nothing, so a program written against that callback
    # hears nothing where the reference client speaks.
    ("Market Data", "tick_efp"),
    # The handshake a third-party program makes with a terminal before that
    # terminal will carry its requests. There is no terminal between this
    # client and the venue to make it with, so nothing here can fire them —
    # which is a real difference to a program that implements them, and is
    # counted as one.
    ("Connection", "verify_message_api"),
    ("Connection", "verify_completed"),
    ("Connection", "verify_and_auth_message_api"),
    ("Connection", "verify_and_auth_completed"),
    # What the reference client reports when its own socket layer fails on
    # Windows. This client has no such layer and reports transport trouble on
    # the error callback like everything else.
    ("Connection", "win_error"),
]


#: Where a re-exported call is documented, since a `pub use` says nothing
#: about which part of the surface a reader will look for it under.
REEXPORT_SECTION = {"parse_algo_params": "orders"}


def reexported_into(stem: str) -> list[dict]:
    """Calls this section publishes that are declared somewhere else.

    A module publishes what it declares and what it re-exports, and a reader
    of the API cannot tell the two apart. Documenting only what a directory
    declares drops a call the day it moves, while it stays callable.
    """
    out = []
    for f in rust_client_files():
        if f.parent == RUST_CLIENT:
            continue
        for m in parse_rust_methods(f):
            if REEXPORT_SECTION.get(m["name"]) == stem:
                out.append(m)
    return out


def rust_client_files() -> list[Path]:
    """Every file the Rust client's surface is written across.

    A module publishes what it declares and what it re-exports, and a reader
    of the API cannot tell the two apart. So the `pub use` lines are followed:
    a call documented only because it was declared in this directory stops
    being documented the day it moves, while still being callable.
    """
    files = [f for f in RUST_CLIENT.glob("*.rs") if f.stem not in ("dispatch", "tests")]
    for line in (RUST_CLIENT / "mod.rs").read_text().splitlines():
        m = re.match(r"pub use crate::([\w:]+)::\w+;", line.strip())
        if not m:
            continue
        path = m.group(1).replace("::", "/")
        for cand in (ROOT / "src" / f"{path}.rs", ROOT / "src" / path / "mod.rs"):
            if cand.exists() and cand not in files:
                files.append(cand)
                break
    return files


def _collect_rust_methods() -> set[str]:
    """Collect all pub fn names from Rust EClient."""
    names = set()
    for f in rust_client_files():
        for m in parse_rust_methods(f):
            names.add(m["name"])
    return names


def _collect_rust_wrapper() -> set[str]:
    """Collect all Wrapper trait callback names."""
    return {m["name"] for m in parse_wrapper_trait(RUST_WRAPPER)}


def _collect_py_methods() -> set[str]:
    """Collect all pymethods fn names from Python EClient."""
    names = set()
    for f in PY_CLIENT.glob("*.rs"):
        for m in parse_pymethods(f):
            names.add(m["name"])
    return names


def _collect_py_wrapper() -> set[str]:
    """Collect all Python EWrapper callback names."""
    return {m["name"] for m in parse_py_wrapper(PY_WRAPPER)}


LIVE_SUITE = ROOT / "tests" / "ib_paper_compat"


def _read_all(files) -> str:
    return "\n".join(f.read_text(errors="ignore") for f in files)


def _evidence_index() -> tuple[str, str]:
    """What the live session exercises, and what the offline suites exercise.

    Test code only. Scanning the library too would credit every call with its
    own definition, which is evidence of nothing.
    """
    # Both suites that need a real session: one drives the engine, the other
    # the client surface itself.
    # Every target that reads the session credentials, read out of the sources:
    # named by hand this said one file while five need a session, and the four
    # it missed had their tests credited as evidence that needs none.
    needs_a_session = {
        f.stem for f in (ROOT / "tests").glob("*.rs")
        if "IB_USERNAME" in f.read_text(encoding="utf-8")
    }
    live_files = list(LIVE_SUITE.rglob("*.rs")) + [
        ROOT / "tests" / f"{stem}.rs" for stem in sorted(needs_a_session)
    ]
    offline_files = [
        f for f in (ROOT / "tests").rglob("*.rs")
        if "ib_paper_compat" not in f.parts and f.stem not in needs_a_session
    ]
    offline_files += [
        f for f in (ROOT / "src").rglob("*.rs")
        if f.stem in ("tests", "test_helpers")
    ]
    # The Python suite is test code too, and reading only the Rust files
    # credited a call exercised there to nothing at all: the calendar calls
    # were published as "Not exercised" while four Python tests drove them.
    # A Python test needs a session when it reads the credentials, which is
    # the same rule `check_status_counts.py` counts the live suite by.
    for f in sorted((ROOT / "tests" / "python").glob("*.py")):
        (live_files if "IB_USERNAME" in f.read_text(errors="ignore")
         else offline_files).append(f)
    live = _read_all(f for f in live_files if f.exists())
    return live, _read_all(offline_files)


def _evidence(name: str, live: str, offline: str, stub_names: set[str]) -> str:
    """How a call's status was established. Derived, never asserted.

    A call is credited to the live session only when the suite that runs
    against a real account names it, and to the offline suites only when a
    test names it. Nothing here is hand-maintained, so nothing here can go
    quietly out of date.
    """
    if name in stub_names:
        return "States why it cannot be served"
    if name in live:
        return "Live session"
    if name in offline:
        return "Offline suites"
    return "Not exercised"


def _status_icon(name: str, impl_set: set[str], stub_names: set[str]) -> str:
    if name in impl_set and name not in stub_names:
        return "Y"
    if name in impl_set and name in stub_names:
        return "STUB"
    return "-"


#: Calls that return as though they acted without reaching the venue.
#:
#: Hand-kept, and it has gone stale twice. The corporate-events calls were
#: listed here after they had been wired: `req_wsh_meta_data` and
#: `req_wsh_event_data` send on the security-definition connection and their
#: answers are dispatched to the wrapper, so a caller gets the calendar, not a
#: warning. The advisor calls were listed here too, and they send
#: `AdvisorConfig` and reach the venue — what does not happen is that their
#: replies are read, which is a callback that does not fire and is recorded as
#: one below. Anything added here has to be a call that genuinely sends
#: nothing, and a call whose answer goes unread is not one of them.
#: Empty, and kept so the next one has somewhere to go.
#:
#: `set_server_log_level` was the last of them to leave. The protocol carries no
#: message asking the venue to change how loudly it talks, and the counterpart
#: sends none either — it keeps what a caller states and logs by it. A drop-in
#: replacement is the thing serving the caller, so the level a caller states is
#: this client's own, and it is applied rather than written down.
STUB_METHODS: set[str] = {
    # The options a reference client hands its gateway on the greeting, for the
    # gateway to read. There is no gateway between this client and the venue,
    # so there is nothing to hand them to and nothing that would read them. An
    # option a caller states is answered on the error callback rather than
    # swallowed: each of them changes how the session behaves, and a caller who
    # set one and heard nothing has a session that is not the one they asked
    # for and no way to learn it.
    "set_connect_options",
}

#: Callbacks nothing fires. Each for its own reason, and none of them a
#: message this client fails to read: they name state the venue does not send
#: here.
STUB_CALLBACKS = {
    # Nothing in this protocol reaches the sender of one: not a market-data
    # request, not an order carrying a hedge. There is no first request after
    # which it could honestly be fired.
    "delta_neutral_validation",
    # The contract a request should be asked for under instead, where the venue
    # reroutes one. Nothing on this connection has been seen to state one.
    "reroute_mkt_data_req",
    "reroute_mkt_depth_req",
    # An exchange-for-physical quote. The tick types it is numbered under are
    # carried; they are not assembled into the record this reports.
    "tick_efp",
    # The handshake a third-party program makes with a terminal before that
    # terminal will carry its requests. There is no terminal here to make it
    # with, so these four can never fire.
    "verify_message_api",
    "verify_completed",
    "verify_and_auth_message_api",
    "verify_and_auth_completed",
    # What the reference client reports when its own socket layer fails on
    # Windows. This client has no such layer.
    "win_error",
}


def generate_coverage_md(ver: str) -> str:
    rust_methods = _collect_rust_methods()
    rust_wrapper = _collect_rust_wrapper()
    py_methods = _collect_py_methods()
    py_wrapper = _collect_py_wrapper()

    out = [
        f"# API Coverage Matrix (v{ver})",
        "",
        "*Auto-generated from source — do not edit.*",
        "",
        "Canonical IB API methods vs ibx implementation status.",
        "",
        "- **Y** = Implemented",
        "- **STUB** = Accepts call but not wired to server (logs warning or no-op)",
        "- **-** = Not present",
        "",
        "The evidence column says how each status was established, and is",
        "derived rather than asserted: a call is credited to the live session",
        "only when a suite that runs against a real account names it, and to",
        "the offline suites only when a test names it.",
        "",
        "One caveat it cannot express: the suite that compares against recorded",
        "captures skips when those captures are absent, and they are not kept in",
        "this repository. A call named only by that suite is named by something",
        "that did not run.",
        "",
    ]

    # Summary
    rust_impl = sum(1 for _, name, _ in IBAPI_ECLIENT if name in rust_methods and name not in STUB_METHODS)
    rust_stub = sum(1 for _, name, _ in IBAPI_ECLIENT if name in rust_methods and name in STUB_METHODS)
    py_impl = sum(1 for _, name, _ in IBAPI_ECLIENT if name in py_methods and name not in STUB_METHODS)
    py_stub = sum(1 for _, name, _ in IBAPI_ECLIENT if name in py_methods and name in STUB_METHODS)
    total_client = len(IBAPI_ECLIENT)

    rw_impl = sum(1 for _, name in IBAPI_EWRAPPER if name in rust_wrapper and name not in STUB_CALLBACKS)
    rw_stub = sum(1 for _, name in IBAPI_EWRAPPER if name in rust_wrapper and name in STUB_CALLBACKS)
    pw_impl = sum(1 for _, name in IBAPI_EWRAPPER if name in py_wrapper and name not in STUB_CALLBACKS)
    pw_stub = sum(1 for _, name in IBAPI_EWRAPPER if name in py_wrapper and name in STUB_CALLBACKS)
    total_wrapper = len(IBAPI_EWRAPPER)

    out.append("## Summary")
    out.append("")
    out.append("| | IB API | Rust | Python |")
    out.append("|---|:---:|:---:|:---:|")
    out.append(f"| **EClient methods** | {total_client} | {rust_impl} impl, {rust_stub} stub | {py_impl} impl, {py_stub} stub |")
    out.append(f"| **EWrapper callbacks** | {total_wrapper} | {rw_impl} impl, {rw_stub} stub | {pw_impl} impl, {pw_stub} stub |")
    out.append("")

    # EClient table
    out.append("## EClient Methods")
    out.append("")
    live, offline = _evidence_index()
    out.append("| Category | IB API Method | C++ Name | Rust | Python | Evidence |")
    out.append("|----------|---------------|----------|:----:|:------:|----------|")
    current_cat = ""
    for cat, name, cpp_name in IBAPI_ECLIENT:
        display_cat = cat if cat != current_cat else ""
        current_cat = cat
        r = _status_icon(name, rust_methods, STUB_METHODS)
        p = _status_icon(name, py_methods, STUB_METHODS)
        e = _evidence(name, live, offline, STUB_METHODS)
        out.append(f"| {display_cat} | `{name}` | `{cpp_name}` | {r} | {p} | {e} |")
    out.append("")

    # EWrapper table
    out.append("## EWrapper Callbacks")
    out.append("")
    out.append("| Category | Callback | Rust | Python |")
    out.append("|----------|----------|:----:|:------:|")
    current_cat = ""
    for cat, name in IBAPI_EWRAPPER:
        display_cat = cat if cat != current_cat else ""
        current_cat = cat
        r = _status_icon(name, rust_wrapper, STUB_CALLBACKS)
        p = _status_icon(name, py_wrapper, STUB_CALLBACKS)
        out.append(f"| {display_cat} | `{name}` | {r} | {p} |")
    out.append("")

    return "\n".join(out)


def main():
    for target in (RUST_REF, PYTHON_REF, COVERAGE_REF):
        target.parent.mkdir(parents=True, exist_ok=True)
    ver = version()
    rust, rc = generate_rust_md(ver)
    py, pc = generate_python_md(ver)
    cov = generate_coverage_md(ver)
    RUST_REF.write_text(rust, encoding="utf-8")
    PYTHON_REF.write_text(py, encoding="utf-8")
    COVERAGE_REF.write_text(cov, encoding="utf-8")
    print(f"{RUST_REF.relative_to(ROOT)}  — {rc} methods")
    print(f"{PYTHON_REF.relative_to(ROOT)} — {pc} methods")
    print(f"{COVERAGE_REF.relative_to(ROOT)}  — {len(IBAPI_ECLIENT)} EClient + {len(IBAPI_EWRAPPER)} EWrapper")


if __name__ == "__main__":
    main()
