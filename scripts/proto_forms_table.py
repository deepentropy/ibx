"""Writes src/python/compat/proto_table.py: the tables behind the protobuf form of the Python client (ibx#725).

    .venv/Scripts/python.exe scripts/proto_forms_table.py

Read from the official client library installed in the environment (its public Python source and its protobuf
classes), so that the tables follow that library when it is upgraded. What cannot be read as a plain field copy is
written by hand in src/python/compat/proto_forms.py; this script prints those callbacks.
"""
import ast
import inspect
import os
import pkgutil
import re
import sys

import ibapi
import ibapi.client
import ibapi.common
import ibapi.decoder
import ibapi.decoder_utils
import ibapi.errors
import ibapi.protobuf
import ibapi.wrapper

OUT = os.path.join(os.path.dirname(os.path.dirname(os.path.abspath(__file__))), "src", "python", "compat", "proto_table.py")

# Official callbacks whose ibx name is not their snake case.
IBX_NAMES = {"realtimeBar": "real_time_bar"}


def snake(name):
    name = name.replace("PnL", "Pnl")
    return IBX_NAMES.get(name) or re.sub(r"(?<=[a-z0-9])(?=[A-Z])|(?<=[A-Z])(?=[A-Z][a-z])", "_", name).lower()


# ── The protobuf messages: name -> ((field, kind), ...) ──

def kind(f):
    T = type(f)
    repeated = f.is_repeated if hasattr(f, "is_repeated") else f.label == T.LABEL_REPEATED
    if f.type == T.TYPE_MESSAGE:
        if f.message_type.GetOptions().map_entry:
            return "map"
        k = "m:" + f.message_type.name
    elif f.type in (T.TYPE_DOUBLE, T.TYPE_FLOAT):
        k = "f"
    elif f.type == T.TYPE_STRING:
        k = "s"
    elif f.type == T.TYPE_BOOL:
        k = "b"
    elif f.type == T.TYPE_BYTES:
        k = "y"
    else:
        k = "i"
    return ("*" if repeated else "") + k


def messages():
    out = {}
    for m in pkgutil.iter_modules(ibapi.protobuf.__path__):
        if not m.name.endswith("_pb2"):
            continue
        mod = __import__("ibapi.protobuf." + m.name, fromlist=["x"])
        for name, d in mod.DESCRIPTOR.message_types_by_name.items():
            out[name] = tuple((f.name, kind(f)) for f in d.fields)
    return dict(sorted(out.items()))


# ── The answers: ibx callback -> (protobuf callback, message, ((field, conversion, default) or None per argument)) ──

ROW = re.compile(r"^\s*(\w+) = (?:(\w+)\()?(\w+)\.(\w+)\)? if \3\.HasField\('(\w+)'\) else (.+)$")


def answers():
    src = inspect.getsource(ibapi.decoder.Decoder)
    tree = ast.parse(src)
    out, by_hand, names = {}, [], {}
    for fn in tree.body[0].body:
        if not (isinstance(fn, ast.FunctionDef) and fn.name.startswith("process") and fn.name.endswith("ProtoBuf")):
            continue
        text = ast.get_source_segment(src, fn)
        rows = {}
        for line in text.splitlines():
            m = ROW.match(line)
            if m:
                var, conv, pv, field, _, default = m.groups()
                rows[var] = (pv, field, conv or "", default.strip())
        calls = [n for n in ast.walk(fn) if isinstance(n, ast.Call) and isinstance(n.func, ast.Attribute)
                 and isinstance(n.func.value, ast.Attribute) and n.func.value.attr == "wrapper"]
        calls.sort(key=lambda n: (n.lineno, n.col_offset))
        first = next((n for n in calls if n.func.attr.endswith("ProtoBuf")), None)
        if first is None:
            continue
        main = first.args[0].id
        made = re.search(rf"^\s*{main} = (\w+)\(\)", text, re.M)
        message = getattr(ibapi.decoder, made.group(1)).DESCRIPTOR.name
        names[snake(first.func.attr)] = first.func.attr
        for call in calls:
            if call is first:
                continue
            args = []
            for a in call.args:
                row = rows.get(a.id) if isinstance(a, ast.Name) else None
                args.append((row[1], row[2], row[3]) if row and row[0] == main else None)
            name = snake(call.func.attr)
            out[name] = (snake(first.func.attr), message, tuple(args))
            if any(a is None for a in args) or len([c for c in calls if c is not first]) > 1:
                by_hand.append(name)
    return out, by_hand, names


# ── The objects: official decoder function -> ((message variable, field, object variable, attribute, conversion), ...) ──

OBJ = re.compile(r"^\s*if \(?(?:\w+ and )?(\w+)\.HasField\('(\w+)'\)\)?: (\w+(?:\.\w+)*)\.(\w+) = (.+)$")


def objects():
    out = {}
    for f in re.split(r"\n(?=def )", inspect.getsource(ibapi.decoder_utils)):
        m = re.match(r"def (\w+)\(", f)
        if not m:
            continue
        rows = []
        for line in f.splitlines()[1:]:
            mm = OBJ.match(line)
            if mm:
                pv, field, obj, attr, expr = mm.groups()
                conv = expr.replace(f"{pv}.{field}", "X").strip()
                conv = {"X": "", "Decimal(X)": "Decimal", "decimalMaxString(X)": "decimalMaxString", "floatMaxString(X)": "floatMaxString",
                        "1 if X else 0": "bool01", "float(X)": "float"}.get(conv, "enum" if conv.startswith("getEnumTypeFromString") else None)
                if conv is None:
                    sys.exit(f"{m.group(1)}: a conversion this script does not know: {expr}")
                rows.append((pv, field, obj, attr, conv))
        if rows:
            out[m.group(1)] = tuple(rows)
    return out


