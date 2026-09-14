"""One table: every call the TWS API names, and which client carries it.

This client exists to be put where a gateway was, so the question a reader
actually has is not "what can it do" but "is anything I use missing". A page
that describes only this client cannot answer that. A row per call, a column
per client, and a mark in each, can.

Every column is read from the client it names — not recalled, not asserted:

  TWS API     the canonical list this repository already generates, which is
              IBKR's own Python client's surface
  ibapi       that client, imported and enumerated
  ib_async    the widely used asynchronous client, imported and enumerated
  ibx         this client, from the coverage matrix the build already checks

A column can only be written for a client that is installed. One that is not
is left out of the table entirely rather than filled with guesses, and the page
says which were read and at what version.

    python scripts/gen_parity_matrix.py
"""

import importlib
import inspect
import pathlib
import re
import sys

ROOT = pathlib.Path(__file__).resolve().parents[1]
COVERAGE = ROOT / "docs" / "book" / "src" / "reference" / "coverage-data.md"
OUT = ROOT / "docs" / "parity.md"

#: Where a reference client lives when it is not on the import path. The
#: official client is not a dependency of this repository — it is what this
#: client replaces — so it is read from wherever it has been unpacked.
EXTRA_PATHS = [
    pathlib.Path.home() / ".claude/jobs/9a4be4a5/tmp/pyref",
    ROOT / "vendor",
]

#: What a mark means. Three states, because two would lie: a call that exists
#: and reports why it cannot be served is not the same as one that is absent,
#: and a reader porting a program needs to tell them apart.
SERVED, TAKEN, ABSENT = "●", "◐", "·"


def as_camel(snake: str) -> str:
    """`req_mkt_data` as a Python client spells it: `reqMktData`."""
    head, *rest = snake.split("_")
    return head + "".join(part[:1].upper() + part[1:] for part in rest)


def rows_from_coverage():
    """The canonical calls and callbacks, with this client's own two columns.

    The matrix's third column is the C++ name — `eConnect` — and a Python
    client does not name its methods that way, so what is asked of one is the
    camel spelling of the call itself. The C++ name is kept as a second guess
    for the few that differ.

    Read from the generated matrix rather than restated here: that file is
    produced from the source and checked against it on every commit, so there
    is one list and it cannot drift from a second copy.
    """
    text = COVERAGE.read_text()
    calls, backs, section = [], [], None
    for line in text.splitlines():
        if line.startswith("## EClient"):
            section = "calls"
            continue
        if line.startswith("## EWrapper"):
            section = "backs"
            continue
        if line.startswith("## "):
            section = None
            continue
        if not section or not line.startswith("| ") or "`" not in line:
            continue
        cells = [c.strip() for c in line.strip().strip("|").split("|")]
        if section == "calls" and len(cells) >= 5 and cells[1].startswith("`"):
            calls.append({
                "category": cells[0],
                "snake": cells[1].strip("`"),
                "camel": as_camel(cells[1].strip("`")),
                "cpp": cells[2].strip("`"),
                "rust": cells[3],
                "python": cells[4],
            })
        elif section == "backs" and len(cells) >= 4 and cells[1].startswith("`"):
            backs.append({
                "category": cells[0],
                "snake": cells[1].strip("`"),
                "camel": as_camel(cells[1].strip("`")),
                "cpp": cells[1].strip("`"),
                "rust": cells[2],
                "python": cells[3],
            })
    # The category cell is left blank on continuation rows in that file.
    for group in (calls, backs):
        held = ""
        for row in group:
            held = row["category"] or held
            row["category"] = held
    return calls, backs


def surface_of(module_name, *attr_paths):
    """Every public method a client's classes define, or None where absent.

    More than one class may be named, and the answer is their union: a client
    that splits the transport from the facade carries some calls on one and
    some on the other, and asking only the transport marks the facade's own as
    missing. One of them does exactly that.

    None and an empty set are different answers: a client that is not
    installed cannot be reported on, and saying it carries nothing would be a
    claim about it rather than about this machine.
    """
    # Appended, never prepended. One of these directories carries a build of
    # a shared dependency for another interpreter, and putting it first made
    # an installed client fail to import — which this function would then have
    # reported as a client that is not installed, and the page as a column
    # nobody could see.
    for extra in EXTRA_PATHS:
        if extra.is_dir() and str(extra) not in sys.path:
            sys.path.append(str(extra))
    try:
        module = importlib.import_module(module_name)
    except Exception:
        return None, None
    names = set()
    for attr_path in attr_paths:
        obj = module
        for part in attr_path.split("."):
            obj = getattr(obj, part, None)
            if obj is None:
                break
        if obj is None or obj is module:
            continue
        names |= {
            name for name, _ in inspect.getmembers(obj, inspect.isfunction)
            if not name.startswith("_")
        }
    if not names:
        return None, None
    version = getattr(module, "__version__", None)
    return names, version


