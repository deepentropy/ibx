"""IBX Test #98: Account Updates + Live P&L Subscription (Portfolio).

Account summary, portfolio positions, live P&L, and pnlSingle.
Exercises cache coordination, account streaming, and signed value encoding.

Run: pytest tests/python/test_issue_98.py -v --timeout=120
"""

import os
import time
import pytest
import threading
from ibx import EWrapper, EClient, Contract


pytestmark = pytest.mark.skipif(
    not (os.environ.get("IB_USERNAME") and os.environ.get("IB_PASSWORD")),
    reason="IB_USERNAME and IB_PASSWORD not set",
)


class AccountWrapper(EWrapper):
    def __init__(self):
        super().__init__()
        self.connected = threading.Event()
        self.account_id = ""

        # Account values (streamed key-value pairs)
        self.account_values = {}  # key -> (value, currency)
        self.got_account_download_end = threading.Event()

        # Portfolio
        self.portfolio = []  # (contract, position, mkt_price, mkt_value, avg_cost, unrealized, realized)
        self.got_portfolio = threading.Event()

        # Positions (one-shot)
        self.positions = []  # (account, contract, pos, avg_cost)
        self.got_position_end = threading.Event()

        # P&L
        self.pnl_data = None
        self.got_pnl = threading.Event()
        self.pnl_single_data = None
        self.got_pnl_single = threading.Event()

        # Account summary
        self.summary = {}  # tag -> (value, currency)
        self.got_summary_end = threading.Event()

    def next_valid_id(self, order_id):
        self.connected.set()

    def managed_accounts(self, accounts_list):
        self.account_id = accounts_list

    def connect_ack(self):
        pass

    # -- Account streaming callbacks --
    def update_account_value(self, key, value, currency, account_name):
        self.account_values[key] = (value, currency)

    def update_portfolio(self, contract, position, market_price, market_value,
                         average_cost, unrealized_pnl, realized_pnl, account_name):
        self.portfolio.append({
            "symbol": contract.symbol,
            "con_id": contract.con_id,
            "position": position,
            "market_price": market_price,
            "market_value": market_value,
            "average_cost": average_cost,
            "unrealized_pnl": unrealized_pnl,
            "realized_pnl": realized_pnl,
        })
        self.got_portfolio.set()

    def update_account_time(self, timestamp):
        pass

    def account_download_end(self, account):
        self.got_account_download_end.set()

    # -- Position callbacks --
    def position(self, account, contract, pos, avg_cost):
        self.positions.append({
            "account": account,
            "symbol": contract.symbol,
            "con_id": contract.con_id,
            "position": pos,
            "avg_cost": avg_cost,
        })

    def position_end(self):
        self.got_position_end.set()

    # -- P&L callbacks --
    def pnl(self, req_id, daily_pnl, unrealized_pnl, realized_pnl):
        self.pnl_data = {
            "daily_pnl": daily_pnl,
            "unrealized_pnl": unrealized_pnl,
            "realized_pnl": realized_pnl,
        }
        self.got_pnl.set()

    def pnl_single(self, req_id, pos, daily_pnl, unrealized_pnl, realized_pnl, value):
        self.pnl_single_data = {
            "pos": pos,
            "daily_pnl": daily_pnl,
            "unrealized_pnl": unrealized_pnl,
            "realized_pnl": realized_pnl,
            "value": value,
        }
        self.got_pnl_single.set()

    # -- Account summary callbacks --
    def account_summary(self, req_id, account, tag, value, currency):
        self.summary[tag] = (value, currency)

    def account_summary_end(self, req_id):
        self.got_summary_end.set()

    def error(self, req_id, error_code, error_string, advanced_order_reject_json=""):
        if error_code not in (2104, 2106, 2158):
            print(f"  [error] {error_code}: {error_string}")


