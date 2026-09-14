#!/usr/bin/env python3
"""Check the counts docs/evidence.md publishes against what exists.

A number in a shipped document is a claim. Typed in once and left, it is a
claim that goes quietly wrong: the suites grow, the figure does not, and a
reader is told something that was true months ago. This counts what is there
and compares.

Counting, not running: `--list` and `--collect-only` name every test including
the ones that skip without credentials, which is what the table describes.

Prints what it found and exits non-zero on a disagreement.
"""

import pathlib
import re
import subprocess
import sys

ROOT = pathlib.Path(__file__).resolve().parent.parent
# The inventory sits with the engineering notes: it is a fact about this
# repository rather than about what the client can do, and the compatibility
# matrix is the latter.
STATUS = ROOT / "docs" / "evidence.md"
MATRIX = ROOT / "docs/evidence.md"

#: Row label in the table, and how to count what it describes.
ROWS = {
    "Rust unit and integration": "rust_offline",
    "Rust, live": "rust_live",
    "Python": "python",
    "Python, live": "python_live",
    "Paper compatibility suite": "paper",
}

#: How a phase of the paper suite names itself in its own output.
PHASE = re.compile(r"Phase \d+[a-z]?\b")
STRING_LITERAL = re.compile(r'"[^"\n]*"')


def _targets_needing_a_session() -> list[str]:
    """Every Rust test target that reads the session credentials.

    Read out of the sources rather than kept as a list here. Kept by hand it
    named two targets while five read the credentials, and three suites that
    cannot run without a session were published as needing none — which is the
    kind of drift a hand-kept list is for.

    The paper suite is counted on its own row and is left out of this one.
    """
    live = []
    for path in sorted((ROOT / "tests").glob("*.rs")):
        if "IB_USERNAME" in path.read_text(encoding="utf-8"):
            live.append(path.stem)
    return live


def _run(args: list[str]) -> str:
    done = subprocess.run(args, cwd=ROOT, capture_output=True, text=True)
    if done.returncode != 0:
        # A run that failed produces no count, and the patterns below then
        # match nothing and read as zero. That is the shape of this whole
        # checker's own complaint: a suite that cannot be collected — the
        # extension not built, the build broken — was published as a suite
        # with no tests in it, and the disagreement named the wrong cause.
        raise SystemExit(
            f"`{' '.join(args)}` failed, so what it names cannot be counted:\n"
            f"{(done.stderr or done.stdout).strip()[-400:]}\n"
            "A count nobody can take is not a count that disagrees."
        )
    return done.stdout


def _python() -> str:
    """An interpreter that can collect the Python suite.

    The virtual environment where one exists, and whatever is running this
    otherwise: a checker that only works on a machine set up one particular
    way fails on the machine that matters, which is the one that decides
    whether a commit lands.
    """
    venv = ROOT / ".venv" / "bin" / "python"
    for candidate in (str(venv), sys.executable):
        if candidate == str(venv) and not venv.exists():
            continue
        probe = subprocess.run(
            [candidate, "-c", "import pytest"], cwd=ROOT, capture_output=True,
        )
        if probe.returncode == 0:
            return candidate
    raise SystemExit(
        "no interpreter here can import pytest, so the Python suite cannot be "
        "counted. A count nobody can take is not a count that agrees."
    )


def _cargo_count(args: list[str]) -> int:
    """Tests named by one `--list` run, summed across its targets."""
    out = _run(["cargo", "test", *args, "--", "--list"])
    return sum(int(m) for m in re.findall(r"^(\d+) tests, \d+ benchmarks$", out, re.M))


