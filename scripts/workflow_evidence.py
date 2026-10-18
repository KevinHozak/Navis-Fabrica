#!/usr/bin/env python3
"""Local evidence/checkpoint CLI. No network, dependency installation, or Git mutations."""
from __future__ import annotations

import argparse
import base64
import hashlib
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import sys
import tempfile
from datetime import datetime, timezone

SCHEMA = 1
CONTRACT = "1.0.0"
CAPSULE_SCHEMA = 1
CAPSULE_LIMIT = 16384
CAPSULE_CODES = {
    "invalid_json", "schema_mismatch", "duplicate_output", "timeout", "truncated",
    "permission", "authentication", "transport", "dependency", "product",
}
EXCLUDE = {".git", ".worktrees", "node_modules", "target", ".venv", "__pycache__"}
SECRET = re.compile(r"(?i)(bearer\s+\S+|(?:api[_-]?key|token|password|secret)\s*[=:]\s*[^\s,;]+)")


def now():
    return datetime.now(timezone.utc).isoformat()


def digest(data):
    return hashlib.sha256(data).hexdigest()


def redact(value):
    if isinstance(value, str):
        return SECRET.sub("[REDACTED]", value)
    if isinstance(value, list):
        return [redact(x) for x in value]
    if isinstance(value, dict):
        return {k: ("[REDACTED]" if re.fullmatch(r"(?i)(api[_-]?key|token|password|secret)", k)
                    else redact(v)) for k, v in value.items()}
    return value


def git(repo, *args, required=True):
    env = dict(os.environ, GIT_OPTIONAL_LOCKS="0", GIT_TERMINAL_PROMPT="0")
    proc = subprocess.run(["git", "-c", "core.fsmonitor=false", "-C", str(repo), *args],
                          capture_output=True, env=env, timeout=30)
    if required and proc.returncode:
        raise ValueError(redact(proc.stderr.decode("utf-8", "replace").strip()))
    return proc.stdout.decode("utf-8", "replace").strip() if proc.returncode == 0 else None


def repository(path):
    path = Path(path).resolve()
    root = git(path, "rev-parse", "--show-toplevel")
    return Path(root).resolve()


def fingerprint(repo):
    names = git(repo, "ls-files", "-z", "--cached", "--others", "--exclude-standard")
    h = hashlib.sha256()
    for name in sorted(set(names.split("\0"))):
        if not name:
            continue
        path = repo / name
        h.update(name.encode("utf-8"))
        h.update(b"\0")
        if path.is_symlink():
            data = b"link:" + os.readlink(path).encode("utf-8")
        elif path.is_file():
            data = path.read_bytes()
        elif path.is_dir():
            data = b"gitlink:" + (git(path, "rev-parse", "HEAD", required=False) or "unknown").encode()
            data += (git(path, "status", "--porcelain", required=False) or "").encode()
        else:
            data = b"deleted"
        h.update(digest(data).encode())
    return h.hexdigest()


def identity(repo):
    return {"path": str(repo), "head": git(repo, "rev-parse", "HEAD", required=False),
            "fingerprint": fingerprint(repo)}


def classify(error):
    text = error.lower()
    if any(x in text for x in ("index.lock", "fetch_head", "permission denied", "access is denied")):
        return "permission"
    if any(x in text for x in ("authentication failed", "bad credentials", "401", "403")):
        return "authentication"
    if any(x in text for x in ("getaddrinfo", "could not resolve", "proxy", "connection refused", "timed out")):
        return "transport"
    if any(x in text for x in ("tsc", "node_modules", "enotcached", "exit handler never called", "module not found")):
        return "dependency"
    return "product"


def manifests(repo):
    found = []
    for root, dirs, files in os.walk(repo, followlinks=False):
        dirs[:] = sorted(d for d in dirs if d not in EXCLUDE and not Path(root, d).is_symlink())
        if len(Path(root).relative_to(repo).parts) > 5:
            dirs[:] = []
            continue
        for name in ("package.json", "Cargo.toml", "pyproject.toml", "pytest.ini", "go.mod"):
            if name in files:
                found.append(Path(root, name))
    return found


