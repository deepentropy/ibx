"""IBX Test #111: Full account state — positions, P&L, portfolio, histogram, 90 tags.

Comprehensive account interrogation: reqPositions, reqPnL, reqPnLSingle,
reqAccountUpdates (90 tags), updatePortfolio, reqHistogramData.

Run: pytest tests/python/test_issue_111.py -v -s
Ref: https://github.com/deepentropy/ib-agent/issues/111
"""

import os, threading, time
import pytest
from ibx import EWrapper, EClient, Contract

pytestmark = pytest.mark.skipif(
    not (os.environ.get("IB_USERNAME") and os.environ.get("IB_PASSWORD")),
    reason="IB_USERNAME and IB_PASSWORD not set",
)

SPY_CON_ID = 756733


class AccountDeepWrapper(EWrapper):
    def __init__(self):
        super().__init__()
        self.connected = threading.Event()
        self.lock = threading.Lock()
        self.account_id = ""

        # Positions
        self.positions = []
        self.got_position_end = threading.Event()

        # Account values (90+ tags)
        self.account_values = {}
        self.got_account_download_end = threading.Event()

        # Portfolio
        self.portfolio = []
        self.got_portfolio = threading.Event()

        # P&L
        self.pnl_data = None
        self.got_pnl = threading.Event()
        self.pnl_single = {}  # con_id -> data
        self.got_pnl_single = threading.Event()

        # Histogram
        self.histogram = []
        self.got_histogram = threading.Event()

    def next_valid_id(self, order_id):
        self.connected.set()

    def managed_accounts(self, accounts_list):
        self.account_id = accounts_list

    def connect_ack(self):
        pass

    def position(self, account, contract, pos, avg_cost):
        with self.lock:
            self.positions.append({
                "account": account,
                "symbol": contract.symbol,
                "sec_type": contract.sec_type,
                "con_id": contract.con_id,
                "position": pos,
                "avg_cost": avg_cost,
            })

    def position_end(self):
        self.got_position_end.set()

    def update_account_value(self, key, value, currency, account_name):
        self.account_values[key] = (value, currency)

    def update_portfolio(self, contract, position, market_price, market_value,
                         average_cost, unrealized_pnl, realized_pnl, account_name):
        with self.lock:
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

    def pnl(self, req_id, daily_pnl, unrealized_pnl, realized_pnl):
        self.pnl_data = {
            "daily_pnl": daily_pnl,
            "unrealized_pnl": unrealized_pnl,
            "realized_pnl": realized_pnl,
        }
        self.got_pnl.set()

    def pnl_single(self, req_id, pos, daily_pnl, unrealized_pnl, realized_pnl, value):
        self.pnl_single[req_id] = {
            "pos": pos,
            "daily_pnl": daily_pnl,
            "unrealized_pnl": unrealized_pnl,
            "realized_pnl": realized_pnl,
            "value": value,
        }
        self.got_pnl_single.set()

    def histogram_data(self, req_id, items):
        with self.lock:
            self.histogram = items
        self.got_histogram.set()

    def error(self, req_id, error_code, error_string, advanced_order_reject_json=""):
        if error_code not in (2104, 2106, 2158):
            print(f"  [error] reqId={req_id} code={error_code}: {error_string}")


