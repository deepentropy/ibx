#!/usr/bin/env python3
"""Count which fields of an order reach the venue.

`gen_wire_reach.py` counts calls. A call can reach the venue while most of what
a caller put in it does not: `place_order` sends an order, and a caller who set
a field the builder never reads has been told nothing about it. The order went
out without it, and the only sign is that the venue does something other than
what was asked.

Every field of `Order` falls into one of three kinds:

  carried   the order builder reads it and it goes out under a tag
  refused   this protocol does not carry it, and the field says so
  dropped   a caller can set it and nothing reads it

`dropped` is the one that matters. It is the order-field form of `silent`: the
call returns, the order is placed, and the field is gone.

Writes target/gates/order-field-reach.md, which is a report and not the gate:
this exits non-zero on its own findings. The number
cannot drift away from the code.
"""

import pathlib
import re
import sys

sys.path.insert(0, str(__import__("pathlib").Path(__file__).resolve().parent))
from _paths import module, module_files, published  # noqa: E402

ROOT = pathlib.Path(__file__).resolve().parent.parent
OUT = ROOT / "target/gates/order-field-reach.md"

# Where an order is turned into tags. A field that reaches the venue is read in
# one of these.
BUILDERS = [
    # Where a caller's order becomes the engine's, which is as much a part of
    # reaching the venue as the encoder is: a field the conversion drops never
    # gets as far as a tag. Leaving this out counted a carried field as lost.
    *module_files("src/types/model"),
    *module_files("src/engine/hot_loop/order_builder"),
    # Not the connection modules: they parse what arrives, and a field an
    # execution report fills is not a field the order carried out. Counted
    # among these, `fa_group` read as carried by the report that names it while
    # placement refused it, and `submitter` read as carried by nothing at all.
    *module_files("src/client_core"),
    *module_files("src/types"),
]


def order_fields() -> list[str]:
    text = module("src/types/model").read_text()
    at = text.index("pub struct Order ")
    end = text.index("\n}", at)
    return re.findall(r"^\s*pub (\w+):", text[at:end], re.M)


def refused() -> dict[str, str]:
    """Fields whose doc comment says this protocol does not carry them.

    Written on the field rather than kept in a list here, so that the reason
    sits where someone reading the field will find it.
    """
    text = module("src/types/model").read_text()
    at = text.index("pub struct Order ")
    end = text.index("\n}", at)
    out = {}
    block = text[at:end]
    for m in re.finditer(r"((?:^\s*///.*\n)+)\s*pub (\w+):", block, re.M):
        doc = " ".join(line.strip(" /") for line in m.group(1).strip().splitlines())
        if "not carried" in doc.lower():
            out[m.group(2)] = doc
    return out


def what_the_call_refuses() -> set[str]:
    """The fields `place_order` will not take a stated value for.

    The note on a field and the refusal in the call have to name the same set:
    a note without a refusal is an order that goes out missing what it says it
    cannot carry, and a refusal without a note is a caller told no with nowhere
    to read why.
    """
    text = module("src/client_core").read_text()
    at = text.index("refuse_if_stated!(")
    end = text.index(");", at)
    listed = text[at + len("refuse_if_stated!("):end]
    # A field may carry its own sentence, and a sentence is full of words that
    # look like field names. Read the names, not the prose.
    listed = re.sub(r'"(?:[^"\\]|\\.)*"', "", listed, flags=re.S)
    named = set()
    for entry in listed.split(","):
        name = entry.split(":")[0].strip()
        if re.fullmatch(r"[a-z_][a-z_0-9]*", name):
            named.add(name)
    return named


def without_tests(text: str) -> str:
    """The file with any `#[cfg(test)]` block after it removed.

    A test names the fields it exercises, and a name is all this looks for. A
    field read nowhere but by the test of the code that used to read it is a
    field nothing carries.
    """
    at = text.find("#[cfg(test)]")
    return text if at < 0 else text[:at]


def without_validation(text: str) -> str:
    """The file with its refusals, and the prose beside them, removed.

    Refusing a field is not carrying it. A validator names the field it is
    refusing, and the comment above it names it again, so a field whose
    conversion had been deleted still counted as reaching the venue on the
    strength of the code that stops it from doing so.
    """
    out, at = [], 0
    while True:
        start = text.find("fn validate_", at)
        if start < 0:
            out.append(text[at:])
            break
        end = text.find("\n    }", start)
        if end < 0:
            out.append(text[at:])
            break
        out.append(text[at:start])
        at = end + 6
    return re.sub(r"^\s*(//|///).*$", "", "".join(out), flags=re.M)


