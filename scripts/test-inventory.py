# /// script
# requires-python = ">=3.12"
# dependencies = ["tree-sitter==0.25.2", "tree-sitter-rust==0.24.0"]
# ///
"""Measure test source independently of Cargo test grouping. Run with uv run."""
import argparse
import io
import json
import re
from pathlib import Path
import subprocess
import tarfile

import tree_sitter
import tree_sitter_rust

parser = argparse.ArgumentParser()
parser.add_argument("--revision", help="Git commit to measure; default is working tree")
args = parser.parse_args()
if args.revision:
    archive = subprocess.check_output(["git", "archive", args.revision])
    with tarfile.open(fileobj=io.BytesIO(archive)) as tar:
        sources = {entry.name: tar.extractfile(entry).read() for entry in tar if entry.isfile()}
else:
    names = subprocess.check_output(["git", "ls-files", "--cached", "--others", "--exclude-standard"], text=True).splitlines()
    sources = {name: Path(name).read_bytes() for name in names if Path(name).is_file()}
rust = tree_sitter.Parser(tree_sitter.Language(tree_sitter_rust.language()))
rows = []
for name, data in sources.items():
    data = data.replace(b"\r\n", b"\n")
    spans = []
    cases = 0
    if name.endswith(".rs"):
        def visit(node):
            global cases
            attrs = []
            for child in node.named_children:
                if child.type == "attribute_item":
                    attrs.append(child)
                    continue
                attributes = b"".join(data[a.start_byte:a.end_byte] for a in attrs)
                if child.type == "function_item" and (b"#[test]" in attributes or b"#[tokio::test" in attributes):
                    cases += 1
                if re.search(rb"cfg\((?:all\()?test(?:[,)]|$)", attributes):
                    spans.append((attrs[0].start_byte, child.end_byte))
                visit(child)
                attrs = []
        visit(rust.parse(data).root_node)
        dedicated = ("/tests/" in name or "/output_tests/" in name or name.startswith("tests/")
                     or name.endswith(("/tests.rs", "/output_tests.rs", "_tests.rs"))
                     or name == "examples/benchmark_journal.rs")
        if dedicated:
            spans = [(0, len(data))]
    elif name.endswith((".py", ".mjs", ".sh")) and (
        name.startswith(("scripts/r13/", "scripts/attachment-", "tests/fixtures/"))
        or name in ("scripts/run-r13-acceptance.py", "scripts/check-cache-recovery.py",
                    "scripts/test-audit-dependencies.sh", "scripts/test-docker-context.sh",
                    "scripts/test-inventory.py")
    ):
        spans = [(0, len(data))]
    outer = [(a, b) for a, b in spans if not any(x < a and b <= y for x, y in spans)]
    lines = sum(len(data[a:b].splitlines()) for a, b in outer)
    size = sum(b - a for a, b in outer)
    if lines or cases:
        rows.append({"path": name, "lines": lines, "bytes": size, "rust_cases": cases})
print(json.dumps({"revision": args.revision or "working-tree", "lines": sum(r["lines"] for r in rows),
                  "bytes": sum(r["bytes"] for r in rows), "rust_cases": sum(r["rust_cases"] for r in rows),
                  "files": rows}, indent=2))