def preflight(repo, issue=None, base=None):
    checks, locks, packages = [], [], []
    for path in manifests(repo):
        rel = path.relative_to(repo).as_posix()
        cwd = str(path.parent)
        if path.name == "package.json":
            data = json.loads(path.read_text(encoding="utf-8-sig"))
            lock = path.with_name("package-lock.json")
            packages.append({"manifest": rel, "node_modules_present": path.with_name("node_modules").is_dir(),
                             "package_manager": data.get("packageManager"), "engines": data.get("engines", {}),
                             "install": ["npm", "ci"] if lock.exists() else None})
            for name in ("lint", "typecheck", "test", "build", "release:verify"):
                if name in data.get("scripts", {}):
                    checks.append({"id": f"{rel}:{name}", "cwd": cwd, "argv": ["npm", "run", name]})
        elif path.name == "Cargo.toml":
            checks.append({"id": f"{rel}:test", "cwd": str(repo),
                           "argv": ["cargo", "test", "--locked", "--manifest-path", rel]})
        elif path.name == "go.mod":
            checks.append({"id": f"{rel}:test", "cwd": cwd, "argv": ["go", "test", "./..."]})
        else:
            checks.append({"id": f"{rel}:test", "cwd": cwd, "argv": [sys.executable, "-m", "pytest"]})
        for name in ("package-lock.json", "Cargo.lock", "uv.lock", "poetry.lock", "go.sum"):
            lock = path.with_name(name)
            if lock.exists() and str(lock) not in [x["path"] for x in locks]:
                locks.append({"path": str(lock), "sha256": digest(lock.read_bytes())})
    tools = {}
    for tool in ("git", "python", "node", "npm", "cargo", "rustc"):
        exe = shutil.which(tool)
        item = {"path": exe, "version": None}
        # cmd/ps1 need an explicit, separately verified host: never shell=True.
        if exe and Path(exe).suffix.lower() not in {".cmd", ".bat", ".ps1"}:
            try:
                result = subprocess.run([exe, "--version"], capture_output=True, timeout=10)
                item["version"] = result.stdout.decode("utf-8", "replace").strip() if result.returncode == 0 else None
            except (OSError, subprocess.TimeoutExpired):
                pass
        tools[tool] = item
    default_ref = git(repo, "symbolic-ref", "refs/remotes/origin/HEAD", required=False)
    return {"schema_version": SCHEMA, "contract_version": CONTRACT, "at": now(), "repository": identity(repo),
            "branch": git(repo, "branch", "--show-current"), "status": git(repo, "status", "--porcelain=v1"),
            "git_dir": git(repo, "rev-parse", "--absolute-git-dir"),
            "common_git_dir": git(repo, "rev-parse", "--path-format=absolute", "--git-common-dir"),
            "base": {"ref": base or default_ref,
                     "sha": git(repo, "rev-parse", "--verify", base or default_ref, required=False) if base or default_ref else None},
            "worktrees": git(repo, "worktree", "list", "--porcelain"),
            "issue": issue, "issue_refs": git(repo, "for-each-ref", "--format=%(refname)", "refs/heads", "refs/remotes") if issue else None,
            "remote_state": "not queried; use GitHub connector and record observed state before publication",
            "tools": tools, "packages": packages, "lockfiles": locks, "suggested_checks": checks}


def load(path):
    return json.loads(Path(path).read_text(encoding="utf-8-sig"))


def save(path, data):
    path = Path(path)
    path.parent.mkdir(parents=True, exist_ok=True)
    fd, tmp = tempfile.mkstemp(prefix=path.name + ".", dir=path.parent)
    try:
        with os.fdopen(fd, "w", encoding="utf-8", newline="\n") as handle:
            json.dump(redact(data), handle, indent=2, ensure_ascii=False)
            handle.write("\n")
        os.replace(tmp, path)
    finally:
        if os.path.exists(tmp):
            os.unlink(tmp)


def validate_run(data, repo):
    if data.get("schema_version") != SCHEMA or data.get("contract_version") != CONTRACT:
        raise ValueError("Unsupported record/contract version; migrate explicitly")
    if Path(data["repository"]["path"]).resolve() != repo:
        raise ValueError("Run belongs to another repository")


