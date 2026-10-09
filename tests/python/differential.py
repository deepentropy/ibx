"""ibx#489: the callbacks of a live ibx run against the reference's callbacks
of the same scenarios, as a CSV of differences.

The ibx side is the callback log of a scenario run on the ibx target
(``events.jsonl`` and ``run.jsonl`` of the run folder). The reference side is
either the recorded scenarios of tests/fixtures/gw1040/scenarios (default) or
the callback log of a run of the same scenarios on the reference.

    python tests/python/differential.py --ibx <run folder> [--reference <run folder>] [--out report.csv]

How two callback lists are compared:

- ``connectAck`` and ``connectionClosed`` are left out: the recorded
  scenarios do not hold them.
- Order, refusal, error and end callbacks: same callbacks, in the same order,
  field by field. A callback one side lacks is a ``missing_in_ibx`` or an
  ``extra_in_ibx`` row; a field that differs is a ``field`` row.
- Data callbacks (ticks, bars, account values, positions, P&L, scanner rows):
  by shape only. The values and the order between streams change from one run
  to the next, so only the kinds of rows are compared (``shape`` rows).
- Values of the session are compared for presence, not for value: the
  account, the order ids (kept as their distance to ``nextValidId``), permId,
  execId, times, and ``nextValidId`` itself. The time of an error is not
  compared at all.
- The orders of an open order listing (the ``openOrder`` / ``orderStatus``
  pairs before an ``openOrderEnd``) are compared in the order of their ids:
  the reference lists its order book by a hash of the permId, a value of the
  session, so the order of a listing changes from one run to the next
  (ibx#522).
- The start and the end of ``historicalDataEnd`` follow the time of the
  request: their form is compared (digits as 9), not their value.
- Prices of the orders follow the reference price of the day the scenario
  ran: they are compared for presence unless both sides ran on the same day
  (``--strict-prices``).
- An object the recording holds without its unset fields is completed with
  the unset values of the official client library.

A row that an open issue explains carries the issue in its ``known`` column.
The exit code is 1 when a row has none.
"""

import argparse
import csv
import difflib
import glob
import inspect
import json
import os
import re
import sys

ACCOUNT = re.compile(r"\b(DU|DF|U|F)[0-9]{6,8}\b")
ACCOUNT_MASK = "DUXXXXXXX"
MAX_DOUBLE, MAX_INT = sys.float_info.max, 2 ** 31 - 1

LEFT_OUT = {"connectAck", "connectionClosed"}

# Data callbacks, compared by shape: the arguments that name the kind of row.
SHAPE = {
    "tickPrice": (0, 1), "tickSize": (0, 1), "tickString": (0, 1), "tickGeneric": (0, 1), "tickEFP": (0, 1),
    "tickOptionComputation": (0, 1), "tickNews": (0,), "tickByTickAllLast": (0, 1), "tickByTickBidAsk": (0,),
    "tickByTickMidPoint": (0,), "historicalData": (0,), "historicalDataUpdate": (0,), "realtimeBar": (0,),
    "historicalTicks": (0,), "historicalTicksBidAsk": (0,), "historicalTicksLast": (0,), "updateMktDepth": (0,),
    "updateMktDepthL2": (0,), "pnl": (0,), "pnlSingle": (0,), "updateAccountValue": (0, 2), "updateAccountTime": (),
    "updatePortfolio": (), "accountSummary": (0, 2, 4), "accountUpdateMulti": (0, 3, 5), "position": (),
    "positionMulti": (0,), "scannerData": (0,), "currentTime": (), "histogramData": (0,),
}

# Fields compared for presence only.
SESSION_FIELDS = {"permId", "parentPermId", "execId", "time", "submitter", "lastTradeTime", "completedTime",
                  "manualOrderTime"}
