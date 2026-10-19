#!/usr/bin/env python3
"""Prepare one content-bound Tauri version shared by build and push workflows.

No build, network access, staging or Git mutation is performed. Success receipts
attest only the files/commit supplied by a caller that verified the real effect.
"""
from __future__ import annotations

import argparse
import contextlib
import hashlib
import json
import os
from pathlib import Path, PurePosixPath
import re
import subprocess
import sys
import tempfile
import time
import uuid

import set_version as setter

SCHEMA = 1
MAX_FILE = 32 * 1024 * 1024
MAX_SOURCE = 256 * 1024 * 1024
DEFAULT_INCLUDES = (
    "src", "src-tauri", "runner-core", "public", "package.json",
    "package-lock.json", "Cargo.toml", "Cargo.lock", "index.html",
    "vite.config.ts", "vite.config.js", "tsconfig.json", "tsconfig.node.json",
    "tsconfig.app.json", "rust-toolchain.toml", ".node-version", ".npmrc",
    "scripts/build-worker.mjs",
)
GENERATED = {"target", "node_modules", "dist", ".git", ".worktrees"}


def git(root, *args, data=None):
    return subprocess.check_output(
        ["git", "-C", str(root), *args], input=data, stderr=subprocess.PIPE,
        timeout=60,
    )


def text_git(root, *args):
    return git(root, *args).decode("utf-8").strip()


def stable(value):
    if not isinstance(value, str) or not re.fullmatch(r"(0|[1-9]\d*)\.(0|[1-9]\d*)\.(0|[1-9]\d*)", value):
        raise ValueError(f"Expected stable SemVer: {value!r}")
    return tuple(map(int, value.split(".")))


def synchronized(versions):
    if len(set(versions.values())) != 1:
        raise ValueError(f"Managed versions disagree: {versions}")
    value = next(iter(versions.values()))
    stable(value)
    return value


def next_patch(version):
    major, minor, patch = stable(version)
    return f"{major}.{minor}.{patch + 1}"


def safe_relative(value):
    value = value.replace("\\", "/")
    if not value or ":" in value or value.startswith("/") or any(p in ("", ".", "..") for p in value.split("/")):
        raise ValueError(f"Unsafe app source path: {value!r}")
    return PurePosixPath(value).as_posix()


def context(repo, state_dir=None):
    repo = Path(repo).resolve()
    root = Path(text_git(repo, "rev-parse", "--show-toplevel")).resolve()
    paths = setter.discover(repo)
    app = paths["tauri"].parent.parent
    app_relative = app.relative_to(root).as_posix()
    common = Path(text_git(repo, "rev-parse", "--git-common-dir"))
    common = (repo / common).resolve() if not common.is_absolute() else common.resolve()
    identity = {"git_common_dir": os.path.normcase(str(common)), "app": app_relative}
    digest = hashlib.sha256(json.dumps(identity, sort_keys=True).encode()).hexdigest()
    directory = Path(state_dir or os.environ.get("FABRICA_VERSION_STATE_DIR") or
                     (Path(os.environ.get("LOCALAPPDATA", Path.home() / ".local" / "state")) /
                      "Navis-Fabrica" / "version-preparation")).resolve()
    if directory == root or root in directory.parents or directory == common or common in directory.parents:
        raise ValueError("Version state must be outside the source repository and Git metadata")
    return {"root": root, "app": app, "paths": paths, "identity": identity,
            "state_path": directory / (digest + ".json")}


def profile_for(ctx, includes=None):
    prefix = ctx["app"].relative_to(ctx["root"]).as_posix()
    prefix = "" if prefix == "." else prefix + "/"
    profile = sorted(set(prefix + safe_relative(p) for p in (includes or DEFAULT_INCLUDES)))
    if len(profile) > 128:
        raise ValueError("App source profile exceeds 128 roots")
    return profile


def selected(name, profile, managed):
    if name in managed:
        return True
    if any(p in GENERATED for p in PurePosixPath(name).parts):
        return False
    if PurePosixPath(name).suffix.lower() in {".md", ".rst"}:
        return name in profile  # Explicitly imported documentation can be a build input.
    return any(name == p or name.startswith(p + "/") for p in profile)