def new_run(repo, spec):
    return {"schema_version": SCHEMA, "contract_version": CONTRACT, "created_at": now(),
            "repository": identity(repo), "issue": spec.get("issue"), "base": spec.get("base"),
            "branch": git(repo, "branch", "--show-current"), "workflow_versions": spec.get("workflow_versions", {}),
            "criteria": spec.get("criteria", []), "invariants": spec.get("invariants", []),
            "required_checks": spec.get("required_checks", []), "checks": {}, "started_checks": {},
            "review": {"status": "pending", "head": None, "actionable": None},
            "repair_rounds": 0, "recoveries": {}, "effects": {}, "remote": None,
            "next_action": spec.get("next_action", "preflight"), "history": []}


def readiness(data, repo):
    current = identity(repo)
    reasons = []
    if not data["required_checks"]:
        reasons.append("required checks not declared")
    for key in data["required_checks"]:
        result = data["checks"].get(key)
        if not result or result["status"] != "passed":
            reasons.append(f"check {key} missing or not passed")
        elif result["fingerprint"] != current["fingerprint"]:
            reasons.append(f"check {key} stale")
        else:
            evidence = result.get("evidence") or {}
            path = Path(evidence.get("path", ""))
            if not path.is_file() or digest(path.read_bytes()) != evidence.get("sha256"):
                reasons.append(f"check {key} evidence missing or changed")
    for item in data["criteria"] + data["invariants"]:
        if item.get("status") != "passed" or not item.get("evidence") or item.get("fingerprint") != current["fingerprint"]:
            reasons.append(f"criterion/invariant {item.get('id', '?')} unverified or deferred")
    review = data["review"]
    if review.get("status") != "complete" or review.get("head") != current["head"] or review.get("actionable") != 0:
        reasons.append("current-head review pending or actionable findings remain")
    remote = data.get("remote") or {}
    if remote.get("head") != current["head"] or remote.get("state") != "open":
        reasons.append("open PR/current remote head not reconciled")
    if git(repo, "status", "--porcelain"):
        reasons.append("working tree has changes; reconcile before readiness")
    return {"ready": not reasons, "status": "ready" if not reasons else "draft", "reasons": reasons,
            "repair_rounds": data["repair_rounds"], "next_action": data["next_action"],
            "repository": current, "remote": remote, "effects": data["effects"]}


