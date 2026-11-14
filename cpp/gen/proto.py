"""The protobuf forms of the C++ library (IBX_PROTOBUF), generated from the
tables and rules of the Python client's protobuf layer (ibx#235).

Read: src/python/compat/proto_table.py (the messages, the answer of each
plain callback, the fields of each object), proto_forms.py (which requests
and callbacks are regular, which fields are always set), the start values
of the Python classes (python_starts.json, written by dump_python_starts.py)
and the tables of spec.py.

Written: EWrapper_prototypes_proto.h, ibx_requests_proto.inc and
ibx_protobuf.h in cpp/include, ibx_proto_generated.inc and
proto_generated.cpp in cpp/src. The rules with a shape of their own are
written by hand in cpp/src/proto.cpp.
"""

import json
import os
import re
import sys

import spec

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(os.path.dirname(HERE))

sys.dont_write_bytecode = True
sys.path.insert(0, os.path.join(ROOT, "src", "python", "compat"))
import proto_forms as F  # noqa: E402
import proto_table as T  # noqa: E402

KINDS = {message: dict(fields) for message, fields in T.MESSAGES.items()}
with open(os.path.join(HERE, "python_starts.json"), encoding="utf-8") as _f:
    STARTS = json.load(_f)

INT_TYPES = ("int", "long long", "TickType", "faDataType")


def lower_first(s):
    return s[0].lower() + s[1:]


def setter(field):
    return "set_" + field.lower()


def quoted(text):
    return '"%s"' % text.replace("\\", "\\\\").replace('"', '\\"')


def number(v):
    """A start value as a C++ number."""
    if v == "max":
        return "UNSET_DOUBLE"
    if isinstance(v, bool):
        return "true" if v else "false"
    if isinstance(v, int):
        return "UNSET_INTEGER" if v == 2 ** 31 - 1 else str(v)
    return repr(float(v))


# ── The callbacks and the requests, with their official C++ names ──

def proto_callbacks():
    """[C++ name, message, the plain callbacks (Rust names) it comes before]."""
    out, at = [], {}
    for plain, (py_name, message, _args) in T.ANSWERS.items():
        official = T.OFFICIAL_NAMES[py_name]
        name = spec.PROTO_NAMES.get(official, official)
        if name not in at:
            at[name] = len(out)
            out.append([name, message, []])
        out[at[name]][2].append(plain)
    for name, message in spec.PROTO_CALLBACKS_ONLY:
        out.append([name, message, []])
    return out


def proto_requests():
    """[C++ name, message, the name in the rules of proto_forms.py]."""
    out = []
    for _id, (py_name, message, _level, _argument) in sorted(T.REQUESTS.items()):
        official = T.OFFICIAL_NAMES[py_name]
        out.append([spec.PROTO_NAMES.get(official, official), message, py_name[:-len("_proto_buf")]])
    return out


def enum_name(callback):
    return "PB_" + callback[:-len("ProtoBuf")]


def parameter(message):
    return "const protobuf::%s& %sProto" % (message, lower_first(message))


# ── A message made from an object: one statement per field ──

