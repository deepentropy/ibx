"""ibx#421: values of the logon reply the reference applies: the clock
offset of the current time, the matching symbols feature, the historical
data years limit and the version cutoff warning."""

import time

from ibx import Contract, EClient, EWrapper

NO_SECDEFTA = "Error validating request.-'ce' : cause - Failed to request matching symbols"
YEARS = ("Error validating request.-'bM' : cause - Historical data request for 2 year(s) rejected. "
         "Max API Backfill Years=1")


class Recorder(EWrapper):
    def __init__(self):
        super().__init__()
        self.events = []

    def current_time(self, time):
        self.events.append(("time", time))

    def error(self, req_id, error_code, error_string, advanced_order_reject_json=""):
        self.events.append(("error", req_id, error_code, error_string))


def stock():
    c = Contract()
    c.con_id = 756733
    c.symbol = "SPY"
    c.sec_type = "STK"
    c.exchange = "SMART"
    c.currency = "USD"
    return c


def connected(features="APIELOG,SECDEFTA", offset=None, years=199, cutoff=None, date=None):
    w = Recorder()
    c = EClient(w)
    c._test_connect("TEST123")
    c._test_apply_logon(offset, features, years, cutoff, date)
    return c, w


def test_current_time_adds_the_logon_offset():
    c, w = connected(offset=60_000)
    c.req_current_time()
    (kind, t), = w.events
    assert kind == "time"
    assert abs(t - int(time.time()) - 60) <= 1


def test_matching_symbols_refused_without_the_feature():
    c, w = connected(features="APIELOG")
    c.req_matching_symbols(8, "AAPL")
    c._test_dispatch_once()
    assert ("error", 8, 321, NO_SECDEFTA) in w.events


def test_years_above_the_logon_limit_are_refused():
    c, w = connected(years=1)
    c.req_historical_data(5, stock(), "", "2 Y", "1 day", "TRADES", 1, 1, False, [])
    c._test_dispatch_once()
    assert ("error", 5, 321, YEARS) in w.events


def test_version_cutoff_warning():
    c, w = connected(cutoff="10411", date="20261201")
    c._test_dispatch_once()
    warnings = [e for e in w.events if e[0] == "error" and e[2] == 2172]
    assert len(warnings) == 1 and warnings[0][1] == -1
    assert "1040.1" in warnings[0][3] and "20261201" in warnings[0][3] and "1041.1" in warnings[0][3]
