#!/usr/bin/env python3
"""Take an online SQLite backup, including committed WAL pages, without stopping serve."""
import argparse
from datetime import datetime, timezone
import os
from pathlib import Path
import sqlite3
import tempfile


def backup(source: Path, directory: Path) -> Path:
    os.umask(0o077)
    # mode=ro must not create a missing source. Do not use immutable=1: it ignores WAL.
    with sqlite3.connect(source.resolve().as_uri() + "?mode=ro", uri=True, timeout=30) as reader:
        directory.mkdir(mode=0o700, parents=True, exist_ok=True)
        os.chmod(directory, 0o700)
        fd, name = tempfile.mkstemp(prefix=".online-", suffix=".tmp", dir=directory)
        os.close(fd)
        temporary = Path(name)
        try:
            with sqlite3.connect(temporary) as destination:
                reader.backup(destination, pages=256, sleep=0.05)
                if destination.execute("PRAGMA integrity_check").fetchall() != [("ok",)]:
                    raise sqlite3.DatabaseError("backup failed integrity_check")
            with temporary.open("rb") as stream:
                os.fsync(stream.fileno())
            stamp = datetime.now(timezone.utc).strftime("%Y%m%dT%H%M%S.%fZ")
            result = directory / f"schlange-{stamp}.db"
            # Atomic publication without ever replacing an existing backup.
            os.link(temporary, result)
            directory_fd = os.open(directory, os.O_RDONLY | os.O_DIRECTORY)
            try:
                os.fsync(directory_fd)
            finally:
                os.close(directory_fd)
            return result
        finally:
            temporary.unlink(missing_ok=True)


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("source", type=Path)
    parser.add_argument("directory", type=Path)
    args = parser.parse_args()
    print(backup(args.source, args.directory))