def package_version(name: str) -> str | None:
    """What a client calls itself, asked of the package and not a module in it.

    A submodule carries no version of its own, and asking one answered
    "unstated" for a client that states it plainly one level up.
    """
    try:
        module = importlib.import_module(name)
    except Exception:
        return None
    stated = getattr(module, "__version__", None)
    if stated:
        return str(stated)
    numbers = getattr(module, "VERSION", None)
    if isinstance(numbers, dict):
        parts = [numbers.get(k) for k in ("major", "minor", "patch")]
        if all(p is not None for p in parts):
            return ".".join(str(p) for p in parts)
    return None


#: What a call used to be called, where the TWS API renamed one. A client that
#: predates the rename carries the thing under its old name, and marking it
#: absent would say it cannot do something it has always done. Each entry is a
#: former name of the same call, and nothing else belongs here: a name that is
#: merely similar is a different call.
FORMER_NAMES = {
    # Renamed when the fees were added alongside the commission.
    "commission_and_fees_report": ["commissionReport"],
}


def plain(name: str) -> str:
    """A name with its spelling taken off, so two clients can be compared.

    One writes `realtimeBar`, another `realTimeBar`, a third `real_time_bar`.
    Compared as written, a client was marked as missing a callback it has —
    which is the one thing a table like this must never do. What is left after
    the letters are lowered and everything else dropped is the same in all
    three, and differs only where the clients genuinely named different things.
    """
    return "".join(ch for ch in name.lower() if ch.isalnum())


def known(row, names) -> bool:
    """Whether a client names this call, however it spells it."""
    spellings = {plain(row["camel"]), plain(row["cpp"]), plain(row["snake"])}
    spellings |= {plain(n) for n in FORMER_NAMES.get(row["snake"], [])}
    return bool(spellings & {plain(n) for n in names})


def mark(present):
    return SERVED if present else ABSENT


def ours(state):
    """This client's own mark, from the coverage matrix's own vocabulary."""
    return {"Y": SERVED, "STUB": TAKEN}.get(state, ABSENT)


def table(rows, columns, title, intro):
    # The category column is only there where the rows have categories; a
    # table of blanks beside every name reads as a column somebody forgot.
    grouped = any(row["category"] for row in rows)
    lead = "| Category | Call | " if grouped else "| Call | "
    rule = "| --- | --- | " if grouped else "| --- | "
    head = [lead + " | ".join(c[0] for c in columns) + " |",
            rule + " | ".join(":---:" for _ in columns) + " |"]
    body = []
    held = None
    for row in rows:
        cells = " | ".join(c[1](row) for c in columns)
        if grouped:
            shown = "" if row["category"] == held else row["category"]
            held = row["category"]
            body.append(f"| {shown} | `{row['snake']}` | {cells} |")
        else:
            body.append(f"| `{row['snake']}` | {cells} |")
    return [f"## {title}", "", intro, ""] + head + body + [""]


