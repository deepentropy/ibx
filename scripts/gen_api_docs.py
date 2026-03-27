#!/usr/bin/env python3
"""Generate API reference docs from Rust source files.

Parses pub fn signatures + doc comments from src/api/client/*.rs,
Wrapper trait from src/api/wrapper.rs, and Python pymethods from
src/python/compat/client/*.rs. Outputs docs/RUST_API.md and docs/PYTHON_API.md.

Usage: py scripts/gen_api_docs.py
"""

import re
import tomllib
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
RUST_CLIENT = ROOT / "src" / "api" / "client"
RUST_WRAPPER = ROOT / "src" / "api" / "wrapper.rs"
PY_CLIENT = ROOT / "src" / "python" / "compat" / "client"
PY_WRAPPER = ROOT / "src" / "python" / "compat" / "wrapper.rs"
DOCS = ROOT / "docs"


def version() -> str:
    with open(ROOT / "Cargo.toml", "rb") as f:
        return tomllib.load(f)["package"]["version"]


# ── Rust parser ──

def parse_rust_methods(path: Path) -> list[dict]:
    """Extract pub fn methods with preceding /// doc comments."""
    text = path.read_text(encoding="utf-8")
    results = []
    # Find all pub fn with optional preceding doc comments
    for m in re.finditer(
        r'((?:\s*///[^\n]*\n)*)\s*pub fn (\w+)\s*\(([^)]*(?:\([^)]*\)[^)]*)*)\)([^{;]*)',
        text,
    ):
        doc_block, name, _args, ret = m.group(1), m.group(2), m.group(3), m.group(4)
        # Parse doc
        doc_lines = []
        for line in doc_block.strip().splitlines():
            line = line.strip().removeprefix("///").strip()
            if line:
                doc_lines.append(line)
        doc = " ".join(doc_lines)
        # Clean doc: remove "Matches `xyz` in C++."
        doc = re.sub(r"\s*Matches `[^`]+` in C\+\+\.?", "", doc)
        # Build signature
        full_line = m.group(0).strip()
        sig = re.sub(r'\s*\{.*', '', full_line).strip()
        sig = re.sub(r'\s+', ' ', sig)
        # Remove preceding doc comments from sig
        sig = re.sub(r'///[^\n]*\n\s*', '', sig).strip()
        results.append({"name": name, "signature": sig, "doc": doc})
    return results


def parse_wrapper_trait(path: Path) -> list[dict]:
    """Extract fn methods from the Wrapper trait block."""
    text = path.read_text(encoding="utf-8")
    # Find the trait block
    trait_m = re.search(r'pub trait Wrapper\s*\{(.*?)\n\}', text, re.DOTALL)
    if not trait_m:
        return []
    body = trait_m.group(1)
    results = []
    for m in re.finditer(
        r'((?:\s*//[^\n]*\n)*)\s*fn (\w+)\s*\(([^)]*(?:\([^)]*\)[^)]*)*)\)([^{}]*)',
        body,
    ):
        comment_block, name, _args, _ret = m.group(1), m.group(2), m.group(3), m.group(4)
        doc_lines = []
        for line in comment_block.strip().splitlines():
            line = line.strip()
            if line.startswith("///"):
                doc_lines.append(line.removeprefix("///").strip())
            elif line.startswith("// "):
                # Section header like "// ── Market Data ──"
                sec = line.removeprefix("//").strip()
                sec = sec.strip("─ ").strip()
                if sec:
                    pass  # Skip section headers as doc
        doc = " ".join(doc_lines)
        results.append({"name": name, "signature": f"fn {name}(...)", "doc": doc})
    return results


# ── Python parser ──

def parse_pymethods(path: Path) -> list[dict]:
    """Extract fn methods from all #[pymethods] impl blocks."""
    text = path.read_text(encoding="utf-8")
    results = []
    # Find all #[pymethods] blocks
    # Strategy: find each #[pymethods], then find the impl block, then extract methods
    blocks = re.split(r'#\[pymethods\]', text)
    for block in blocks[1:]:  # skip text before first #[pymethods]
        # Find the impl opening
        impl_m = re.match(r'\s*impl\s+\w+\s*\{', block)
        if not impl_m:
            continue
        # Track braces to find end of impl block
        start = impl_m.end() - 1  # position of opening {
        depth = 0
        end = start
        for i in range(start, len(block)):
            if block[i] == '{':
                depth += 1
            elif block[i] == '}':
                depth -= 1
                if depth == 0:
                    end = i
                    break
        impl_body = block[start + 1:end]
        # Extract methods from this impl body
        for fm in re.finditer(
            r'((?:\s*(?:///|//)[^\n]*\n|\s*#\[pyo3[^\]]*\]\s*\n)*)\s*fn (\w+)\s*\(([^)]*(?:\([^)]*\)[^)]*)*)\)',
            impl_body,
        ):
            preamble, name, args_str = fm.group(1), fm.group(2), fm.group(3)
            # Skip __new__ internal
            if name.startswith("__"):
                continue
            # Extract doc
            doc_lines = []
            pyo3_sig = ""
            for line in preamble.strip().splitlines():
                line = line.strip()
                if line.startswith("///"):
                    doc_lines.append(line.removeprefix("///").strip())
                elif line.startswith("#[pyo3(signature"):
                    pyo3_sig = line
            doc = " ".join(doc_lines)
            doc = re.sub(r"\s*Matches `[^`]+` in C\+\+\.?", "", doc)
            # Build Python signature
            py_sig = _build_py_sig(name, args_str, pyo3_sig)
            results.append({"name": name, "signature": py_sig, "doc": doc})
    return results