def without_declarations(text: str) -> str:
    """The file with its struct definitions and their defaults removed.

    A declaration names every field and reads none. Left in, the attributes
    struct beside the builder said each name and the field counted as carried:
    a field could lose both its conversion and the tag it went out under and
    still be reported as reaching the venue, because two declarations of it
    remained.
    """
    # A declaration starts a line. Found anywhere, the word "struct" in a
    # comment ("the pre-drop struct through") opened a cut that ran to the
    # next brace in column zero — the end of the whole impl block — and every
    # field read in the thousands of lines between reported as dropped.
    declaration = re.compile(r"^(?:pub(?:\([^)]*\))?\s+)?struct\s+\w+|^impl Default for ", re.M)
    out, at = [], 0
    while True:
        m = declaration.search(text, at)
        if not m:
            out.append(text[at:])
            return "".join(out)
        start = m.start()
        end = text.find("\n}", start)
        if end < 0:
            out.append(text[at:])
            return "".join(out)
        out.append(text[at:start])
        at = end + 2


def the_conversion(text: str) -> str:
    """The order's conversion, and everything it calls to finish the job.

    In the file that declares the order, only the conversion carries a field to
    the engine. Everything else there names fields without carrying them — the
    declaration, the defaults, and a predicate asking whether an order has
    extended attributes at all, which names forty of them and would report
    every one as carried. `deactivate_on_disconnect` could lose both its
    conversion and its tag and stay counted, because that predicate still said
    its name.

    The conversion is not one function: it hands the scale, the delta hedge and
    the rest to helpers beside it. Taking only its own body reports every field
    those carry as dropped, so the calls are followed.
    """
    def body_of(name: str) -> str:
        at = text.find(f"fn {name}(")
        if at < 0:
            return ""
        end = text.find("\n    }\n", at)
        return text[at:end] if end > 0 else text[at:]

    wanted, seen, out = ["attrs"], set(), []
    while wanted:
        name = wanted.pop()
        if name in seen:
            continue
        seen.add(name)
        body = body_of(name)
        out.append(body)
        wanted += re.findall(r"self\.([a-z_0-9]+)\(", body)
    return "\n".join(out)


def reported() -> dict[str, str]:
    """Fields the venue fills on the way back, which an order does not carry out.

    Neither sent nor refused: a caller holding one is holding what the venue
    told them, and an order read back and placed again would be refused for
    carrying it. Counted apart from the fields nothing carries, because the
    reason is different and so is what a caller should do about it.
    """
    text = module("src/types/model").read_text()
    at = text.index("pub struct Order ")
    end = text.index("\n}", at)
    out = {}
    for m in re.finditer(r"((?:^\s*///.*\n)+)\s*pub (\w+):", text[at:end], re.M):
        doc = " ".join(line.strip(" /") for line in m.group(1).strip().splitlines())
        if "reported by the venue" in doc.lower():
            out[m.group(2)] = doc
    return out


def held() -> dict[str, str]:
    """Fields this client acts on itself rather than sending or refusing.

    The counterpart this replaces reads some of what the reference client sends
    it and never puts it on the wire — an order held back is created there and
    not transmitted. Such a field is neither carried nor refused, and it is not
    dropped either: something happens because it was set. Counted apart so that
    a field genuinely going nowhere still shows up as one.
    """
    text = module("src/types/model").read_text()
    at = text.index("pub struct Order ")
    end = text.index("\n}", at)
    out = {}
    for m in re.finditer(r"((?:^\s*///.*\n)+)\s*pub (\w+):", text[at:end], re.M):
        doc = " ".join(line.strip(" /") for line in m.group(1).strip().splitlines())
        if "held here" in doc.lower():
            out[m.group(2)] = doc
    return out


def where_fields_are_read() -> str:
    """Everything that reads an order, with the order's own definition removed.

    `src/types/model.rs` both declares the fields and converts them for the
    engine. Searching it whole matches every field against its own
    declaration and reports that all of them are carried, which is the same
    blindness in the other direction; searching without it reports a field the
    conversion carries as lost.

    The tests written beside the code are removed for the same reason the
    declaration is: a test naming a field is not code carrying it. Left in,
    a field whose only remaining mention was the test that used to check it
    read as carried — `deactivate_on_disconnect` could lose both its
    conversion and its tag and still be counted.
    """
    out = []
    for path in BUILDERS:
        if not path.exists():
            continue
        text = without_validation(without_declarations(without_tests(path.read_text())))
        # Whichever file holds it, not whichever file held it once: keyed on
        # a filename, this went quiet the day the model moved and reported
        # every field as carried by its own declaration. That is the fourth
        # time a moved file has silently disabled a check here.
        if "pub fn attrs(&self)" in text:
            text = the_conversion(text)
        out.append(text)
    return "\n".join(out)


#: Fields the order conversion fills with a literal on purpose, and why.
#:
#: Anything else filled with a literal is a caller's value being replaced by a
#: constant. That is how `good_after_time` was lost: the caller set it, the
#: conversion wrote a zero over it, the builder declined to send a zero, and
#: the reach count called it carried because its name appeared in the builder.
#: Every step read correctly on its own.
STATED_CONSTANTS = {
    "primary_exchange": "carried on the contract, not on the order",
    "exercise_action": "set by the exercise call, not by an order",
    "combo_legs": "carried on the contract, not on the order",
    "delta_neutral_contract": "carried on the contract, not on the order",
    "oca_group": "the caller's group name travels whole in oca_group_str; "
                 "the number is the engine's own naming for a bracket's legs",
}