def failure_capsule(data, stamp, value):
    """Capture metadata on an existing run; never retain provider response text.

    This is an observation, not a runtime dispatcher, retry, billing reconciler,
    or independent reviewer. All validation precedes mutation of the run.
    """
    allowed = {"id", "attempt_id", "stage", "diagnostic_code", "parser", "response",
               "usage_reservations", "synthetic_reproduction", "next_action"}
    if not isinstance(value, dict) or set(value) - allowed:
        raise ValueError("Unknown failure-capsule fields; raw response content is not accepted")
    if len(json.dumps(value, ensure_ascii=False).encode("utf-8")) > CAPSULE_LIMIT:
        raise ValueError("Failure capsule exceeds 16384 bytes")

    def identifier(item):
        if not isinstance(item, str) or not re.fullmatch(r"[A-Za-z0-9][A-Za-z0-9_.:-]{0,127}", item):
            raise ValueError("Capsule identifiers must be 1-128 ASCII identifier characters")
        return item

    key, attempt = identifier(value.get("id")), identifier(value.get("attempt_id"))
    if key in data.get("failure_capsules", {}):
        raise ValueError("Failure capsule already recorded; preserve the original observation")
    stage = value.get("stage")
    if not isinstance(stage, str) or stage not in {"preflight", "implementation", "check", "review", "publication"}:
        raise ValueError("Invalid failure stage")
    code = value.get("diagnostic_code")
    if not isinstance(code, str) or code not in CAPSULE_CODES:
        raise ValueError("Invalid diagnostic code; use a bounded code, not raw error text")
    next_action = value.get("next_action")
    if not isinstance(next_action, str) or next_action not in {"inspect-protocol", "inspect-environment", "repair-product",
                           "wait-for-user", "reconcile-remote"}:
        raise ValueError("Invalid failure next action")
    parser = value.get("parser")
    if parser is not None:
        if not isinstance(parser, dict) or set(parser) != {"name", "version", "schema"}:
            raise ValueError("Parser needs name, version and schema identifiers")
        parser = {name: identifier(item) for name, item in parser.items()}
    if stage == "review" and parser is None:
        raise ValueError("Review failure needs parser/schema identifiers")
    response = value.get("response")
    if not isinstance(response, dict) or set(response) - {"state", "sha256", "size_bytes"}:
        raise ValueError("Response accepts only state, sha256 and size_bytes metadata")
    if not isinstance(response.get("state"), str) or response["state"] not in {"malformed", "schema_mismatch", "truncated", "timeout", "unavailable"}:
        raise ValueError("Invalid failed response state")
    if "sha256" in response and (not isinstance(response["sha256"], str)
                                  or not re.fullmatch(r"[a-f0-9]{64}", response["sha256"])):
        raise ValueError("Response sha256 must be a lowercase SHA-256 digest")
    if "size_bytes" in response and (type(response["size_bytes"]) is not int or response["size_bytes"] < 0):
        raise ValueError("Response size_bytes must be a nonnegative integer")
    reservations = value.get("usage_reservations")
    if not isinstance(reservations, list) or len(reservations) > 32:
        raise ValueError("Declare at most 32 retained usage reservations (or an empty list)")
    seen = set()
    for item in reservations:
        if (not isinstance(item, dict) or set(item) != {"id", "state"}
                or not isinstance(item["state"], str) or item["state"] not in {"reserved", "unknown"}):
            raise ValueError("Reservations accept only id and reserved/unknown state; never release holds")
        reservation = identifier(item["id"])
        if reservation in seen:
            raise ValueError("Duplicate reservation ID")
        seen.add(reservation)
    reproduction = None
    if "synthetic_reproduction" in value:
        if not isinstance(value["synthetic_reproduction"], str) or not value["synthetic_reproduction"]:
            raise ValueError("Synthetic reproduction must name an existing synthetic file")
        path = Path(value["synthetic_reproduction"]).resolve(strict=True)
        if not path.is_file() or path.stat().st_size > CAPSULE_LIMIT:
            raise ValueError("Synthetic reproduction must be a file of at most 16384 bytes")
        # Hash only. The caller must supply synthetic, non-sensitive fixture data.
        reproduction = {"path": str(path), "sha256": digest(path.read_bytes())}
    capsule = {
        "schema_version": CAPSULE_SCHEMA, "id": key, "attempt_id": attempt,
        "at": now(), "stage": stage, "diagnostic_code": code,
        "source": dict(stamp), "parser": parser, "response": dict(response),
        "response_content_retained": False,
        "capture_limit": "Body omitted; metadata alone cannot establish the exact response or root cause.",
        "checks": {name: {field: result.get(field) for field in ("status", "fingerprint", "head")}
                   for name, result in data["checks"].items()},
        "usage_reservations": [dict(item) for item in reservations],
        "synthetic_reproduction": reproduction, "next_action": next_action,
    }
    if stage == "review":
        # Preserve only the earlier disposition, never arbitrary review text.
        capsule["previous_review"] = {field: data["review"].get(field)
                                      for field in ("status", "head", "actionable")}
    if len(json.dumps(capsule, ensure_ascii=False).encode("utf-8")) > 2 * CAPSULE_LIMIT:
        raise ValueError("Failure capsule including check metadata exceeds 32768 bytes")
    data.setdefault("failure_capsules", {})[key] = capsule
    if stage == "review":
        # A failed new review supersedes an older pass without erasing its observation.
        data["review"] = {"status": "unavailable", "head": stamp["head"],
                          "actionable": None, "failure_capsule": key}
    data["next_action"] = next_action


