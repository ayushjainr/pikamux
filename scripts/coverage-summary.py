#!/usr/bin/env python3
"""Summarize measured recovery branches; absent instrumentation is an error."""
import json
from pathlib import Path
import sys

FILES = ("core.rs", "store.rs", "hooks.rs", "process.rs", "tmux.rs", "cli.rs")


def summarize(document):
    if document.get("type") != "llvm.coverage.json.export":
        raise ValueError("Not an LLVM coverage export")
    found = {}
    for unit in document["data"]:
        for file in unit["files"]:
            path = file["filename"].replace("\\", "/")
            for name in FILES:
                if path.endswith("/src/" + name) or path == "src/" + name:
                    if name in found:
                        raise ValueError(f"Duplicate coverage entry: {name}")
                    found[name] = file["summary"]
    lines = ["# Recovery coverage", "", "Measured on the coverage-only nightly; not a release-wide percentage.", "", "| Source | Branch outcomes | Lines |", "| --- | ---: | ---: |"]
    for name in FILES:
        if name not in found:
            raise ValueError(f"Missing recovery source: {name}")
        cells = []
        for metric in ("branches", "lines"):
            values = found[name][metric]
            count, covered = values["count"], values["covered"]
            if type(count) is not int or type(covered) is not int or count <= 0 or not 0 <= covered <= count:
                raise ValueError(f"Missing or invalid {metric} instrumentation: {name}")
            cells.append(f"{covered}/{count} ({100 * covered / count:.1f}%)")
        lines.append(f"| src/{name} | {' | '.join(cells)} |")
    lines += ["", "No blanket coverage target. Review uncovered recovery decisions in the HTML artifact.", "Branch outcomes are compiler instrumentation, not all possible state/interleaving paths."]
    return "\n".join(lines) + "\n"


if __name__ == "__main__":
    try:
        print(summarize(json.loads(Path(sys.argv[1]).read_text())), end="")
    except (ValueError, KeyError, OSError, IndexError) as error:
        sys.exit(str(error))