def main() -> int:
    if not COVERAGE.is_file():
        print(f"{COVERAGE} is not there; run scripts/gen_api_docs.py first")
        return 1
    calls, backs = rows_from_coverage()

    ibapi_calls, _ = surface_of("ibapi.client", "EClient")
    ibapi_v = package_version("ibapi")
    ibapi_backs, _ = surface_of("ibapi.wrapper", "EWrapper")
    async_calls, async_v = surface_of("ib_async", "client.Client", "IB")
    async_backs, _ = surface_of("ib_async", "wrapper.Wrapper", "IB")

    columns = []
    read = []
    if ibapi_calls is not None:
        columns.append(("ibapi", lambda r: mark(known(r, ibapi_calls))))
        read.append(
            f"**ibapi** — IBKR's own Python client, version `{ibapi_v or 'unstated'}`, "
            "imported and enumerated. This is the copy published to PyPI; the "
            "version IBKR distributes directly is numbered 10.x and names calls "
            "this one predates, so a gap in this column is a gap in the copy "
            "that was read and not necessarily in the client you have."
        )
    if async_calls is not None:
        columns.append(("ib_async", lambda r: mark(known(r, async_calls))))
        read.append(
            f"**ib_async** — version `{async_v or 'unstated'}`, imported and "
            "enumerated across both the transport and the facade, because it "
            "carries some calls on one and some on the other."
        )
    columns += [
        ("ibx Rust", lambda r: ours(r["rust"])),
        ("ibx Python", lambda r: ours(r["python"])),
    ]

    back_columns = []
    if ibapi_backs is not None:
        back_columns.append(("ibapi", lambda r: mark(known(r, ibapi_backs))))
    if async_backs is not None:
        back_columns.append(("ib_async", lambda r: mark(known(r, async_backs))))
    back_columns += [
        ("ibx Rust", lambda r: ours(r["rust"])),
        ("ibx Python", lambda r: ours(r["python"])),
    ]

    out = [
        "# Parity",
        "",
        "*Generated by `scripts/gen_parity_matrix.py`. Do not edit.*",
        "",
        "**Every row is a call the TWS API names.** This client exists to be put",
        "where a gateway was, so the question is not what it can do but whether",
        "anything you already use is missing — which is a question about the",
        "columns, not about the rows.",
        "",
        f"| Mark | Meaning |",
        "| :---: | --- |",
        f"| {SERVED} | Carried: the call exists and does what it says |",
        f"| {TAKEN} | Taken and not applied: the call exists and reports why it cannot be served, rather than failing to exist |",
        f"| {ABSENT} | Absent: no such call |",
        "",
        "Each column is read from the client it names, on the machine that",
        "generated this page:",
        "",
    ] + [f"- {line}" for line in read] + [
        "",
        "A client that is not installed is left out of the table rather than",
        "filled in from memory. A mark here is a thing that was read.",
        "",
    ]

    out += table(
        calls, columns, "Calls",
        "What a program asks the venue for.",
    )
    out += table(
        backs, back_columns, "Callbacks",
        "What the venue says back. `ib_async` delivers these as events as well "
        "as methods, so a mark here says the method exists on its wrapper, not "
        "that the information is unavailable by another route.",
    )

    # What this client has that the documented surface does not name. Read
    # from the binding itself, so it cannot be a list somebody keeps up.
    ours_extra, _ = surface_of("ibx", "EClient")
    if ours_extra is not None:
        canon = {plain(r["snake"]) for r in calls} | {plain(r["camel"]) for r in calls}
        extra = sorted(n for n in ours_extra if plain(n) not in canon)
        if extra:
            rows = [{"category": "", "snake": n, "camel": n, "cpp": n,
                     "rust": "Y", "python": "Y"} for n in extra]
            beyond = [c for c in columns if c[0] != "ibx Rust"]
            beyond = [(n, f) for n, f in beyond if n != "ibx Python"]
            beyond.append(("ibx", lambda r: SERVED))
            out += table(
                rows, beyond, "Beyond the documented API",
                "The terminal's own connection carries more than the documented\n"
                "surface names, and this client speaks that connection — so some of\n"
                "what it answers has no call in the API at all. These fall into three\n"
                "kinds, and the table does not try to sort them: things the venue\n"
                "states that no documented call asks for (what it permits this\n"
                "account, which algorithms it offers, the order defaults it holds,\n"
                "what it says about an issuer, which session holds the account); the\n"
                "same question answered rather than delivered on a callback; and this\n"
                "client's own instrumentation, which is about the client and not the\n"
                "venue.\n\n"
                "A mark against a reference client here means it happens to name the\n"
                "same thing, not that the documented API does.",
            )

    totals = []
    for name, fn in columns:
        served = sum(1 for r in calls if fn(r) == SERVED)
        taken = sum(1 for r in calls if fn(r) == TAKEN)
        totals.append(f"| {name} | {served} | {taken} | {len(calls) - served - taken} |")
    out += [
        "## Calls, counted",
        "",
        "| Client | Carried | Taken, not applied | Absent |",
        "| --- | ---: | ---: | ---: |",
    ] + totals + [""]

    OUT.write_text("\n".join(out) + "\n")
    print(f"{OUT.relative_to(ROOT)} — {len(calls)} calls, {len(backs)} callbacks, "
          f"{len(columns)} columns")
    for line in totals:
        print("   ", line.strip("| ").replace(" | ", "  "))
    return 0


if __name__ == "__main__":
    sys.exit(main())
