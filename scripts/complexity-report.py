#!/usr/bin/env python3
"""Rank the functions in a Rust file by cyclomatic and cognitive complexity.

Feeds `rust-code-analysis-cli`'s JSON output through the metrics it buries
in a nested `spaces` tree and prints the functions worst-first, so a file
scc has flagged as heavier than its size accounts for can be narrowed down
to which functions actually carry that weight.

    rust-code-analysis-cli -p firmware/src/main.rs -m -O json \\
        | scripts/complexity-report.py

`--self-test` runs the parsing below without invoking rust-code-analysis-cli.
"""
import argparse
import io
import json
import sys


def functions(tree):
    stack = [tree]
    while stack:
        node = stack.pop()
        metrics = node.get("metrics", {})
        if node.get("kind") in ("function", "function_item", "closure"):
            yield (
                metrics.get("cyclomatic", {}).get("sum") or 0,
                metrics.get("cognitive", {}).get("sum") or 0,
                metrics.get("loc", {}).get("sloc") or 0,
                node.get("name") or "<anonymous>",
                node.get("start_line"),
                node.get("end_line"),
            )
        stack.extend(node.get("spaces", []))


def report(tree, out):
    rows = sorted(functions(tree), key=lambda row: -row[0])
    for cyclo, cogn, sloc, name, start, end in rows:
        out.write(
            f"{cyclo:>6.1f} cyclo  {cogn:>6.1f} cogn  {sloc:>5} sloc  "
            f"{name}  (lines {start}-{end})\n"
        )
    return rows


SELF_TEST_TREE = {
    "kind": "function",
    "name": "outer",
    "start_line": 1,
    "end_line": 10,
    "metrics": {
        "cyclomatic": {"sum": 3},
        "cognitive": {"sum": 1},
        "loc": {"sloc": 8},
    },
    "spaces": [
        {
            "kind": "closure",
            "name": "<anonymous>",
            "start_line": 4,
            "end_line": 6,
            "metrics": {
                "cyclomatic": {"sum": 9},
                "cognitive": {"sum": 4},
                "loc": {"sloc": 2},
            },
            "spaces": [],
        }
    ],
}


def self_test():
    out = io.StringIO()
    rows = report(SELF_TEST_TREE, out)
    assert [row[3] for row in rows] == ["<anonymous>", "outer"], rows
    assert "9.0 cyclo" in out.getvalue().splitlines()[0]
    print("self-test passed")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--self-test", action="store_true")
    args = parser.parse_args()

    if args.self_test:
        self_test()
        return

    report(json.load(sys.stdin), sys.stdout)


if __name__ == "__main__":
    main()
