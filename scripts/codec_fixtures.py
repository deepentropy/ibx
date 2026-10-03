"""Codec fixtures (ibx#486): slices of the four-leg reference scenarios with
the API side decoded, for the golden codec tests.

Each fixture is one JSONL file under tests/fixtures/gw1040/codec/:

- line 1, the header: `format` (`codec/1`), `scenario`, `area`, `source` (the
  four-leg file), `capture_date` (dd/mm/yyyy), `market_session`,
  `machine_zone` (the zone of the machine that ran the gateway), `notes`;
- then the kept records of the source in their order, with `seq`, `leg`,
  `conn`, `raw_b64` and the message type (`msg_type` for a frame, `msg_name`
  for an API message), plus the API side decoded with the official client
  library:
  - `api_out` (client to gateway): `request`, the request's fields; for
    placeOrder the order and the contract as the client library reads them
    back (`decodeOrder`, `decodeContract`), only the fields that differ
    from a new object;
  - `api_in` (gateway to client): `callbacks`, the wrapper calls the client
    library makes for the message, `[name, arg, ...]` (an object argument
    holds only its fields that differ from a new object).

Run with the official client library 10.40 or later (protobuf messages):

    uv run --project <dir with ibapi> python scripts/codec_fixtures.py \
        --captures <captures dir> [--only name]
"""

import argparse
import base64
import json
import sys
from decimal import Decimal
from pathlib import Path

from google.protobuf.json_format import MessageToDict
from ibapi import decoder_utils
from ibapi.common import PROTOBUF_MSG_ID
from ibapi.decoder import Decoder
from ibapi.wrapper import EWrapper
from ibapi.comm import read_fields

OUT = Path(__file__).resolve().parent.parent / "tests" / "fixtures" / "gw1040" / "codec"

# The request messages and their protobuf class.
REQUESTS = {
    "REQ_MKT_DATA": "MarketDataRequest",
    "CANCEL_MKT_DATA": "CancelMarketData",
    "REQ_MARKET_DATA_TYPE": "MarketDataTypeRequest",
    "REQ_SMART_COMPONENTS": "SmartComponentsRequest",
    "CANCEL_ORDER": "CancelOrderRequest",
    "REQ_CONTRACT_DATA": "ContractDataRequest",
    "REQ_HISTORICAL_DATA": "HistoricalDataRequest",
    "CANCEL_HISTORICAL_DATA": "CancelHistoricalData",
    "REQ_HEAD_TIMESTAMP": "HeadTimestampRequest",
    "CANCEL_HEAD_TIMESTAMP": "CancelHeadTimestamp",
    "REQ_HISTORICAL_TICKS": "HistoricalTicksRequest",
    "REQ_SEC_DEF_OPT_PARAMS": "SecDefOptParamsRequest",
    "REQ_MATCHING_SYMBOLS": "MatchingSymbolsRequest",
    "REQ_ACCT_DATA": "AccountDataRequest",
    "REQ_POSITIONS": "PositionsRequest",
    "CANCEL_POSITIONS": "CancelPositions",
    "REQ_PNL": "PnLRequest",
    "REQ_PNL_SINGLE": "PnLSingleRequest",
    "REQ_ACCOUNT_SUMMARY": "AccountSummaryRequest",
    "CANCEL_ACCOUNT_SUMMARY": "CancelAccountSummary",
    "REQ_OPEN_ORDERS": "OpenOrdersRequest",
    "REQ_ALL_OPEN_ORDERS": "AllOpenOrdersRequest",
    "REQ_AUTO_OPEN_ORDERS": "AutoOpenOrdersRequest",
    "REQ_EXECUTIONS": "ExecutionRequest",
    "REQ_MKT_DEPTH": "MarketDepthRequest",
    "CANCEL_MKT_DEPTH": "CancelMarketDepth",
    "REQ_TICK_BY_TICK_DATA": "TickByTickRequest",
    "CANCEL_TICK_BY_TICK_DATA": "CancelTickByTick",
    "REQ_NEWS_PROVIDERS": "NewsProvidersRequest",
    "START_API": "StartApiRequest",
    "REQ_SCANNER_SUBSCRIPTION": "ScannerSubscriptionRequest",
    "CANCEL_SCANNER_SUBSCRIPTION": "CancelScannerSubscription",
    "REQ_HISTOGRAM_DATA": "HistogramDataRequest",
    "REQ_REAL_TIME_BARS": "RealTimeBarsRequest",
    "CANCEL_REAL_TIME_BARS": "CancelRealTimeBars",
}

