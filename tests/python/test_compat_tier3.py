"""Compatibility tests for Tier 3 ibapi-compatible API additions.

Tests cover:
- Financial Advisor: request_fa, replace_fa
- Display Groups: query_display_groups, subscribe_to_group_events, etc.
- Market Rules: req_market_rule
- Smart Components: req_smart_components
- Soft Dollar Tiers: req_soft_dollar_tiers
- Family Codes: req_family_codes
- Histogram Data: req_histogram_data, cancel_histogram_data
- Server Log Level: set_server_log_level
- User Info: req_user_info
- WSH: req_wsh_meta_data, req_wsh_event_data
- Completed Orders: req_completed_orders
- EWrapper Tier 3 Callbacks
"""

from ibx import EClient, EWrapper, Contract


# ── Helper fixtures ──

def make_client():
    """Create an unconnected EClient + EWrapper pair."""
    w = EWrapper()
    c = EClient(w)
    return c, w


def make_contract(**kwargs):
    """Create a Contract with optional overrides."""
    c = Contract()
    for k, v in kwargs.items():
        setattr(c, k, v)
    return c


# ═══════════════════════════════════════════════════════════
# Financial Advisor (stubs — no connection check)
# ═══════════════════════════════════════════════════════════

def test_request_fa_stub():
    c, w = make_client()
    c.request_fa(1)  # should not raise, just logs warning


def test_replace_fa_stub():
    c, w = make_client()
    c.replace_fa(1, 1, "<xml/>")  # should not raise


def test_fa_signatures():
    c, w = make_client()
    assert hasattr(c, "request_fa")
    assert hasattr(c, "replace_fa")


# ═══════════════════════════════════════════════════════════
# Display Groups (stubs — no connection check)
# ═══════════════════════════════════════════════════════════

def test_query_display_groups_stub():
    c, w = make_client()
    c.query_display_groups(1)


def test_subscribe_to_group_events_stub():
    c, w = make_client()
    c.subscribe_to_group_events(1, 1)


def test_unsubscribe_from_group_events_stub():
    c, w = make_client()
    c.unsubscribe_from_group_events(1)


def test_update_display_group_stub():
    c, w = make_client()
    c.update_display_group(1, "265598")


def test_display_group_signatures():
    c, w = make_client()
    assert hasattr(c, "query_display_groups")
    assert hasattr(c, "subscribe_to_group_events")
    assert hasattr(c, "unsubscribe_from_group_events")
    assert hasattr(c, "update_display_group")


# ═══════════════════════════════════════════════════════════
# Market Rules (checks connection for cache lookup)
# ═══════════════════════════════════════════════════════════

def test_req_market_rule_not_connected():
    """Without connection, req_market_rule has no shared state — logs warning."""
    c, w = make_client()
    c.req_market_rule(26)  # no shared state → logs warning, no crash


def test_req_market_rule_signature():
    c, w = make_client()
    assert hasattr(c, "req_market_rule")


# ═══════════════════════════════════════════════════════════
# Smart Components (fires empty callback)
# ═══════════════════════════════════════════════════════════

class SmartComponentsCapture(EWrapper):
    def __init__(self):
        super().__init__()
        self.req_id = None
        self.components = None

    def smart_components(self, req_id, smart_component_map):
        self.req_id = req_id
        self.components = smart_component_map


def test_req_smart_components_fires_callback():
    w = SmartComponentsCapture()
    c = EClient(w)
    c.req_smart_components(1, "a]AMEX")
    assert w.req_id == 1
    assert len(w.components) == 0  # Empty map (gateway-local data not available)


def test_req_smart_components_signature():
    c, w = make_client()
    assert hasattr(c, "req_smart_components")


# ═══════════════════════════════════════════════════════════
# Soft Dollar Tiers (gateway-local, returns empty list)
# ═══════════════════════════════════════════════════════════

class SoftDollarTiersCapture(EWrapper):
    def __init__(self):
        super().__init__()
        self.tiers = None
        self.req_id = None

    def soft_dollar_tiers(self, req_id, tiers):
        self.req_id = req_id
        self.tiers = tiers


