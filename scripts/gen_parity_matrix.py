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
import os
import pathlib
import re
import sys

ROOT = pathlib.Path(__file__).resolve().parents[1]
COVERAGE = ROOT / "docs" / "book" / "src" / "reference" / "coverage-data.md"
OUT = ROOT / "docs" / "capabilities.md"
README = ROOT / "README.md"

#: Where the README carries the same answer. Written between these, so the
#: page a reader lands on cannot drift from the one this script generates —
#: a comparison somebody keeps by hand is a comparison that is wrong by the
#: second release.
README_OPEN = "<!-- capabilities:begin — written by scripts/gen_parity_matrix.py -->"
README_SHUT = "<!-- capabilities:end -->"

#: Where a reference client lives when it is not on the import path.
#:
#: The official client is not a dependency of this repository — it is what this
#: client replaces — so it is installed beside the tests rather than depended
#: on, and read from the import path like any other package.
#:
#: These are the fallbacks for a copy that was unpacked rather than installed.
#: A directory under somebody's home used to be named here, which made this
#: page reproducible on exactly one machine: the check that compares a
#: generated file against the committed one then failed everywhere else, and
#: the columns for both reference clients silently vanished from the answer.
#: `IBX_REFERENCE_CLIENTS` names such a directory where there is one.
EXTRA_PATHS = [
    pathlib.Path(p) for p in os.environ.get("IBX_REFERENCE_CLIENTS", "").split(os.pathsep) if p
] + [ROOT / "vendor"]

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


def stub_methods():
    """Calls that exist and state why they cannot be served.

    Read from the generator that already keeps the list, so the two pages
    cannot disagree about which a call is.
    """
    sys.path.insert(0, str(ROOT / "scripts"))
    try:
        import gen_api_docs

        return set(gen_api_docs.STUB_METHODS)
    except Exception:
        return set()
    finally:
        sys.path.pop(0)


def surface_from_reference(page: str):
    """Every method a generated reference page names, or None where absent.

    The Rust surface cannot be imported and enumerated the way a Python one
    can, and guessing it from the binding is how this page came to mark calls
    as carried in Rust that exist only in Python. The reference pages are
    produced from the source on every commit, so they are the surface.
    """
    path = ROOT / "docs" / "book" / "src" / "api" / page
    if not path.is_file():
        return None
    names = set(re.findall(r"^#### `([A-Za-z_][A-Za-z_0-9]*)", path.read_text(), re.M))
    return names or None


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


def counted(rows, columns):
    """Carried, taken and absent per client, over one table's rows."""
    for name, fn in columns:
        served = sum(1 for r in rows if fn(r) == SERVED)
        taken = sum(1 for r in rows if fn(r) == TAKEN)
        yield name, served, taken, len(rows) - served - taken


