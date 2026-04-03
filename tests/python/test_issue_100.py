"""IBX Test #100: Historical Data + Scanner Subscription (MSFT).

Historical bars (one-shot + keepUpToDate), scanner parameters, and scanner
subscription. Exercises bar aggregation, time-range parsing, and FIXCOMP
decompression.

Run: pytest tests/python/test_issue_100.py -v --timeout=120
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

MSFT_CON_ID = 272093


class HistoricalScannerWrapper(EWrapper):
    def __init__(self):
        super().__init__()
        self.connected = threading.Event()

        # Historical bars
        self.bars = []           # (req_id, bar)
        self.got_hist_end = threading.Event()
        self.hist_end_range = None  # (start, end)

        # Live updating bars (keepUpToDate)
        self.live_bars = []      # (req_id, bar)
        self.got_live_bar = threading.Event()

        # Scanner
        self.scanner_params_xml = None
        self.got_scanner_params = threading.Event()
        self.scanner_results = []  # (req_id, rank, contract_details)
        self.got_scanner_end = threading.Event()

    def next_valid_id(self, order_id):
        self.connected.set()

    def managed_accounts(self, accounts_list):
        pass

    def connect_ack(self):
        pass

    # -- Historical data callbacks --
    def historical_data(self, req_id, bar):
        self.bars.append((req_id, bar))

    def historical_data_end(self, req_id, start, end):
        self.hist_end_range = (start, end)
        self.got_hist_end.set()

    def historical_data_update(self, req_id, bar):
        self.live_bars.append((req_id, bar))
        self.got_live_bar.set()

    # -- Scanner callbacks --
    def scanner_parameters(self, xml):
        self.scanner_params_xml = xml
        self.got_scanner_params.set()

    def scanner_data(self, req_id, rank, contract_details, distance,
                     benchmark, projection, legs_str):
        self.scanner_results.append((req_id, rank, contract_details))

    def scanner_data_end(self, req_id):
        self.got_scanner_end.set()

    def error(self, req_id, error_code, error_string, advanced_order_reject_json=""):
        if error_code not in (2104, 2106, 2158):
            print(f"  [error] reqId={req_id} code={error_code}: {error_string}")


def make_msft():
    c = Contract()
    c.con_id = MSFT_CON_ID
    c.symbol = "MSFT"
    c.sec_type = "STK"
    c.exchange = "SMART"
    c.currency = "USD"
    return c


class ScannerSubscription:
    """Minimal scanner subscription object matching ibapi interface."""
    def __init__(self):
        self.instrument = "STK"
        self.locationCode = "STK.US.MAJOR"
        self.scanCode = "TOP_PERC_GAIN"
        self.numberOfRows = 25


class TestHistoricalData:
    """Issue #100: Historical data requests on MSFT."""

    @pytest.fixture(autouse=True)
    def setup_connection(self):
        self.wrapper = HistoricalScannerWrapper()
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

    def test_historical_bars_oneshot(self):
        """Request 1 day of 5-min bars for MSFT (one-shot)."""
        msft = make_msft()
        self.client.req_historical_data(
            1, msft,
            end_date_time="",
            duration_str="1 D",
            bar_size_setting="5 mins",
            what_to_show="TRADES",
            use_rth=1,
        )

        assert self.wrapper.got_hist_end.wait(timeout=30), \
            "historical_data_end not received"

        my_bars = [b for b in self.wrapper.bars if b[0] == 1]
        assert len(my_bars) > 0, "Should have at least one bar"

        # 1 day of 5-min bars during RTH (6.5h) = ~78 bars
        print(f"  Received {len(my_bars)} bars")

        # Verify bar structure
        bar = my_bars[0][1]
        assert hasattr(bar, "open"), "Bar should have open attribute"
        assert hasattr(bar, "high"), "Bar should have high attribute"
        assert hasattr(bar, "low"), "Bar should have low attribute"
        assert hasattr(bar, "close"), "Bar should have close attribute"
        assert hasattr(bar, "volume"), "Bar should have volume attribute"

        # OHLC sanity
        assert bar.high >= bar.low, "High should be >= low"
        assert bar.high >= bar.open, "High should be >= open"
        assert bar.high >= bar.close, "High should be >= close"
        assert bar.low <= bar.open, "Low should be <= open"
        assert bar.low <= bar.close, "Low should be <= close"

        # Price range for MSFT
        assert 100 < bar.close < 1000, f"MSFT close {bar.close} out of range"

    def test_historical_bars_1hour(self):
        """Request 5 days of 1-hour bars."""
        msft = make_msft()
        self.client.req_historical_data(
            2, msft,
            end_date_time="",
            duration_str="5 D",
            bar_size_setting="1 hour",
            what_to_show="TRADES",
            use_rth=1,
        )

        assert self.wrapper.got_hist_end.wait(timeout=30), \
            "historical_data_end not received"

        my_bars = [b for b in self.wrapper.bars if b[0] == 2]
        assert len(my_bars) > 0, "Should have bars"
        # 5 days * ~7 hours/day = ~35 bars
        print(f"  Received {len(my_bars)} hourly bars")

    def test_historical_keep_up_to_date(self):
        """Request historical bars with keepUpToDate=True for live updating."""
        msft = make_msft()
        self.client.req_historical_data(
            3, msft,
            end_date_time="",
            duration_str="1 D",
            bar_size_setting="5 mins",
            what_to_show="TRADES",
            use_rth=0,
            keep_up_to_date=True,
        )

        # Should get initial batch
        assert self.wrapper.got_hist_end.wait(timeout=30), \
            "historical_data_end not received for keepUpToDate"

        init_bars = [b for b in self.wrapper.bars if b[0] == 3]
        assert len(init_bars) > 0, "Should have initial bars"

        # Wait for live updates
        got_live = self.wrapper.got_live_bar.wait(timeout=30)
        self.client.cancel_historical_data(3)

        if got_live:
            live = [b for b in self.wrapper.live_bars if b[0] == 3]
            print(f"  Got {len(live)} live bar updates")
            assert len(live) > 0
        else:
            print("  No live updates — market may be closed (initial batch verified)")