def row_statement(G, struct, message, field, attr, conv, start):
    """Sets `field` of the message `m` from the member of `x` the attribute
    `attr` is, unless the value stands for none or is the one the Python
    object starts with (`start`: its type and value, None when there is
    none)."""
    kind = KINDS[message][field]
    put = "m.%s" % setter(field)
    if (message, field) in spec.PROTO_VALUES:
        cond, value = spec.PROTO_VALUES[(message, field)]
        return "%s%s(%s);" % ("if (%s) " % cond if cond else "", put, value)
    member = spec.PROTO_MEMBERS.get((struct, attr), attr)
    found = [m for m in G.members(next(st for st in spec.STRUCTS if st["name"] == struct)) if m["name"] == member]
    if not found:
        raise SystemExit("protobuf %s.%s: no member %s.%s" % (message, field, struct, member))
    ctype, x = found[0]["ctype"], "x." + member
    always = (message, field) in F._SET_AT_START
    known = start is not None and start[0] not in ("none", "other")
    if start is not None and not known:
        raise SystemExit("protobuf %s.%s: the start value of %s.%s needs a rule in PROTO_VALUES" % (message, field, struct, attr))
    value = start[1] if known else None
    if conv == "bool01":
        if kind != "b":
            raise SystemExit("protobuf %s.%s: bool01 on a field of kind %s" % (message, field, kind))
        return "if (%s) %s(true);" % (x, put)
    if ctype == "std::string" and kind == "s":
        conds = ["!%s.empty()" % x]
        if known and value != "" and not always:
            conds.append("%s != %s" % (x, quoted(value)))
        return "if (%s) %s(%s);" % (" && ".join(conds), put, x)
    if ctype == "std::string" and kind == "f":
        # A number the structure keeps as a text.
        return "{ double v; if (ibx::pb::number(%s, v)) %s(v); }" % (x, put)
    if ctype in ("int", "long long") and kind == "i":
        conds = ["%s != UNSET_INTEGER" % x] + (["%s != UNSET_LLONG" % x] if ctype == "long long" else [])
        if known and not always and number(value) != "UNSET_INTEGER":
            conds.append("%s != %s" % (x, number(value)))
        return "if (%s) %s(%s);" % (" && ".join(conds), put, x)
    if ctype == "double" and kind in ("f", "s"):
        conds = ["%s != UNSET_DOUBLE" % x]
        if known and not always and number(value) != "UNSET_DOUBLE":
            conds.append("%s != %s" % (x, number(value)))
        return "if (%s) %s(%s);" % (" && ".join(conds), put, x if kind == "f" else "ibx::pb::floatText(%s)" % x)
    if ctype == "bool" and kind == "b":
        if not known or always:
            return "%s(%s);" % (put, x)
        return "if (%s%s) %s(%s);" % ("!" if value else "", x, put, "false" if value else "true")
    if ctype == "Decimal" and kind == "s":
        if known and value != "unset":
            raise SystemExit("protobuf %s.%s: a quantity that starts set" % (message, field))
        return "if (!%s.isUnset()) %s(ibx::pb::decimalText(%s));" % (x, put, x)
    raise SystemExit("protobuf %s.%s: no rule for %s %s.%s as kind %s" % (message, field, ctype, struct, member, kind))


def rows(G):
    decls, defs = [], []
    for (reader, variable), (py_class, struct, message) in spec.PROTO_OBJECTS.items():
        starts = STARTS.get(py_class) if py_class else None
        body = []
        for _mv, field, var, attr, conv in T.OBJECTS[reader]:
            if var != variable:
                continue
            body.append(row_statement(G, struct, message, field, attr, conv, starts.get(attr) if starts else None))
        sig = "void rows(const %s& x, protobuf::%s& m)" % (struct, message)
        decls.append(sig + ";")
        defs.append("%s {\n%s\n}\n" % (sig, "\n".join("  " + b for b in body)))
    return decls, defs


# ── The message of a plain callback ──

def head_statement(cb_name, message, field, ctype, name):
    """Sets `field` from the argument `name` of a plain callback: a number
    unless it stands for none, a text when not empty, a flag when set."""
    kind = KINDS[message][field]
    put = "m.%s" % setter(field)
    b = re.sub(r"^const\s+|\s*&$", "", ctype).strip()
    if b in INT_TYPES and kind == "i":
        v = name if b in ("int", "long long") else "static_cast<int>(%s)" % name
        conds = ["%s != UNSET_INTEGER" % v] + (["%s != UNSET_LLONG" % v] if b == "long long" else [])
        return "if (%s) %s(%s);" % (" && ".join(conds), put, v)
    if b == "double" and kind in ("f", "s"):
        return "if (%s != UNSET_DOUBLE) %s(%s);" % (name, put, name if kind == "f" else "ibx::pb::floatText(%s)" % name)
    if b == "std::string" and kind == "s":
        return "if (!%s.empty()) %s(%s);" % (name, put, name)
    if b == "Decimal" and kind == "s":
        return "if (!%s.isUnset()) %s(ibx::pb::decimalText(%s));" % (name, put, name)
    if b == "bool" and kind == "b":
        return "if (%s) %s(true);" % (name, put)
    raise SystemExit("protobuf callback of %s: no rule for %s %s as %s.%s of kind %s" % (cb_name, ctype, name, message, field, kind))


def plain_callbacks(G):
    """The plain callbacks the client gives: Rust name -> its entry of spec.CALLBACKS."""
    return {(cb["rust"] or G.snake(cb["name"])): cb for cb in spec.CALLBACKS if cb["rust"] is not None}


