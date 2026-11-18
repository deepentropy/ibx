"""Release notes for a version tag (release.yml): the commit subjects since
the previous version tag.

    python scripts/release_notes.py <tag>

Prints the notes as Markdown on stdout.
"""
import re
import subprocess
import sys


def sh(*args: str) -> str:
    return subprocess.run(args, check=True, capture_output=True, text=True).stdout.strip()


def main() -> None:
    tag = sys.argv[1]
    # The release job may check out the new tag alone: fetch the others,
    # then take the newest version tag in its history.
    try:
        sh("git", "fetch", "--tags", "--force", "--quiet")
    except subprocess.CalledProcessError:
        pass
    try:
        previous = sh("git", "describe", "--tags", "--abbrev=0", "--match", "v*.*.*", f"{tag}~1")
    except subprocess.CalledProcessError:
        previous = None

    span = f"{previous}..{tag}" if previous else tag
    subjects = [
        s for s in sh("git", "log", span, "--no-merges", "--format=%s").splitlines()
        if s and not re.fullmatch(r"release: v\d+\.\d+\.\d+", s)
    ]
    lines = []
    if subjects:
        lines += [f"### Changes since {previous}" if previous else "### Changes", ""]
        lines += [f"- {s}" for s in subjects]
        lines.append("")
    lines.append("Install the Python wheel below with `pip install <file>.whl`, or build from source "
                 "with `pip install git+https://github.com/deepentropy/ibx@" + tag + "`.")
    sys.stdout.write("\n".join(lines) + "\n")


if __name__ == "__main__":
    main()