def counted() -> dict[str, int]:
    # `--tests` names every target with tests in it — the library's own and
    # each integration target — so it is the whole of what the row describes.
    # The python feature adds the binding's tests to the same library, which
    # makes it the superset.
    # With the injection methods too: the tests that drive a client through
    # them run wherever the lib tests run, and counted without the feature
    # they are three tests this repo says it does not have.
    rust = _cargo_count(["--tests", "--features", "python,test-helpers"])

    python = _python()
    py = _run([python, "-m", "pytest", "tests/python", "--collect-only", "-q", "--color=no"])
    collected = re.search(r"^(\d+) tests collected", py, re.M)
    python_all = int(collected.group(1)) if collected else 0

    # A live test is one that needs a session, which is to say one that reads
    # the credentials — not one with "live" in its name. The table names these
    # apart because a reader without credentials runs none of them.
    live_files = [p for p in sorted((ROOT / "tests" / "python").glob("*.py"))
                  if "IB_USERNAME" in p.read_text()]
    live = _run([python, "-m", "pytest", *[str(p) for p in live_files],
                 "--collect-only", "-q", "--color=no"])
    live_hit = re.search(r"^(\d+) tests collected", live, re.M)
    python_live = int(live_hit.group(1)) if live_hit else 0

    return {
        "rust": rust,
        "python": python_all - python_live,
        "python_live": python_live,
        "paper": _cargo_count(["--test", "ib_paper_compat"]),
        # The other Rust target that needs a session. It states so in its own
        # first lines and reads the credentials out of the environment, and the
        # page generator already counts it as live evidence — counted offline
        # here, it would be published as needing no session by one script and
        # as needing one by another.
        "rust_live": sum(_cargo_count(["--test", t]) for t in _targets_needing_a_session()),
        # What is left of the Rust targets once every one that needs a session
        # is taken out, which is what "requires credentials: no" has to mean.
        "rust_offline": rust
            - _cargo_count(["--test", "ib_paper_compat"])
            - sum(_cargo_count(["--test", t]) for t in _targets_needing_a_session()),
        # Announced phases only: a phase names itself in the line it prints,
        # so the number has to sit inside a string literal. Counted over the
        # whole file, a phase number written in a comment -- a note pointing at
        # another phase, or one commented out -- counted as a phase that runs.
        # It happens not to today, and the published figure is the same either
        # way; this closes the way it would drift.
        "phases": len({
            m for f in (ROOT / "tests" / "ib_paper_compat").glob("*.rs")
            for lit in STRING_LITERAL.findall(f.read_text())
            for m in PHASE.findall(lit)
        }),
    }


def published() -> dict[str, int]:
    out: dict[str, int] = {}
    # The phase count rides in the paper suite's own row label.
    phases = re.search(r"Paper compatibility suite \((\d+) phases\)", STATUS.read_text())
    if phases:
        out["phases"] = int(phases.group(1))
    for line in STATUS.read_text().splitlines():
        if not line.startswith("|"):
            continue
        cells = [c.strip() for c in line.strip("|").split("|")]
        if len(cells) < 2:
            continue
        # Longest label first: "Python, live" also starts with "Python", and
        # matching the shorter one would read one row's count into both.
        for label in sorted(ROWS, key=len, reverse=True):
            if cells[0].startswith(label) and ROWS[label] not in out:
                digits = re.sub(r"[^0-9]", "", cells[1])
                if digits:
                    out[ROWS[label]] = int(digits)
                break
    return out


#: Every status the legend defines. A mark outside this set is a typo or a
#: status nobody wrote down, and either way the row carrying it must not be
#: counted silently.
STATUSES = frozenset({
    "\u2705 Supported",
    "\U0001f52c Implemented",
    "\u26d4 Unavailable",
    "\u2705 Documented",
})

#: The characters a status cell starts with, used to tell one from a capability
#: name or a verification note in the same row. Membership is tested on the
#: first character of a non-empty cell: an empty cell is a substring of any
#: string, so testing a slice would match every blank column in the file.
MARKS = "\u2705\U0001f52c\u26d4"


def capabilities() -> tuple[int, int]:
    """How many capabilities the matrix lists, and how many are verified.

    Counting only the marks this script already knows lets an unrecognised one
    drop a whole row out of both figures without failing anything, so the
    totals stay self-consistent while the matrix quietly says less than it
    lists. Every mark is collected first and checked against the legend.
    """
    text = (ROOT / "docs/evidence.md").read_text()
    # From the first capability section, so the table defining the marks is
    # not counted as capabilities carrying them.
    rows = text[text.index("## Client surfaces"):].splitlines()
    marks = [
        cell
        for line in rows
        if line.startswith("| ")
        for cell in (c.strip() for c in line.split("|"))
        if cell and cell[0] in MARKS
    ]
    unknown = sorted(set(marks) - STATUSES)
    if unknown:
        raise SystemExit(
            "docs/evidence.md carries a status its legend does not define: "
            + ", ".join(unknown)
        )
    verified = sum(1 for m in marks if m == "\u2705 Supported")
    return verified, len(marks)