def normalize(paths, texts):
    """Mask only fields the setter owns; dependency versions remain source."""
    setter.read_versions(paths, texts)
    result = dict(texts)
    for key, original in texts.items():
        if key in ("tauri", "package", "package_lock"):
            changes = {("version",): "0.0.0"}
            if key == "package_lock" and "" in json.loads(original).get("packages", {}):
                changes[("packages", "", "version")] = "0.0.0"
            if key == "tauri":
                for i, window in enumerate(json.loads(original).get("app", {}).get("windows", [])):
                    title = window.get("title", "")
                    match = setter.TITLE_VERSION.search(title)
                    if match:
                        changes[("app", "windows", i, "title")] = title[:match.start(1)] + "0.0.0" + title[match.end(1):]
            result[key] = setter.replace_json(original, changes)
        elif key == "cargo":
            result[key], count = re.subn(
                r'(?ms)(^\[package\][^\n]*\n(?:(?!^\[).)*?^\s*version\s*=\s*")[^"]+(".*?$)',
                lambda m: m[1] + "0.0.0" + m[2], original, count=1)
            if count != 1:
                raise ValueError("Cannot normalize Cargo application version")
        elif key == "cargo_lock":
            match = re.search(setter.lock_pattern(setter.package_field(texts["cargo"], "name")), original)
            if not match:
                raise ValueError("Cargo.lock has no application package entry")
            result[key] = original[:match.start(1)] + "0.0.0" + original[match.end(1):]
    return result


def snapshot(ctx, profile, ref=None):
    root, paths = ctx["root"], ctx["paths"]
    managed = {p.relative_to(root).as_posix(): key for key, p in paths.items()}
    files = {}
    if ref is None:
        names = set(git(root, "ls-files", "--cached", "--others", "--exclude-standard", "-z").decode("utf-8").split("\0"))
        total = 0
        for name in sorted(n for n in names if n and selected(n, profile, managed)):
            safe_relative(name)
            path = root / name
            if path.is_symlink() or any(p.is_symlink() for p in path.parents if p != root):
                raise ValueError(f"Symlink source is unsupported: {name}")
            if not path.exists():  # Tracked deletion participates by absence.
                continue
            if root not in path.resolve().parents or not path.is_file():
                raise ValueError(f"Source must be a repository-contained regular file: {name}")
            size = path.stat().st_size
            total += size
            if size > MAX_FILE or total > MAX_SOURCE or len(files) >= 20000:
                raise ValueError(f"Source file exceeds size limit: {name}")
            files[name] = path.read_bytes()
    else:
        entries = []
        for item in git(root, "ls-tree", "-r", "-z", "--full-tree", ref).split(b"\0"):
            if not item:
                continue
            meta, raw_name = item.split(b"\t", 1)
            name = raw_name.decode("utf-8")
            if not selected(name, profile, managed):
                continue
            safe_relative(name)
            mode, kind, oid = meta.decode("ascii").split()
            if mode not in {"100644", "100755"} or kind != "blob":
                raise ValueError(f"Unsupported source tree entry: {name}")
            entries.append((name, oid))
        if len(entries) > 20000:
            raise ValueError("App source exceeds inventory limits")
        requests = ("".join(oid + "\n" for _, oid in entries)).encode("ascii")
        sizes = git(root, "cat-file", "--batch-check", data=requests).splitlines()
        if len(sizes) != len(entries):
            raise ValueError("Incomplete Git source inventory")
        lengths = [int(row.split()[2]) for row in sizes]
        if any(n > MAX_FILE for n in lengths) or sum(lengths) > MAX_SOURCE:
            raise ValueError("Git source exceeds size limits")
        content = git(root, "cat-file", "--batch", data=requests)
        offset = 0
        for (name, _), length in zip(entries, lengths):
            offset = content.index(b"\n", offset) + 1
            files[name] = content[offset:offset + length]
            offset += length + 1
    if len(files) > 20000 or sum(map(len, files.values())) > MAX_SOURCE:
        raise ValueError("App source exceeds inventory limits")
    texts = {}
    for name, key in managed.items():
        if name not in files:
            raise ValueError(f"Managed file missing from source snapshot: {name}")
        texts[key] = files[name].decode("utf-8")
    versions = setter.read_versions(paths, texts)
    for key, value in normalize(paths, texts).items():
        files[paths[key].relative_to(root).as_posix()] = value.encode("utf-8")
    digest = hashlib.sha256()
    for name, data in sorted(files.items()):
        # Canonical text line endings keep Windows checkout and committed content comparable.
        if b"\0" not in data:
            try:
                data.decode("utf-8")
                data = data.replace(b"\r\n", b"\n")
            except UnicodeDecodeError:
                pass
        name_bytes = name.encode("utf-8")
        digest.update(len(name_bytes).to_bytes(8, "big") + name_bytes)
        digest.update(hashlib.sha256(data).digest())
    return digest.hexdigest(), versions


