#!/usr/bin/env python3
"""Create Pika's deterministic, source-backed third-party attribution file."""

from __future__ import annotations

import argparse
import hashlib
import json
import re
import tarfile
from collections import defaultdict
from dataclasses import dataclass
from pathlib import Path


ALLOWED_LICENSES = {
    "MIT",
    "Apache-2.0",
    "Apache-2.0 OR MIT",
    "MIT OR Apache-2.0",
    "Apache-2.0/MIT",
    "MIT/Apache-2.0",
    "(MIT OR Apache-2.0) AND Unicode-3.0",
    "Apache-2.0 WITH LLVM-exception OR Apache-2.0 OR MIT",
    "MIT OR Apache-2.0 OR LGPL-2.1-or-later",
    "BSD-3-Clause",
    "ISC",
    "MPL-2.0",
    "Zlib",
    "Unlicense OR MIT",
    "Unlicense/MIT",
}
NOTICE_PREFIXES = ("license", "copying", "notice", "copyright", "authors")
LICENSE_PREFIXES = ("license", "copying", "copyright")
MAX_NOTICE_BYTES = 512 * 1024
MAX_TOTAL_NOTICE_BYTES = 4 * 1024 * 1024


@dataclass(frozen=True)
class Notice:
    digest: str
    text: str


def fail(message: str) -> None:
    raise SystemExit(f"third-party generation failed: {message}")


def markdown(value: str) -> str:
    return (
        value.replace("&", "&amp;")
        .replace("<", "&lt;")
        .replace(">", "&gt;")
        .replace("|", "\\|")
        .replace("\r", " ")
        .replace("\n", " ")
    )


def read_notice(raw: bytes, source: str) -> Notice:
    if len(raw) > MAX_NOTICE_BYTES:
        fail(f"notice exceeds {MAX_NOTICE_BYTES} bytes: {source}")
    try:
        text = raw.decode("utf-8")
    except UnicodeDecodeError:
        fail(f"notice is not UTF-8: {source}")
    text = text.replace("\r\n", "\n").replace("\r", "\n")
    text = "\n".join(line.rstrip() for line in text.split("\n")).rstrip() + "\n"
    normalized = text.encode("utf-8")
    return Notice(hashlib.sha256(normalized).hexdigest(), text)


def fence_for(text: str) -> str:
    longest = max((len(run) for run in re.findall(r"`+", text)), default=0)
    return "`" * max(4, longest + 1)


def package_key(package: dict[str, object]) -> tuple[str, str]:
    return str(package["name"]), str(package["version"])


