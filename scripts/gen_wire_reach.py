#!/usr/bin/env python3
"""Count what reaches the venue, as opposed to what merely exists.

A call that exists, carries the right name and the right arguments, and returns
without error is not thereby a call that did anything. Counting the surface says
a program written elsewhere compiles here; it does not say the request left the
machine. The two are different claims and only one of them is worth making.

Every caller-facing request falls into one of five kinds:

  wire      it sends a command to the engine, which puts it on the venue's wire
  session   it answers from what the venue has already pushed to this session
  refused   it reports that this client cannot serve it, and says why
  missing   it should reach the venue and does not
  withdrawn it withdraws something that was never running here, and says so
  silent    it returns as though it did something and did not

`silent` must stay at zero. The others are facts to be stated, not failures:
the venue pushes account and position data on login, and these calls answer
from that push.

Writes target/gates/wire-reach.md, which is a report and not the gate: this
exits non-zero on its own findings, and on a total the capability matrix
publishes that no longer matches. The number cannot
drift away from the code.
"""

import pathlib
import re
import sys
from _paths import published
from collections import Counter

ROOT = pathlib.Path(__file__).resolve().parent.parent
# The binding's request surface. The two clients carry the same calls — a
# separate gate compares them name by name — so reading one measures both; a
# published figure has to say which one it read, because "the same calls" is a
# claim that gate makes and not one this script checks.
CLIENT = ROOT / "src/python/compat/client"
OUT = ROOT / "target/gates/wire-reach.md"

REQUEST = re.compile(
    r"^(req_|cancel_|place_|exercise_|replace_|request_|calculate_|query_|"
    r"subscribe_|unsubscribe_|update_display)"
)


def bodies() -> dict[str, str]:
    """Each request's body, keyed by name, with the comment above it kept.

    The comment matters: a withdrawal of something that was never running is
    correct and must say so, and the only way to tell it from an oversight is
    that someone wrote down which it is.
    """
    found: dict[str, str] = {}
    for path in sorted(CLIENT.glob("*.rs")):
        if path.name in ("test_helpers.rs", "dispatch.rs"):
            continue
        text = path.read_text()
        # What a caller can reach, which the tests beside it are not. Indented
        # by the same four spaces, a test function read as a request the venue
        # carries — and one that happened to be named after a request was
        # counted as a second copy of it.
        text = text.split("\n#[cfg(test)]\n", 1)[0]
        # `pub` as well as `pub(crate)`: a request declared plainly public was
        # invisible here, so a call delegating to one traced to nothing and
        # read as silent — and the public call itself was never classified at
        # all.
        for m in re.finditer(r"\n    (?:pub(?:\(crate\))? )?fn ([a-z_0-9]+)\(", text):
            start = m.end()
            lead = leading_comment(text, m.start())
            after = [
                text.find(s, start)
                for s in ("\n    fn ", "\n    pub(crate) fn ", "\n    pub fn ")
            ]
            after = [x for x in after if x > 0] + [len(text)]
            found[m.group(1)] = lead + text[start : min(after)]
    return found


def leading_comment(text: str, at: int) -> str:
    """Only the comment block written directly above this function.

    Taking a fixed window instead pulls in the tail of whatever came before, so
    a function following a documented one inherits its documentation and is
    counted as whatever that one was.
    """
    lines = text[:at].splitlines()
    kept: list[str] = []
    for line in reversed(lines):
        stripped = line.strip()
        if stripped.startswith(("///", "//", "#[")) or not stripped:
            if stripped:
                kept.append(stripped)
            continue
        break
    return "\n".join(reversed(kept))


def classify(name: str, all_bodies: dict[str, str], seen: set[str] | None = None) -> str:
    seen = seen or set()
    if name in seen:
        return "silent"
    seen.add(name)
    body = all_bodies.get(name, "")

    if "send_control" in body or "ControlCommand" in body:
        return "wire"
    # A refusal is a refusal whether it names the call or states a reason of
    # its own. Recognising only the first shape reported a call that answers
    # on the error channel as one that answered nothing.
    #
    # Stating a reason on a failure path is not refusing: a call that serves
    # the request and reports why it could not this once still serves it, and
    # counting it as refused understates what reaches the venue.
    refuses_outright = "report_unserviceable" in body or "unserviceable(" in body
    states_a_reason = "report_reason(" in body and "if let Err" not in body
    if refuses_outright or states_a_reason:
        return "refused"
    if "not yet implemented" in body:
        return "missing"
    if "nothing to withdraw" in body:
        return "withdrawn"

    # A call that forwards to another is whatever that one is. Without this a
    # request reached through one line of delegation reads as doing nothing.
    for m in re.finditer(r"self\.([a-z_0-9]+)\(", body):
        target = m.group(1)
        if target in all_bodies and target != name:
            kind = classify(target, all_bodies, seen)
            if kind in ("wire", "refused", "missing", "session", "withdrawn"):
                return kind

    if "self.core." in body or "shared" in body.lower() or "callback" in body:
        return "session"
    return "silent"


def main() -> int:
    all_bodies = bodies()
    calls = {n: classify(n, all_bodies) for n in all_bodies if REQUEST.match(n)}
    counts = Counter(calls.values())

    lines = [
        "# What reaches the venue",
        "",
        "Generated by `scripts/gen_wire_reach.py`. Do not edit.",
        "",
        "A call that exists and returns without error is not thereby a call that",
        "did anything. This counts what leaves the machine.",
        "",
        "| Kind | Count | Meaning |",
        "| --- | ---: | --- |",
        f"| wire | {counts.get('wire', 0)} | sends a command that reaches the venue |",
        f"| session | {counts.get('session', 0)} | answered from what the venue pushed to this session |",
        f"| refused | {counts.get('refused', 0)} | reports that this client cannot serve it |",
        f"| missing | {counts.get('missing', 0)} | should reach the venue and does not |",
        f"| withdrawn | {counts.get('withdrawn', 0)} | withdraws something that was never running, and says so |",
        f"| silent | {counts.get('silent', 0)} | returns as though it acted, and did not |",
        "",
        "`silent` is held at zero. A call that quietly does nothing is worse than",
        "one that refuses: a caller waiting on it waits for something that is",
        "never coming, with nothing to say why.",
        "",
    ]

    for kind, heading in (
        ("missing", "Should reach the venue and does not"),
        ("refused", "Reports that it cannot be served"),
        ("silent", "Returns as though it acted"),
    ):
        named = sorted(n for n, v in calls.items() if v == kind)
        lines.append(f"## {heading}")
        lines.append("")
        lines.append(", ".join(f"`{n}`" for n in named) if named else "None.")
        lines.append("")

    OUT.parent.mkdir(parents=True, exist_ok=True)
    OUT.write_text("\n".join(lines))

    silent = sorted(n for n, v in calls.items() if v == "silent")
    if silent:
        print("a call returns as though it acted and did not:", ", ".join(silent))
        return 1
    # Every place the figure is published, not only the headline: the same
    # count is stated three times over and each one is a claim.
    for pattern in (r"\| Requests \| ([\d,]+)\.",
                    r"([\d,]+)/([\d,]+) callable",
                    r"\| ([\d,]+) requests, none silent \|"):
        for stated in published(pattern):
            if any(n != len(calls) for n in stated):
                print(f"docs/evidence.md publishes {stated} where "
                      f"{len(calls)} requests exist ({pattern})")
                return 1
    print(f"{len(calls)} requests: " + ", ".join(f"{k}={v}" for k, v in sorted(counts.items())))
    return 0


if __name__ == "__main__":
    sys.exit(main())
