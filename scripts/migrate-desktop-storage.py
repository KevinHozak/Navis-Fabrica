"""Copy and verify desktop state, then activate its private storage locator.

Run with Fabrica and its workers closed. Originals are retained for rollback.
SQLite files use the backup API; existing destination files are never replaced.
"""
import argparse
from contextlib import closing
import hashlib
import json
import os
from pathlib import Path
import shutil
import sqlite3
import tempfile


def digest(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def write_new(path, value):
    encoded = (json.dumps(value, indent=2) + "\n").encode("utf-8")
    if path.exists():
        if json.loads(path.read_text(encoding="utf-8")) != value:
            raise RuntimeError(f"Existing configuration differs: {path}")
        return
    with path.open("xb") as handle:
        handle.write(encoded)


def migrate(source, target, logo, projects):
    source, target, logo = source.resolve(), target.resolve(), logo.resolve()
    if source == target or source in target.parents or target in source.parents:
        raise RuntimeError("Source and target must be separate directories")
    if not source.is_dir() or not logo.is_file():
        raise RuntimeError("Source state and logo must exist")
    files = []
    for path in source.rglob("*"):
        if path.is_symlink() or path.is_junction():
            raise RuntimeError(f"Linked state path requires manual migration: {path}")
        if path.is_file() and path.name != "storage-location.json":
            files.append(path)
    target.mkdir(parents=True, exist_ok=True)
    copied = []
    for path in files:
        relative = path.relative_to(source)
        # WAL contents are captured through SQLite's backup API below.
        if path.name.endswith(("-wal", "-shm")):
            database = path.with_name(path.name.rsplit("-", 1)[0])
            if database.exists() and database.read_bytes()[:16] == b"SQLite format 3\x00":
                continue
            raise RuntimeError(f"Orphan SQLite sidecar: {path}")
        dest = target / relative
        dest.parent.mkdir(parents=True, exist_ok=True)
        with path.open("rb") as handle:
            sqlite = handle.read(16) == b"SQLite format 3\x00"
        before = digest(path)
        if dest.exists():
            if sqlite or digest(dest) != before:
                raise RuntimeError(f"Refusing to replace existing state: {dest}")
        elif sqlite:
            with closing(sqlite3.connect(path.as_uri() + "?mode=ro", uri=True)) as src:
                with closing(sqlite3.connect(dest)) as dst:
                    src.backup(dst)
                    if dst.execute("PRAGMA integrity_check").fetchone()[0] != "ok":
                        raise RuntimeError(f"SQLite integrity check failed: {dest}")
        else:
            shutil.copy2(path, dest)
        if digest(path) != before or (not sqlite and digest(dest) != before):
            raise RuntimeError(f"State changed during migration: {path}")
        copied.append({"file": relative.as_posix(), "sha256": digest(dest)})
    logos = target / "logos"
    logos.mkdir(exist_ok=True)
    dest_logo = logos / "gamersplane.png"
    if dest_logo.exists() and digest(dest_logo) != digest(logo):
        raise RuntimeError("Existing GamersPlane logo differs")
    if not dest_logo.exists():
        shutil.copy2(logo, dest_logo)
    if digest(dest_logo) != digest(logo):
        raise RuntimeError("Logo read-back failed")
    write_new(target / "logo-overrides.json", {
        "schemaVersion": 1,
        "projects": {str(project.resolve()): "logos/gamersplane.png" for project in projects},
    })
    write_new(target / "migration-receipt.json", {
        "source": str(source), "destination": str(target),
        "originalsRetained": True, "verifiedFiles": copied,
    })
    location = {"schemaVersion": 1, "dataRoot": str(target)}
    locator = source / "storage-location.json"
    if locator.exists():
        write_new(locator, location)
    else:
        # Activate only after every copy, configuration and read-back succeeds.
        with tempfile.NamedTemporaryFile(dir=source, delete=False) as handle:
            temp = Path(handle.name)
            handle.write((json.dumps(location, indent=2) + "\n").encode("utf-8"))
        os.replace(temp, locator)
    assert json.loads(locator.read_text(encoding="utf-8")) == location
    print(f"Verified {len(copied)} state files and logo; originals retained. Active data root: {target}")


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--source", type=Path, required=True)
    parser.add_argument("--target", type=Path, required=True)
    parser.add_argument("--logo", type=Path, required=True)
    parser.add_argument("--project", type=Path, action="append", required=True)
    args = parser.parse_args()
    migrate(args.source, args.target, args.logo, args.project)