def order_types() -> int:
    """How many order types a caller can place.

    Read off the one list that decides it: the check every placement passes
    before anything is built. Aliases the same type answers to are counted
    once — the venue is asked for one type whichever spelling the caller used.
    """
    text = (ROOT / "src/client_core/mod.rs").read_text()
    at = text.index('match order_type.as_str() {')
    block = text[at:text.index("_ => return Err", at)]
    spellings = set(re.findall(r'"([A-Z][A-Z0-9 +]*)"', block))
    # The spellings that name a type already in the set under another name.
    same = {
        "PEG MIDPT": "PEG MID",
        "MIDPRICE": "MIDPX",
        "SNAP MIDPT": "SNAP MID",
        "SNAP PRIM": "SNAP PRI",
        "PEGBENCH": "PEG BENCH",
    }
    for alias, named in same.items():
        if alias in spellings and named in spellings:
            spellings.discard(alias)
    return len(spellings)


def readme_says() -> tuple[int, int] | None:
    """What the matrix's own test row states, offline and session-only.

    Both figures count tests as written, not as run: the second set lives in the
    suites that open a session, so it is what would run against one and not a
    record of what has.

    The row is prose rather than a table of counts, so it is read back out of
    the sentence it is written in. A figure nobody re-derives is one that goes
    stale quietly — this one was two hundred tests out before anything checked
    it.
    """
    m = re.search(r"\| ([\d,]+) offline, and ([\d,]+) more that live in the suites run against a broker session \|",
                  MATRIX.read_text())
    if not m:
        return None
    return (int(m.group(1).replace(",", "")), int(m.group(2).replace(",", "")))



#: The generated coverage matrix, which `gen_api_docs.py` writes from the
#: source. What the engineering notes publish is checked against it here.
COVERAGE = ROOT / "docs" / "book" / "src" / "reference" / "coverage-data.md"


def api_surface() -> dict[str, int]:
    """The call and callback surface, counted off the generated matrix.

    The notes state these as a table anyone can retype, and nothing read them
    back: the served figure stood at 69 while the matrix listed 73, because a
    call that stopped being a stub changed the matrix and not the sentence
    about it.
    """
    text = COVERAGE.read_text()
    section, rows = None, {"EClient": [], "EWrapper": []}
    for line in text.splitlines():
        if line.startswith("## EClient"):
            section = "EClient"
            continue
        if line.startswith("## EWrapper"):
            section = "EWrapper"
            continue
        if line.startswith("## "):
            section = None
            continue
        if section and line.startswith("| ") and "`" in line:
            cells = [c.strip() for c in line.strip().strip("|").split("|")]
            if len(cells) >= 4 and cells[1].startswith("`"):
                rows[section].append(cells)

    calls, backs = rows["EClient"], rows["EWrapper"]
    # The surface columns sit after the category and the two names for a call,
    # and after the category and the one name for a callback.
    call_rust, call_py = (3, 4)
    back_rust, back_py = (2, 3)
    return {
        "Canonical calls": len(calls),
        "Served, Rust": sum(1 for c in calls if c[call_rust] == "Y"),
        "Served, Python": sum(1 for c in calls if c[call_py] == "Y"),
        # What the matrix marks STUB is a call taken and not applied: it returns
        # and reaches nothing. Counted under the status that reports on the
        # error callback, the figure said a call answers a caller who is told
        # nothing.
        "Taken and not applied, Rust": sum(1 for c in calls if c[call_rust] == "STUB"),
        "Taken and not applied, Python": sum(1 for c in calls if c[call_py] == "STUB"),
        "Canonical callbacks": len(backs),
        "Calls where the two surfaces differ":
            sum(1 for c in calls if c[call_rust] != c[call_py]),
        "Callbacks where the two surfaces differ":
            sum(1 for c in backs if c[back_rust] != c[back_py]),
    }