def test_req_soft_dollar_tiers_fires_callback():
    w = SoftDollarTiersCapture()
    c = EClient(w)
    c.req_soft_dollar_tiers(42)
    assert w.req_id == 42
    assert len(w.tiers) == 0  # Paper accounts return empty


def test_req_soft_dollar_tiers_signature():
    c, w = make_client()
    assert hasattr(c, "req_soft_dollar_tiers")


# ═══════════════════════════════════════════════════════════
# Family Codes (gateway-local, returns account info)
# ═══════════════════════════════════════════════════════════

class FamilyCodesCapture(EWrapper):
    def __init__(self):
        super().__init__()
        self.codes = None

    def family_codes(self, codes):
        self.codes = codes


def test_req_family_codes_fires_callback():
    w = FamilyCodesCapture()
    c = EClient(w)
    c.req_family_codes()
    assert w.codes is not None
    assert len(w.codes) == 1
    # Each entry is (accountID, familyCodeStr)
    assert w.codes[0][1] == ""  # Empty family code on unconnected


def test_req_family_codes_signature():
    c, w = make_client()
    assert hasattr(c, "req_family_codes")


# ═══════════════════════════════════════════════════════════
# Histogram Data (checks connection)
# ═══════════════════════════════════════════════════════════

def test_req_histogram_data_not_connected():
    c, w = make_client()
    con = make_contract(con_id=265598, symbol="AAPL", sec_type="STK", exchange="SMART")
    try:
        c.req_histogram_data(1, con, True, "1 week")
        assert False, "Should raise"
    except RuntimeError as e:
        assert "Not connected" in str(e)


def test_cancel_histogram_data_not_connected():
    c, w = make_client()
    try:
        c.cancel_histogram_data(1)
        assert False, "Should raise"
    except RuntimeError as e:
        assert "Not connected" in str(e)


def test_histogram_data_signatures():
    c, w = make_client()
    assert hasattr(c, "req_histogram_data")
    assert hasattr(c, "cancel_histogram_data")


# ═══════════════════════════════════════════════════════════
# Historical Schedule (routed via req_historical_data + SCHEDULE)
# ═══════════════════════════════════════════════════════════

def test_req_historical_schedule_not_connected():
    c, w = make_client()
    con = make_contract(con_id=756733, symbol="SPY", sec_type="STK", exchange="SMART")
    try:
        c.req_historical_data(1, con, "", "5 D", "1 day", "SCHEDULE", 1)
        assert False, "Should raise"
    except RuntimeError as e:
        assert "Not connected" in str(e)


def test_req_historical_schedule_signature():
    """SCHEDULE is routed via req_historical_data, not a separate method."""
    c, w = make_client()
    assert hasattr(c, "req_historical_data")


# ═══════════════════════════════════════════════════════════
# Server Log Level (local-only, always succeeds)
# ═══════════════════════════════════════════════════════════

def test_set_server_log_level_all_levels():
    """set_server_log_level should succeed for all valid levels."""
    c, w = make_client()
    for level in [1, 2, 3, 4, 5]:
        c.set_server_log_level(level)


def test_set_server_log_level_default():
    """Default log level (2 = warn)."""
    c, w = make_client()
    c.set_server_log_level()  # uses default


def test_set_server_log_level_signature():
    c, w = make_client()
    assert hasattr(c, "set_server_log_level")


# ═══════════════════════════════════════════════════════════
# User Info (gateway-local, returns empty whiteBrandingId)
# ═══════════════════════════════════════════════════════════

class UserInfoCapture(EWrapper):
    def __init__(self):
        super().__init__()
        self.req_id = None
        self.white_branding_id = None

    def user_info(self, req_id, white_branding_id):
        self.req_id = req_id
        self.white_branding_id = white_branding_id


def test_req_user_info_fires_callback():
    w = UserInfoCapture()
    c = EClient(w)
    c.req_user_info(7)
    assert w.req_id == 7
    assert w.white_branding_id == ""  # Empty on paper


