"""IBX Test #104: Options greeks streaming via market data (AAPL).

Subscribe to option market data with genericTickList for greeks,
verify tickOptionComputation callbacks fire with IV, delta, gamma, etc.

Note: reqSecDefOptParams is not yet implemented (stub). This test uses
hardcoded ATM option conIds from wire capture.

Run: pytest tests/python/test_issue_104.py -v -s
"""

import os, threading, time
import pytest
from ibx import EWrapper, EClient, Contract

pytestmark = pytest.mark.skipif(
    not (os.environ.get("IB_USERNAME") and os.environ.get("IB_PASSWORD")),
    reason="IB_USERNAME and IB_PASSWORD not set",
)

AAPL_CON_ID = 265598


def make_aapl():
    c = Contract()
    c.con_id = AAPL_CON_ID
    c.symbol = "AAPL"
    c.sec_type = "STK"
    c.exchange = "SMART"
    c.currency = "USD"
    return c


def make_option(con_id, symbol, right, strike, expiry):
    c = Contract()
    c.con_id = con_id
    c.symbol = symbol
    c.sec_type = "OPT"
    c.exchange = "SMART"
    c.currency = "USD"
    c.right = right
    c.strike = strike
    c.last_trade_date_or_contract_month = expiry
    c.multiplier = "100"
    return c


class OptionsWrapper(EWrapper):
    def __init__(self):
        super().__init__()
        self.connected = threading.Event()
        self.lock = threading.Lock()

        # Underlying ticks
        self.underlying_price = 0.0
        self.got_underlying = threading.Event()

        # Option greeks
        self.greeks = {}  # req_id -> list of (tick_type, iv, delta, gamma, vega, theta, und_price, opt_price)
        self.got_greeks = threading.Event()

        # Option ticks
        self.option_ticks = {}  # req_id -> {bid, ask, last}
        self.got_option_tick = threading.Event()

    def next_valid_id(self, order_id):
        self.connected.set()

    def managed_accounts(self, accounts_list):
        pass

    def connect_ack(self):
        pass

    def tick_price(self, req_id, tick_type, price, attrib):
        if price > 0:
            with self.lock:
                if req_id == 1000:
                    if tick_type in (1, 2, 4):
                        self.underlying_price = price
                        self.got_underlying.set()
                else:
                    self.option_ticks.setdefault(req_id, {})
                    if tick_type == 1:
                        self.option_ticks[req_id]["bid"] = price
                    elif tick_type == 2:
                        self.option_ticks[req_id]["ask"] = price
                    elif tick_type == 4:
                        self.option_ticks[req_id]["last"] = price
                    self.got_option_tick.set()

    def tick_size(self, req_id, tick_type, size):
        pass

    def tick_option_computation(self, req_id, tick_type, tick_attrib,
                                 implied_vol, delta, opt_price, pv_dividend,
                                 gamma, vega, theta, und_price):
        with self.lock:
            self.greeks.setdefault(req_id, []).append({
                "tick_type": tick_type,
                "implied_vol": implied_vol,
                "delta": delta,
                "gamma": gamma,
                "vega": vega,
                "theta": theta,
                "und_price": und_price,
                "opt_price": opt_price,
            })
        self.got_greeks.set()

    def error(self, req_id, error_code, error_string, advanced_order_reject_json=""):
        if error_code not in (2104, 2106, 2119, 2158):
            print(f"  [error] reqId={req_id} code={error_code}: {error_string}")


class TestOptionsGreeks:
    """Issue #104: Option greek streaming via tickOptionComputation."""

    @pytest.fixture(autouse=True)
    def setup_connection(self):
        self.wrapper = OptionsWrapper()
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

    def _get_underlying_price(self):
        self.client.req_mkt_data(1000, make_aapl(), "", False)
        got = self.wrapper.got_underlying.wait(timeout=30)
        self.client.cancel_mkt_data(1000)
        return self.wrapper.underlying_price if got else None

    def test_option_greek_streaming(self):
        """Subscribe to call option market data with greek ticks."""
        price = self._get_underlying_price()
        if price is None:
            pytest.skip("No underlying price — market may be closed")

        # Use ATM strike nearest to current price (round to nearest 5)
        strike = round(price / 5) * 5
        print(f"  AAPL underlying: {price}, ATM strike: {strike}")

        # Request contract details to find the option conId
        # Since reqSecDefOptParams is a stub, we'll request option market data
        # with a constructed contract — the engine will qualify it
        call = Contract()
        call.symbol = "AAPL"
        call.sec_type = "OPT"
        call.exchange = "SMART"
        call.currency = "USD"
        call.right = "C"
        call.strike = strike
        call.last_trade_date_or_contract_month = "20260424"
        call.multiplier = "100"

        # Subscribe with greek tick types: 100=optVolume, 101=optOI, 104=histVol, 106=optIV
        self.client.req_mkt_data(2001, call, "100,101,104,106", False)

        got = self.wrapper.got_greeks.wait(timeout=30)

        # Also try a put
        put = Contract()
        put.symbol = "AAPL"
        put.sec_type = "OPT"
        put.exchange = "SMART"
        put.currency = "USD"
        put.right = "P"
        put.strike = strike
        put.last_trade_date_or_contract_month = "20260424"
        put.multiplier = "100"

        self.client.req_mkt_data(2002, put, "100,101,104,106", False)
        time.sleep(10)  # Allow greeks to stream

        self.client.cancel_mkt_data(2001)
        self.client.cancel_mkt_data(2002)

        if not got:
            pytest.skip("No greek data — usopt farm may not be available")

        # Verify call greeks
        call_greeks = self.wrapper.greeks.get(2001, [])
        assert len(call_greeks) > 0, "Should have call greek data"

        g = call_greeks[-1]
        print(f"  Call greeks: IV={g['implied_vol']:.4f} delta={g['delta']:.4f} "
              f"gamma={g['gamma']:.4f} vega={g['vega']:.4f} theta={g['theta']:.4f}")

        # Call delta should be positive (0 to 1 for ATM)
        if g["delta"] != -2.0:  # -2.0 is sentinel for "not computed"
            assert 0 < g["delta"] < 1, f"Call delta {g['delta']} out of range"

        # IV should be positive
        if g["implied_vol"] != -2.0:
            assert g["implied_vol"] > 0, f"IV {g['implied_vol']} should be positive"

        # Verify put greeks
        put_greeks = self.wrapper.greeks.get(2002, [])
        if put_greeks:
            pg = put_greeks[-1]
            print(f"  Put greeks:  IV={pg['implied_vol']:.4f} delta={pg['delta']:.4f} "
                  f"gamma={pg['gamma']:.4f} vega={pg['vega']:.4f} theta={pg['theta']:.4f}")
            # Put delta should be negative (-1 to 0 for ATM)
            if pg["delta"] != -2.0:
                assert -1 < pg["delta"] < 0, f"Put delta {pg['delta']} out of range"