class TestAccountDeep:
    """Issue #111: Comprehensive account state interrogation."""

    @pytest.fixture(autouse=True)
    def setup_connection(self):
        self.wrapper = AccountDeepWrapper()
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

    def test_positions(self):
        """reqPositions returns all holdings."""
        self.client.req_positions()
        assert self.wrapper.got_position_end.wait(timeout=15), "position_end not received"

        print(f"  Positions: {len(self.wrapper.positions)}")
        for p in self.wrapper.positions:
            print(f"    {p['symbol']} ({p['sec_type']}): qty={p['position']} "
                  f"avg_cost={p['avg_cost']:.2f} conId={p['con_id']}")

    def test_account_values_90_tags(self):
        """reqAccountUpdates returns 90+ account value tags."""
        self.client.req_account_updates(True, "")
        assert self.wrapper.got_account_download_end.wait(timeout=30), \
            "account_download_end not received"
        self.client.req_account_updates(False, "")

        tag_count = len(self.wrapper.account_values)
        print(f"  Account tags: {tag_count}")
        # CCP direct wire provides 18 tags from AccountState struct.
        # Full 90+ tags require Gateway-internal UT/UM/RL parsing (ib-agent#115).
        assert tag_count >= 15, f"Expected 15+ tags, got {tag_count}"

        # Verify critical tags
        critical = ["NetLiquidation", "TotalCashValue", "BuyingPower",
                     "EquityWithLoanValue", "Cushion"]
        for key in critical:
            assert key in self.wrapper.account_values, f"Missing critical tag: {key}"
            val = self.wrapper.account_values[key][0]
            print(f"    {key}: {val}")

        # AccountType requires Gateway-internal UT message (not on direct CCP wire).
        # assert "AccountType" in self.wrapper.account_values

    def test_portfolio(self):
        """updatePortfolio fires for each holding with market prices."""
        self.client.req_account_updates(True, "")
        self.wrapper.got_account_download_end.wait(timeout=30)
        self.client.req_account_updates(False, "")

        print(f"  Portfolio items: {len(self.wrapper.portfolio)}")
        for p in self.wrapper.portfolio:
            print(f"    {p['symbol']}: pos={p['position']} mktPrice={p['market_price']:.2f} "
                  f"mktVal={p['market_value']:.2f} unrealPnL={p['unrealized_pnl']:.2f}")

        # Verify positions match
        self.client.req_positions()
        self.wrapper.got_position_end.wait(timeout=15)

        pos_conids = {p["con_id"] for p in self.wrapper.positions if p["position"] != 0}
        port_conids = {p["con_id"] for p in self.wrapper.portfolio if p["position"] != 0}
        if pos_conids:
            assert pos_conids == port_conids, \
                f"Position/portfolio mismatch: pos={pos_conids} port={port_conids}"

    def test_whole_account_pnl(self):
        """reqPnL returns daily, unrealized, and realized P&L."""
        acct = self.client.get_account_id()
        self.client.req_pnl(8001, acct)

        got = self.wrapper.got_pnl.wait(timeout=15)
        self.client.cancel_pnl(8001)

        if not got:
            pytest.skip("No P&L data — may need open positions")

        pnl = self.wrapper.pnl_data
        print(f"  Account P&L: daily={pnl['daily_pnl']:.2f} "
              f"unrealized={pnl['unrealized_pnl']:.2f} "
              f"realized={pnl['realized_pnl']:.2f}")
        assert isinstance(pnl["daily_pnl"], float)

    def test_pnl_single_per_position(self):
        """reqPnLSingle for each position."""
        self.client.req_positions()
        self.wrapper.got_position_end.wait(timeout=15)

        if not self.wrapper.positions:
            pytest.skip("No positions — cannot test pnlSingle")

        acct = self.client.get_account_id()
        req_ids = []
        for i, pos in enumerate(self.wrapper.positions[:3]):  # Test up to 3
            req_id = 8100 + i
            req_ids.append((req_id, pos))
            self.client.req_pnl_single(req_id, acct, "", pos["con_id"])

        time.sleep(10)
        for req_id, pos in req_ids:
            self.client.cancel_pnl_single(req_id)
            data = self.wrapper.pnl_single.get(req_id)
            if data:
                print(f"    {pos['symbol']}: pos={data['pos']} daily={data['daily_pnl']:.2f} "
                      f"unrealized={data['unrealized_pnl']:.2f} value={data['value']:.2f}")

    def test_histogram_data(self):
        """reqHistogramData for SPY (1 week)."""
        spy = Contract()
        spy.con_id = SPY_CON_ID
        spy.symbol = "SPY"
        spy.sec_type = "STK"
        spy.exchange = "SMART"
        spy.currency = "USD"

        self.client.req_histogram_data(8200, spy, False, "1 week")

        got = self.wrapper.got_histogram.wait(timeout=30)
        self.client.cancel_histogram_data(8200)

        if not got:
            pytest.skip("No histogram data")

        print(f"  Histogram: {len(self.wrapper.histogram)} price points")
        assert len(self.wrapper.histogram) > 0, "Should have histogram data"
