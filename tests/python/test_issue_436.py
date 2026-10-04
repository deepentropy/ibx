"""ibx#436: contract details rows with every field of the reference.

Golden test: ibx's rows of the captured definition replies, field by field
against the rows the official client received from the gateway (paper,
26/09/2026, 28/09/2026 and 02/10/2026; fixture tests/fixtures/contract_details)."""

import json
import re
from datetime import datetime, timedelta, timezone
from decimal import Decimal
from pathlib import Path

from ibx import EClient, EWrapper

FIXTURE = Path(__file__).resolve().parents[1] / "fixtures" / "contract_details" / "golden_rows.json"
UNSET = 1.7976931348623157e308
UNSET_DECIMAL = Decimal("170141183460469231731687303715884105727")
# Paris time of both capture days (summer time).
PARIS = timezone(timedelta(hours=2))


class Recorder(EWrapper):
    def __init__(self):
        super().__init__()
        self.rows = []

    def contract_details(self, req_id, contract_details):
        self.rows.append(("contractDetails", contract_details))

    def bond_contract_details(self, req_id, contract_details):
        self.rows.append(("bondContractDetails", contract_details))


def snake(name):
    return re.sub(r"(?<=[a-z0-9])([A-Z])", r"_\1", name).lower()


def now_ms(ts):
    local = datetime.strptime(ts, "%d/%m/%Y %H:%M:%S.%f").replace(tzinfo=PARIS)
    return int(local.timestamp() * 1000)


def ibx_row(c, w, fixture, row):
    frames = fixture["frames"]
    expected = row["expected"]["contract"]
    c._test_push_reply_row(
        1, frames[row["secdef"]], expected["conId"], expected.get("exchange", ""), row["continuous"],
        [(ex, rid) for ex, rid in row["rules"]], [frames[f] for f in row["company"]],
        frames[row["schedule"]] if row["schedule"] else None, now_ms(row["ts"]))
    c._test_dispatch_once()
    assert len(w.rows) == 1, w.rows
    return w.rows.pop()


def compare(expected, d):
    out = []
    contract = expected["contract"]
    for name in ("conId", "symbol", "secType", "lastTradeDateOrContractMonth", "lastTradeDate", "right",
                 "multiplier", "exchange", "primaryExchange", "currency", "localSymbol", "tradingClass"):
        out.append(("contract." + name, contract.get(name, 0 if name == "conId" else ""),
                    getattr(d.contract, snake(name))))
    out.append(("contract.strike", contract.get("strike", 0.0), d.contract.strike))
    texts = ("marketName", "orderTypes", "validExchanges", "longName", "contractMonth", "industry", "category",
             "subcategory", "timeZoneId", "tradingHours", "liquidHours", "evRule", "underSymbol", "underSecType",
             "marketRuleIds", "realExpirationDate", "lastTradeTime", "stockType", "cusip", "ratings", "descAppend",
             "bondType", "couponType", "maturity", "issueDate", "nextOptionDate", "nextOptionType", "notes",
             "fundName", "fundFamily", "fundType")
    for name in texts:
        out.append((name, expected.get(name, ""), getattr(d, snake(name))))
    numbers = ("minTick", "priceMagnifier", "underConId", "evMultiplier", "aggGroup", "coupon")
    for name in numbers:
        out.append((name, float(expected.get(name, 0)), float(getattr(d, snake(name)))))
    flags = ("callable", "putable", "convertible", "nextOptionPartial", "fundClosed", "fundClosedForNewInvestors",
             "fundClosedForNewMoney")
    for name in flags:
        out.append((name, expected.get(name, False), getattr(d, snake(name))))
    for name in ("minSize", "sizeIncrement", "suggestedSizeIncrement"):
        # The official API's Decimals.
        out.append((name, Decimal(str(expected[name])) if name in expected else UNSET_DECIMAL, getattr(d, snake(name))))
    out.append(("secIdList", expected.get("secIdList", []), [{"tag": t.tag, "value": t.value} for t in d.sec_id_list or []]))
    out.append(("ineligibilityReasonList", expected.get("ineligibilityReasonList", []),
                [{"id_": r.id_, "description": r.description} for r in d.ineligibility_reason_list or []]))
    out.append(("fundDistributionPolicyIndicator", expected.get("fundDistributionPolicyIndicator", "None"),
                d.fund_distribution_policy_indicator or "None"))
    out.append(("fundAssetType", expected.get("fundAssetType", "None"), d.fund_asset_type or "None"))
    return out


def gap(row, name):
    """State the gateway had from before the capture window."""
    return (name == "marketRuleIds" and row["rules_incomplete"]) or (
        row["schedule_cached"] and name in ("timeZoneId", "tradingHours", "liquidHours", "lastTradeTime"))


def test_rows_match_the_captured_gateway_rows_field_by_field():
    fixture = json.loads(FIXTURE.read_text(encoding="utf-8"))
    mismatches = []
    for row in fixture["rows"]:
        w = Recorder()
        c = EClient(w)
        c._test_connect("TEST123")
        callback, d = ibx_row(c, w, fixture, row)
        assert callback == row["callback"], (row["req_id"], callback)
        for name, want, got in compare(row["expected"], d):
            same = abs(want - got) < 1e-12 if isinstance(want, float) else want == got
            if not same and not gap(row, name):
                mismatches.append(f"{row['scenario']} req {row['req_id']} conId {row['expected']['contract']['conId']}: "
                                  f"{name}: gateway {want!r} ibx {got!r}")
    assert len(fixture["rows"]) >= 140
    assert not mismatches, "\n".join(mismatches)