def parse_lock(path: Path) -> list[dict[str, str]]:
    """Parse the scalar package identity fields from Cargo's generated lockfile."""
    packages: list[dict[str, str]] = []
    current: dict[str, str] | None = None
    for line in path.read_text(encoding="utf-8").splitlines():
        if line == "[[package]]":
            if current is not None:
                packages.append(current)
            current = {}
            continue
        if current is None:
            continue
        match = re.fullmatch(r'(name|version|source|checksum) = ("(?:[^"\\]|\\.)*")', line)
        if match:
            current[match.group(1)] = json.loads(match.group(2))
    if current is not None:
        packages.append(current)
    return packages


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--metadata", type=Path, required=True)
    parser.add_argument("--tree", type=Path, required=True)
    parser.add_argument("--lock", type=Path, required=True)
    parser.add_argument("--rust-copyright", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()

    metadata = json.loads(args.metadata.read_text(encoding="utf-8"))
    lock_packages = parse_lock(args.lock)

    packages_by_key: dict[tuple[str, str], list[dict[str, object]]] = defaultdict(list)
    for package in metadata["packages"]:
        packages_by_key[package_key(package)].append(package)

    locks_by_key: dict[tuple[str, str], list[dict[str, object]]] = defaultdict(list)
    for package in lock_packages:
        locks_by_key[package_key(package)].append(package)

    selected: list[tuple[dict[str, object], str]] = []
    for line in args.tree.read_text(encoding="utf-8").splitlines():
        try:
            package_label, tree_license = line.split("\t", 1)
            name, version = package_label.rsplit(" v", 1)
        except ValueError:
            fail(f"cannot parse cargo tree row: {line!r}")
        candidates = packages_by_key[(name, version)]
        if len(candidates) != 1:
            fail(
                f"expected one metadata package for {name} {version}, "
                f"found {len(candidates)}"
            )
        package = candidates[0]
        declared = package.get("license")
        if not isinstance(declared, str) or declared != tree_license:
            fail(f"license metadata disagrees for {name} {version}")
        if declared not in ALLOWED_LICENSES:
            fail(f"unreviewed license expression for {name} {version}: {declared}")
        selected.append((package, declared))

    notices: dict[str, Notice] = {}
    notice_uses: dict[str, list[tuple[str, str]]] = defaultdict(list)
    rows: list[dict[str, str]] = []
    sqlite_notice_id: str | None = None

    for package, declared in sorted(selected, key=lambda item: package_key(item[0])):
        name, version = package_key(package)
        source = package.get("source")
        if not isinstance(source, str) or not source.startswith("registry+"):
            fail(f"{name} {version} is not a locked registry dependency")
        lock_candidates = [
            entry
            for entry in locks_by_key[(name, version)]
            if entry.get("source") == source
        ]
        if len(lock_candidates) != 1:
            fail(f"expected one Cargo.lock package for {name} {version}")
        checksum = lock_candidates[0].get("checksum")
        if not isinstance(checksum, str) or not re.fullmatch(r"[0-9a-f]{64}", checksum):
            fail(f"missing locked registry checksum for {name} {version}")

        crate_dir = Path(str(package["manifest_path"])).parent.resolve(strict=True)
        source_namespace = crate_dir.parent.name
        registry_root = list(crate_dir.parents)[2]
        archive = registry_root / "cache" / source_namespace / f"{name}-{version}.crate"
        if archive.is_symlink() or not archive.is_file():
            fail(
                f"checksummed crate archive is missing for {name} {version}; "
                "run `cargo fetch --locked` once"
            )
        archive_bytes = archive.read_bytes()
        if len(archive_bytes) > 20 * 1024 * 1024:
            fail(f"crate archive exceeds 20 MiB for {name} {version}")
        if hashlib.sha256(archive_bytes).hexdigest() != checksum:
            fail(f"crate archive checksum disagrees with Cargo.lock for {name} {version}")

        prefix = f"{name}-{version}/"
        package_notice_ids: list[str] = []
        with tarfile.open(archive, mode="r:gz") as crate:
            members: dict[str, tarfile.TarInfo] = {}
            for member in crate.getmembers():
                if not member.name.startswith(prefix):
                    fail(f"crate archive has an unexpected root for {name} {version}")
                relative = member.name[len(prefix) :]
                if relative in members:
                    fail(f"crate archive has a duplicate member for {name} {version}")
                members[relative] = member

            candidates = {
                relative
                for relative, member in members.items()
                if "/" not in relative
                and member.isfile()
                and relative.lower().startswith(NOTICE_PREFIXES)
            }
            license_file = package.get("license_file")
            if isinstance(license_file, str):
                if license_file.startswith("/") or ".." in Path(license_file).parts:
                    fail(f"license-file escapes package root for {name} {version}")
                candidates.add(license_file)
            if not any(
                Path(relative).name.lower().startswith(LICENSE_PREFIXES)
                for relative in candidates
            ):
                fail(f"no source license text found for {name} {version}")

            for relative in sorted(candidates, key=str.casefold):
                member = members.get(relative)
                if member is None or not member.isfile() or member.size > MAX_NOTICE_BYTES:
                    fail(f"invalid notice member for {name} {version}: {relative}")
                extracted = crate.extractfile(member)
                if extracted is None:
                    fail(f"cannot read notice member for {name} {version}: {relative}")
                notice = read_notice(extracted.read(MAX_NOTICE_BYTES + 1), relative)
                prior = notices.get(notice.digest)
                if prior is not None and prior.text != notice.text:
                    fail(f"SHA-256 collision while reading {name} {version}/{relative}")
                notices[notice.digest] = notice
                notice_uses[notice.digest].append((f"{name} {version}", relative))
                package_notice_ids.append(notice.digest)

            if name == "libsqlite3-sys":
                features = next(
                    node["features"]
                    for node in metadata["resolve"]["nodes"]
                    if node["id"] == package["id"]
                )
                if "bundled" not in features:
                    fail("libsqlite3-sys is expected to use its bundled feature")
                sqlite_member = members.get("sqlite3/sqlite3.c")
                if sqlite_member is None or not sqlite_member.isfile():
                    fail("bundled SQLite amalgamation was not found")
                extracted = crate.extractfile(sqlite_member)
                if extracted is None:
                    fail("bundled SQLite amalgamation could not be read")
                source_text = extracted.read(65536).decode("utf-8", errors="strict")
                match = re.search(
                    r"The author disclaims copyright to this source code\.  In place of\n"
                    r"\*\* a legal notice, here is a blessing:\n"
                    r"\*\*\n"
                    r"\*\*    May you do good and not evil\.\n"
                    r"\*\*    May you find forgiveness for yourself and forgive others\.\n"
                    r"\*\*    May you share freely, never taking more than you give\.",
                    source_text,
                )
                if match is None:
                    fail("bundled SQLite public-domain notice was not found")
                sqlite_text = (
                    re.sub(r"^\*\* ?", "", match.group(0), flags=re.MULTILINE) + "\n"
                )
                sqlite_raw = sqlite_text.encode("utf-8")
                sqlite_notice = Notice(hashlib.sha256(sqlite_raw).hexdigest(), sqlite_text)
                notices[sqlite_notice.digest] = sqlite_notice
                notice_uses[sqlite_notice.digest].append(
                    (f"{name} {version}", "sqlite3/sqlite3.c public-domain notice")
                )
                package_notice_ids.append(sqlite_notice.digest)
                sqlite_notice_id = sqlite_notice.digest

        rows.append(
            {
                "name": name,
                "version": version,
                "license": declared,
                "checksum": checksum,
                "authors": "; ".join(
                    str(author) for author in package.get("authors", [])
                )
                or "—",
                "source_url": f"https://crates.io/crates/{name}/{version}",
                "notices": ", ".join(
                    f"N-{digest[:12]}" for digest in package_notice_ids
                ),
            }
        )

    notice_bytes = sum(len(notice.text.encode("utf-8")) for notice in notices.values())
    if notice_bytes > MAX_TOTAL_NOTICE_BYTES:
        fail("deduplicated notices exceed the 4 MiB output budget")
    if sqlite_notice_id is None:
        fail("the expected bundled SQLite attribution was not generated")
    if args.rust_copyright.is_symlink() or not args.rust_copyright.is_file():
        fail("Rust standard-library copyright report is not a regular file")
    rust_notice = read_notice(
        args.rust_copyright.read_bytes(), "Rust 1.88.0 COPYRIGHT-library.html"
    )
    notices[rust_notice.digest] = rust_notice
    notice_uses[rust_notice.digest].append(
        ("Rust standard library 1.88.0", "COPYRIGHT-library.html")
    )
    rust_notice_id = rust_notice.digest
    short_ids: dict[str, str] = {}
    for digest in notices:
        short = digest[:12]
        if short in short_ids and short_ids[short] != digest:
            fail(f"notice identifier collision: {short}")
        short_ids[short] = digest

    output: list[str] = [
        "# Third-party software",
        "",
        "Generated from `Cargo.lock` and the checksummed Cargo crate archives by "
        "`scripts/generate-third-party.sh`; do not edit by hand. The inventory is "
        "the union of normal and build dependencies for Pika's four native macOS/Linux "
        "host targets and its Windows client target. Dev-only packages are excluded.",
        "",
        "Pika enables bundled SQLite. The SQLite amalgamation is included in the binary "
        "and its public-domain declaration is reproduced below with the crate notices.",
        "",
        "The binary also statically links the Rust 1.88.0 standard library. Its complete "
        f"library copyright report is included as N-{rust_notice_id[:12]}.",
        "",
        "| Package | Version | License | Cargo.lock SHA-256 | Declared authors | Source | Included notices |",
        "| --- | --- | --- | --- | --- | --- | --- |",
    ]
    for row in rows:
        escaped = {key: markdown(value) for key, value in row.items()}
        output.append(
            "| `{name}` | `{version}` | `{license}` | `{checksum}` | {authors} | "
            "[crate]({source_url}) | {notices} |".format(**escaped)
        )

    output.extend(["", "## Included notices and license texts", ""])
    for digest in sorted(notices):
        notice = notices[digest]
        uses = sorted(notice_uses[digest])
        output.extend(
            [
                f"### N-{digest[:12]}",
                "",
                f"SHA-256: `{digest}`",
                "",
                "Used by: "
                + "; ".join(
                    f"`{markdown(package)}` (`{markdown(path)}`)"
                    for package, path in uses
                ),
                "",
            ]
        )
        fence = fence_for(notice.text)
        output.extend([f"{fence}text", notice.text.rstrip("\n"), fence, ""])

    args.output.write_text("\n".join(output).rstrip() + "\n", encoding="utf-8")


if __name__ == "__main__":
    main()