UNSET_FLOAT = sys.float_info.max
DEFAULTS = {}


def plain(v):
    """A JSON value of an argument of the client library."""
    if isinstance(v, Decimal):
        return str(v)
    if isinstance(v, float):
        if v != v:
            return "NaN"
        if v in (float("inf"), float("-inf")):
            return "Infinity" if v > 0 else "-Infinity"
        return "MAX" if v == UNSET_FLOAT else v
    if isinstance(v, (str, int, bool)) or v is None:
        return v
    if isinstance(v, (list, tuple)):
        return [plain(x) for x in v]
    if isinstance(v, dict):
        return {str(k): plain(x) for k, x in v.items()}
    if hasattr(v, "__dict__"):
        cls = type(v)
        try:
            base = DEFAULTS.setdefault(cls, vars(cls()) if cls.__init__ is not object.__init__ else {})
        except TypeError:
            base = {}
        out = {"_type": cls.__name__} if cls.__name__.endswith("Condition") else {}
        for k, x in vars(v).items():
            if k.startswith("_") or (k in base and base[k] == x and not isinstance(x, (list, dict))) or x == [] or x is None:
                continue
            out[k] = plain(x)
        return out
    return repr(v)


class Recorder(EWrapper):
    """Every wrapper call, in order."""

    def __init__(self):
        super().__init__()
        self.calls = []


def _recording(name):
    def call(self, *args):
        self.calls.append([name] + [plain(a) for a in args])
    return call


for _name in dir(EWrapper):
    if _name.startswith("_") or _name.endswith("ProtoBuf") or not callable(getattr(EWrapper, _name)):
        continue
    setattr(Recorder, _name, _recording(_name))


def callbacks(body: bytes):
    """The wrapper calls of one API message (its body after the length)."""
    rec = Recorder()
    dec = Decoder(rec, 214)
    msg_id = int.from_bytes(body[:4], "big")
    text = body[4:]
    if msg_id > PROTOBUF_MSG_ID:
        dec.processProtoBuf(text, msg_id - PROTOBUF_MSG_ID)
    else:
        dec.interpret(read_fields(text), msg_id)
    # The error time is the gateway's clock: not part of the callback.
    for c in rec.calls:
        if c[0] == "error" and len(c) >= 3:
            c[2] = None
    return rec.calls


def request(name: str, body: bytes):
    """The fields of one API request."""
    from importlib import import_module
    payload = body[4:]
    if name == "PLACE_ORDER":
        m = import_module("ibapi.protobuf.PlaceOrderRequest_pb2").PlaceOrderRequest()
        m.ParseFromString(payload)
        order = decoder_utils.decodeOrder(m.orderId, m.contract, m.order)
        # Not read back by decodeOrder (an openOrder carries no transmit);
        # the client library writes it only when true.
        order.transmit = m.order.HasField("transmit") and m.order.transmit
        contract = decoder_utils.decodeContract(m.contract)
        out = {"orderId": m.orderId, "contract": plain(contract), "order": plain(order)}
        if m.HasField("attachedOrders"):
            out["attachedOrders"] = MessageToDict(m.attachedOrders)
        return out
    cls = REQUESTS.get(name)
    if cls is None:
        return None
    m = getattr(import_module(f"ibapi.protobuf.{cls}_pb2"), cls)()
    m.ParseFromString(payload)
    return MessageToDict(m)