def heads(G):
    """The fields the arguments of a plain callback give to its message."""
    source = open(F.__file__, encoding="utf-8").read()
    by_hand_users = set(re.findall(r'_head\("(\w+)"', source)) | set(re.findall(r'_with_contract\("(\w+)"', source))
    plain = plain_callbacks(G)
    decls, defs = [], []
    for rname, (_py, message, argspec) in T.ANSWERS.items():
        cb = plain.get(rname)
        if cb is None or cb["raw"]:
            continue
        if rname in F._BUILDERS and rname not in by_hand_users:
            continue
        params = G.cpp_params(cb["params"])
        if len(params) != len(argspec):
            raise SystemExit("protobuf callback of %s: %d arguments here, %d in the table" % (cb["name"], len(params), len(argspec)))
        body = []
        for (ctype, name), a in zip(params, argspec):
            body.append("(void)%s;" % name if a is None else head_statement(cb["name"], message, a[0], ctype, name))
        sig = "void head_%s(protobuf::%s& m%s)" % (cb["name"], message, "".join(", " + p for p in G.split_params(cb["params"])))
        decls.append(sig + ";")
        defs.append("%s {\n%s\n}\n" % (sig, "\n".join("  " + b for b in body) or "  (void)m;"))
    return decls, defs


def trampolines(G, callbacks):
    """The callback table of a build with the protobuf forms: each entry
    gives the protobuf callback of its answer, then the plain callback."""
    proto_of = {plain: (name, message) for name, message, plains in callbacks for plain in plains}
    decls, out, table, pairs = [], [], [], []
    for t in G.TRAMPOLINES:
        cb, rname = t["cb"], t["rname"]
        abi = "".join(", %s %s" % a for a in t["abi"])
        head = "static void pb_%s(void* user%s) noexcept {\n  ibx::Sink* s = static_cast<ibx::Sink*>(user);\n" % (rname, abi)
        table.append("  &pb_%s," % rname)
        if cb["raw"] or rname not in proto_of:
            # No message for this callback: what waits is given, then it.
            # One with no plain callback has its own statement.
            call = cb["raw"].get("proto", t["call"]) if cb["raw"] else t["call"]
            out.append(head + "  s->flush();\n  try {\n%s    %s\n  } catch (...) { s->failed(); }\n}\n" % (
                "".join("    %s\n" % d for d in t["cdecode"]), call))
            continue
        name, message = proto_of[rname]
        # An argument made by a call is made once, for both callbacks.
        local, args = [], []
        for k, a in enumerate(t["cargs"]):
            if "::" in a:
                local.append("const auto a%d = %s;" % (k, a))
                args.append("a%d" % k)
            else:
                args.append(a)
        pre = "".join("    %s\n" % d for d in t["cdecode"] + local)
        joined = ", ".join(args)
        params = "".join(", " + p for p in G.split_params(cb["params"]))
        if rname in spec.PROTO_CUSTOM:
            decls.append("bool %s(Sink& s%s);" % (cb["name"], params))
            # The rule is asked only when it has something to do.
            out.append(head + "  try {\n%s    if (s->quiet(ibx::%s) || ibx::pb::%s(*s%s)) s->w->%s(%s);\n  } catch (...) { s->failed(); }\n}\n" % (
                pre, enum_name(name), cb["name"], "".join(", " + a for a in args), cb["name"], joined))
            continue
        if rname in F._BUILDERS:
            if F._BUILDERS[rname] is None:
                raise SystemExit("protobuf callback of %s: it has no message of its own and no rule in PROTO_CUSTOM" % cb["name"])
            fill = cb["name"]
            decls.append("void %s(protobuf::%s& m%s);" % (fill, message, params))
        else:
            fill = "head_" + cb["name"]
        pairs.append((cb["name"], name))
        out.append(head + "  try {\n%s    if (!s->quiet(ibx::%s)) {\n      s->flush();\n      if (s->wants(ibx::%s)) {\n        protobuf::%s m;\n"
                   "        ibx::pb::%s(m%s);\n        ++ibx::protoMessagesBuilt;\n        s->w->%s(m);\n        s->given();\n      }\n    }\n"
                   "    s->w->%s(%s);\n  } catch (...) { s->failed(); }\n}\n" % (
                       pre, enum_name(name), enum_name(name), message, fill, "".join(", " + a for a in args), name, cb["name"], joined))
    return decls, out, table, pairs