def constants_in_the_conversion() -> list[str]:
    """Fields the conversion fills with a literal rather than the caller's value."""
    text = module("src/types/model").read_text()
    at = text.index("fn attrs(")
    end = text.index("\n    }", at)
    return [
        m.group(1)
        for m in re.finditer(
            # Every shape a literal takes here, not the four that were thought
            # of first: a float, a negative, `None`, an empty collection and a
            # named constant are all a caller's value being replaced, and each
            # of them read as carried.
            r"^\s*(\w+): (-?\d+(?:\.\d+)?|false|true|None"
            r"|String::new\(\)|Vec::new\(\)|Default::default\(\)"
            r"|[A-Z][A-Z0-9_]{2,}),",
            text[at:end], re.M,
        )
    ]


def main() -> int:
    fields = order_fields()
    says_so = refused()
    read = where_fields_are_read()

    echoed = reported()
    kept = held()
    carried, dropped = [], []
    for field in fields:
        if field in says_so or field in echoed or field in kept:
            continue
        if re.search(rf"\b{field}\b", read):
            carried.append(field)
        else:
            dropped.append(field)

    lines = [
        "# What of an order reaches the venue",
        "",
        "Generated by `scripts/gen_order_field_reach.py`. Do not edit.",
        "",
        "An order that reaches the venue is not thereby an order carrying what",
        "a caller put in it. A field the builder never reads is gone, and the",
        "only sign is the venue doing something other than what was asked.",
        "",
        "| Kind | Count | Meaning |",
        "| --- | ---: | --- |",
        f"| carried | {len(carried)} | goes out under a tag |",
        f"| refused | {len(says_so)} | this protocol does not carry it, and the field says so |",
        f"| reported | {len(echoed)} | the venue fills it on the way back; an order does not carry it out |",
        f"| held | {len(kept)} | this client acts on it rather than sending it |",
        f"| dropped | {len(dropped)} | a caller can set it and nothing reads it |",
        "",
        "`dropped` is the order-field form of `silent`: the call returns, the",
        "order is placed, and the field is not on it.",
        "",
        "## Set by a caller and not read",
        "",
    ]
    lines.append(", ".join(f"`{f}`" for f in sorted(dropped)) if dropped else "None.")
    lines += ["", "## Acted on here, not sent", ""]
    lines.append(
        "\n".join(f"- `{f}` — {why}" for f, why in sorted(kept.items())) if kept else "None."
    )
    lines += ["", "## Reported by the venue, not sent", ""]
    lines.append(", ".join(f"`{f}`" for f in sorted(echoed)) if echoed else "None.")
    lines += ["", "## Not carried by this protocol", ""]
    lines.append(
        "\n".join(f"- `{f}` — {why}" for f, why in sorted(says_so.items()))
        if says_so
        else "None."
    )
    lines.append("")

    OUT.parent.mkdir(parents=True, exist_ok=True)
    OUT.write_text("\n".join(lines))

    if dropped:
        print("a caller can set these and nothing reads them. The call returns,")
        print("the order is placed, and the field is not on it:")
        for f in dropped:
            print(f"  {f}")
        return 1

    refuses = what_the_call_refuses()
    if refuses != set(says_so):
        print("a field's note and the call disagree about what can be placed:")
        for f in sorted(set(says_so) - refuses):
            print(f"  {f} is documented as not carried and the call takes it anyway")
        for f in sorted(refuses - set(says_so)):
            print(f"  {f} is refused by the call and no field says why")
        return 1

    unstated = [f for f in constants_in_the_conversion() if f not in STATED_CONSTANTS]
    if unstated:
        print("the order conversion fills a field with a literal and nobody has")
        print("said why. A caller's value replaced by a constant reaches the venue")
        print("as though it were never set:")
        for f in unstated:
            print(f"  {f}")
        return 1

    have = [len(fields), len(carried), len(says_so)]
    for pattern, want in (
        (r"\| Order fields \| ([\d,]+)\. ([\d,]+) are sent; ([\d,]+) have no field .*?; ([\d,]+) are what",
         have + [len(echoed)]),
        (r"An order has ([\d,]+) fields\. ([\d,]+) are sent\. ([\d,]+) have no field .*?\. ([\d,]+) more are",
         have + [len(echoed)]),
        (r"\| ([\d,]+) order fields, none dropped \|", [len(fields)]),
        (r"\*\*([\d,]+) order fields are not transmitted\.\*\*", [len(says_so)]),
    ):
        for stated in published(pattern):
            if stated != want:
                print(f"docs/evidence.md publishes {stated} where {want} is "
                      f"what exists ({pattern})")
                return 1
    both = sorted(set(echoed) & set(refuses))
    if both:
        print("reported by the venue and refused by the call, which cannot both")
        print("be right — an order read back and placed again would be refused:")
        for f in both:
            print(f"  {f}")
        return 1

    print(f"{len(fields)} order fields: carried={len(carried)} "
          f"refused={len(says_so)} reported={len(echoed)} dropped={len(dropped)}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
