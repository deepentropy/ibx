"""ibx#489: the comparison of a live ibx run with the reference's callbacks
(tests/python/differential.py). No network."""

import csv
import json
import os

import pytest

import differential as d

FIXTURES = os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "fixtures", "gw1040", "scenarios")

ORDER = {"orderId": 1, "clientId": 7, "permId": 111, "action": "BUY", "totalQuantity": "1", "orderType": "LMT",
         "lmtPrice": 100.0, "tif": "DAY"}
CONTRACT = {"conId": 265598, "symbol": "AAPL", "secType": "STK", "exchange": "SMART", "currency": "USD"}


def run(first_id, perm_id, account="DU1234567", price=100.0):
    order = {**ORDER, "orderId": first_id, "permId": perm_id, "lmtPrice": price, "account": account}
    return [
        ["connectAck"],
        ["managedAccounts", account],
        ["nextValidId", first_id],
        ["openOrder", first_id, CONTRACT, order, {"status": "Submitted"}],
        ["orderStatus", first_id, "Submitted", "0", "1", 0.0, perm_id, 0, 0.0, 7, "", 0.0],
        ["tickPrice", 5, 1, 100.0 + first_id, {}],
        ["orderStatus", first_id, "Cancelled", 0.0, 1.0, 0.0, perm_id, 0, 0.0, 7, "", 0.0],
        ["error", first_id, 1700000000000 + first_id, 202, "Order Canceled - reason:", ""],
        ["error", first_id + 5000, None, 10147, f"OrderId {first_id + 5000} that needs to be cancelled is not found.", ""],
        ["connectionClosed"],
    ]


def kinds(rows):
    return [(r["kind"], r["callback"], r["key"], r["field"]) for r in rows]


def test_two_sessions_of_the_same_behaviour_have_no_difference():
    # Other order ids, permId, account, prices, tick values, number forms and error times.
    assert d.compare(run(1, 111), run(31, 999, "DU7654321", 250.5)) == []


def test_a_field_that_differs_is_one_row():
    ours = run(1, 111)
    ours[4][2] = "PreSubmitted"
    rows = d.compare(run(1, 111), ours)
    assert kinds(rows) == [("field", "orderStatus", "{id+0}", "status")]
    assert (rows[0]["reference"], rows[0]["ibx"]) == ("Submitted", "PreSubmitted")


def test_a_field_of_an_object_is_named_by_its_path():
    ours = run(1, 111)
    ours[3][3] = {**ours[3][3], "tif": "GTC"}
    rows = d.compare(run(1, 111), ours)
    assert kinds(rows) == [("field", "openOrder", "{id+0}", "order.tif")] and rows[0]["known"] == ""
    theirs = run(1, 111)
    theirs[3][3] = {**theirs[3][3], "ocaType": 3}
    assert [(r["field"], r["known"]) for r in d.compare(theirs, run(1, 111))] == [("order.ocaType", "")]


def test_callbacks_one_side_lacks():
    reference, ours = run(1, 111), run(1, 111)
    reference.insert(3, ["error", -1, None, 2104, "Market data farm connection is OK:usfarm", ""])
    ours.insert(3, ["openOrderEnd"])
    rows = d.compare(reference, ours)
    assert kinds(rows) == [("missing_in_ibx", "error", "-1|2104", ""), ("extra_in_ibx", "openOrderEnd", "", "")]
    assert rows[0]["known"] == "" and rows[1]["known"] == ""


def test_data_callbacks_are_compared_by_the_kinds_of_rows():
    reference, ours = run(1, 111), run(1, 111)
    reference += [["tickPrice", 5, 2, 1.0, {}], ["tickPrice", 5, 2, 2.0, {}]]
    ours += [["tickSize", 5, 0, 3.0]]
    assert kinds(d.compare(reference, ours)) == [("shape", "tickPrice", "5|2", ""), ("shape", "tickSize", "5|0", "")]


def test_a_price_that_is_set_against_one_that_is_not():
    ours = run(1, 111)
    ours[3][3] = {**ours[3][3], "lmtPrice": d.MAX_DOUBLE}
    rows = d.compare(run(1, 111), ours)
    assert [(r["field"], r["reference"], r["ibx"]) for r in rows] == [("order.lmtPrice", "{price}", "MAX")]
    assert kinds(d.compare(run(1, 111), run(1, 111, price=99.0), strict_prices=True))[0][3] == "order.lmtPrice"


