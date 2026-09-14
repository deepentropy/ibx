"""Finding a module whichever file it lives in.

A module is `x.rs` until it grows a folder, and then it is `x/mod.rs`. A
generator that names one form stops reading the day it moves — and one that
reads nothing reports everything it was watching as absent, which is worse
than failing. Both `gen_wire_coverage` and `gen_order_field_reach` were caught
by this the day the engine's files were split.
"""

import re
import pathlib

ROOT = pathlib.Path(__file__).resolve().parent.parent


def module(name: str) -> pathlib.Path:
    """The file a module lives in, named without its extension.

    `module("src/gateway")` finds `src/gateway.rs` or `src/gateway/mod.rs`,
    whichever exists. Raises rather than returning a path to nothing: a
    generator reading an empty string reports the same as one reading a file
    with nothing in it, and only one of those is true.
    """
    flat = ROOT / f"{name}.rs"
    if flat.exists():
        return flat
    folded = ROOT / name / "mod.rs"
    if folded.exists():
        return folded
    raise FileNotFoundError(f"no module at {name}.rs or {name}/mod.rs")


def module_files(name: str) -> list[pathlib.Path]:
    """Every file a module is written across, tests aside.

    A module is one file until it is several. Reading only `mod.rs` after a
    split reports the code that moved as absent — which for a reach count
    means every field that code carries is counted as dropped.
    """
    flat = ROOT / f"{name}.rs"
    if flat.exists():
        return [flat]
    folder = ROOT / name
    if not folder.is_dir():
        raise FileNotFoundError(f"no module at {name}.rs or {name}/")
    return sorted(f for f in folder.glob("*.rs") if f.stem != "tests")


def published(pattern: str) -> list[list[int]]:
    """The figures the capability matrix states, read back out of the sentence.

    A number in a shipped document is a claim. This one was stated once and
    then drifted, and the check that was supposed to hold it compared a
    generated report against its own committed copy — which is regenerated and
    committed by the same commit that moves the figure, so it never failed.
    Compare against the published prose instead.
    """
    text = (ROOT / "docs/evidence.md").read_text()
    found = [
        [int(g.replace(",", "")) for g in m.groups()]
        for m in re.finditer(pattern, text)
    ]
    if not found:
        # A claim that has been reworded is a claim nobody is checking. Read as
        # "nothing published", this skipped silently and the figure it was
        # written to hold drifted anyway.
        raise SystemExit(
            f"docs/evidence.md states nothing matching {pattern!r}, so the "
            f"figure it publishes is measured by nobody"
        )
    return found