def checkpoint(data, repo, action, value):
    stamp = identity(repo)
    if action == "begin-check":
        data["started_checks"][value["id"]] = {**stamp, "at": now()}
    elif action == "review":
        if value.get("status") not in {"pending", "complete", "unavailable"}:
            raise ValueError("Invalid review status")
        data["review"] = value
    elif action == "repair":
        if data["repair_rounds"] >= 2:
            raise ValueError("Two repair rounds exhausted")
        data["repair_rounds"] += 1
    elif action == "recovery":
        category = classify(value["error"])
        if data["recoveries"].get(category):
            raise ValueError(f"Recovery for {category} already attempted; report blocker")
        data["recoveries"][category] = {"at": now(), "error": redact(value["error"]), "remedy": value["remedy"]}
    elif action == "failure-capsule":
        failure_capsule(data, stamp, value)
    elif action == "remote":
        if not value.get("head") or not value.get("url") or value.get("state") not in {"open", "closed", "merged"}:
            raise ValueError("Remote observation needs head, URL, and state")
        data["remote"] = {**value, "at": now()}
    elif action == "effect":
        key = value["id"]
        if key in data["effects"]:
            raise ValueError("Side effect already recorded; reconcile before repeating")
        if value.get("kind") in {"push", "pr"} and not data.get("remote"):
            raise ValueError("Record remote lookup (including absent result) before remote side effects")
        data["effects"][key] = {**value, "at": now(), "head": stamp["head"]}
    elif action == "lookup":
        # An absent PR lookup is useful reconciliation evidence; not proof of readiness.
        data["remote"] = {**value, "at": now()}
    elif action == "acceptance":
        bucket = value.get("kind", "criteria")
        if bucket not in {"criteria", "invariants"} or value.get("status") not in {"passed", "failed", "pending", "deferred"}:
            raise ValueError("Invalid acceptance kind/status")
        item = next((x for x in data[bucket] if x["id"] == value["id"]), None)
        if item is None:
            raise ValueError("Unknown criterion/invariant")
        if value["status"] == "passed" and not value.get("evidence"):
            raise ValueError("Passing acceptance needs observed evidence")
        if value["status"] == "deferred" and not value.get("user_authorization"):
            raise ValueError("Deferral needs explicit user authorization")
        item.update(value, fingerprint=stamp["fingerprint"])
    elif action == "next":
        data["next_action"] = value["next_action"]
    else:
        raise ValueError("Unknown checkpoint action")
    data["history"].append({"action": action, "at": now(), "head": stamp["head"]})


def record_check(data, repo, value):
    key = value["id"]
    start = data["started_checks"].get(key)
    if not start:
        raise ValueError("begin-check must be recorded before execution")
    current = identity(repo)
    if start["fingerprint"] != current["fingerprint"]:
        raise ValueError("Source changed during check; rerun against final source")
    status = value["status"]
    if status not in {"passed", "failed", "blocked", "pending", "skipped"}:
        raise ValueError("Invalid check status")
    if status == "passed" and (value.get("exit_code") != 0 or not value.get("argv") or not value.get("evidence")):
        raise ValueError("Passing check needs argv, exit_code=0, and evidence file")
    if status == "passed" and (not value.get("environment") or not isinstance(value.get("duration_seconds"), (float, int))):
        raise ValueError("Passing check needs environment and observed duration_seconds")
    evidence = None
    if value.get("evidence"):
        path = Path(value["evidence"]).resolve(strict=True)
        if path.stat().st_mtime + 2 < datetime.fromisoformat(start["at"]).timestamp():
            raise ValueError("Evidence predates check start; observe fresh completion")
        evidence = {"path": str(path), "sha256": digest(path.read_bytes())}
    data["checks"][key] = {**value, "evidence": evidence, "fingerprint": current["fingerprint"],
                           "head": current["head"], "started_at": start["at"], "finished_at": now()}
    data["started_checks"].pop(key)


def snapshot(paths):
    result = {}
    for name in paths:
        path = Path(name).absolute()
        if path.is_symlink() or (path.exists() and not path.is_file()):
            raise ValueError("Snapshot targets must be regular files or absent files")
        payload = path.read_bytes() if path.exists() else None
        result[str(path)] = {"before": base64.b64encode(payload).decode() if payload is not None else None,
                             "sha256": digest(payload) if payload is not None else None}
    return result


def cycle_decision(value):
    if value.get("evidence_sufficient") is not True:
        return "insufficient-evidence"
    baseline, candidate = value.get("baseline", {}), value.get("candidate", {})
    targets, regressions, held_out = value.get("targets", []), value.get("regressions", []), value.get("held_out", [])
    if not targets or not regressions or not held_out or (set(targets + regressions) & set(held_out)):
        return "insufficient-evidence"
    required = set(targets + regressions + held_out)
    if any(key not in baseline or key not in candidate for key in required):
        return "insufficient-evidence"
    if any(candidate[key] is not True for key in targets + regressions + held_out):
        return "revert"
    if any(baseline[key] is False and candidate[key] is True for key in targets):
        return "retain"
    return "revert"