def test_req_user_info_signature():
    c, w = make_client()
    assert hasattr(c, "req_user_info")


# ═══════════════════════════════════════════════════════════
# WSH (stubs)
# ═══════════════════════════════════════════════════════════

def test_req_wsh_meta_data_stub():
    c, w = make_client()
    c.req_wsh_meta_data(1)


def test_req_wsh_event_data_stub():
    c, w = make_client()
    c.req_wsh_event_data(1)


def test_req_wsh_event_data_with_arg():
    c, w = make_client()
    c.req_wsh_event_data(1, None)  # optional arg


def test_wsh_signatures():
    c, w = make_client()
    assert hasattr(c, "req_wsh_meta_data")
    assert hasattr(c, "req_wsh_event_data")


# ═══════════════════════════════════════════════════════════
# Completed Orders (session archive)
# ═══════════════════════════════════════════════════════════

def test_req_completed_orders_no_shared_state():
    """Without connection, req_completed_orders should not crash."""
    c, w = make_client()
    c.req_completed_orders(True)


def test_req_completed_orders_default_arg():
    c, w = make_client()
    c.req_completed_orders()  # api_only defaults to False


def test_req_completed_orders_signature():
    c, w = make_client()
    assert hasattr(c, "req_completed_orders")


# ═══════════════════════════════════════════════════════════
# EWrapper Tier 3 Callbacks (no-op defaults)
# ═══════════════════════════════════════════════════════════

def test_wrapper_display_group_list():
    w = EWrapper()
    w.display_group_list(1, "1|2|3")


def test_wrapper_display_group_updated():
    w = EWrapper()
    w.display_group_updated(1, "265598")


def test_wrapper_market_rule():
    w = EWrapper()
    w.market_rule(26, None)


def test_wrapper_smart_components():
    w = EWrapper()
    w.smart_components(1, None)


def test_wrapper_soft_dollar_tiers():
    w = EWrapper()
    w.soft_dollar_tiers(1, None)


def test_wrapper_family_codes():
    w = EWrapper()
    w.family_codes(None)


def test_wrapper_histogram_data():
    w = EWrapper()
    w.histogram_data(1, None)


def test_wrapper_user_info():
    w = EWrapper()
    w.user_info(1, "")


def test_wrapper_wsh_meta_data():
    w = EWrapper()
    w.wsh_meta_data(1, "{}")


def test_wrapper_wsh_event_data():
    w = EWrapper()
    w.wsh_event_data(1, "{}")


def test_wrapper_completed_order():
    w = EWrapper()
    w.completed_order(None, None, None)


def test_wrapper_completed_orders_end():
    w = EWrapper()
    w.completed_orders_end()


def test_wrapper_order_bound():
    w = EWrapper()
    w.order_bound(1, 0, 1)


def test_wrapper_tick_req_params():
    w = EWrapper()
    w.tick_req_params(1, 0.01, "SMART", 1)


def test_wrapper_bond_contract_details():
    w = EWrapper()
    w.bond_contract_details(1, None)


def test_wrapper_delta_neutral_validation():
    w = EWrapper()
    w.delta_neutral_validation(1, None)


def test_wrapper_historical_schedule():
    w = EWrapper()
    w.historical_schedule(1, "20230101", "20230201", "US/Eastern", None)


# ═══════════════════════════════════════════════════════════
# Positions Multi (one-shot from SharedState)
# ═══════════════════════════════════════════════════════════

def test_wrapper_position_multi():
    w = EWrapper()
    w.position_multi(1, "DU12345", "", None, 100.0, 50.5)


def test_wrapper_position_multi_end():
    w = EWrapper()
    w.position_multi_end(1)


# ═══════════════════════════════════════════════════════════
# Account Updates Multi (one-shot from SharedState)
# ═══════════════════════════════════════════════════════════

def test_wrapper_account_update_multi():
    w = EWrapper()
    w.account_update_multi(1, "DU12345", "", "NetLiquidation", "100000.00", "USD")


def test_wrapper_account_update_multi_end():
    w = EWrapper()
    w.account_update_multi_end(1)