def calls_with_no_counterpart() -> int:
    """How many `Client` calls answer with a refusal rather than a result.

    Typed into the page as a figure and counted nowhere, it stood at three
    while the table in the source listed five — so a reader picking this
    surface for a profit-driven program was told the call is served.
    """
    text = (ROOT / "src/api/direct.rs").read_text()
    table = text.split("pub const NO_COUNTERPART", 1)[1].split("];", 1)[0]
    return len(re.findall(r'^\s*\("', table, re.M))


def api_surface_published() -> dict[str, int]:
    """The same measures as the engineering notes state them."""
    said = {}
    for line in STATUS.read_text().splitlines():
        if not line.startswith("| "):
            continue
        cells = [c.strip() for c in line.strip().strip("|").split("|")]
        if len(cells) == 2 and cells[1].isdigit():
            said[cells[0]] = int(cells[1])
    return said


def main() -> int:
    have, said = counted(), published()
    wrong = []

    # The prose states the same thing in a sentence.
    #
    # The Rust count is every Rust test target, the paper suite among them, and
    # the paper suite is also the live side of this split. Counted straight, its
    # tests are published as offline and as live at once — a total that adds up
    # only because the same tests are in it twice. Taken out of the offline side,
    # which is the side they are not on.
    offline = have["rust_offline"] + have["python"]
    live = have["python_live"] + have["paper"] + have["rust_live"]
    verified, total = capabilities()
    # The prose above the matrix restates what the matrix lists. Both live in
    # the same file now, which is not a reason to trust one against the other:
    # the sentence is typed and the marks are counted.
    matrix_text = MATRIX.read_text()
    if f"{verified} of the {total} capabilities" not in matrix_text:
        wrong.append(
            f"docs/evidence.md does not say {verified} of {total} "
            f"capabilities, which is what its own matrix lists"
        )
    else:
        print(f"capabilities: {verified} of {total} verified")

    refuse = calls_with_no_counterpart()
    if f"{refuse} return an error saying why" not in matrix_text:
        wrong.append(
            f"docs/evidence.md does not say {refuse} calls return an error saying "
            f"why, which is what src/api/direct.rs names"
        )
    else:
        print(f"calls with no counterpart: {refuse}")

    types = order_types()
    if f"| {types} order types |" not in matrix_text:
        wrong.append(
            f"docs/evidence.md does not say {types} order types, which is what "
            f"the check every placement passes accepts"
        )
    else:
        print(f"order types: {types}")

    stated = readme_says()
    if stated is None:
        wrong.append("docs/evidence.md states no test counts")
    elif stated != (offline, live):
        wrong.append(
            f"docs/evidence.md says {stated[0]:,} offline and {stated[1]:,} live, "
            f"{offline:,} and {live:,} exist"
        )
    else:
        print(f"readme: {offline:,} offline, {live:,} live")

    # `rust` is every Rust target together, which no row publishes: the table
    # names the two buckets it splits into, because "requires credentials" is
    # not one answer for all of them.
    for key, n in have.items():
        if key == "rust":
            continue
        if key not in said:
            wrong.append(f"{key}: nothing published, {n} exist")
        elif said[key] != n:
            wrong.append(f"{key}: docs/evidence.md says {said[key]:,}, {n:,} exist")
        else:
            print(f"{key}: {n:,}")
    surface, surface_said = api_surface(), api_surface_published()
    for key, n in surface.items():
        if key not in surface_said:
            wrong.append(f"docs/evidence.md publishes no {key!r}")
        elif surface_said[key] != n:
            wrong.append(
                f"{key}: docs/evidence.md says {surface_said[key]}, "
                f"the generated matrix lists {n}"
            )
    if not any("surfaces differ" in w or "Served" in w or "Canonical" in w for w in wrong):
        print(f"api surface: {surface['Served, Rust']} of "
              f"{surface['Canonical calls']} calls served on both")

    if wrong:
        print("\nA published count is not what is there:")
        for w in wrong:
            print(f"  {w}")
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
