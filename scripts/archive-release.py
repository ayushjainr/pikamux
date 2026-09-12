#!/usr/bin/env python3
"""Create one deterministic, self-contained native release archive."""

from __future__ import annotations

import gzip
import io
from pathlib import Path
import stat
import sys
import tarfile
from typing import NoReturn
import zipfile


EPOCH = 315532800  # 1980-01-01, representable by both ZIP and tar.
NOTICE_NAMES = ("LICENSE", "THIRD_PARTY.md")
MAX_EXECUTABLE_BYTES = 50 * 1024 * 1024
MAX_NOTICE_BYTES = 2 * 1024 * 1024


def fail(message: str) -> NoReturn:
    raise SystemExit(f"Pika release archive: {message}")


def payloads(
    repository: Path, executable: Path, windows: bool
) -> list[tuple[str, bytes, int]]:
    executable_name = "pika.exe" if windows else "pika"
    if executable.stat().st_size == 0 or executable.stat().st_size > MAX_EXECUTABLE_BYTES:
        fail("executable size is outside the release limit")
    values = [(executable_name, executable.read_bytes(), 0o755)]
    for name in NOTICE_NAMES:
        data = (repository / name).read_bytes()
        if not data or len(data) > MAX_NOTICE_BYTES:
            fail(f"notice size is outside the release limit: {name}")
        values.append((name, data, 0o644))
    return sorted(values, key=lambda row: row[0])


def write_tar(output: Path, values: list[tuple[str, bytes, int]]) -> None:
    with output.open("xb") as raw:
        with gzip.GzipFile(filename="", mode="wb", fileobj=raw, compresslevel=9, mtime=0) as zipped:
            with tarfile.open(fileobj=zipped, mode="w", format=tarfile.GNU_FORMAT) as archive:
                for name, data, mode in values:
                    info = tarfile.TarInfo(name)
                    info.size = len(data)
                    info.mode = mode
                    info.mtime = EPOCH
                    info.uid = 0
                    info.gid = 0
                    info.uname = ""
                    info.gname = ""
                    archive.addfile(info, io.BytesIO(data))


def write_zip(output: Path, values: list[tuple[str, bytes, int]]) -> None:
    with zipfile.ZipFile(output, "x", compression=zipfile.ZIP_DEFLATED, compresslevel=9) as archive:
        for name, data, mode in values:
            info = zipfile.ZipInfo(name, (1980, 1, 1, 0, 0, 0))
            info.compress_type = zipfile.ZIP_DEFLATED
            info.create_system = 3
            info.external_attr = (stat.S_IFREG | mode) << 16
            archive.writestr(info, data, compresslevel=9)


def main(argv: list[str]) -> int:
    if len(argv) != 5 or argv[1] not in {"tar.gz", "zip"}:
        fail("usage: archive-release.py FORMAT REPOSITORY EXECUTABLE OUTPUT")
    archive_format, repository_raw, executable_raw, output_raw = argv[1:]
    repository = Path(repository_raw)
    executable = Path(executable_raw)
    output = Path(output_raw)
    if output.exists() or output.is_symlink():
        fail("output already exists")
    if executable.is_symlink() or not executable.is_file():
        fail("executable must be a regular file")
    for name in NOTICE_NAMES:
        path = repository / name
        if path.is_symlink() or not path.is_file():
            fail(f"missing regular notice file: {name}")
    values = payloads(repository, executable, archive_format == "zip")
    if archive_format == "zip":
        write_zip(output, values)
    else:
        write_tar(output, values)
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv))