def _build_py_sig(name: str, rust_args: str, pyo3_sig: str) -> str:
    if pyo3_sig:
        m = re.search(r'signature\s*=\s*\((.+)\)', pyo3_sig)
        if m:
            return f"{name}({m.group(1)})"
    # Parse rust args, skip &self, py: Python
    args = [a.strip() for a in rust_args.split(',')]
    py_args = []
    for arg in args:
        if not arg or arg in ("&self", "&mut self"):
            continue
        if "Python" in arg:
            continue
        am = re.match(r'(\w+)\s*:', arg)
        if am:
            py_args.append(am.group(1))
    return f"{name}({', '.join(py_args)})"


def parse_py_wrapper(path: Path) -> list[dict]:
    """Extract callback methods from Python EWrapper pymethods."""
    return parse_pymethods(path)


# ── Markdown generation ──

SECTION_NAMES = {
    "mod": "Connection",
    "account": "Account & Portfolio",
    "orders": "Orders",
    "market_data": "Market Data",
    "reference": "Reference Data",
    "stubs": "Gateway-Local & Stubs",
    "dispatch": None,
    "tests": None,
    "test_helpers": None,
}

FILE_ORDER = ["mod", "account", "orders", "market_data", "reference", "stubs"]


def fmt_row(m: dict) -> str:
    return f"| `{m['name']}` | {m['doc']} |"


def generate_rust_md(ver: str) -> str:
    out = [
        f"# Rust API Reference (v{ver})",
        "",
        "*Auto-generated from source — do not edit.*",
        "",
        "## EClient Methods",
        "",
    ]
    for stem in FILE_ORDER:
        fname = RUST_CLIENT / f"{stem}.rs"
        if not fname.exists():
            continue
        section = SECTION_NAMES.get(stem, stem.replace("_", " ").title())
        if section is None:
            continue
        methods = parse_rust_methods(fname)
        if not methods:
            continue
        out.append(f"### {section}")
        out.append("")
        out.append("| Method | Description |")
        out.append("|--------|-------------|")
        for m in methods:
            out.append(fmt_row(m))
        out.append("")

    # Wrapper
    out.append("## Wrapper Callbacks")
    out.append("")
    wm = parse_wrapper_trait(RUST_WRAPPER)
    out.append("| Callback | Description |")
    out.append("|----------|-------------|")
    for m in wm:
        out.append(fmt_row(m))
    out.append("")

    # Signatures appendix
    out.append("## Full Signatures")
    out.append("")
    out.append("<details>")
    out.append("<summary>EClient</summary>")
    out.append("")
    out.append("```rust")
    for stem in FILE_ORDER:
        fname = RUST_CLIENT / f"{stem}.rs"
        if not fname.exists():
            continue
        if SECTION_NAMES.get(stem) is None:
            continue
        for m in parse_rust_methods(fname):
            out.append(m["signature"])
    out.append("```")
    out.append("</details>")
    out.append("")
    out.append("<details>")
    out.append("<summary>Wrapper</summary>")
    out.append("")
    out.append("```rust")
    for m in wm:
        out.append(m["signature"])
    out.append("```")
    out.append("</details>")
    out.append("")

    return "\n".join(out)


def generate_python_md(ver: str) -> str:
    out = [
        f"# Python API Reference (v{ver})",
        "",
        "*Auto-generated from source — do not edit.*",
        "",
        "## EClient Methods",
        "",
    ]
    for stem in FILE_ORDER:
        fname = PY_CLIENT / f"{stem}.rs"
        if not fname.exists():
            continue
        section = SECTION_NAMES.get(stem, stem.replace("_", " ").title())
        if section is None:
            continue
        methods = parse_pymethods(fname)
        if not methods:
            continue
        out.append(f"### {section}")
        out.append("")
        out.append("| Method | Description |")
        out.append("|--------|-------------|")
        for m in methods:
            out.append(fmt_row(m))
        out.append("")

    # Wrapper
    out.append("## EWrapper Callbacks")
    out.append("")
    wm = parse_py_wrapper(PY_WRAPPER)
    out.append("| Callback | Description |")
    out.append("|----------|-------------|")
    for m in wm:
        out.append(fmt_row(m))
    out.append("")

    # Signatures
    out.append("## Full Signatures")
    out.append("")
    out.append("<details>")
    out.append("<summary>EClient</summary>")
    out.append("")
    out.append("```python")
    for stem in FILE_ORDER:
        fname = PY_CLIENT / f"{stem}.rs"
        if not fname.exists():
            continue
        if SECTION_NAMES.get(stem) is None:
            continue
        for m in parse_pymethods(fname):
            out.append(f"def {m['signature']}")
    out.append("```")
    out.append("</details>")
    out.append("")
    out.append("<details>")
    out.append("<summary>EWrapper</summary>")
    out.append("")
    out.append("```python")
    for m in wm:
        out.append(f"def {m['signature']}")
    out.append("```")
    out.append("</details>")
    out.append("")

    return "\n".join(out)


def main():
    DOCS.mkdir(exist_ok=True)
    ver = version()
    rust = generate_rust_md(ver)
    py = generate_python_md(ver)
    (DOCS / "RUST_API.md").write_text(rust, encoding="utf-8")
    (DOCS / "PYTHON_API.md").write_text(py, encoding="utf-8")
    rc = rust.count("| `")
    pc = py.count("| `")
    print(f"docs/RUST_API.md  — {rc} methods")
    print(f"docs/PYTHON_API.md — {pc} methods")


if __name__ == "__main__":
    main()
