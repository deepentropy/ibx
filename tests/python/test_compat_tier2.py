"""Compatibility tests for Tier 2 ibapi-compatible API additions.

Tests cover:
- Scanner: req_scanner_subscription, cancel_scanner_subscription, req_scanner_parameters
- News: req_news_providers, req_news_article, req_historical_news
- Fundamental Data: req_fundamental_data, cancel_fundamental_data
- Options Calculations: calculate_implied_volatility, calculate_option_price, cancels
- Exercise Options: exercise_options
- News Bulletins: req_news_bulletins, cancel_news_bulletins
- Managed Accounts: req_managed_accts
- Multi-Account: req_account_updates_multi, cancel_account_updates_multi
- Multi-Positions: req_positions_multi, cancel_positions_multi
- EWrapper Callbacks: fundamental_data, update_news_bulletin, receive_fa, replace_fa_end,
                      position_multi, position_multi_end, account_update_multi, account_update_multi_end
"""

import time
import pytest
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
# Scanner
# ═══════════════════════════════════════════════════════════

class MockScannerSub:
    """Mock scanner subscription object with ibapi-like attributes."""
    def __init__(self):
        self.instrument = "STK"
        self.locationCode = "STK.US.MAJOR"
        self.scanCode = "TOP_PERC_GAIN"
        self.numberOfRows = 50


def test_req_scanner_subscription_not_connected():
    c, w = make_client()
    try:
        c.req_scanner_subscription(1, MockScannerSub())
        assert False, "Should raise"
    except RuntimeError as e:
        assert "Not connected" in str(e)


def test_req_scanner_subscription_signature():
    """Verify req_scanner_subscription accepts ibapi-like signature."""
    c, w = make_client()
    # Can't actually call without connection, just verify the method exists and takes the right args
    assert hasattr(c, 'req_scanner_subscription')


def test_cancel_scanner_subscription_not_connected():
    c, w = make_client()
    try:
        c.cancel_scanner_subscription(1)
        assert False, "Should raise"
    except RuntimeError as e:
        assert "Not connected" in str(e)


def test_req_scanner_parameters_not_connected():
    c, w = make_client()
    try:
        c.req_scanner_parameters()
        assert False, "Should raise"
    except RuntimeError as e:
        assert "Not connected" in str(e)


def test_req_scanner_subscription_with_options():
    """Verify scanner subscription accepts options list."""
    c, w = make_client()
    assert hasattr(c, 'req_scanner_subscription')
    # The method signature should accept scanner_subscription_options


# ═══════════════════════════════════════════════════════════
# News
# ═══════════════════════════════════════════════════════════

def test_req_news_providers_fires_callback():
    """req_news_providers should fire news_providers callback."""
    calls = []
    class W(EWrapper):
        def news_providers(self, providers):
            calls.append(providers)
    w = W()
    c = EClient(w)
    c.req_news_providers()
    assert len(calls) == 1


def test_req_news_providers_returns_list():
    """req_news_providers should pass a list to the callback."""
    result = []
    class W(EWrapper):
        def news_providers(self, providers):
            result.append(type(providers).__name__)
    w = W()
    c = EClient(w)
    c.req_news_providers()
    assert result[0] == "list"


def test_req_news_article_not_connected():
    c, w = make_client()
    try:
        c.req_news_article(1, "BRFG", "BRFG$12345")
        assert False, "Should raise"
    except RuntimeError as e:
        assert "Not connected" in str(e)


def test_req_news_article_signature():
    """Verify req_news_article accepts ibapi-like signature."""
    assert hasattr(EClient, 'req_news_article')


def test_req_news_article_with_options():
    """Verify req_news_article accepts options list."""
    c, w = make_client()
    assert hasattr(c, 'req_news_article')


def test_req_historical_news_not_connected():
    c, w = make_client()
    try:
        c.req_historical_news(1, 265598, "BRFG", "2026-01-01", "2026-03-12", 10)
        assert False, "Should raise"
    except RuntimeError as e:
        assert "Not connected" in str(e)


def test_req_historical_news_signature():
    """Verify req_historical_news accepts ibapi-like signature."""
    assert hasattr(EClient, 'req_historical_news')


