"""Writes cpp/gen/python_starts.json: the values the objects of the Python
client start with, for the attributes the protobuf forms read (ibx#235).

A protobuf message leaves out a field whose value is the one its object
starts with; the C++ library follows the Python client there, so the
generator needs these values. Run with the Python the client is built for,
after a change of its classes:

    .venv/Scripts/python.exe cpp/gen/dump_python_starts.py
"""

import decimal
import enum
import json
import os
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(os.path.dirname(HERE))
sys.path.insert(0, HERE)
sys.path.insert(0, os.path.join(ROOT, "src", "python", "compat"))

import ibx  # noqa: E402
import proto_table as T  # noqa: E402
import spec  # noqa: E402

UNSET_DECIMAL = decimal.Decimal(2 ** 127 - 1)


def plain(v):
    """A value as the protobuf layer compares it: a member of an enumeration is its value."""
    if isinstance(v, enum.Enum):
        v = v.value
    return v[0] if isinstance(v, tuple) else v


def typed(v):
    v = plain(v)
    if v is None:
        return ["none", None]
    if isinstance(v, bool):
        return ["bool", v]
    if isinstance(v, int):
        return ["int", v]
    if isinstance(v, float):
        return ["float", "max" if v == sys.float_info.max else v]
    if isinstance(v, str):
        return ["str", v]
    if isinstance(v, decimal.Decimal):
        return ["decimal", "unset" if v == UNSET_DECIMAL else str(v)]
    return ["other", type(v).__name__]


def main():
    out = {}
    for table, rows_of in ((spec.PROTO_OBJECTS, lambda key: [r[3] for r in T.OBJECTS[key[0]] if r[2] == key[1]]),
                           (spec.PROTO_CREATORS, lambda key: [r[1] for r in T.CREATORS[key[0]] if r[0] == key[1]])):
        for key, entry in table.items():
            name = entry[0]
            cls = getattr(ibx, name, None) if name else None
            if cls is None:
                continue
            fresh = cls()
            values = out.setdefault(name, {})
            for attr in rows_of(key):
                if hasattr(fresh, attr):
                    values[attr] = typed(getattr(fresh, attr))
    # One attribute per line: a change of the classes shows as a small difference.
    blocks = []
    for name in sorted(out):
        rows = ",\n".join("  %s: %s" % (json.dumps(attr), json.dumps(out[name][attr])) for attr in sorted(out[name]))
        blocks.append(" %s: {\n%s\n }" % (json.dumps(name), rows))
    path = os.path.join(HERE, "python_starts.json")
    with open(path, "w", encoding="utf-8", newline="\n") as f:
        f.write("{\n%s\n}\n" % ",\n".join(blocks))
    print("%s: %d classes, %d attributes" % (path, len(out), sum(len(v) for v in out.values())))


if __name__ == "__main__":
    main()