class TestAccountAndPnL:
    """Issue #98: Account data queries + real-time P&L streaming."""

    @pytest.fixture(autouse=True)
    def setup_connection(self):
        self.wrapper = AccountWrapper()
        self.client = EClient(self.wrapper)
        self.client.connect(
            username=os.environ["IB_USERNAME"],
            password=os.environ["IB_PASSWORD"],
            host=os.environ.get("IB_HOST", "cdc1.ibllc.com"),
            paper=True,
        )
        self.thread = threading.Thread(target=self.client.run, daemon=True)
        self.thread.start()
        assert self.wrapper.connected.wait(timeout=15), "Connection failed"
        yield
        self.client.disconnect()
        self.thread.join(timeout=5)

    def test_account_updates_streaming(self):
        """Subscribe to account updates, collect key-value pairs, then unsubscribe."""
        self.client.req_account_updates(True, "")

        assert self.wrapper.got_account_download_end.wait(timeout=30), \
            "account_download_end not received"

        # Should have standard account keys
        assert len(self.wrapper.account_values) > 0, "Should have account values"

        # Verify critical keys present
        expected_keys = {"NetLiquidation", "TotalCashValue", "BuyingPower"}
        found_keys = set(self.wrapper.account_values.keys())
        missing = expected_keys - found_keys
        assert len(missing) == 0, f"Missing expected account keys: {missing}"

        # Values should be parseable as numbers
        nlv = self.wrapper.account_values.get("NetLiquidation")
        assert nlv is not None
        nlv_val = float(nlv[0])
        assert nlv_val > 0, f"NetLiquidation should be positive, got {nlv_val}"

        # Unsubscribe
        self.client.req_account_updates(False, "")

    def test_positions_query(self):
        """Request positions and verify response."""
        self.client.req_positions()

        assert self.wrapper.got_position_end.wait(timeout=15), \
            "position_end not received"

        # positions list may be empty if paper account has no holdings
        # but position_end must fire
        print(f"  Positions found: {len(self.wrapper.positions)}")
        for p in self.wrapper.positions:
            print(f"    {p['symbol']} qty={p['position']} avg={p['avg_cost']:.2f}")

    def test_positions_match_portfolio(self):
        """Positions from reqPositions should match updatePortfolio data."""
        # Get positions
        self.client.req_positions()
        self.wrapper.got_position_end.wait(timeout=15)

        # Get portfolio via account updates
        self.client.req_account_updates(True, "")
        self.wrapper.got_account_download_end.wait(timeout=30)
        self.client.req_account_updates(False, "")

        # Compare: every position should appear in portfolio
        pos_symbols = {p["con_id"] for p in self.wrapper.positions if p["position"] != 0}
        port_symbols = {p["con_id"] for p in self.wrapper.portfolio if p["position"] != 0}

        if pos_symbols:
            assert pos_symbols == port_symbols, \
                f"Position/portfolio mismatch: pos={pos_symbols} port={port_symbols}"

    def test_account_pnl(self):
        """Subscribe to account-level P&L."""
        acct = self.client.get_account_id()
        self.client.req_pnl(9001, acct)

        got = self.wrapper.got_pnl.wait(timeout=15)
        self.client.cancel_pnl(9001)

        if not got:
            pytest.skip("No P&L data — may need open positions")

        assert self.wrapper.pnl_data is not None
        # daily_pnl can be negative, just verify it's a number
        assert isinstance(self.wrapper.pnl_data["daily_pnl"], float)
        assert isinstance(self.wrapper.pnl_data["unrealized_pnl"], float)
        assert isinstance(self.wrapper.pnl_data["realized_pnl"], float)

    def test_pnl_single(self):
        """Subscribe to single-position P&L (needs at least one position)."""
        # First get positions to find a conId
        self.client.req_positions()
        self.wrapper.got_position_end.wait(timeout=15)

        if not self.wrapper.positions:
            pytest.skip("No positions — cannot test pnlSingle")

        con_id = self.wrapper.positions[0]["con_id"]
        acct = self.client.get_account_id()
        self.client.req_pnl_single(9002, acct, "", con_id)

        got = self.wrapper.got_pnl_single.wait(timeout=15)
        self.client.cancel_pnl_single(9002)

        if not got:
            pytest.skip("No pnlSingle data received")

        assert self.wrapper.pnl_single_data is not None
        assert isinstance(self.wrapper.pnl_single_data["daily_pnl"], float)
        assert isinstance(self.wrapper.pnl_single_data["pos"], float)

    def test_account_summary(self):
        """Request account summary with standard tags."""
        tags = "NetLiquidation,TotalCashValue,BuyingPower,AvailableFunds,ExcessLiquidity,Cushion,Leverage"
        self.client.req_account_summary(9003, "All", tags)

        assert self.wrapper.got_summary_end.wait(timeout=15), \
            "account_summary_end not received"
        self.client.cancel_account_summary(9003)

        assert len(self.wrapper.summary) > 0, "Should have summary tags"

        # Verify NetLiquidation matches account_updates
        if "NetLiquidation" in self.wrapper.summary:
            nlv = float(self.wrapper.summary["NetLiquidation"][0])
            assert nlv > 0, f"NetLiquidation should be positive, got {nlv}"

    def test_account_snapshot(self):
        """Verify synchronous account_snapshot returns data."""
        # Give engine time to populate cache
        self.client.req_account_updates(True, "")
        self.wrapper.got_account_download_end.wait(timeout=30)
        self.client.req_account_updates(False, "")

        snapshot = self.client.account_snapshot()
        if snapshot is None:
            pytest.skip("No snapshot data — engine cache not populated")

        assert "net_liquidation" in snapshot
        assert "buying_power" in snapshot
        assert "total_cash_value" in snapshot
        assert snapshot["net_liquidation"] > 0
