"""IBX Test #113 : 5 concurrent scanners + filtered scanner + symbol search.

Tests reqScannerParameters, 5 concurrent scanner subscriptions, filtered scanner
with price/volume constraints, and reqMatchingSymbols.

Run: pytest tests/python/test_issue_ib113.py -v -s
"""

import os, threading, time
import pytest
from ibx import EWrapper, EClient


pytestmark = pytest.mark.skipif(
    not (os.environ.get("IB_USERNAME") and os.environ.get("IB_PASSWORD")),
    reason="IB_USERNAME and IB_PASSWORD not set",
)


class ScannerSubscription:
    """Minimal scanner subscription matching ibapi interface."""
    def __init__(self, scan_code="TOP_PERC_GAIN", num_rows=25,
                 instrument="STK", location="STK.US.MAJOR"):
        self.scanCode = scan_code
        self.numberOfRows = num_rows
        self.instrument = instrument
        self.locationCode = location
        self.abovePrice = 0
        self.belowPrice = 0
        self.aboveVolume = 0


class MultiScannerWrapper(EWrapper):
    def __init__(self):
        super().__init__()
        self.connected = threading.Event()
        self.lock = threading.Lock()

        # Scanner params
        self.scanner_xml = None
        self.got_scanner_params = threading.Event()

        # Scanner results per req_id
        self.scanner_results = {}  # req_id -> [(rank, contract_details)]
        self.got_scanner_end = {}  # req_id -> Event

        # Symbol search
        self.symbol_matches = []
        self.got_symbols = threading.Event()

    def next_valid_id(self, order_id):
        self.connected.set()

    def managed_accounts(self, accounts_list):
        pass

    def connect_ack(self):
        pass

    def scanner_parameters(self, xml):
        self.scanner_xml = xml
        self.got_scanner_params.set()

    def scanner_data(self, req_id, rank, contract_details, distance,
                     benchmark, projection, legs_str):
        with self.lock:
            self.scanner_results.setdefault(req_id, []).append(
                (rank, contract_details))

    def scanner_data_end(self, req_id):
        ev = self.got_scanner_end.get(req_id)
        if ev:
            ev.set()

    def symbol_samples(self, req_id, contract_descriptions):
        with self.lock:
            self.symbol_matches = contract_descriptions
        self.got_symbols.set()

    def error(self, req_id, error_code, error_string, advanced_order_reject_json=""):
        if error_code not in (2104, 2106, 2158, 162):
            print(f"  [error] reqId={req_id} code={error_code}: {error_string}")


class TestMultiScanner:
    """Issue #113 : Scanner parameters, concurrent scanners, symbol search."""

    @pytest.fixture(autouse=True)
    def setup_connection(self):
        self.wrapper = MultiScannerWrapper()
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
        """reqScannerParameters returns large XML catalog."""
        self.client.req_scanner_parameters()
        assert self.wrapper.got_scanner_params.wait(timeout=30), \
            "scanner_parameters not received"

        xml = self.wrapper.scanner_xml
        assert xml is not None
        print(f"  Scanner params XML: {len(xml)} bytes")
        assert len(xml) > 10000, "Scanner XML should be substantial (>10KB)"

        # Count scan types
        scan_count = xml.lower().count("<scantype")
        if scan_count == 0:
            scan_count = xml.lower().count("scancode")
        print(f"  Scan type references: {scan_count}")

    def test_5_concurrent_scanners(self):
        """Run 5 different scanner types concurrently."""
        scan_types = [
            "TOP_PERC_GAIN",
            "TOP_PERC_LOSE",
            "MOST_ACTIVE",
            "HOT_BY_VOLUME",
            "HIGH_OPEN_GAP",
        ]

        base_req = 10001
        for i, scan_code in enumerate(scan_types):
            req_id = base_req + i
            self.wrapper.got_scanner_end[req_id] = threading.Event()
            sub = ScannerSubscription(scan_code=scan_code, num_rows=25)
            self.client.req_scanner_subscription(req_id, sub)

        # Wait for all to complete
        results_received = 0
        for i, scan_code in enumerate(scan_types):
            req_id = base_req + i
            got = self.wrapper.got_scanner_end[req_id].wait(timeout=30)
            self.client.cancel_scanner_subscription(req_id)

            results = self.wrapper.scanner_results.get(req_id, [])
            if got and results:
                results_received += 1
                symbols = []
                for rank, cd in results[:5]:
                    sym = cd.contract.symbol if hasattr(cd, "contract") and hasattr(cd.contract, "symbol") else "?"
                    symbols.append(sym)
                print(f"  {scan_code}: {len(results)} results — {', '.join(symbols)}...")
            else:
                print(f"  {scan_code}: no results")

        assert results_received >= 3, \
            f"Expected at least 3 scanners with results, got {results_received}"

    def test_filtered_scanner(self):
        """Scanner with price/volume filter (TOP_PERC_GAIN, $10-$500, >1M volume)."""
        req_id = 10010
        self.wrapper.got_scanner_end[req_id] = threading.Event()

        sub = ScannerSubscription(scan_code="TOP_PERC_GAIN", num_rows=50)
        sub.abovePrice = 10.0
        sub.belowPrice = 500.0
        sub.aboveVolume = 1000000

        self.client.req_scanner_subscription(req_id, sub)
        got = self.wrapper.got_scanner_end[req_id].wait(timeout=30)
        self.client.cancel_scanner_subscription(req_id)

        if not got:
            pytest.skip("Filtered scanner returned no data")

        results = self.wrapper.scanner_results.get(req_id, [])
        print(f"  Filtered scanner: {len(results)} results")

        symbols = []
        for rank, cd in results[:10]:
            sym = cd.contract.symbol if hasattr(cd, "contract") and hasattr(cd.contract, "symbol") else "?"
            symbols.append(sym)
        if symbols:
            print(f"    Top: {', '.join(symbols)}")

    def test_matching_symbols(self):
        """reqMatchingSymbols("TSLA") returns cross-exchange matches."""
        self.client.req_matching_symbols(10020, "TSLA")

        got = self.wrapper.got_symbols.wait(timeout=15)
        if not got:
            pytest.skip("No symbol matches returned")

        matches = self.wrapper.symbol_matches
        print(f"  TSLA matches: {len(matches)}")

        for m in matches[:5]:
            if hasattr(m, "contract"):
                c = m.contract
                print(f"    {c.symbol} ({c.sec_type}) {c.exchange}/{c.currency} conId={c.con_id}")
            else:
                print(f"    {m}")

        assert len(matches) > 0, "Should have TSLA matches"