# Not compared: the recorded scenarios do not hold it, and ibx takes it from the local clock.
NOT_COMPARED = {"errorTime"}
PRICE_FIELDS = {"lmtPrice", "auxPrice", "trailStopPrice", "avgFillPrice", "lastFillPrice", "price", "avgPrice",
                "triggerPrice", "adjustedStopPrice", "adjustedStopLimitPrice", "mktCapPrice", "startingPrice",
                "stockRefPrice"}
ORDER_ID_FIELDS = {"orderId", "parentId"}
# Errors whose id is an order id even when the order was never placed.
ORDER_ERRORS = {103, 104, 105, 110, 135, 161, 201, 202, 399, 10147, 10148}

# Notices about the state of the data connections: which ones a client
# gets depends on what the reference had open at that moment.
CONNECTION_NOTICES = {"2103", "2104", "2105", "2106", "2107", "2108", "2119", "2120", "2157", "2158", "2159", "2160"}


def _connection_notice(r):
    # The three notices of the connect are compared as they are (ibx#517).
    return not r.get("at_connect") and r["callback"] == "error" and (
        r["key"].split("|")[-1] in CONNECTION_NOTICES
        or (r["field"] == "errorString" and " farm " in r["reference"] and " farm " in r["ibx"]))


def _company_known(r):
    # The reference keeps the company data of an underlying for the whole
    # life of its process: only the first derivative row of that life lacks
    # its industry. An ibx run logs in again for every scenario, so the first
    # row of each scenario lacks it (ibx#526, not a defect).
    return (r["callback"] == "contractDetails" and r["kind"] == "field" and not r["ibx"]
            and r["field"] in ("contractDetails.industry", "contractDetails.category", "contractDetails.subcategory"))


# What explains a row: (an open issue or a reason, test of the row).
KNOWN = [
    ("session: company data the reference kept from an earlier request", _company_known),
    ("session: state of the data connections", _connection_notice),
    ("recording older than notice 2172", lambda r: r["callback"] == "error" and r["kind"] == "extra_in_ibx"
     and r["key"] == "-1|2172"),
]

COLUMNS = ["scenario", "reference_session", "ibx_session", "kind", "callback", "key", "field", "reference", "ibx",
           "known"]

# ── The official client library, when installed: argument names and unset values ──

try:
    from ibapi.wrapper import EWrapper as _Official
except ImportError:  # the comparison still works on two callback logs
    _Official = None


def _parameters(name):
    f = getattr(_Official, name, None) if _Official else None
    if f is None:
        return []
    return [p for p in inspect.signature(f).parameters.values() if p.name != "self"]


def argument_names(name, count):
    names = [p.name for p in _parameters(name)]
    return [names[i] if i < len(names) else f"arg{i}" for i in range(count)]


def _plain(v):
    """An official object as the callback logs write it."""
    if hasattr(v, "__dict__"):
        return {k: _plain(x) for k, x in vars(v).items()}
    if isinstance(v, (list, tuple)):
        return [_plain(x) for x in v]
    return v if isinstance(v, (str, int, float, bool)) or v is None else str(v)


def _unset(name, index):
    """The unset form of the object argument `index` of a callback."""
    params = _parameters(name)
    cls = params[index].annotation if index < len(params) else None
    if not inspect.isclass(cls):
        return None
    try:
        return _plain(cls())
    except Exception:
        return None


def completed(value, unset):
    """`value` with the fields it lacks taken from `unset`."""
    if not isinstance(value, dict) or not isinstance(unset, dict):
        return value
    out = dict(unset)
    for k, v in value.items():
        out[k] = completed(v, unset.get(k))
    return out


# ── Reading the two sides ──


def mask_accounts(text):
    return ACCOUNT.sub(ACCOUNT_MASK, text)