def test_the_quantity_of_an_order_message_is_not_taken_for_its_id():
    text = "Order Message:\nBUY 1 AAPL NASDAQ.NMS"
    name, key, fields = d.comparable(["error", 1, None, 399, text, ""], d.Session([["nextValidId", 1]]), False)
    assert fields["errorString"] == text and key == "{id+0}|399"


def test_an_open_order_listing_is_compared_whatever_its_order():
    def listing(ids):
        out = [["nextValidId", 1]]
        for i in ids:
            out += [["openOrder", i, CONTRACT, {**ORDER, "orderId": i}, {"status": "Submitted"}],
                    ["orderStatus", i, "Submitted", 0.0, 1.0, 0.0, 111, 0, 0.0, 7, "", 0.0]]
        return out + [["openOrderEnd"]]

    # The reference lists by a hash of the permId (ibx#522): 1, 2, 3 on one side, 3, 2, 1 on the other.
    assert d.compare(listing([1, 2, 3]), listing([3, 2, 1])) == []
    assert kinds(d.compare(listing([1, 2, 3]), listing([3, 1]))) == [
        ("missing_in_ibx", "openOrder", "{id+1}", ""), ("missing_in_ibx", "orderStatus", "{id+1}", "")]
    # Callbacks of orders outside a listing keep their order.
    live = listing([2, 1])[:-1]
    assert d.compare(live, listing([1, 2])[:-1]) != []


def test_connection_notices_after_the_connect_are_explained_by_the_session():
    def side(*notices):
        return [["nextValidId", 1]] + [["error", -1, None, code, text, ""] for code, text in notices] + [["openOrderEnd"]]

    connect = [(2104, "Market data farm connection is OK:usfarm"), (2106, "HMDS data farm connection is OK:ushmds"),
               (2158, "Sec-def data farm connection is OK:secdefil")]
    later = (2104, "Market data farm connection is OK:cashfarm")
    # A farm the reference had open and ibx had not: explained, not a failure.
    rows = d.compare(side(*connect, later), side(*connect))
    assert [(r["key"], r["known"]) for r in rows] == [("-1|2104", "session: state of the data connections")]
    # A notice of the connect that is missing is not explained (ibx#517).
    rows = d.compare(side(*connect), side(*connect[1:]))
    assert [(r["kind"], r["key"], r["known"]) for r in rows] == [("missing_in_ibx", "-1|2104", "")]


def test_an_industry_the_reference_kept_from_an_earlier_request_is_explained():
    theirs = [["contractDetails", 1, {"longName": "APPLE INC", "industry": "Technology", "category": "Computers"}]]
    ours = [["contractDetails", 1, {"longName": "APPLE INC"}]]
    rows = d.compare(theirs, ours)
    assert [r["field"] for r in rows] == ["contractDetails.category", "contractDetails.industry"]
    assert all(r["known"].startswith("session: company data") for r in rows)
    # Another value, or a missing one on the reference's side, is not explained.
    ours[0][2]["industry"] = "Energy"
    assert [r["known"] for r in d.compare(theirs, ours) if r["field"].endswith("industry")] == [""]


def test_a_scenario_on_the_login_of_its_batch_has_no_connect_notices(tmp_path):
    reference, ours = tmp_path / "reference", tmp_path / "ibx"
    reference.mkdir(), ours.mkdir()
    notices = [["error", -1, None, 2104, "Market data farm connection is OK:usfarm", ""],
               ["error", -1, None, 2172, "The version ...", ""]]
    theirs = run(1, 111)
    theirs[3:3] = notices
    write_run(reference, {"first": theirs, "later": theirs})
    write_run(ours, {"first": run(1, 111), "later": run(1, 111)})
    with open(ours / "run.jsonl", "w", encoding="utf-8") as fh:
        fh.write(json.dumps({"event": "scenario", "name": "first", "market_session": "rth"}) + "\n")
        fh.write(json.dumps({"event": "scenario", "name": "later", "market_session": "rth", "shared_login": True}) + "\n")
    rows, _ = d.report(str(ours), None, str(reference))
    known = {(r["scenario"], r["key"]): r["known"] for r in rows}
    # The scenario that opened the login is compared as before; the later one is explained.
    assert known[("first", "-1|2104")] == "" and known[("later", "-1|2104")] == d.ONE_LOGIN
    assert known[("later", "-1|2172")] == d.ONE_LOGIN