# ── A request message read into the plain request ──

def copy_statement(G, struct, message, attr, field, when, conv, start):
    """Reads `field` of the message `p` into the member of `o`."""
    kind = KINDS[message][field]
    names = {m["name"]: m for m in G.members(next(st for st in spec.STRUCTS if st["name"] == struct))}
    member = attr if attr in names else "m_" + attr
    if member not in names:
        raise SystemExit("protobuf request %s.%s: no member %s.%s" % (message, field, struct, attr))
    ctype, o, v = names[member]["ctype"], "o." + member, "p.%s()" % field.lower()
    if (struct, member) in spec.PROTO_READS:
        value = re.sub(r"\bv\b", lambda _m: v, spec.PROTO_READS[(struct, member)])
    elif ctype == "std::string" and kind == "s" and not conv:
        value = v
    elif ctype == "std::string" and kind == "f" and conv == "float":
        value = "ibx::pb::floatMaxText(%s)" % v
    elif ctype in ("int", "long long") and kind == "i" and not conv:
        value = "static_cast<%s>(%s)" % (ctype, v)
    elif ctype == "double" and kind == "f" and not conv:
        value = v
    elif ctype == "bool" and kind == "b" and not conv:
        value = v
    elif ctype == "Decimal" and kind == "s" and conv == "decimalMaxString":
        value = "DecimalFunctions::stringToDecimal(%s)" % v
    else:
        raise SystemExit("protobuf request %s.%s: no rule for %s %s.%s as kind %s (%s)" % (message, field, ctype, struct, member, kind, conv))
    text = "if (p.has_%s()) %s = %s;" % (field.lower(), o, value)
    # A field the official client leaves out for no, nothing or zero: a
    # member that starts as yes is put to no.
    if when == "truthy" and start and isinstance(start, (bool, int, float, str)):
        zero = {"bool": "false", "std::string": "std::string()"}.get(ctype, "0")
        text += " else %s = %s;" % (o, zero)
    return text


def copies(G):
    decls, defs = [], []
    for (writer, variable), (py_class, struct, message) in spec.PROTO_CREATORS.items():
        starts = STARTS.get(py_class, {}) if py_class else None
        body = []
        for var, attr, field, when, conv in T.CREATORS[writer]:
            if var != variable:
                continue
            if starts is None:
                start = T.DEFAULTS.get(struct, {}).get(attr)
            else:
                entry = starts.get(attr)
                start = None if entry is None or entry[0] in ("none", "other", "decimal") or entry[1] == "max" else entry[1]
            body.append(copy_statement(G, struct, message, attr, field, when, conv, start))
        sig = "void copy(const protobuf::%s& p, %s& o)" % (message, struct)
        decls.append(sig + ";")
        defs.append("%s {\n%s\n}\n" % (sig, "\n".join("  " + b for b in body)))
    return decls, defs


def scalar(field, kind, ctype, conv):
    """A field of a request message as the argument of the plain request:
    its value, or the one that stands for none."""
    v = "p.%s()" % field.lower()
    none = "NO_VALID_ID" if field == "reqId" else "0" if field == "orderId" else {"i": "UNSET_INTEGER", "f": "UNSET_DOUBLE", "s": "std::string()", "b": "false"}[kind]
    b = re.sub(r"^const\s+|\s*&$", "", ctype).strip()
    ok = (kind == "i" and b in ("int", "faDataType")) or (kind == "f" and b == "double") or (kind == "s" and b == "std::string") \
        or (kind == "b" and (b == "bool" or (b == "int" and conv == "int")))
    if not ok:
        raise SystemExit("protobuf request: no rule for the field %s of kind %s as %s" % (field, kind, ctype))
    text = "p.has_%s() ? %s : %s" % (field.lower(), v, none)
    if b == "faDataType" or (b == "int" and kind != "s"):
        return "static_cast<%s>(%s)" % (b, text)
    return text