def read_run(folder):
    """A run folder: {scenario: [[callback, args...], ...]} and {scenario: market session}."""
    calls, sessions = {}, {}
    with open(os.path.join(folder, "events.jsonl"), encoding="utf-8") as fh:
        for line in fh:
            e = json.loads(mask_accounts(line))
            if e["cb"].endswith("ProtoBuf") or not isinstance(e["args"], list):
                continue
            calls.setdefault(e["conn"], []).append([e["cb"], *e["args"]])
    manifest = os.path.join(folder, "run.jsonl")
    if os.path.exists(manifest):
        with open(manifest, encoding="utf-8") as fh:
            for line in fh:
                r = json.loads(line)
                if r.get("event") == "scenario":
                    sessions[r["name"]] = r.get("market_session", "")
    calls.pop("setup", None)
    return calls, sessions


def find_recording(root, scenario):
    """The latest recording of a scenario under the fixture folder."""
    found = sorted(glob.glob(os.path.join(root, "*", scenario + ".api.jsonl")))
    return found[-1] if found else None


def read_recording(path):
    """A recorded scenario: its callbacks and its market session."""
    calls = []
    with open(path, encoding="utf-8") as fh:
        for line in fh:
            r = json.loads(mask_accounts(line))
            calls.extend(r.get("callbacks") or [])
    session = ""
    frames = path[:-len(".api.jsonl")] + ".jsonl"
    if os.path.exists(frames):
        with open(frames, encoding="utf-8") as fh:
            session = json.loads(fh.readline()).get("market_session", "")
    return calls, session


# ── The comparable form of a callback ──


def number(v):
    """Numbers in one form: a decimal text is its number, an unset value is MAX."""
    if isinstance(v, bool):
        return v
    if isinstance(v, str):
        if v == "MAX":
            return "MAX"
        try:
            v = float(v)
        except ValueError:
            return v
    if isinstance(v, (int, float)):
        if v == MAX_INT or v >= MAX_DOUBLE:
            return "MAX"
        return float(v)
    return v


def empty(v):
    return v is None or (isinstance(v, (str, list, dict)) and not v)


def flat(prefix, v, out):
    if isinstance(v, dict):
        for k, x in v.items():
            # Internals of a Python enum, written by the callback log.
            if not str(k).startswith("_"):
                flat(f"{prefix}.{k}", x, out)
    elif isinstance(v, list):
        for i, x in enumerate(v):
            flat(f"{prefix}[{i}]", x, out)
    elif not empty(v) and v != "<cycle>":
        if isinstance(v, str) and v.startswith("{") and v.endswith("}") and ":" not in v:
            # A Python set, written in no order: its items sorted.
            v = "{" + ", ".join(sorted(x.strip() for x in v[1:-1].split(","))) + "}"
        out[prefix] = number(mask_accounts(v) if isinstance(v, str) else v)


class Session:
    """What is known of one side's session: the first order id it was given
    and the order ids it used."""

    def __init__(self, calls):
        self.base = next((c[1] for c in calls if c[0] == "nextValidId"), 1)
        self.orders = {c[1] for c in calls if c[0] in ("orderStatus", "openOrder")}

    def order_id(self, v):
        return f"{{id+{int(v) - self.base}}}" if isinstance(v, (int, float)) and not isinstance(v, bool) and v > 0 else v


