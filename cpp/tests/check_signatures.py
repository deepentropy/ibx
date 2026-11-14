"""Compares the headers of this library with the ones of the official C++
client: the callbacks, the requests, the fields of the structures and the
tick kinds must have the same names and types, so that a program written
for the official client compiles against this one (ibx#235).

    python cpp/tests/check_signatures.py <official client folder> <cpp/include>

The official headers are read where they are installed; nothing of them is
copied. Exit code 1 when something differs beyond the lists below.
"""

import os
import re
import sys

# Requests of this client the official list no longer has.
OURS_ONLY_REQUESTS = {"reqFundamentalData", "cancelFundamentalData"}

# Members of the official structures this library declares in another way,
# with the reason.
KNOWN_FIELDS = {
    # (structure, member): reason
}

# Public functions of the official client class that are part of its
# socket and message machinery, which this client does not have.
NOT_REQUESTS = {
    "startApi", "validateInvalidSymbols", "setOptionalCapabilities", "setConnectOptions", "disableUseV100Plus",
    "eConnectBase", "eDisconnectBase", "setClientId", "setExtraAuth", "setHost", "setPort", "EncodeContract",
    "EncodeTagValueList", "EncodeMsgId", "EncodeFieldMax", "EncodeRawInt", "EncodeField", "eDisconnect",
    "prepareBufferImpl", "prepareBuffer",
}


# Classes whose members are their own machinery, not data a program reads.
CLASSES_WITH_STATE = {"EReader", "EReaderOSSignal", "EClient", "EClientSocket", "EDecoder", "EMessage", "EMutex", "ESocket", "EOrderDecoder"}


def read(path):
    with open(path, encoding="utf-8", errors="replace") as f:
        text = f.read()
    text = re.sub(r"/\*.*?\*/", " ", text, flags=re.S)
    return re.sub(r"//[^\n]*", "", text)


def norm(ctype):
    t = " ".join(ctype.split())
    t = re.sub(r"\s*([<>,&*])\s*", r"\1", t)
    return t.replace("const ", "const ").strip()


def split_params(text):
    out, depth, cur = [], 0, ""
    for ch in text:
        if ch in "<(":
            depth += 1
        elif ch in ">)":
            depth -= 1
        if ch == "," and depth == 0:
            out.append(cur)
            cur = ""
        else:
            cur += ch
    if cur.strip():
        out.append(cur)
    return out


def param_types(params):
    """The types of a parameter list, without the names."""
    types = []
    for p in split_params(params):
        p = " ".join(p.split())
        p = re.sub(r"\s*=\s*[^,]+$", "", p)
        m = re.match(r"^(.*?)([A-Za-z_]\w*)$", p)
        if not m or m.group(1).strip() in ("", "const", "unsigned"):
            types.append(norm(p))  # a type with no name
        else:
            types.append(norm(m.group(1)))
    return types


def callbacks(folder):
    text = read(os.path.join(folder, "EWrapper_prototypes.h")).replace("\\\n", " ")
    lists = {}
    for kind in ("regular", "proto"):
        m = re.search(r"#define EWrapper_methods_%s\(X, ARGS\)(.*?)(?=\n\s*#|\Z)" % kind, text, re.S)
        found = {}
        if m:
            for name, params in re.findall(r"X\(\s*(\w+)\s*,\s*\((.*?)\)\s*\)(?=\s*X\(|\s*$)", m.group(1), re.S):
                found[name] = param_types(params)
        lists[kind] = found
    return lists


def class_body(text, name):
    m = re.search(r"(?:class|struct)\s+(?:TWSAPIDLLEXP\s+)?%s\b[^;{]*\{" % re.escape(name), text)
    if not m:
        return None
    depth, at = 1, m.end()
    while depth and at < len(text):
        depth += {"{": 1, "}": -1}.get(text[at], 0)
        at += 1
    return text[m.end():at - 1]


def requests(folder, ours):
    if ours:
        text = read(os.path.join(folder, "ibx_requests.inc"))
        extra = os.path.join(folder, "ibx_requests_proto.inc")
        if os.path.exists(extra):
            text += read(extra)
    else:
        text = class_body(read(os.path.join(folder, "EClient.h")), "EClient") or ""
    text = " ".join(text.split())
    found = {}
    for name, params in re.findall(r"\bvoid (\w+)\s*\(([^()]*)\)\s*;", text):
        found[name] = param_types(params)
    return found


MEMBER = re.compile(r"^\s*((?:const\s+)?[A-Za-z_][\w:]*(?:\s*<[^;=()]*>)?(?:\s+long)?)\s+(\w+)\s*(?:=[^;]*)?;\s*$")


def strip_blocks(body):
    """The body of a structure without the bodies of its functions."""
    out, depth = "", 0
    for ch in body:
        if ch == "{":
            depth += 1
        elif ch == "}":
            depth -= 1
            if depth == 0:
                out += ";\n"
        elif depth == 0:
            out += ch
    return out