def requests(G, reqs):
    by_call = {}
    for q in spec.REQUESTS:
        by_call[re.match(r"c\.(\w+)\(", q["call"]).group(1)] = q
    by_name = {q["name"]: q for q in spec.REQUESTS}
    inc, defs, by_hand = [], [], []
    for name, message, short in reqs:
        inc.append("  void %s(%s);" % (name, parameter(message)))
        rule = F._REQUESTS.get(short)
        if not isinstance(rule, tuple):
            by_hand.append(name)
            continue
        plain, fields = rule[0], rule[1].split()
        q = by_name[spec.PROTO_PLAIN[plain]] if plain in spec.PROTO_PLAIN else by_call.get(plain)
        if q is None:
            raise SystemExit("protobuf request %s: no C++ request for %s" % (name, plain))
        params = G.cpp_params(q["params"])
        if len(params) != len(fields):
            raise SystemExit("protobuf request %s: %d fields for the %d arguments of %s" % (name, len(fields), len(params), q["name"]))
        args = []
        for word, (ctype, _pname) in zip(fields, params):
            field, _, conv = word.partition(":")
            kind = KINDS[message][field]
            if kind == "m:Contract":
                args.append("p.has_%s() ? ibx::pb::contractOf(p.%s()) : Contract()" % (field.lower(), field.lower()))
            elif kind == "map":
                args.append("ibx::pb::tagsOf(p.%s())" % field.lower())
            else:
                args.append(scalar(field, kind, ctype, conv))
        defs.append("void EClient::%s(const protobuf::%s& p) {\n%s  %s(%s);\n}\n" % (
            name, message, "" if args else "  (void)p;\n", q["name"], ",\n    ".join(args)))
    return inc, defs, by_hand


# ── For the tests: the plain requests made through their protobuf form ──

def twin_statement(G, struct, message, attr, field, when, conv):
    """Sets `field` of the request message `p` from the member of `o`, as a
    program that fills the message from the object of the plain request."""
    kind = KINDS[message][field]
    names = {m["name"]: m for m in G.members(next(st for st in spec.STRUCTS if st["name"] == struct))}
    member = attr if attr in names else "m_" + attr
    ctype, o, put = names[member]["ctype"], "o." + member, "p.%s" % setter(field)
    if ctype in ("ThreeStateBoolean", "UsePriceMmgtAlgo"):
        yes, none = ("STATE_YES", "STATE_DEFAULT") if ctype == "ThreeStateBoolean" else ("USE", "DEFAULT")
        return "if (%s != %s) %s(%s == %s ? 1 : 0);" % (o, none, put, o, yes)
    if ctype == "Decimal":
        return "if (!%s.isUnset()) %s(DecimalFunctions::decimalStringToDisplay(%s));" % (o, put, o)
    if ctype == "std::string" and kind == "f":
        return "if (!%s.empty()) %s(std::stod(%s));" % (o, put, o)
    value = "static_cast<int>(%s)" % o if ctype == "Origin" else "%s == 1" % o if (struct, member) in spec.PROTO_READS and ctype == "int" else o
    if when == "truthy":
        cond = "!%s.empty()" % o if ctype == "std::string" else "%s == 1" % o if value != o and ctype == "int" else "%s" % o if ctype == "bool" else "%s != 0" % o
    elif when == "float":
        cond = "%s != UNSET_DOUBLE" % o
    else:
        cond = "%s != UNSET_INTEGER" % o
    return "if (%s) %s(%s);" % (cond, put, value)