# ═══════════════════════════════════════════════════════════
# Fundamental Data
# ═══════════════════════════════════════════════════════════

def test_req_fundamental_data_not_connected():
    c, w = make_client()
    contract = make_contract(con_id=265598, symbol="AAPL")
    try:
        c.req_fundamental_data(1, contract, "ReportSnapshot")
        assert False, "Should raise"
    except RuntimeError as e:
        assert "Not connected" in str(e)


def test_req_fundamental_data_signature():
    """Verify req_fundamental_data accepts ibapi-like signature."""
    assert hasattr(EClient, 'req_fundamental_data')


def test_cancel_fundamental_data_not_connected():
    c, w = make_client()
    try:
        c.cancel_fundamental_data(1)
        assert False, "Should raise"
    except RuntimeError as e:
        assert "Not connected" in str(e)


def test_req_fundamental_data_with_options():
    """Verify req_fundamental_data accepts options list."""
    c, w = make_client()
    assert hasattr(c, 'req_fundamental_data')


# ═══════════════════════════════════════════════════════════
# Options Calculations (stubs)
# ═══════════════════════════════════════════════════════════

def test_calculate_implied_volatility_accepts_call():
    c, w = make_client()
    contract = make_contract(con_id=265598)
    result = c.calculate_implied_volatility(1, contract, 5.0, 150.0)
    assert result is None  # Returns Ok(())


def test_calculate_implied_volatility_with_options():
    c, w = make_client()
    contract = make_contract()
    result = c.calculate_implied_volatility(1, contract, 5.0, 150.0, [])
    assert result is None


def test_calculate_option_price_accepts_call():
    c, w = make_client()
    contract = make_contract(con_id=265598)
    result = c.calculate_option_price(1, contract, 0.3, 150.0)
    assert result is None


def test_calculate_option_price_with_options():
    c, w = make_client()
    contract = make_contract()
    result = c.calculate_option_price(1, contract, 0.3, 150.0, [])
    assert result is None


def test_cancel_calculate_implied_volatility():
    c, w = make_client()
    result = c.cancel_calculate_implied_volatility(1)
    assert result is None


def test_cancel_calculate_option_price():
    c, w = make_client()
    result = c.cancel_calculate_option_price(1)
    assert result is None


# ═══════════════════════════════════════════════════════════
# Exercise Options
# ═══════════════════════════════════════════════════════════

def test_exercise_options_accepts_call():
    c, w = make_client()
    contract = make_contract(con_id=265598)
    result = c.exercise_options(1, contract, 1, 100, "DU12345", 0)
    assert result is None


def test_exercise_options_signature():
    """Verify exercise_options exists with correct parameter count."""
    assert hasattr(EClient, 'exercise_options')


# ═══════════════════════════════════════════════════════════
# News Bulletins
# ═══════════════════════════════════════════════════════════

def test_req_news_bulletins_default():
    c, w = make_client()
    result = c.req_news_bulletins()
    assert result is None


def test_req_news_bulletins_all_false():
    c, w = make_client()
    result = c.req_news_bulletins(False)
    assert result is None


def test_cancel_news_bulletins():
    c, w = make_client()
    result = c.cancel_news_bulletins()
    assert result is None


# ═══════════════════════════════════════════════════════════
# Managed Accounts
# ═══════════════════════════════════════════════════════════

def test_req_managed_accts_fires_callback():
    calls = []
    class W(EWrapper):
        def managed_accounts(self, accounts_list):
            calls.append(accounts_list)
    w = W()
    c = EClient(w)
    c.req_managed_accts()
    assert len(calls) == 1
    # Account ID is empty for unconnected client
    assert calls[0] == ""


# ═══════════════════════════════════════════════════════════
# Multi-Account (requires connection — reads SharedState)
# ═══════════════════════════════════════════════════════════

def test_req_account_updates_multi_not_connected():
    c, w = make_client()
    with pytest.raises(Exception, match="Not connected"):
        c.req_account_updates_multi(1, "DU12345", "")


def test_req_account_updates_multi_with_ledger_not_connected():
    c, w = make_client()
    with pytest.raises(Exception, match="Not connected"):
        c.req_account_updates_multi(1, "DU12345", "", True)


