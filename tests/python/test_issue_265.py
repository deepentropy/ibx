"""Regression tests for ibx#265: the Python execution filter.

`clientId` and `time` were parsed by the Rust core but never extracted from
the Python filter object, so executions the caller had explicitly filtered
out — another client's fills, or ones before the requested cutoff — were
replayed anyway.
"""
from ibx import EClient, EWrapper


class _Filter:
    """Stand-in for ibapi's ExecutionFilter: plain attributes, read by name."""

    def __init__(self, **kw):
        self.clientId, self.acctCode, self.time = 0, "", ""
        self.symbol, self.secType, self.exchange, self.side = "", "", "", ""
        for k, v in kw.items():
            setattr(self, k, v)


class _Counter(EWrapper):
    def __init__(self):
        super().__init__()
        self.rows = 0

    def exec_details(self, req_id, contract, execution):
        self.rows += 1


def _client_with_one_execution():
    w = _Counter()
    c = EClient(w)
    c._test_connect("T")
    c._test_map_instrument(90, 0)
    c._test_push_fill(0, 1, "BUY", 10.0, 5, 0)
    c._test_dispatch_once()
    w.rows = 0
    return c, w


def test_an_unfiltered_request_still_replays():
    c, w = _client_with_one_execution()
    c.req_executions(1, _Filter())
    assert w.rows == 1, "the baseline request must replay the stored execution"


def test_a_foreign_client_id_filters_everything_out():
    c, w = _client_with_one_execution()
    c.req_executions(1, _Filter(clientId=999999))
    assert w.rows == 0, f"another client's request must replay nothing, got {w.rows}"


def test_a_cutoff_after_the_execution_filters_it_out():
    c, w = _client_with_one_execution()
    c.req_executions(1, _Filter(time="20990101-00:00:00"))
    assert w.rows == 0, f"a future cutoff must replay nothing, got {w.rows}"