def export(src: Path, name: str, area: str, keep, notes: str):
    lines = src.read_text(encoding="utf-8").splitlines()
    head = json.loads(lines[0])
    out = [{
        "type": "header", "format": "codec/1", "scenario": name, "area": area,
        "source": f"{src.parent.name}/{src.name}", "source_scenario": head["scenario"],
        "capture_date": head["capture_date"], "market_session": head.get("market_session"),
        "gateway_version": head.get("gateway_version"),
        # The zone of the machine that ran the gateway: a time without a
        # zone is read in it (all captures so far: the Paris desktop).
        "machine_zone": "Europe/Paris", "notes": notes,
    }]
    for line in lines[1:]:
        r = json.loads(line)
        if not keep(r):
            continue
        rec = {k: r[k] for k in ("seq", "leg", "conn") if k in r}
        if r["kind"] == "api":
            rec["msg_name"] = r.get("msg_name")
            body = base64.b64decode(r["raw_b64"])
            rec["raw_b64"] = r["raw_b64"]
            if r["leg"] == "api_in" and r.get("msg_name") not in ("SERVER_VERSION",):
                rec["callbacks"] = callbacks(body)
            elif r["leg"] == "api_out" and r.get("binary_id"):
                rec["request"] = request(r.get("msg_name"), body)
        else:
            rec["msg_type"] = r.get("msg_type")
            rec["raw_b64"] = r["raw_b64"]
        out.append(rec)
    OUT.mkdir(parents=True, exist_ok=True)
    path = OUT / f"{name}.jsonl"
    path.write_text("".join(json.dumps(o, separators=(",", ":")) + "\n" for o in out), encoding="utf-8")
    print(f"{path.name}: {len(out) - 1} records, {path.stat().st_size} bytes")


def fix_types(*types, conns=None):
    def keep(r):
        if r["kind"] == "api":
            return True
        if conns and not any(r["conn"].startswith(c) for c in conns):
            return False
        return r.get("msg_type") in types
    return keep


MD_API = ("REQ_MKT_DATA", "CANCEL_MKT_DATA", "REQ_MARKET_DATA_TYPE", "REQ_SMART_COMPONENTS", "TICK_PRICE",
          "TICK_SIZE", "TICK_STRING", "TICK_GENERIC", "TICK_REQ_PARAMS", "MARKET_DATA_TYPE", "TICK_SNAPSHOT_END",
          "ERR_MSG", "SMART_COMPONENTS")


def md_slice(ranges, farms=("usfarm",)):
    """Market data records of the seq ranges: the requests and callbacks,
    the farm frames of `farms`, the symbol lookups and their replies."""
    lookups = set()

    def keep(r):
        if not any(a <= r["seq"] <= b for a, b in ranges):
            return False
        if r["kind"] == "api":
            return r.get("msg_name") in MD_API
        f = dict((t, v) for t, v in r.get("fields", []))
        if r["conn"] == "CCP" and r.get("msg_type") in ("c", "d"):
            rid = f.get("320", "")
            if r.get("msg_type") == "c" and rid.startswith("FixSecDefReqBySymbol"):
                lookups.add(rid)
            return rid in lookups
        return r["conn"] in farms and r.get("msg_type") in ("Q", "L", "P", "G", "3", "V")
    return keep


ORDER_API = ("PLACE_ORDER", "CANCEL_ORDER", "START_API", "REQ_GLOBAL_CANCEL", "REQ_OPEN_ORDERS", "REQ_ALL_OPEN_ORDERS",
             "OPEN_ORDER", "ORDER_STATUS", "ERR_MSG", "EXECUTION_DATA", "COMMISSION_REPORT", "NEXT_VALID_ID",
             "OPEN_ORDER_END")


def order_slice(ranges=None):
    """Order records: the order requests and callbacks, the auth link's
    order messages, reports and definition lookups (not the option chain
    ones)."""
    def keep(r):
        if ranges and not any(a <= r["seq"] <= b for a, b in ranges):
            return False
        if r["kind"] == "api":
            return r.get("msg_name") in ORDER_API
        if r["conn"] != "CCP":
            return False
        fields = dict((t, v) for t, v in r.get("fields", []))
        if r.get("msg_type") in ("c", "d"):
            return not fields.get("320", "").startswith("getECsForConidExchangePairs")
        # The commission reports.
        if r.get("msg_type") == "U":
            return fields.get("6040") == "60"
        return r.get("msg_type") in ("D", "G", "F", "8", "9", "H")
    return keep