@contextlib.contextmanager
def locked(path):
    path.parent.mkdir(parents=True, exist_ok=True)
    with path.with_suffix(".lock").open("a+b") as stream:
        if stream.tell() == 0:
            stream.write(b"\0")
            stream.flush()
        deadline = time.monotonic() + 15
        acquired = False
        try:
            while not acquired:
                try:
                    stream.seek(0)
                    if os.name == "nt":
                        import msvcrt
                        msvcrt.locking(stream.fileno(), msvcrt.LK_NBLCK, 1)
                    else:
                        import fcntl
                        fcntl.flock(stream.fileno(), fcntl.LOCK_EX | fcntl.LOCK_NB)
                    acquired = True
                except OSError:
                    if time.monotonic() >= deadline:
                        raise ValueError("Version preparation is busy; retry after the current operation")
                    time.sleep(0.05)
            yield
        finally:
            if acquired:
                stream.seek(0)
                if os.name == "nt":
                    import msvcrt
                    msvcrt.locking(stream.fileno(), msvcrt.LK_UNLCK, 1)
                else:
                    import fcntl
                    fcntl.flock(stream.fileno(), fcntl.LOCK_UN)


def save(path, state):
    payload = json.dumps(state, sort_keys=True, indent=2) + "\n"
    if len(payload.encode("utf-8")) > 256 * 1024:
        raise ValueError("Version state exceeds size limit")
    fd, tmp = tempfile.mkstemp(dir=path.parent, prefix=path.name, suffix=".tmp")
    try:
        with os.fdopen(fd, "w", encoding="utf-8", newline="\n") as stream:
            stream.write(payload)
            stream.flush()
            os.fsync(stream.fileno())
        os.replace(tmp, path)
    finally:
        with contextlib.suppress(FileNotFoundError):
            os.unlink(tmp)


def validate_prepared(prepared):
    if not isinstance(prepared, dict):
        raise ValueError("Corrupt prepared version record")
    stable(prepared.get("version"))
    for key, size in (("token", 32), ("fingerprint", 64)):
        if not isinstance(prepared.get(key), str) or not re.fullmatch("[0-9a-f]{" + str(size) + "}", prepared[key]):
            raise ValueError(f"Corrupt prepared {key}")
    if any(type(prepared.get(k)) is not bool for k in ("allocated", "sealed")):
        raise ValueError("Corrupt preparation allocation/success state")


def load(ctx):
    path = ctx["state_path"]
    if not path.exists():
        return None
    if path.stat().st_size > 256 * 1024:
        raise ValueError("Version state exceeds size limit")
    try:
        state = json.loads(path.read_text(encoding="utf-8"))
        if state["schema_version"] != SCHEMA or state["identity"] != ctx["identity"]:
            raise ValueError("Version state schema or app identity differs")
        if not isinstance(state["profile"], list) or not state["profile"] or len(state["profile"]) > 128:
            raise ValueError("Corrupt source profile")
        for p in state["profile"]:
            safe_relative(p)
        if state["prepared"] is not None:
            validate_prepared(state["prepared"])
        if state["pending"] is not None:
            stable(state["pending"]["from_version"])
            validate_prepared(state["pending"]["prepared"])
            if stable(state["pending"]["prepared"]["version"]) < stable(state["pending"]["from_version"]) or state["pending"]["prepared"]["sealed"]:
                raise ValueError("Invalid recovery intent")
        for key in ("last_build", "last_push"):
            if state[key] is not None and not isinstance(state[key], dict):
                raise ValueError("Corrupt success receipt")
        if state["prepared"] is None and state["pending"] is None:
            raise ValueError("State has no preparation or recovery intent")
        return state
    except (KeyError, TypeError, json.JSONDecodeError) as exc:
        raise ValueError("Corrupt version state; reconcile it instead of resetting it") from exc