def structures(folder):
    found = {}
    for name in sorted(os.listdir(folder)):
        if not name.endswith(".h"):
            continue
        text = read(os.path.join(folder, name))
        # The structures; of the classes only the data one (the others keep private state).
        for struct in re.findall(r"^\s*(?:struct|class)\s+(?:TWSAPIDLLEXP\s+)?(\w+)\s*(?::[^{;]*)?\{", text, re.M):
            if struct in CLASSES_WITH_STATE:
                continue
            body = class_body(text, struct)
            if body is None:
                continue
            members = {}
            for line in strip_blocks(body).split(";"):
                line = " ".join(line.split())
                if not line or "(" in line or line.startswith(("typedef", "static", "friend", "public", "private", "protected", "enum", "return", "using")):
                    continue
                line = re.sub(r"^(public|private|protected)\s*:\s*", "", line)
                m = MEMBER.match(line + ";")
                if m:
                    members[m.group(2)] = norm(m.group(1))
            if members:
                found.setdefault(struct, members)
    return found


def tick_types(folder):
    text = read(os.path.join(folder, "EWrapper.h"))
    m = re.search(r"enum\s+TickType\s*\{(.*?)\}", text, re.S)
    out, value = {}, -1
    for item in (m.group(1) if m else "").split(","):
        item = item.strip()
        if not item:
            continue
        name, _, given = item.partition("=")
        value = int(given) if given.strip() else value + 1
        out[name.strip()] = value
    return out


def main():
    official, ours = sys.argv[1], sys.argv[2]
    problems, notes = [], []

    theirs_cb, ours_cb = callbacks(official), callbacks(ours)
    proto_built = bool(ours_cb["proto"]) or os.path.exists(os.path.join(ours, "EWrapper_prototypes_proto.h"))
    for kind in ("regular", "proto"):
        if kind == "proto":
            path = os.path.join(ours, "EWrapper_prototypes_proto.h")
            if not os.path.exists(path):
                notes.append("the protobuf callbacks are not compared: no EWrapper_prototypes_proto.h")
                continue
            text = read(path).replace("\\\n", " ")
            ours_list = {n: param_types(p) for n, p in re.findall(r"X\(\s*(\w+)\s*,\s*\((.*?)\)\s*\)(?=\s*X\(|\s*$)", text, re.S)}
        else:
            ours_list = ours_cb[kind]
        for name, types in theirs_cb[kind].items():
            if name not in ours_list:
                problems.append("callback %s is missing" % name)
            elif ours_list[name] != types:
                problems.append("callback %s: %s here, %s in the official client" % (name, ours_list[name], types))
        for name in ours_list:
            if name not in theirs_cb[kind]:
                problems.append("callback %s is not in the official client" % name)
        notes.append("%d %s callbacks compared" % (len(theirs_cb[kind]), kind))

    theirs_rq, ours_rq = requests(official, False), requests(ours, True)
    compared = 0
    for name, types in theirs_rq.items():
        if name in NOT_REQUESTS:
            continue
        if name.endswith("ProtoBuf") and not proto_built:
            continue
        compared += 1
        if name not in ours_rq:
            problems.append("request %s is missing" % name)
        elif ours_rq[name] != types:
            problems.append("request %s: %s here, %s in the official client" % (name, ours_rq[name], types))
    for name in ours_rq:
        if name not in theirs_rq and name not in OURS_ONLY_REQUESTS:
            problems.append("request %s is not in the official client" % name)
    notes.append("%d requests compared" % compared)

    theirs_st, ours_st = structures(official), structures(ours)
    compared = 0
    for struct, members in sorted(ours_st.items()):
        if struct not in theirs_st:
            continue
        compared += 1
        for member, ctype in theirs_st[struct].items():
            if member.startswith("m_") and struct not in ("ExecutionFilter",):
                continue  # private state of a class
            if (struct, member) in KNOWN_FIELDS:
                continue
            if member not in members:
                problems.append("%s.%s is missing" % (struct, member))
            elif members[member] != ctype:
                problems.append("%s.%s: %s here, %s in the official client" % (struct, member, members[member], ctype))
        for member in members:
            if member not in theirs_st[struct] and not member.startswith("m_"):
                problems.append("%s.%s is not in the official client" % (struct, member))
    notes.append("%d structures compared" % compared)

    theirs_tt, ours_tt = tick_types(official), tick_types(ours)
    if theirs_tt != ours_tt:
        problems.append("tick kinds differ: %s" % sorted(set(theirs_tt.items()) ^ set(ours_tt.items())))
    notes.append("%d tick kinds compared" % len(theirs_tt))

    for n in notes:
        print(n)
    for p in problems:
        print("DIFFERENT: " + p)
    print("%d differences" % len(problems))
    return 1 if problems else 0


if __name__ == "__main__":
    sys.exit(main())