def comparable(call, session, strict_prices):
    """(callback, key, {field: value}) of a strict callback."""
    name, args = call[0], list(call[1:])
    names = argument_names(name, len(args))
    fields = {}
    is_order = name in ("orderStatus", "openOrder", "orderBound") or (
        name == "error" and args and (args[0] in session.orders or (len(args) > 2 and args[2] in ORDER_ERRORS)))
    raw_id = args[0] if args else None
    for i, (arg_name, v) in enumerate(zip(names, args)):
        flat(arg_name, completed(v, _unset(name, i)) if isinstance(v, dict) else v, fields)
    for path in list(fields):
        leaf = re.split(r"[.\]]", path)[-1] or path
        v = fields[path]
        if leaf in NOT_COMPARED:
            del fields[path]
        elif name == "historicalDataEnd" and leaf in ("start", "end") and isinstance(v, str):
            # The window of the request follows the time it was made: its form only.
            fields[path] = re.sub(r"\d", "9", v)
        elif leaf in SESSION_FIELDS or name == "nextValidId":
            fields[path] = "{set}"
        elif leaf in PRICE_FIELDS and not strict_prices:
            fields[path] = v if v in ("MAX", 0.0) else "{price}"
        elif leaf in ORDER_ID_FIELDS or (is_order and path == names[0]):
            fields[path] = session.order_id(v)
        elif isinstance(v, str) and is_order and isinstance(raw_id, int):
            # The order id an error text names ("OrderId 5001 that needs ...").
            fields[path] = re.sub(rf"(?i)(order ?id:? ?){raw_id}\b", rf"\g<1>{session.order_id(raw_id)}", v)
    first = fields.get(names[0], "") if names and name not in ("managedAccounts", "nextValidId") else ""
    code = fields.get("errorCode", "") if name == "error" else ""
    show = lambda x: str(int(x)) if isinstance(x, float) and x.is_integer() else str(x)
    key = "|".join(show(x) for x in (first, code) if x != "")
    return name, key, fields


def shape_of(call):
    name, args = call[0], call[1:]
    return name, "|".join(str(args[i]) for i in SHAPE[name] if i < len(args))


def listings_by_id(calls):
    """The calls with the orders of every open order listing in the order of
    their ids: the openOrder / orderStatus pairs right before an
    openOrderEnd."""
    out = list(calls)
    for end in [i for i, c in enumerate(out) if c[0] == "openOrderEnd"]:
        start = end
        while (start >= 2 and out[start - 2][0] == "openOrder" and out[start - 1][0] == "orderStatus"
               and out[start - 2][1] == out[start - 1][1]):
            start -= 2
        pairs = [out[i:i + 2] for i in range(start, end, 2)]
        pairs.sort(key=lambda pair: [int(n) for n in re.findall(r"-?\d+", pair[0][1])])
        out[start:end] = [c for pair in pairs for c in pair]
    return out


# ── The comparison ──


def text(v):
    if isinstance(v, float) and v.is_integer():
        return str(int(v))
    return "" if v is None else str(v)


def compare(reference, ours, strict_prices=False):
    """The difference rows of one scenario (without the scenario columns)."""
    rows = []
    reference = [c for c in reference if c[0] not in LEFT_OUT]
    ours = [c for c in ours if c[0] not in LEFT_OUT]

    def add(kind, callback, key, field="", ref="", ibx="", at_connect=False):
        rows.append({"kind": kind, "callback": callback, "key": key, "field": field,
                     "reference": text(ref), "ibx": text(ibx), "at_connect": at_connect})

    def connect_notices(calls):
        """Places of the first market data, historical data and contract data notice of a side."""
        first = {}
        for i, (name, key, _) in enumerate(calls):
            kind = {"2103": "md", "2104": "md", "2105": "hist", "2106": "hist", "2157": "sec", "2158": "sec"}.get(
                key.split("|")[-1]) if name == "error" and key.startswith("-1|") else None
            if kind:
                first.setdefault(kind, i)
        return set(first.values())

    # Data callbacks: the kinds of rows each side got.
    theirs_shape = {shape_of(c) for c in reference if c[0] in SHAPE}
    ours_shape = {shape_of(c) for c in ours if c[0] in SHAPE}
    for name, key in sorted(theirs_shape - ours_shape):
        add("shape", name, key, ref="present")
    for name, key in sorted(ours_shape - theirs_shape):
        add("shape", name, key, ibx="present")

    # The other callbacks, in order.
    ref_session, our_session = Session(reference), Session(ours)
    a = listings_by_id([comparable(c, ref_session, strict_prices) for c in reference if c[0] not in SHAPE])
    b = listings_by_id([comparable(c, our_session, strict_prices) for c in ours if c[0] not in SHAPE])
    at_a, at_b = connect_notices(a), connect_notices(b)
    matcher = difflib.SequenceMatcher(None, [c[:2] for c in a], [c[:2] for c in b], autojunk=False)
    for op, i1, i2, j1, j2 in matcher.get_opcodes():
        if op == "equal":
            for k, ((name, key, theirs), (_, _, mine)) in enumerate(zip(a[i1:i2], b[j1:j2])):
                for field in sorted(set(theirs) | set(mine)):
                    if theirs.get(field) != mine.get(field):
                        add("field", name, key, field, theirs.get(field), mine.get(field),
                            at_connect=i1 + k in at_a or j1 + k in at_b)
            continue
        for i, (name, key, fields) in enumerate(a[i1:i2], i1):
            add("missing_in_ibx", name, key, ref=fields.get("errorString", fields.get("status", "")), at_connect=i in at_a)
        for j, (name, key, fields) in enumerate(b[j1:j2], j1):
            add("extra_in_ibx", name, key, ibx=fields.get("errorString", fields.get("status", "")), at_connect=j in at_b)
    for row in rows:
        row["known"] = "; ".join(issue for issue, explains in KNOWN if explains(row))
        del row["at_connect"]
    return rows


