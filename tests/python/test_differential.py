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
    assert [(r["field"], r["known"]) for r in d.compare(theirs, run(1, 111))] == [("order.ocaType", "ibx#519")]


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


def test_the_report_file_and_the_exit_code(tmp_path):
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
    assert (rows[0]["scenario"], rows[0]["field"], rows[0]["known"]) == ("s", "order.ocaType", "ibx#519")
    assert "DU7654321" not in out.read_text(encoding="utf-8")

    mine = run(4, 222)
    mine[4][2] = "Inactive"
    write_run(ours, {"s": mine})
    assert d.main(["--ibx", str(ours), "--reference", str(reference), "--out", str(out)]) == 1