def cycle_operation(path, action, value):
    if action == "snapshot":
        if path.exists():
            raise ValueError("Cycle exists; never overwrite a baseline")
        data = {"schema_version": SCHEMA, "created_at": now(), "files": snapshot(value["paths"]),
                "workflow_versions": value.get("workflow_versions", {}), "sources": value.get("sources", []),
                "hypotheses": value.get("hypotheses", []), "attempts": [], "unresolved": [], "status": "baseline"}
        if len(data["hypotheses"]) > 3:
            raise ValueError("Select at most three related improvements")
    else:
        data = load(path)
        if data.get("schema_version") != SCHEMA:
            raise ValueError("Unsupported cycle record")
        if action == "candidate":
            if len(data["attempts"]) >= 2:
                raise ValueError("Two candidate revisions exhausted")
            for name, item in data["files"].items():
                target = Path(name)
                if target.is_symlink():
                    raise ValueError("Target became a symlink")
                item["candidate_sha256"] = digest(target.read_bytes()) if target.exists() else None
            decision = cycle_decision(value)
            data["attempts"].append({"at": now(), "evaluation": value, "decision": decision})
            data["status"] = decision
        elif action == "rollback":
            # Validate EVERY target before restoring ANY; refuse concurrent edits.
            for name, item in data["files"].items():
                target = Path(name)
                if "candidate_sha256" not in item:
                    raise ValueError("Record candidate before rollback")
                current = digest(target.read_bytes()) if target.is_file() else None
                if target.is_symlink() or current != item["candidate_sha256"]:
                    raise ValueError(f"Concurrent edit at {name}; reconcile manually")
            for name, item in data["files"].items():
                target = Path(name)
                if item["before"] is None:
                    if target.exists():
                        target.unlink()
                else:
                    target.parent.mkdir(parents=True, exist_ok=True)
                    target.write_bytes(base64.b64decode(item["before"]))
            data["status"] = "rolled-back"
        else:
            raise ValueError("Unknown cycle action")
    # Snapshots preserve exact bytes; these are private local artifacts, not redacted cycle summaries.
    save(path, data)
    return {"status": data["status"], "attempts": len(data["attempts"])}


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    sub = parser.add_subparsers(dest="command", required=True)
    pre = sub.add_parser("preflight")
    pre.add_argument("--repo", required=True)
    pre.add_argument("--issue")
    pre.add_argument("--base")
    for name in ("checkpoint", "record-check", "report"):
        cmd = sub.add_parser(name)
        cmd.add_argument("--repo", required=True)
        cmd.add_argument("--record", type=Path, required=True)
        if name != "report":
            cmd.add_argument("--input", type=Path, required=True)
        if name == "checkpoint":
            cmd.add_argument("--action", required=True,
                             choices=("init", "begin-check", "review", "repair", "recovery", "failure-capsule", "remote", "lookup", "effect", "acceptance", "next"))
    cyc = sub.add_parser("cycle")
    cyc.add_argument("--record", type=Path, required=True)
    cyc.add_argument("--action", required=True, choices=("snapshot", "candidate", "rollback"))
    cyc.add_argument("--input", type=Path)
    args = parser.parse_args(argv)
    try:
        if args.command == "cycle":
            result = cycle_operation(args.record.resolve(), args.action, load(args.input) if args.input else {})
        else:
            repo = repository(args.repo)
            if args.command == "preflight":
                result = preflight(repo, args.issue, args.base)
            else:
                record = args.record.resolve()
                if record.is_relative_to(repo):
                    raise ValueError("Store evidence outside the repository to avoid fingerprint feedback")
                if args.command == "checkpoint" and args.action == "init":
                    if record.exists():
                        raise ValueError("Record already exists; resume instead")
                    data = new_run(repo, load(args.input))
                else:
                    data = load(record)
                    validate_run(data, repo)
                    if args.command == "checkpoint":
                        checkpoint(data, repo, args.action, load(args.input))
                    elif args.command == "record-check":
                        record_check(data, repo, load(args.input))
                if args.command != "report":
                    save(record, data)
                result = readiness(data, repo)
        print(json.dumps(redact(result), indent=2, ensure_ascii=False))
        return 0
    except (ValueError, KeyError, OSError, subprocess.SubprocessError) as exc:
        print(json.dumps({"error": redact(str(exc)), "class": classify(str(exc))}), file=sys.stderr)
        return 2


if __name__ == "__main__":
    raise SystemExit(main())
