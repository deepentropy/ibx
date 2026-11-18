"""Prepares a release (prepare-release.yml): bumps the version in every file
that carries it and regenerates the API reference pages.

    python scripts/release_bump.py <part>
      part: "patch" | "minor"

Releases when main has commits since the tag of the current version.

Writes release=true|false and version=<x.y.z> to $GITHUB_OUTPUT (stdout
without it). Files are changed only when release=true.
"""
import os
import re
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent


def sh(*args: str) -> str:
    return subprocess.run(args, cwd=ROOT, check=True, capture_output=True, text=True).stdout.strip()


def output(**values: str) -> None:
    lines = "".join(f"{k}={v}\n" for k, v in values.items())
    path = os.environ.get("GITHUB_OUTPUT")
    if path:
        with open(path, "a", encoding="utf-8") as f:
            f.write(lines)
    else:
        sys.stdout.write(lines)


def replace_once(path: str, pattern: str, repl: str) -> None:
    file = ROOT / path
    text = file.read_text(encoding="utf-8")
    new, n = re.subn(pattern, repl, text, count=1, flags=re.MULTILINE)
    if n != 1:
        sys.exit(f"version not found in {path}")
    file.write_bytes(new.encode("utf-8"))


def main() -> None:
    part = sys.argv[1] if len(sys.argv) > 1 else "patch"
    if part not in ("patch", "minor"):
        sys.exit(f"part must be patch or minor, not {part}")
    current = re.search(r'^version = "([^"]+)"', (ROOT / "Cargo.toml").read_text(encoding="utf-8"), re.MULTILINE)[1]
    commits = int(sh("git", "rev-list", "--count", f"v{current}..HEAD"))
    print(f"commits since v{current}: {commits}", file=sys.stderr)
    if commits == 0:
        print("Nothing to release.", file=sys.stderr)
        output(release="false", version=current)
        return

    major, minor, patch = (int(x) for x in current.split("."))
    version = f"{major}.{minor + 1}.0" if part == "minor" else f"{major}.{minor}.{patch + 1}"
    print(f"version: {current} -> {version}", file=sys.stderr)

    package = r'^(version = ")[^"]+(")'
    for path in ("Cargo.toml", "pyproject.toml", "fuzz/ibx-rlib/Cargo.toml"):
        replace_once(path, package, rf"\g<1>{version}\g<2>")
    replace_once("cpp/CMakeLists.txt", r"^(project\(ibx_cpp VERSION )[0-9.]+", rf"\g<1>{version}")
    for path in ("Cargo.lock", "uv.lock"):
        replace_once(path, r'^(name = "ibx"\r?\nversion = ")[^"]+(")', rf"\g<1>{version}\g<2>")

    # The API reference pages, as a release by hand regenerates them.
    subprocess.run([sys.executable, "scripts/gen_api_docs.py"], cwd=ROOT, check=True)
    output(release="true", version=version)


if __name__ == "__main__":
    main()