def report(ibx_folder, fixtures=None, reference_folder=None, strict_prices=False):
    """The rows of every scenario of the ibx run, and one summary line each."""
    ours, our_sessions = read_run(ibx_folder)
    theirs, their_sessions = read_run(reference_folder) if reference_folder else ({}, {})
    rows, lines = [], []
    for scenario, calls in ours.items():
        if reference_folder:
            reference, session = theirs.get(scenario), their_sessions.get(scenario, "")
        else:
            path = find_recording(fixtures, scenario)
            reference, session = read_recording(path) if path else (None, "")
        head = {"scenario": scenario, "reference_session": session, "ibx_session": our_sessions.get(scenario, "")}
        if reference is None:
            rows.append({**head, "kind": "not_compared", "callback": "", "key": "", "field": "",
                         "reference": "no reference for this scenario", "ibx": "", "known": ""})
            lines.append(f"{scenario}: no reference")
            continue
        found = compare(reference, calls, strict_prices)
        rows.extend({**head, **r} for r in found)
        unknown = sum(not r["known"] for r in found)
        lines.append(f"{scenario}: {len(found)} differences, {unknown} without an issue"
                     f" (reference {session or '?'}, ibx {head['ibx_session'] or '?'})")
    return rows, lines


def write_csv(rows, path):
    with open(path, "w", newline="", encoding="utf-8") as fh:
        out = csv.DictWriter(fh, fieldnames=COLUMNS)
        out.writeheader()
        out.writerows(rows)


def main(argv=None):
    here = os.path.dirname(os.path.abspath(__file__))
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--ibx", required=True, help="folder of the ibx run (events.jsonl, run.jsonl)")
    ap.add_argument("--reference", help="folder of a run of the same scenarios on the reference")
    ap.add_argument("--fixtures", default=os.path.join(here, "..", "fixtures", "gw1040", "scenarios"),
                    help="recorded scenarios, used without --reference")
    ap.add_argument("--strict-prices", action="store_true", help="both sides ran on the same day: compare the prices")
    ap.add_argument("--out", help="the CSV (default <ibx folder>/differences.csv)")
    a = ap.parse_args(argv)
    if not a.reference and _Official is None:
        raise SystemExit("the recorded scenarios need the official client library to complete their objects")
    rows, lines = report(a.ibx, a.fixtures, a.reference, a.strict_prices)
    out = a.out or os.path.join(a.ibx, "differences.csv")
    write_csv(rows, out)
    print("\n".join(lines))
    print(f"{len(rows)} rows in {out}")
    return 1 if any(not r["known"] for r in rows) else 0


if __name__ == "__main__":
    sys.exit(main())