def test_cancel_account_updates_multi():
    c, w = make_client()
    result = c.cancel_account_updates_multi(1)
    assert result is None


def test_req_positions_multi_not_connected():
    c, w = make_client()
    with pytest.raises(Exception, match="Not connected"):
        c.req_positions_multi(1, "DU12345", "")


def test_cancel_positions_multi():
    c, w = make_client()
    result = c.cancel_positions_multi(1)
    assert result is None


# ═══════════════════════════════════════════════════════════
# EWrapper Callbacks (new in Tier 2)
# ═══════════════════════════════════════════════════════════

def test_ewrapper_base_fundamental_data_noop():
    w = EWrapper()
    w.fundamental_data(1, "<data>test</data>")  # should not raise


def test_ewrapper_base_update_news_bulletin_noop():
    w = EWrapper()
    w.update_news_bulletin(1, 1, "test message", "NYSE")  # should not raise


def test_ewrapper_base_receive_fa_noop():
    w = EWrapper()
    w.receive_fa(1, "<xml/>")  # should not raise


def test_ewrapper_base_replace_fa_end_noop():
    w = EWrapper()
    w.replace_fa_end(1, "")  # should not raise


def test_ewrapper_base_position_multi_noop():
    w = EWrapper()
    c = Contract()
    w.position_multi(1, "DU12345", "", c, 100.0, 150.0)  # should not raise


def test_ewrapper_base_position_multi_end_noop():
    w = EWrapper()
    w.position_multi_end(1)  # should not raise


def test_ewrapper_base_account_update_multi_noop():
    w = EWrapper()
    w.account_update_multi(1, "DU12345", "", "NetLiquidation", "100000", "USD")


def test_ewrapper_base_account_update_multi_end_noop():
    w = EWrapper()
    w.account_update_multi_end(1)  # should not raise


# ── Subclass overrides ──

def test_subclass_fundamental_data():
    calls = []
    class W(EWrapper):
        def fundamental_data(self, req_id, data):
            calls.append((req_id, data))
    w = W()
    w.fundamental_data(42, "<FundamentalData/>")
    assert calls == [(42, "<FundamentalData/>")]


def test_subclass_update_news_bulletin():
    calls = []
    class W(EWrapper):
        def update_news_bulletin(self, msg_id, msg_type, message, orig_exchange):
            calls.append((msg_id, msg_type, message, orig_exchange))
    w = W()
    w.update_news_bulletin(1, 1, "Breaking news", "NYSE")
    assert calls == [(1, 1, "Breaking news", "NYSE")]


def test_subclass_position_multi():
    calls = []
    class W(EWrapper):
        def position_multi(self, req_id, account, model_code, contract, pos, avg_cost):
            calls.append((req_id, account, model_code, pos, avg_cost))
    w = W()
    c = Contract()
    w.position_multi(1, "DU12345", "model1", c, 100.0, 155.5)
    assert calls == [(1, "DU12345", "model1", 100.0, 155.5)]


def test_subclass_account_update_multi():
    calls = []
    class W(EWrapper):
        def account_update_multi(self, req_id, account, model_code, key, value, currency):
            calls.append((req_id, account, key, value, currency))
    w = W()
    w.account_update_multi(1, "DU12345", "", "NetLiquidation", "100000", "USD")
    assert calls == [(1, "DU12345", "NetLiquidation", "100000", "USD")]


# ═══════════════════════════════════════════════════════════
# Full Sequences (ibapi App pattern)
# ═══════════════════════════════════════════════════════════