def test_sets_enum_internals_and_request_windows():
    theirs = [["securityDefinitionOptionParameter", 1, "SMART", 265598, "AAPL", "100", "{'b', 'a'}", "{2.0, 1.0}"],
              ["historicalDataEnd", 2, "20260926 15:46:37 US/Eastern", "20260926 16:16:37 US/Eastern"],
              ["contractDetails", 3, {"longName": "X", "fundAssetType": {"_value_": 1, "__objclass__": "<cycle>"}}]]
    ours = [["securityDefinitionOptionParameter", 1, "SMART", 265598, "AAPL", "100", "{'a', 'b'}", "{1.0, 2.0}"],
            ["historicalDataEnd", 2, "20261008 16:10:40 US/Eastern", "20261008 16:40:40 US/Eastern"],
            ["contractDetails", 3, {"longName": "X"}]]
    assert d.compare(theirs, ours) == []
    ours[1][3] = "20261008 16:40:40"
    assert [r["field"] for r in d.compare(theirs, ours)] == ["end"]


def test_accounts_are_masked():
    assert d.mask_accounts('"DU1234567" U7654321 DUXXXXXXX x') == '"DUXXXXXXX" DUXXXXXXX DUXXXXXXX x'


def test_a_recorded_object_is_completed_with_the_unset_values():
    pytest.importorskip("ibapi")
    name, key, fields = d.comparable(["openOrder", 1, CONTRACT, ORDER, {"status": "Submitted"}],
                                     d.Session([["nextValidId", 1]]), False)
    assert fields["order.transmit"] is True and fields["order.auxPrice"] == "MAX"
    assert fields["order.orderId"] == "{id+0}" and fields["order.permId"] == "{set}"


def write_run(folder, calls):
    with open(os.path.join(folder, "events.jsonl"), "w", encoding="utf-8") as fh:
        for scenario, items in calls.items():
            for c in items:
                fh.write(json.dumps({"conn": scenario, "cb": c[0], "args": c[1:]}) + "\n")
    with open(os.path.join(folder, "run.jsonl"), "w", encoding="utf-8") as fh:
        for scenario in calls:
            fh.write(json.dumps({"event": "scenario", "name": scenario, "market_session": "rth"}) + "\n")


def test_a_recorded_scenario_against_itself(tmp_path):
    pytest.importorskip("ibapi")
    calls, session = d.read_recording(d.find_recording(FIXTURES, "lmt_cancel"))
    assert session == "closed" and any(c[0] == "openOrder" for c in calls)
    write_run(tmp_path, {"lmt_cancel": calls, "not_recorded": [["nextValidId", 1]], "setup": [["nextValidId", 1]]})
    rows, lines = d.report(str(tmp_path), FIXTURES)
    assert [(r["scenario"], r["kind"]) for r in rows] == [("not_recorded", "not_compared")]
    assert lines[0] == "lmt_cancel: 0 differences, 0 without an issue (reference closed, ibx rth)"


def test_the_report_file_and_the_exit_code(tmp_path, monkeypatch):
    # A row an open issue explains carries the issue and does not fail the run.
    monkeypatch.setattr(d, "KNOWN", [("ibx#1", lambda r: r["field"] == "order.ocaType")])
    reference, ours = tmp_path / "reference", tmp_path / "ibx"
    reference.mkdir(), ours.mkdir()
    theirs = run(1, 111)
    theirs[3][3] = {**theirs[3][3], "ocaType": 3}
    write_run(reference, {"s": theirs})
    write_run(ours, {"s": run(4, 222, "DU7654321")})
    out = tmp_path / "report.csv"
    assert d.main(["--ibx", str(ours), "--reference", str(reference), "--out", str(out)]) == 0
    with open(out, encoding="utf-8", newline="") as fh:
        rows = list(csv.DictReader(fh))
    assert list(rows[0]) == d.COLUMNS and len(rows) == 1
    assert (rows[0]["scenario"], rows[0]["field"], rows[0]["known"]) == ("s", "order.ocaType", "ibx#1")
    assert "DU7654321" not in out.read_text(encoding="utf-8")

    mine = run(4, 222)
    mine[4][2] = "Inactive"
    write_run(ours, {"s": mine})
    assert d.main(["--ibx", str(ours), "--reference", str(reference), "--out", str(out)]) == 1
