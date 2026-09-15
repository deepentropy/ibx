#!/usr/bin/env python3
"""A figure the venue left out must reach a wrapper that did not write the method.

The dispatcher hands a caller's wrapper `None` for a figure the venue did not
state, because that is what a caller written against the reference client is
given. The wrapper's own method is what runs when the caller has not written one
of their own, and if it declares that parameter as a number rather than as one
the venue may have left out, it refuses the call — and the refusal travels out
of the reading loop and closes the session.

It cost a session once: `tick_option_computation` declared eleven numbers, the
dispatcher passed `None` for the dividend on the first model that carried none,
and every caller watching an option without that method written lost its
connection there. Nothing caught it, because every test in the suite writes that
method itself, so the one that was broken was never the one called.

Checked here because the test suite structurally cannot: a default nobody calls
is a default nothing exercises, and the only way to see the mismatch is to read
the call against the signature.
"""
import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
WRAPPER = ROOT / "src" / "python" / "compat" / "wrapper.rs"
DISPATCH = ROOT / "src" / "python" / "compat" / "client" / "dispatch.rs"

# An argument written any of these ways may be nothing at all.
MAY_BE_NOTHING = ("or_unstated", ".filter(", "Some(", "None", ".ok()", "or_none")


def signatures(text: str) -> dict[str, list[str]]:
    """Each default method's parameters, in order, as they are declared."""
    out: dict[str, list[str]] = {}
    for m in re.finditer(r"\n    fn ([a-z0-9_]+)\(\s*&self,(.*?)\)\s*(?:->[^{]*)?\{", text, re.S):
        params = [p.strip() for p in re.split(r",(?![^<>()]*[>)])", m.group(2)) if p.strip()]
        out[m.group(1)] = params
    return out


def arguments(args: str) -> list[str]:
    """One call's arguments, split on the commas that separate them."""
    out, depth, current = [], 0, ""
    for ch in args:
        if ch in "(<[":
            depth += 1
        elif ch in ")>]":
            depth -= 1
        if ch == "," and depth == 0:
            out.append(current.strip())
            current = ""
            continue
        current += ch
    if current.strip():
        out.append(current.strip())
    return out


def main() -> int:
    sigs = signatures(WRAPPER.read_text())
    dispatch = DISPATCH.read_text()
    problems: list[str] = []
    checked = 0

    calls = re.findall(
        r'call_wrapper!\(\s*self,\s*\w+,\s*\w+,\s*"([a-z0-9_]+)"\s*,\s*\((.*?)\)\s*\)\s*\)?;',
        dispatch,
        re.S,
    )
    for name, raw in calls:
        args = arguments(raw)
        if not any(tok in arg for arg in args for tok in MAY_BE_NOTHING):
            continue
        params = sigs.get(name)
        if params is None:
            problems.append(f"  {name} is called but the wrapper states no such method")
            continue
        checked += 1
        for at, arg in enumerate(args):
            if not any(tok in arg for tok in MAY_BE_NOTHING):
                continue
            if at >= len(params):
                problems.append(
                    f"  {name} is handed {len(args)} arguments and states {len(params)}"
                )
                break
            if "Option<" not in params[at]:
                problems.append(
                    f"  {name} may hand nothing for `{params[at]}`, which is declared "
                    f"as a figure that is always there"
                )

    if problems:
        print("a wrapper's own method would refuse what the dispatcher hands it:")
        print("\n".join(problems))
        print(
            "\nDeclare the parameter as one the venue may have left out. A caller "
            "who has not written the method is the one this reaches."
        )
        return 1
    print(
        f"every figure that may be nothing reaches a wrapper method that takes "
        f"nothing ({checked} call site(s) of {len(calls)})"
    )
    return 0


if __name__ == "__main__":
    sys.exit(main())