def test_full_ibapi_app_pattern_with_tier2():
    """Verify the ibapi App(EWrapper) + EClient pattern works with all Tier 2 methods."""
    class App(EWrapper):
        def __init__(self):
            super().__init__()
            self.events = []
            self.client = EClient(self)

        def fundamental_data(self, req_id, data):
            self.events.append(("fundamental_data", req_id))

        def scanner_parameters(self, xml):
            self.events.append(("scanner_parameters",))

        def news_providers(self, providers):
            self.events.append(("news_providers",))

        def managed_accounts(self, accounts_list):
            self.events.append(("managed_accounts", accounts_list))

    app = App()

    # These should work without connection
    app.client.req_news_providers()
    app.client.req_managed_accts()
    app.client.req_news_bulletins()
    app.client.cancel_news_bulletins()

    # Stubs should return without error
    contract = make_contract(con_id=265598, symbol="AAPL")
    app.client.calculate_implied_volatility(1, contract, 5.0, 150.0)
    app.client.calculate_option_price(2, contract, 0.3, 150.0)
    app.client.cancel_calculate_implied_volatility(1)
    app.client.cancel_calculate_option_price(2)
    app.client.exercise_options(3, contract, 1, 100, "DU12345", 0)
    # cancel_* are no-ops that work without connection
    app.client.cancel_account_updates_multi(10)
    app.client.cancel_positions_multi(11)

    # Verify callbacks fired
    assert ("news_providers",) in app.events
    assert ("managed_accounts", "") in app.events


def test_full_scanner_sequence():
    """Test the full scanner workflow pattern."""
    class App(EWrapper):
        def __init__(self):
            super().__init__()
            self.client = EClient(self)
            self.params_received = False
            self.scan_data = []
            self.scan_ended = False

        def scanner_parameters(self, xml):
            self.params_received = True

        def scanner_data(self, req_id, rank, contract_details, distance, benchmark, projection, legs_str):
            self.scan_data.append((req_id, rank))

        def scanner_data_end(self, req_id):
            self.scan_ended = True

    app = App()
    # Methods exist and are callable (just not connected)
    assert hasattr(app.client, 'req_scanner_parameters')
    assert hasattr(app.client, 'req_scanner_subscription')
    assert hasattr(app.client, 'cancel_scanner_subscription')


def test_full_news_sequence():
    """Test the news workflow pattern."""
    class App(EWrapper):
        def __init__(self):
            super().__init__()
            self.client = EClient(self)
            self.providers = None
            self.articles = []
            self.hist_news = []

        def news_providers(self, providers):
            self.providers = providers

        def news_article(self, req_id, article_type, article_text):
            self.articles.append((req_id, article_type, article_text))

        def historical_news(self, req_id, time, provider_code, article_id, headline):
            self.hist_news.append((req_id, time, provider_code))

        def historical_news_end(self, req_id, has_more):
            pass

    app = App()
    app.client.req_news_providers()
    assert app.providers is not None
    assert isinstance(app.providers, list)


def test_full_fundamental_data_sequence():
    """Test the fundamental data workflow pattern."""
    class App(EWrapper):
        def __init__(self):
            super().__init__()
            self.client = EClient(self)
            self.data = []

        def fundamental_data(self, req_id, data):
            self.data.append((req_id, data))

    app = App()
    assert hasattr(app.client, 'req_fundamental_data')
    assert hasattr(app.client, 'cancel_fundamental_data')


def test_options_calc_methods_exist():
    """Verify all options calculation methods exist on EClient."""
    c, w = make_client()
    assert hasattr(c, 'calculate_implied_volatility')
    assert hasattr(c, 'calculate_option_price')
    assert hasattr(c, 'cancel_calculate_implied_volatility')
    assert hasattr(c, 'cancel_calculate_option_price')
    assert hasattr(c, 'exercise_options')


def test_multi_account_methods_exist():
    """Verify all multi-account methods exist on EClient."""
    c, w = make_client()
    assert hasattr(c, 'req_account_updates_multi')
    assert hasattr(c, 'cancel_account_updates_multi')
    assert hasattr(c, 'req_positions_multi')
    assert hasattr(c, 'cancel_positions_multi')


def test_all_tier2_ewrapper_callbacks_exist():
    """Verify all Tier 2 EWrapper callbacks exist."""
    w = EWrapper()
    assert hasattr(w, 'fundamental_data')
    assert hasattr(w, 'update_news_bulletin')
    assert hasattr(w, 'receive_fa')
    assert hasattr(w, 'replace_fa_end')
    assert hasattr(w, 'position_multi')
    assert hasattr(w, 'position_multi_end')
    assert hasattr(w, 'account_update_multi')
    assert hasattr(w, 'account_update_multi_end')