def write_readme(page, calls, backs, columns, back_columns, beyond):
    """Put the same answer on the page a reader lands on.

    Somebody deciding whether to put this client where their gateway is has
    one question — is anything I already use missing — and sending them to
    another file to find out is sending most of them away. The whole page goes
    in, behind a fold so it does not bury the rest of the readme, under a
    summary that answers the question on its own.

    Written rather than kept: a comparison table maintained by hand is a
    comparison table that is wrong by the second release, and this one is read
    from the clients themselves on every commit.
    """
    if not README.is_file():
        return
    text = README.read_text()
    if README_OPEN not in text or README_SHUT not in text:
        return

    calls_by = {n: (s, t, a) for n, s, t, a in counted(calls, columns)}
    backs_by = {n: (s, t, a) for n, s, t, a in counted(backs, back_columns)}
    # One line per client, and this client's two surfaces last, because they
    # are the answer and the rest is what the answer is measured against.
    ours = [n for n in calls_by if n.startswith("ibx")]
    theirs = [n for n in calls_by if not n.startswith("ibx") and n != "Gateway wire"]

    def row(name):
        served, taken, absent = calls_by.get(name, (0, 0, 0))
        b_served, b_taken, b_absent = backs_by.get(name, (0, 0, 0))
        of_calls = f"{served} / {len(calls)}"
        of_backs = f"{b_served} / {len(backs)}" if name in backs_by else "—"
        missing = []
        if absent:
            missing.append(f"{absent} absent")
        if taken:
            missing.append(f"{taken} taken, not applied")
        if b_taken:
            missing.append(f"{b_taken} callback taken, not applied"
                           if b_taken == 1 else
                           f"{b_taken} callbacks taken, not applied")
        if b_absent:
            missing.append(f"{b_absent} callbacks absent")
        note = ", ".join(missing) or "nothing missing"
        mine = name.startswith("ibx")
        cells = [f"**{name}**" if mine else name,
                 f"**{of_calls}**" if mine else of_calls,
                 f"**{of_backs}**" if mine else of_backs,
                 note]
        return "| " + " | ".join(cells) + " |"

    summary = [
        "| Client | Calls carried | Callbacks carried | |",
        "| --- | ---: | ---: | --- |",
    ] + [row(n) for n in theirs + ours]

    # Said from the figures, not asserted: if a gap ever opens this line
    # reports it instead of claiming there is none.
    gone = sum(calls_by.get(n, (0, 0, 0))[2] + backs_by.get(n, (0, 0, 0))[2] for n in ours)
    held = sum(calls_by.get(n, (0, 0, 0))[1] + backs_by.get(n, (0, 0, 0))[1] for n in ours)
    if gone:
        verdict = (
            f"**{gone} of them are absent here**, counted rather than left out of "
            "the denominator. The table below says which."
        )
    else:
        verdict = (
            "**Nothing on that list is absent here.** "
            + (f"{held // max(len(ours), 1)} exist and never fire, because the venue "
               "states nothing on this connection for them to carry: there is no terminal "
               "between this client and the venue to make a verification handshake with, "
               "no socket layer of the reference client's own to report an error from, "
               "this connection does not reroute a request to another contract, and "
               "neither an exchange-for-physical quote nor a delta-neutral pairing is "
               "stated on it — a share, a fund and two futures were read together and the "
               "venue stated fifteen kinds of tick, none of them those. Each says so where "
               "it is declared, so a program that implements one still compiles and runs. "
               if held else "")
            + "Everything else is carried."
        )

    block = [
        README_OPEN,
        "",
        "## Capabilities",
        "",
        f"One row per capability, one column per client — every one of the "
        f"{len(calls)} calls and {len(backs)} callbacks the documented API names, "
        "read from each client rather than recalled.",
        "",
    ] + summary + [
        "",
        verdict,
    ] + ([
        "",
        f"**And {len(beyond)} more beyond that list.** The connection a terminal opens "
        "carries more than the documented calls describe — what the venue permits this "
        "account, which algorithms it offers, the order defaults it fills an order's "
        "blanks from, what it says about an issuer, which session holds the account — "
        "and a client that speaks that connection can answer them. Most have no call in "
        "the documented API at all; a few are one a reference client happens to name "
        "too, and the table below marks which is which, under *Beyond the canonical "
        "list*.",
    ] if beyond else []) + [
        "",
        "Every figure here is read from the client it names, on the machine that "
        "generated it. A client that is not installed is left out rather than "
        "filled in from memory.",
        "",
        "<details>",
        "<summary><b>The whole table — every call, every callback, and what the "
        "gateway connection carries beyond them</b></summary>",
        "",
    ]
    # The page itself, minus its own title and the note about where it comes
    # from, which the readme has said already.
    body = [ln for ln in page if ln.strip() != "# Capabilities"]
    while body and not body[0].strip():
        body.pop(0)
    if body and body[0].startswith("*Generated by"):
        body.pop(0)
    block += body + [
        "</details>",
        "",
        "The same table stands on its own in "
        "[docs/capabilities.md](docs/capabilities.md), and what each claim rests "
        "on is in [docs/evidence.md](docs/evidence.md).",
        "",
        README_SHUT,
    ]

    head, _, rest = text.partition(README_OPEN)
    _, _, tail = rest.partition(README_SHUT)
    README.write_text(head + "\n".join(block) + tail)


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

    # What the connection carries, which is what puts a row in this table at
    # all. Constant by construction and kept anyway: the whole point of the
    # page is the gap between this column and the next one, and a reader
    # cannot see a gap against a column that is not there.
    columns = [("Gateway wire", lambda r: SERVED)]
    read = [
        "**Gateway wire** — the connection a terminal opens. Every row is "
        "something it carries: either the documented API names it, or this "
        "client was written after reading it off that connection.",
        "**TWS API** — the documented surface, as this repository generates it "
        "from source. Where this column is empty and the one beside it is not, "
        "the connection carries something the documented API never named.",
    ]
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
    columns.insert(1, ("TWS API", lambda r: SERVED if r.get("documented", True) else ABSENT))
    columns += [
        ("ibx Rust", lambda r: ours(r["rust"])),
        ("ibx Python", lambda r: ours(r["python"])),
    ]

    back_columns = [("Gateway wire", lambda r: SERVED)]
    if ibapi_backs is not None:
        back_columns.append(("ibapi", lambda r: mark(known(r, ibapi_backs))))
    if async_backs is not None:
        back_columns.append(("ib_async", lambda r: mark(known(r, async_backs))))
    back_columns.insert(1, ("TWS API", lambda r: SERVED))
    back_columns += [
        ("ibx Rust", lambda r: ours(r["rust"])),
        ("ibx Python", lambda r: ours(r["python"])),
    ]

    out = [
        "# Capabilities",
        "",
        "*Generated by `scripts/gen_parity_matrix.py`. Do not edit.*",
        "",
        "One row per capability, one column per client. This client exists to be",
        "put where a gateway was, so the question is not what it can do but",
        "whether anything you already use is missing — which is a question about",
        "the columns, not about the rows.",
        "",
        "**Every row is something the gateway connection carries.** Most of them",
        "the documented API names too, and those are the rows a port has to",
        "match. The last table is the rest: what that connection carries and the",
        "documented API never named — the terminal reads it, so it is on the",
        "wire, and a client that speaks the wire can answer it.",
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

    # What each surface has that the canonical list does not name. Each column
    # is read from its own surface: taking the Python binding's methods and
    # marking both surfaces carried invented a Rust column, and calls that
    # exist only in the binding were published as Rust's too.
    beyond: list[str] = []
    ours_extra, _ = surface_of("ibx", "EClient")
    rust_extra = surface_from_reference("rust-reference.md")
    if ours_extra is not None:
        # Both tables above, not only the calls. The Rust surface is read off
        # its reference page, which names the callbacks beside the calls — so
        # measured against the calls alone, every documented callback this
        # client implements was published here as something beyond the
        # canonical list, in a table whose first column says "Call".
        canon = {plain(r[spelling]) for r in calls + backs for spelling in ("snake", "camel")}
        rust_plain = {plain(n) for n in (rust_extra or set())}
        both = sorted(
            {n for n in ours_extra if plain(n) not in canon}
            | {n for n in (rust_extra or set()) if plain(n) not in canon},
            # The spelling breaks the tie, so the same one is picked every
            # time. Sorted on the comparison key alone, which of the two
            # surfaces' spellings a row was written under came out of a set's
            # iteration order and changed from one run of this script to the
            # next, for a page that is checked against what is committed.
            key=lambda name: (plain(name), name),
        )
        # One row per capability, not one per spelling: the two surfaces name
        # the same thing in their own cases.
        seen, extra = set(), []
        for name in both:
            if plain(name) in seen:
                continue
            seen.add(plain(name))
            extra.append(name)
        beyond = extra
        if extra:
            # A call that exists and states why it cannot be served is not
            # carried, and the canonical table already tells the two apart.
            # Told apart only there, a call in this table read as carried while
            # the page's own key said otherwise.
            stubs = {plain(n) for n in stub_methods()}
            rows = [{
                "category": "", "snake": n, "camel": n, "cpp": n,
                "rust": ("STUB" if plain(n) in stubs else "Y")
                        if plain(n) in rust_plain else "-",
                "python": ("STUB" if plain(n) in stubs else "Y")
                          if plain(n) in {plain(m) for m in ours_extra} else "-",
                # The documented surface names it after all where a reference
                # client does: the canonical list this page is built from is
                # not the whole of what that client publishes.
                "documented": bool(
                    (ibapi_calls and known({"snake": n, "camel": n, "cpp": n}, ibapi_calls))
                    or (async_calls and known({"snake": n, "camel": n, "cpp": n}, async_calls))
                ),
            } for n in extra]
            out += table(
                rows, columns, "Beyond the canonical list",
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
    write_readme(out, calls, backs, columns, back_columns, beyond)
    print(f"{OUT.relative_to(ROOT)} — {len(calls)} calls, {len(backs)} callbacks, "
          f"{len(columns)} columns")
    for line in totals:
        print("   ", line.strip("| ").replace(" | ", "  "))
    return 0


if __name__ == "__main__":
    sys.exit(main())