def commit_id(ctx, value):
    if not value or value.startswith("-"):
        raise ValueError("Expected a local commit/ref")
    return text_git(ctx["root"], "rev-parse", "--verify", "--end-of-options", value + "^{commit}")


def recover(ctx, state):
    pending = state["pending"]
    if pending is None:
        return
    proposed = pending["prepared"]
    fingerprint, versions = snapshot(ctx, state["profile"])
    allowed = {pending["from_version"], proposed["version"]}
    if fingerprint != proposed["fingerprint"] or not set(versions.values()) <= allowed:
        raise ValueError("Source or versions changed during an interrupted preparation; reconcile first")
    setter.set_versions(ctx["paths"], proposed["version"])
    after_fingerprint, after_versions = snapshot(ctx, state["profile"])
    if after_fingerprint != proposed["fingerprint"] or synchronized(after_versions) != proposed["version"]:
        raise ValueError("Source drift while recovering preparation; intent retained")
    state["prepared"], state["pending"] = proposed, None
    save(ctx["state_path"], state)


def output(ctx, state, action):
    return {"action": action, **state["prepared"], "state_path": str(ctx["state_path"]),
            "managed_paths": [str(p) for p in ctx["paths"].values()],
            "last_build": state["last_build"], "last_push": state["last_push"]}


def prepare(ctx, *, base=None, version=None, includes=None):
    if version is not None:
        stable(version)
    with locked(ctx["state_path"]):
        state = load(ctx)
        profile = profile_for(ctx, includes)
        if state is not None:
            if includes is not None and profile != state["profile"]:
                raise ValueError("Source profile changed; reconcile existing state before replacing it")
            profile = state["profile"]
            recover(ctx, state)
        fingerprint, versions = snapshot(ctx, profile)
        current = synchronized(versions)
        old = state["prepared"] if state else None
        if old:
            if current != old["version"] and version != current:
                raise ValueError("Declared version differs from prepared state; use an explicit reconciled target")
            if stable(current) < stable(old["version"]):
                raise ValueError("Declared version is older than prepared state")
            changed = fingerprint != old["fingerprint"]
            minimum = next_patch(old["version"]) if changed and (old["sealed"] or not old["allocated"]) else old["version"]
            target = max((current, minimum), key=stable)
            allocated = old["allocated"] or changed or current != old["version"]
            action = "bumped" if target != current else ("updated_preparation" if changed else "reused")
        else:
            if base is None:
                raise ValueError("First use requires --base with a verified local baseline; missing state proves no history")
            baseline = commit_id(ctx, base)
            base_fingerprint, base_versions = snapshot(ctx, profile, baseline)
            base_version = synchronized(base_versions)
            if stable(current) < stable(base_version):
                raise ValueError("Declared version is older than the baseline")
            changed = fingerprint != base_fingerprint
            allocated = current != base_version or changed
            target = current if current != base_version or not changed else next_patch(current)
            action = "adopted" if current != base_version else ("bumped" if changed else "baseline")
            state = {"schema_version": SCHEMA, "identity": ctx["identity"], "profile": profile,
                     "prepared": None, "pending": None, "last_build": None, "last_push": None}
        if version is not None:
            if stable(version) < stable(current) or stable(version) < stable(target):
                raise ValueError("Explicit version would downgrade or reuse a completed version for new source")
            target, allocated = version, True
            if target != current:
                action = "explicit"
        same = old and old["fingerprint"] == fingerprint and old["version"] == target and old["allocated"] == allocated
        if same:
            return output(ctx, state, "reused")
        proposed = {"token": uuid.uuid4().hex, "fingerprint": fingerprint, "version": target,
                    "allocated": bool(allocated), "sealed": False}
        state["pending"] = {"from_version": current, "prepared": proposed}
        save(ctx["state_path"], state)  # Must precede every managed file write.
        setter.set_versions(ctx["paths"], target)
        after_fingerprint, after_versions = snapshot(ctx, profile)
        if after_fingerprint != fingerprint or synchronized(after_versions) != target:
            raise ValueError("Source drift while preparing version; saved intent retained for reconciliation")
        state["prepared"], state["pending"] = proposed, None
        save(ctx["state_path"], state)
        return output(ctx, state, action)