def twins(G, reqs):
    by_call = {}
    for q in spec.REQUESTS:
        by_call[re.match(r"c\.(\w+)\(", q["call"]).group(1)] = q
    by_name = {q["name"]: q for q in spec.REQUESTS}
    objects = []
    for (writer, variable), (_py, struct, message) in spec.PROTO_CREATORS.items():
        body = [twin_statement(G, struct, message, attr, field, when, conv)
                for var, attr, field, when, conv in T.CREATORS[writer] if var == variable]
        objects.append("inline void toProto(const %s& o, protobuf::%s& p) {\n%s\n}\n" % (struct, message, "\n".join("  " + b for b in body)))
    methods = []
    for name, message, short in reqs:
        rule = F._REQUESTS.get(short)
        if not isinstance(rule, tuple):
            continue
        plain, fields = rule[0], rule[1].split()
        q = by_name[spec.PROTO_PLAIN[plain]] if plain in spec.PROTO_PLAIN else by_call[plain]
        body = []
        for word, (ctype, pname) in zip(fields, G.cpp_params(q["params"])):
            field = word.partition(":")[0]
            kind = KINDS[message][field]
            if kind == "m:Contract":
                body.append("toProto(%s, *p.mutable_%s(), nullptr);" % (pname, field.lower()))
            elif kind == "map":
                body.append("toProto(%s, *p.mutable_%s());" % (pname, field.lower()))
            elif kind == "b" and G.bare(ctype) == "int":
                body.append("p.%s(%s != 0);" % (setter(field), pname))
            elif G.bare(ctype) == "faDataType":
                body.append("p.%s(static_cast<int>(%s));" % (setter(field), pname))
            else:
                body.append("p.%s(%s);" % (setter(field), pname))
        methods.append("  void %s(%s) {\n    protobuf::%s p;\n%s    %s(p);\n  }" % (
            q["name"], q["params"], message, "".join("    %s\n" % b for b in body), name))
    return """// %s
// For the tests of the protobuf forms: the message a program fills from the
// object of a plain request, and a client whose plain requests go out
// through their protobuf form. Included in namespace ibxtest.

%s
// The options of a request.
inline void toProto(const TagValueListSPtr& tags, google::protobuf::Map<std::string, std::string>& map) {
  if (!tags)
    return;
  for (const TagValueSPtr& tag : *tags) {
    if (tag)
      map[tag->tag] = tag->value;
  }
}

// A contract with its legs; a leg takes its price from the order.
inline void toProto(const Contract& o, protobuf::Contract& p, const Order* order) {
  toProto(o, p);
  if (o.comboLegs) {
    size_t at = 0;
    for (const ComboLegSPtr& leg : *o.comboLegs) {
      protobuf::ComboLeg& f = *p.add_combolegs();
      if (leg)
        toProto(*leg, f);
      if (order && order->orderComboLegs && at < order->orderComboLegs->size() && (*order->orderComboLegs)[at]
          && (*order->orderComboLegs)[at]->price != UNSET_DOUBLE)
        f.set_perlegprice((*order->orderComboLegs)[at]->price);
      ++at;
    }
  }
  if (o.deltaNeutralContract) {
    p.mutable_deltaneutralcontract()->set_conid(o.deltaNeutralContract->conId);
    p.mutable_deltaneutralcontract()->set_delta(o.deltaNeutralContract->delta);
    p.mutable_deltaneutralcontract()->set_price(o.deltaNeutralContract->price);
  }
}

// The requests whose message is read field by field. The others are
// declared by the class that derives from this one.
class ProtoTwinRequests : public EClientSocket {
public:
  using EClientSocket::EClientSocket;

%s
};
""" % (G.NOTE.replace("cpp/gen/spec.py", "cpp/gen/proto.py and the tables of the Python client's protobuf layer"),
       "\n".join(objects), "\n".join(methods))


# ── The files ──

