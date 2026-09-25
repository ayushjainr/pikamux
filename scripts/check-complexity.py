#!/usr/bin/env python3
"""Ratchet per-function Lizard CCN without rewriting legacy code to hit a score."""
import argparse
import collections
import importlib.metadata
import json
from pathlib import Path
import sys

ANALYZER = "lizard 1.24.0 + pika Rust char-literal tokenizer fix 1"
LIMIT = 15
ROOT = Path(__file__).resolve().parents[1]


def fix_rust_char_literal_tokenization(lizard):
    """Keep Lizard's lifetime alternative from consuming Rust char literals.

    Lizard 1.24.0 puts ``'word`` before its general quoted-literal token. Without
    the negative lookahead, ``'c'`` becomes two tokens (``'c`` and ``'``), which
    can corrupt its brace/function state. A real lifetime such as ``'a`` remains
    covered by the first alternative.
    """
    from lizard_languages.code_reader import CodeReader
    from lizard_languages.rust import RustReader

    def rust_tokens(source_code, addition="", token_class=None):
        return CodeReader.generate_tokens(
            source_code, r"|(?:'\w+\b(?!'))", token_class
        )

    RustReader.generate_tokens = staticmethod(rust_tokens)


def measure(root):
    import lizard

    if importlib.metadata.version("lizard") != "1.24.0":
        raise ValueError("Install scripts/quality-requirements.txt (pinned analyzer required)")
    fix_rust_char_literal_tokenization(lizard)
    groups = collections.defaultdict(list)
    files = sorted((root / "src").rglob("*.rs"))
    if not files:
        raise ValueError("No Rust source files found")
    for path in files:
        analysis = lizard.analyze_file(str(path))
        for function in analysis.function_list:
            # Line numbers are deliberately not identity. Lizard omits Rust impl
            # scopes: same-named methods are a descending multiset, not overwritten.
            key = f"{path.relative_to(root).as_posix()}::{function.name}"
            groups[key].append(function.cyclomatic_complexity)
    if not groups:
        raise ValueError("Analyzer returned no functions")
    return {key: sorted(values, reverse=True) for key, values in sorted(groups.items())}


def baseline_for(measured):
    return {
        "schema": 1,
        "analyzer": ANALYZER,
        "new_function_limit": LIMIT,
        "hotspots": {
            key: [value for value in values if value > LIMIT]
            for key, values in measured.items() if max(values) > LIMIT
        },
    }


def check(measured, baseline):
    if (baseline.get("schema"), baseline.get("analyzer"), baseline.get("new_function_limit")) != (1, ANALYZER, LIMIT):
        raise ValueError("Unsupported complexity baseline schema, analyzer, or limit")
    hotspots = baseline["hotspots"]
    for key, values in hotspots.items():
        if not isinstance(key, str) or not isinstance(values, list) or not values or any(type(x) is not int or x <= LIMIT for x in values) or values != sorted(values, reverse=True):
            raise ValueError("Malformed hotspot baseline")
    failures = []
    for key, values in measured.items():
        allowed = hotspots.get(key, [])
        for index, value in enumerate(values):
            ceiling = allowed[index] if index < len(allowed) else LIMIT
            if value > ceiling:
                failures.append(f"{key} [{index + 1}]: CCN {value} exceeds {ceiling}")
    # Retire/reduce old allowances, so a later change cannot spend old complexity.
    if not failures and baseline_for(measured) != baseline:
        failures.append("Hotspots improved or disappeared; refresh the baseline to lock in the reduction")
    return failures


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--root", type=Path, default=ROOT)
    parser.add_argument("--baseline", type=Path, default=ROOT / "tests/quality/complexity-baseline.json")
    parser.add_argument("--write-baseline", action="store_true", help="explicit reviewed maintenance only; never used by CI")
    args = parser.parse_args()
    measured = measure(args.root)
    candidate = baseline_for(measured)
    if args.write_baseline:
        args.baseline.parent.mkdir(parents=True, exist_ok=True)
        args.baseline.write_text(json.dumps(candidate, indent=2) + "\n")
        print(f"Wrote {args.baseline}; review every allowance change")
        return 0
    failures = check(measured, json.loads(args.baseline.read_text()))
    for failure in failures:
        print(failure, file=sys.stderr)
    total = sum(len(values) for values in measured.values())
    hotspots = sum(len(values) for values in candidate["hotspots"].values())
    print(f"Complexity: {total} functions, {hotspots} existing hotspots (CCN > {LIMIT}); {len(failures)} failures")
    return int(bool(failures))


if __name__ == "__main__":
    try:
        sys.exit(main())
    except (ValueError, KeyError, OSError) as error:
        sys.exit(str(error))