def hash_file(path):
    digest = hashlib.sha256()
    with path.open("rb") as stream:
        for chunk in iter(lambda: stream.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def record(ctx, *, token, artifacts=None, commit=None, remote_head=None):
    with locked(ctx["state_path"]):
        state = load(ctx)
        if state is None or state["pending"] is not None or state["prepared"]["token"] != token:
            raise ValueError("Missing, interrupted or stale preparation token")
        prepared = state["prepared"]
        fingerprint, versions = snapshot(ctx, state["profile"])
        if fingerprint != prepared["fingerprint"] or synchronized(versions) != prepared["version"]:
            raise ValueError("Source/version drift since preparation; success cannot be recorded")
        receipt = {k: prepared[k] for k in ("token", "fingerprint", "version")}
        if artifacts is not None:
            if not artifacts or len(artifacts) > 64:
                raise ValueError("Supply between one and 64 verified artifacts")
            receipt["artifacts"] = {}
            for item in artifacts:
                path = Path(item).resolve(strict=True)
                if not path.is_file():
                    raise ValueError(f"Not an artifact file: {path}")
                receipt["artifacts"][str(path)] = {"sha256": hash_file(path), "bytes": path.stat().st_size}
            kind = "last_build"
        else:
            sha = commit_id(ctx, commit)
            if not isinstance(remote_head, str) or not re.fullmatch(r"[0-9a-f]{40}|[0-9a-f]{64}", remote_head) or sha != remote_head:
                raise ValueError("Observed remote SHA must exactly equal the delivered commit")
            pushed_fingerprint, pushed_versions = snapshot(ctx, state["profile"], sha)
            if pushed_fingerprint != fingerprint or synchronized(pushed_versions) != prepared["version"]:
                raise ValueError("Pushed commit source/version does not match preparation")
            receipt.update(commit=sha, observed_remote_head=remote_head)
            kind = "last_push"
        # Check again after artifact hashing/commit reads; those can take time.
        final_fingerprint, final_versions = snapshot(ctx, state["profile"])
        if final_fingerprint != fingerprint or synchronized(final_versions) != prepared["version"]:
            raise ValueError("Source drift during receipt verification")
        state[kind], prepared["sealed"] = receipt, True
        save(ctx["state_path"], state)
        return {"action": kind.removeprefix("last_") + "_recorded", **receipt}


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    sub = parser.add_subparsers(dest="command", required=True)
    for name in ("prepare", "status", "record-build", "record-push"):
        cmd = sub.add_parser(name)
        cmd.add_argument("--repo", required=True, type=Path)
        cmd.add_argument("--state-dir", type=Path)
        if name == "prepare":
            cmd.add_argument("--base")
            cmd.add_argument("--version")
            cmd.add_argument("--include", action="append")
        if name.startswith("record-"):
            cmd.add_argument("--token", required=True)
        if name == "record-build":
            cmd.add_argument("--artifact", action="append", required=True)
        if name == "record-push":
            cmd.add_argument("--commit", required=True)
            cmd.add_argument("--remote-head", required=True)
    args = parser.parse_args(argv)
    try:
        ctx = context(args.repo, args.state_dir)
        if args.command == "prepare":
            result = prepare(ctx, base=args.base, version=args.version, includes=args.include)
        elif args.command == "record-build":
            result = record(ctx, token=args.token, artifacts=args.artifact)
        elif args.command == "record-push":
            result = record(ctx, token=args.token, commit=args.commit, remote_head=args.remote_head)
        else:
            state = load(ctx)
            result = {"state_path": str(ctx["state_path"]), "state": state}
            if state:
                fingerprint, versions = snapshot(ctx, state["profile"])
                result.update(current_fingerprint=fingerprint, current_versions=versions,
                              source_matches=state["prepared"] is not None and fingerprint == state["prepared"]["fingerprint"],
                              version_matches=state["prepared"] is not None and set(versions.values()) == {state["prepared"]["version"]})
                for kind, field in (("build", "built_current_source"), ("push", "pushed_current_source")):
                    receipt = state["last_" + kind]
                    result[field] = bool(receipt and state["pending"] is None and
                        receipt.get("fingerprint") == fingerprint and set(versions.values()) == {receipt.get("version")})
        print(json.dumps(result, indent=2, sort_keys=True))
        return 0
    except (ValueError, OSError, subprocess.SubprocessError, UnicodeError) as exc:
        print(json.dumps({"error": str(exc)}), file=sys.stderr)
        return 2


if __name__ == "__main__":
    raise SystemExit(main())