def account_slice():
    """The account summary request and its answer on the auth link."""
    def keep(r):
        if r["kind"] == "api":
            return True
        return r["conn"] == "CCP" and r.get("msg_type") in ("U", "UM", "RL", "EB", "UT", "UP")
    return keep


def hmds_slice(farms=("ushmds",)):
    """Historical requests: the API side, the historical farm's queries and
    answers (no heartbeats), the contract lookups."""
    def keep(r):
        if r["kind"] == "api":
            return True
        if r["conn"] == "CCP":
            return r.get("msg_type") in ("c", "d")
        return r["conn"] in farms and r.get("msg_type") not in ("0", "1")
    return keep


L1_IN = ("Q", "L", "P", "G", "3", "d")
L1_OUT = ("V", "c")
ORDER_TYPES = ("D", "G", "F", "8", "c", "d")


def specs(cap: Path):
    b1 = cap / "four-leg" / "20261002-b1" / "scenarios"
    s26 = cap / "four-leg" / "20260926"
    s26b = cap / "four-leg" / "20260926b"
    s28 = cap / "four-leg" / "20260928"
    yield (b1 / "b1_441_smart_components.jsonl", "l1_aapl_spy_preopen", "decode-l1",
           fix_types(*L1_IN, *L1_OUT, conns=("usfarm", "CCP")),
           "AAPL and SPY top of book before the open: acks, definitions, trades, quotes, smart components")
    yield (s28 / "rth_order_types.jsonl", "l1_spy_qqq_rth", "decode-l1", md_slice([(5186, 5420), (6581, 6670)]),
           "SPY then QQQ, then SPY again, in the session (top of book streams)")
    yield (s28 / "premarket_order_types.jsonl", "l1_aapl_preopen_delayed", "decode-l1",
           md_slice([(21956, 22700)]),
           "AAPL (and BMW, 7203 on other farms) with delayed data asked, then AAPL with real-time data, before the open")
    yield (b1 / "b1_431_hist_format.jsonl", "hmds_bars_and_head_timestamp", "decode-hmds", hmds_slice(),
           "AAPL bars with formatDate 1 and 2, head timestamps and cancels, before the open")
    yield (s26 / "account_summary.jsonl", "account_summary", "decode-account", account_slice(),
           "reqAccountSummary of four tags and $LEDGER:ALL, its rows and ends, its cancel (market closed)")
    for name in ("lmt_cancel", "modify_cancelled", "bracket", "oca_group"):
        yield (s26 / f"{name}.jsonl", f"orders_{name}", "orders", order_slice(),
               f"{name}: the API orders, the gateway's order messages and the server reports")
    yield (s26b / "bracket.jsonl", "orders_bracket_b", "orders", order_slice(),
           "bracket (second session): the API orders, the gateway's order messages and the server reports")
    for name in ("premarket_order_types", "rth_order_types", "i196_overnight"):
        yield (s28 / f"{name}.jsonl", f"orders_{name}", "orders", order_slice(),
               f"{name}: the API orders, the gateway's order messages and the server reports")
    for name in ("b1_416_time_condition", "b1_416_time_near", "b1_462_whatif", "b1_263_algo_refusals"):
        yield (b1 / f"{name}.jsonl", f"orders_{name}", "orders", order_slice(),
               f"{name}: the API orders, the gateway's order messages and the server reports")


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--captures", required=True, type=Path)
    ap.add_argument("--only")
    a = ap.parse_args()
    for src, name, area, keep, notes in specs(a.captures):
        if a.only and a.only not in name:
            continue
        export(src, name, area, keep, notes)


if __name__ == "__main__":
    main()