# ── The request objects: official creator -> ((object variable, attribute, field, when it is set, conversion), ...) ──

MADE = re.compile(r"^\s*if (.+?): (\w+)\.(\w+) = (.+)$")
WHEN = {"X": "truthy", "isValidIntValue(X)": "int", "isValidLongValue(X)": "int", "isValidFloatValue(X)": "float",
        "isValidDecimalValue(X)": "decimal", "X is not None": "set"}


def creators():
    import ibapi.client_utils
    out = {}
    for f in re.split(r"\n(?=def )", inspect.getsource(ibapi.client_utils)):
        m = re.match(r"def (\w+)\(", f)
        if not m:
            continue
        rows = []
        for line in f.splitlines()[1:]:
            mm = MADE.match(line)
            ref = mm and re.search(r"(\w+)\.(\w+)", mm.group(4))
            if not ref:
                continue
            cond, _, field, expr = mm.groups()
            x = f"{ref.group(1)}.{ref.group(2)}"
            when = WHEN.get(cond.replace(x, "X"))
            conv = {"X": "", "1 if X else 0": "bool01", "float(X)": "float", "decimalMaxString(X)": "decimalMaxString"}.get(expr.replace(x, "X"))
            if when is None or conv is None:
                sys.exit(f"{m.group(1)}: a line this script does not know: {line.strip()}")
            rows.append((ref.group(1), ref.group(2), field, when, conv))
        if rows:
            out[m.group(1)] = tuple(rows)
    return out


def defaults():
    """The attributes of the request objects ibx reads by attribute, as the official library makes them."""
    from ibapi.execution import ExecutionFilter
    from ibapi.order_cancel import OrderCancel
    from ibapi.scanner import ScannerSubscription
    return {c.__name__: dict(vars(c())) for c in (ExecutionFilter, OrderCancel, ScannerSubscription)}


# ── The requests: outgoing message id -> (official protobuf request, message, session level) ──

def requests():
    src = inspect.getsource(ibapi.client.EClient)
    known = messages()
    out, names = {}, {}
    for m in re.finditer(r"\n    def (\w+ProtoBuf)\(self, (\w+)(?:: (\w+))?\):(.*?)(?=\n    def |\Z)", src, re.S):
        name, param, ann, body = m.groups()
        sent = re.search(r"OUT\.(\w+) \+ PROTOBUF_MSG_ID", body)
        if not sent:
            continue
        msg_id = int(getattr(ibapi.client.OUT, sent.group(1)))
        cls = getattr(ibapi.client, ann, None) if ann else None
        message = cls.DESCRIPTOR.name if cls is not None else (ann or param).removesuffix("Proto")
        message = message[0].upper() + message[1:]
        if message not in known:
            sys.exit(f"{name}: no protobuf message {message}")
        # The argument under the official name in snake case (as tests/python/official_names.py makes it).
        argument = re.sub(r"(?<=[a-z0-9])(?=[A-Z])|(?<=[A-Z])(?=[A-Z][a-z])", "_", param).lower()
        out[msg_id] = (snake(name), message, ibapi.common.PROTOBUF_MSG_IDS.get(msg_id, 0), argument)
        names[snake(name)] = name
    return dict(sorted(out.items())), names


def client_errors():
    return tuple(sorted({v.code() for v in vars(ibapi.errors).values() if isinstance(v, ibapi.errors.CodeMsgPair)}))


def literal(name, comment, value, f):
    f.write(f"\n# {comment}\n{name} = {{\n" if isinstance(value, dict) else f"\n# {comment}\n{name} = {value!r}\n")
    if isinstance(value, dict):
        for k, v in value.items():
            f.write(f"    {k!r}: {v!r},\n")
        f.write("}\n")


def main():
    ans, by_hand, cb_names = answers()
    reqs, rq_names = requests()
    with open(OUT, "w", encoding="utf-8", newline="\n") as f:
        f.write(f'"""Tables of the protobuf form of the Python client (ibx#725).\n\n'
                f'Written by scripts/proto_forms_table.py from the official client library {ibapi.get_version_string()}. Not edited by hand.\n"""\n')
        literal("LIBRARY", "The version of the official client library the tables were read from.", ibapi.get_version_string(), f)
        literal("MESSAGES", "Protobuf message -> ((field, kind), ...). Kind: i f s b y, m:<message>, map; a leading * for a list.", messages(), f)
        literal("ANSWERS", "ibx callback -> (its protobuf callback, message, per argument (field, conversion, value when the field is not set) or None).", ans, f)
        literal("OBJECTS", "Official decoder -> ((message variable, field, object variable, attribute, conversion), ...).", objects(), f)
        literal("CREATORS", "Official creator -> ((object variable, attribute, field, when the field is set, conversion), ...).", creators(), f)
        literal("DEFAULTS", "The request objects read by attribute, with the values the official library gives them.", defaults(), f)
        literal("REQUESTS", "Outgoing message id -> (protobuf request, message, session level from which the official library sends it so, argument).", reqs, f)
        literal("OFFICIAL_NAMES", "The official names of the protobuf requests and callbacks.", dict(sorted({**cb_names, **rq_names}.items())), f)
        literal("CLIENT_ERRORS", "Codes of the errors the official library makes itself: they have no protobuf form.", client_errors(), f)
    print(f"{OUT}: {len(ans)} callbacks, {len(reqs)} requests")
    print("written by hand in proto_forms.py:", " ".join(sorted(by_hand)))


if __name__ == "__main__":
    main()