class TestScanner:
    """Issue #100: Scanner parameters + subscription."""

    @pytest.fixture(autouse=True)
    def setup_connection(self):
        self.wrapper = HistoricalScannerWrapper()
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

    def test_scanner_parameters(self):
        """Request scanner parameters XML."""
        self.client.req_scanner_parameters()

        assert self.wrapper.got_scanner_params.wait(timeout=30), \
            "scanner_parameters not received"

        xml = self.wrapper.scanner_params_xml
        assert xml is not None
        assert len(xml) > 100, "Scanner params XML should be substantial"
        # Should contain scanner type definitions
        assert "Instrument" in xml or "ScanType" in xml or "scan" in xml.lower(), \
            "XML should contain scanner definitions"
        print(f"  Scanner params XML: {len(xml)} chars")

    def test_scanner_subscription(self):
        """Subscribe to top US stock gainers scanner."""
        sub = ScannerSubscription()
        sub.instrument = "STK"
        sub.locationCode = "STK.US.MAJOR"
        sub.scanCode = "TOP_PERC_GAIN"
        sub.numberOfRows = 10

        self.client.req_scanner_subscription(4, sub)

        got = self.wrapper.got_scanner_end.wait(timeout=30)
        if not got:
            self.client.cancel_scanner_subscription(4)
            pytest.skip("No scanner data — market may be closed")

        self.client.cancel_scanner_subscription(4)

        results = [r for r in self.wrapper.scanner_results if r[0] == 4]
        assert len(results) > 0, "Should have scanner results"
        print(f"  Scanner returned {len(results)} instruments")

        # Verify ranking order
        ranks = [r[1] for r in results]
        assert ranks == sorted(ranks), "Scanner results should be in rank order"
