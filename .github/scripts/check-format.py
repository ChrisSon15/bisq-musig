#!/usr/bin/env python3
"""Checks the formatting of the lines of Rust files that differ from a base commit.

Usage: check-format.py <base>          report lines that are not formatted, exit code 1 if any
       check-format.py <base> --fix    format these lines

The formatting rules are the ones of rustfmt.toml, which needs the nightly version of rustfmt.
Only changed lines are looked at: several files of the repository are not formatted as a whole,
and a change must not have to reformat lines that it does not touch. A file that is new compared
to the base is checked as a whole.
"""
import json
import re
import subprocess
import sys


def git(*args):
    return subprocess.run(("git",) + args, capture_output=True, text=True, check=True).stdout


def changed_ranges(base):
    """Returns {path: [[first line, last line], ...]}, or {path: None} for a new file."""
    new_files = set(git("diff", "--name-only", "--diff-filter=A", base, "--", "*.rs").split())
    ranges = {}
    path = None
    for line in git("diff", "--diff-filter=AM", "-U0", base, "--", "*.rs").splitlines():
        if line.startswith("+++ b/"):
            path = line[6:]
            ranges[path] = None if path in new_files else []
        elif line.startswith("@@") and path and ranges[path] is not None:
            match = re.search(r"\+(\d+)(?:,(\d+))?", line)
            start, count = int(match.group(1)), int(match.group(2) or 1)
            if count:
                ranges[path].append([start, start + count - 1])
    return ranges


def edition_of(path):
    """The `rpc` crate is on the edition 2021, all other crates on 2024."""
    return "2021" if path.startswith("rpc/") else "2024"


def main():
    if len(sys.argv) < 2:
        sys.exit(__doc__)
    base, fix = sys.argv[1], "--fix" in sys.argv[2:]
    unformatted = []
    for path, ranges in sorted(changed_ranges(base).items()):
        if ranges == []:
            continue
        command = ["rustfmt", "+nightly", "--unstable-features", "--edition", edition_of(path),
                   "--config", "skip_children=true"]
        if not fix:
            command.append("--check")
        if ranges is not None:
            lines = [{"file": path, "range": line_range} for line_range in ranges]
            command += ["--file-lines", json.dumps(lines)]
        result = subprocess.run(command + [path], capture_output=True, text=True)
        if result.returncode != 0:
            unformatted.append(path)
            print(result.stdout + result.stderr)
    if unformatted:
        print("Not formatted (changed lines only):", *unformatted, sep="\n  ")
        print(f"\nTo format them: python3 .github/scripts/check-format.py {base} --fix")
        sys.exit(1)
    print("The changed lines of all Rust files are formatted.")


if __name__ == "__main__":
    main()
