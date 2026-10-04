"""Seed corpus of the fuzz targets (ibx#488) from the recorded server frames.

Reads every codec and scenario fixture of tests/fixtures/gw1040/, takes the
frames the gateway received (leg fix_in) and the messages of the compressed
ones, and writes each to the corpus of the targets that decode it:
fuzz/corpus/<target>/<sha1>. Standard library only. Safe to run again: a
file already there is kept.

    python fuzz/seed_corpus.py
"""
import base64
import hashlib
import json
import pathlib
import re
import zlib

ROOT = pathlib.Path(__file__).resolve().parent.parent
FIXTURES = ROOT / "tests" / "fixtures" / "gw1040"
CORPUS = ROOT / "fuzz" / "corpus"


def msg_type(raw):
    m = re.search(rb"(?:^|\x01)35=([^\x01]*)", raw[:64])
    return m.group(1).decode("latin-1") if m else ""


def tag(raw, t):
    m = re.search(rb"(?:^|\x01)" + str(t).encode() + rb"=([^\x01]*)", raw)
    return m.group(1) if m else None


def inner_messages(raw):
    m = re.search(rb"\x0195=(\d+)\x0196=", raw)
    if not m:
        return []
    start = m.end()
    try:
        plain = zlib.decompress(raw[start:start + int(m.group(1))])
    except zlib.error:
        return []
    return [b"8=" + p for p in re.split(rb"(?=8=FIX\.|8=O\x01)", plain)[1:] if p] or [plain]


def targets(raw, conn):
    out = {"connection", "engine"}
    t = msg_type(raw)
    if raw.startswith(b"#%#%"):
        return out | {"ns", "xyz", "key_exchange"}
    if raw.startswith(b"8=FIXCOMP"):
        return out | {"fixcomp", "logon", "hmds_binary", "fix"}
    if raw.startswith(b"8=FIX"):
        out.add("fix")
    out |= {
        "P": {"ticks"}, "Y": {"depth"}, "Z": {"depth"}, "G": {"generic"}, "E": {"tick_by_tick"},
        "W": {"hmds_xml", "hmds_binary"}, "d": {"contracts"}, "8": {"auth"},
    }.get(t, {"auth", "contracts", "hmds_xml"} if t == "U" and conn == "CCP" else
          {"hmds_xml", "hmds_binary"} if t == "U" else {"farm_text", "logon"})
    return out


def vlq(v):
    groups = [(v & 0x7F) | 0x80]
    v >>= 7
    while v:
        groups.append(v & 0x7F)
        v >>= 7
    return bytes(reversed(groups))


def tick_by_tick_samples():
    """No tick-by-tick frame is recorded: a few are built."""
    out = []
    for entries in ([(0, 1759400000, [1234, 0, 100], b"ARCA")],
                    [(2, 1759400001, [5, 7, 0, 300, 200], b"")],
                    [(4, 1759400002, [3, 0, 1], b""), (7, 1759400003, [9, 9, 9, 9, 9], b"X")]):
        data = b""
        for ident, time, numbers, text in entries:
            data += vlq(ident) + vlq(time) + b"".join(vlq(n) for n in numbers)
            data += (text[:-1] + bytes([text[-1] | 0x80])) if text else b"\x80"
        out.append(((len(data) * 8) & 0xFFFF).to_bytes(2, "big") + data)
    return out


def write(target, data):
    folder = CORPUS / target
    folder.mkdir(parents=True, exist_ok=True)
    path = folder / hashlib.sha1(data).hexdigest()
    if not path.exists():
        path.write_bytes(data)
        return 1
    return 0


def main():
    files = sorted(FIXTURES.glob("codec/*.jsonl")) + sorted(FIXTURES.glob("scenarios/*/*.jsonl"))
    written = 0
    for n, path in enumerate(files, 1):
        with path.open(encoding="utf-8") as f:
            next(f)
            for line in f:
                rec = json.loads(line)
                if rec.get("leg") != "fix_in" or not rec.get("raw_b64"):
                    continue
                raw = base64.b64decode(rec["raw_b64"])
                conn = rec.get("conn", "")
                for target in targets(raw, conn):
                    written += write(target, raw)
                for m in inner_messages(raw) if raw.startswith(b"8=FIXCOMP") else []:
                    for target in targets(m, conn):
                        written += write(target, m)
                xml = tag(raw, 6118)
                if xml:
                    written += write("hmds_xml", xml)
        print(f"[{n}/{len(files)}] {path.relative_to(FIXTURES)}: {written} files so far")
    for sample in tick_by_tick_samples():
        written += write("tick_by_tick", sample)
    print(f"done: {written} new files in {CORPUS}")


if __name__ == "__main__":
    main()