def generate(G):
    callbacks, reqs = proto_callbacks(), proto_requests()
    row_decls, row_defs = rows(G)
    head_decls, head_defs = heads(G)
    tramp_decls, tramps, table, pairs = trampolines(G, callbacks)
    copy_decls, copy_defs = copies(G)
    inc, request_defs, by_hand = requests(G, reqs)
    note = G.NOTE.replace("cpp/gen/spec.py", "cpp/gen/proto.py and the tables of the Python client's protobuf layer")

    G.OUT["cpp/include/EWrapper_prototypes_proto.h"] = (
        "// %s\n// The protobuf callbacks of the wrapper (IBX_PROTOBUF): each comes before\n"
        "// the plain callback of its answer, with the answer as its message.\n\n"
        "#define EWrapper_methods_proto(X, ARGS) \\\n%s\n" % (
            note, " \\\n".join("  X(%s, (%s))" % (name, parameter(message)) for name, message, _ in callbacks)))
    G.OUT["cpp/include/ibx_requests_proto.inc"] = (
        "// %s\n// The protobuf requests of the client (IBX_PROTOBUF), declared inside the\n"
        "// class: each reads its message and makes the plain request.\n%s\n" % (note, "\n".join(inc)))
    messages = sorted({m for _, m, _ in callbacks} | {m for _, m, _ in reqs})
    G.OUT["cpp/include/ibx_protobuf.h"] = """// %s
// The messages of the protobuf callbacks and requests (IBX_PROTOBUF), as
// protoc writes them from the .proto files of the official client, and the
// numbers of the protobuf callbacks.
#pragma once
#ifndef IBX_PROTOBUF_H
#define IBX_PROTOBUF_H

#include <cstdint>

%s

namespace ibx {

enum ProtoCallback {
%s
  PROTO_CALLBACK_COUNT
};

// A set of protobuf callbacks: the ones a wrapper leaves empty.
struct ProtoCallbackSet {
  std::uint64_t bits[(PROTO_CALLBACK_COUNT + 63) / 64] = {};

  bool has(ProtoCallback c) const { return (bits[c >> 6] >> (c & 63)) & 1; }
  void add(ProtoCallback c) { bits[c >> 6] |= std::uint64_t(1) << (c & 63); }
};

}  // namespace ibx

#endif
""" % (note, "\n".join('#include "%s.pb.h"' % m for m in messages), "\n".join("  %s," % enum_name(name) for name, _, _ in callbacks))

    G.OUT["cpp/src/ibx_proto_generated.inc"] = """// %s
// What the generated and the hand-written parts of the protobuf forms
// share, declared in namespace ibx::pb.

// The fields a message takes from an object.
%s

// The fields a message takes from the arguments of its plain callback.
%s

// The messages and callbacks written by hand (proto.cpp).
%s

// The fields of a request message, read into the object of the plain request.
%s
""" % (note, "\n".join(row_decls), "\n".join(head_decls), "\n".join(tramp_decls), "\n".join(copy_decls))

    errors = sorted(T.CLIENT_ERRORS)
    G.OUT["cpp/src/proto_generated.cpp"] = """// %s
// The protobuf forms (IBX_PROTOBUF): the fields of the messages, the
// callback table that gives each protobuf callback before its plain
// callback, and the protobuf requests.

#include <algorithm>

#include "DefaultEWrapper.h"
#include "ibx_proto.h"

namespace ibx {
namespace pb {

// ── Messages from objects ──

%s
// ── Messages from the arguments of a callback ──

%s
// The errors the official client makes itself: they have no message.
bool clientError(int code)
{
  static const int codes[] = {%s};
  return std::binary_search(std::begin(codes), std::end(codes), code);
}

// ── Request messages ──

%s
}  // namespace pb
}  // namespace ibx

// ── Callbacks ──

%s
namespace ibx {
const ibx_callbacks callbackTable = {
%s
};
}  // namespace ibx

// ── The wrapper that does nothing: it notes each protobuf callback that
// reached it, and the message of that one is no longer built ──

%s

// ── Requests ──

%s""" % (note, "\n".join(row_defs), "\n".join(head_defs), ", ".join(str(c) for c in errors), "\n".join(copy_defs),
         "\n".join(tramps), "\n".join(table),
         "\n".join("void DefaultEWrapper::%s(const protobuf::%s&) { m_ibxUnused.add(ibx::%s); }" % (name, message, enum_name(name))
                   for name, message, _ in callbacks),
         "\n".join(request_defs))

    # For the tests: a wrapper derived from EWrapper notes each protobuf
    # callback, and the pairs (plain callback, protobuf callback before it).
    G.PROTO_CHECK_METHODS = "\n".join(
        "  void %s(const protobuf::%s& m) override { proto(\"%s\", m); }" % (name, message, name) for name, message, _ in callbacks)
    G.PROTO_LOG_METHODS = "\n".join(
        "  void %s(const protobuf::%s& m) override { protoRow(\"%s\", m); }" % (name, message, name) for name, message, _ in callbacks)
    G.PROTO_PAIRS = ", ".join('{"%s", "%s"}' % p for p in pairs)
    G.PROTO_BY_HAND = by_hand
    G.OUT["cpp/tests/generated_proto_twins.inc"] = twins(G, reqs)
    lists = {"REQ_ID": [], "ORDER_ID": [], "NO_ID": [], "NOTHING": []}
    for name, message, short in reqs:
        fields = KINDS[message]
        kind = "NOTHING" if short in F.NOT_GIVEN else "REQ_ID" if "reqId" in fields else "ORDER_ID" if "orderId" in fields else "NO_ID"
        lists[kind].append("X(%s, %s)" % (name, message))
    G.PROTO_REQUEST_LISTS = "\n".join("#define IBX_TEST_PROTO_REQUESTS_%s(X) %s" % (kind, " ".join(items)) for kind, items in lists.items())
