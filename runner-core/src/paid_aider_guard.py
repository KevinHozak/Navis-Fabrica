#!/usr/bin/env python3
"""Fail-closed, runner-owned entry point for the pinned Aider paid adapter."""

from __future__ import annotations

import argparse
import importlib.metadata
import json
import os
from pathlib import Path
import stat
import sys
from typing import Iterable

EXPECTED_DISTRIBUTION = "aider-chat"
EXPECTED_VERSION = "0.86.2"
MAX_ALLOWED_FILES = 128
MAX_ALLOWLIST_BYTES = 64 * 1024


class GuardRefusal(RuntimeError):
    pass


def _task_path_form(path: Path) -> Path:
    """Normalize Windows verbatim aliases without resolving filesystem links."""
    if os.name != "nt":
        return path
    raw = str(path)
    if raw.startswith("\\\\?\\UNC\\"):
        return Path("\\\\" + raw[8:])
    if raw.startswith("\\\\?\\"):
        return Path(raw[4:])
    return path


def _under(path: Path, root: Path) -> bool:
    try:
        path = _task_path_form(path)
        root = _task_path_form(root)
        path.relative_to(root)
        return True
    except ValueError:
        return False


def _install_facts() -> dict[str, object]:
    """Verify the running venv owns the exact Aider distribution; do not start it."""
    lexical_prefix = Path(sys.prefix).absolute()
    lexical_interpreter = Path(sys.executable).absolute()
    facts: dict[str, object] = {
        "distribution": EXPECTED_DISTRIBUTION,
        "expectedVersion": EXPECTED_VERSION,
        "interpreter": str(lexical_interpreter),
        "prefix": str(lexical_prefix),
    }
    try:
        prefix = Path(sys.prefix).resolve(strict=True)
        interpreter_real = Path(sys.executable).resolve(strict=True)
        version = importlib.metadata.version(EXPECTED_DISTRIBUTION)
        import aider

        package = Path(aider.__file__).resolve(strict=True)
        distribution = importlib.metadata.distribution(EXPECTED_DISTRIBUTION)
        metadata_package = Path(distribution.locate_file("aider/__init__.py")).resolve(strict=True)
        package_owned = _under(package, prefix)
        metadata_matches = metadata_package == package
        # POSIX virtualenvs commonly expose Python through a symlink in
        # <venv>/bin. Preserve that lexical launcher path for Rust to invoke;
        # the resolved target is reported separately but is not required to
        # live under the venv on POSIX. Windows keeps the stricter resolved
        # executable ownership check used by the original guard.
        if os.name == "nt":
            interpreter_owned = _under(interpreter_real, prefix)
        else:
            interpreter_owned = _under(lexical_interpreter, lexical_prefix)
        ok = (
            version == EXPECTED_VERSION
            and sys.prefix != sys.base_prefix
            and interpreter_owned
            and package_owned
            and metadata_matches
            and package.name == "__init__.py"
        )
        facts.update(
            {
                "ok": ok,
                "version": version,
                "interpreter": str(lexical_interpreter),
                "interpreterRealPath": str(interpreter_real),
                "interpreterRealPathWithinPrefix": _under(interpreter_real, prefix),
                "prefix": str(lexical_prefix),
                "resolvedPrefix": str(prefix),
                "packagePath": str(package),
                "packageWithinPrefix": package_owned,
                "interpreterWithinPrefix": interpreter_owned,
                "distributionPathMatchesPackage": metadata_matches,
                "basePrefix": str(Path(sys.base_prefix).absolute()),
            }
        )
    except Exception:
        facts.update({"ok": False, "reason": "Aider installation ownership could not be verified."})
    return facts


def _emit_probe() -> int:
    facts = _install_facts()
    sys.stdout.write(json.dumps(facts, ensure_ascii=False, sort_keys=True) + "\n")
    return 0 if facts.get("ok") is True else 2


def _parse_guard_argv(argv: list[str]) -> tuple[argparse.Namespace, list[str]]:
    parser = argparse.ArgumentParser(add_help=False)
    parser.add_argument("--probe", action="store_true")
    parser.add_argument("--worktree")
    parser.add_argument("--allowed-files-json")
    parser.add_argument("remainder", nargs=argparse.REMAINDER)
    args = parser.parse_args(argv)
    if args.probe:
        if args.worktree or args.allowed_files_json or args.remainder:
            raise GuardRefusal("probe mode does not accept task arguments")
        return args, []
    if not args.worktree or not args.allowed_files_json:
        raise GuardRefusal("worktree and allowlist paths are required")
    if not args.remainder or args.remainder[0] != "--":
        raise GuardRefusal("Aider arguments must follow the explicit separator")
    return args, args.remainder[1:]


def _canonical_root(raw_root: str) -> Path:
    try:
        root = _task_path_form(Path(raw_root).resolve(strict=True))
    except (OSError, RuntimeError) as exc:
        raise GuardRefusal("task worktree could not be resolved") from exc
    if not root.is_dir():
        raise GuardRefusal("task worktree is not a directory")
    return root


def _canonical_task_path(root: Path, raw_path: str, allow_missing: bool) -> Path | None:
    """Return an in-root canonical path, rejecting traversal and symlink components."""
    try:
        candidate = _task_path_form(Path(raw_path))
        if not candidate.is_absolute():
            candidate = root / candidate
        relative = candidate.relative_to(root)
        if not relative.parts or any(part in ("..", "") for part in relative.parts):
            return None

        cursor = root
        for part in relative.parts:
            cursor = cursor / part
            if _is_reparse_component(cursor):
                return None

        resolved = _task_path_form(candidate.resolve(strict=not allow_missing))
        if not _under(resolved, root) or resolved == root:
            return None
        if resolved.exists():
            if not resolved.is_file():
                return None
        elif not allow_missing:
            return None
        else:
            # Aider creates admitted new files itself. Preserve support for
            # nested new paths by validating the nearest existing ancestor,
            # rather than requiring the immediate parent to exist already.
            ancestor = resolved
            while not ancestor.exists():
                parent = ancestor.parent
                if parent == ancestor or not _under(parent, root):
                    return None
                ancestor = parent
            if not ancestor.is_dir():
                return None
            cursor = ancestor
            while cursor != root:
                if _is_reparse_component(cursor):
                    return None
                cursor = cursor.parent
        return resolved
    except (OSError, RuntimeError, ValueError):
        return None


def _is_reparse_component(path: Path) -> bool:
    try:
        if path.is_symlink():
            return True
        is_junction = getattr(path, "is_junction", None)
        if is_junction is not None and is_junction():
            return True
        attributes = getattr(path.lstat(), "st_file_attributes", 0)
        reparse_flag = getattr(stat, "FILE_ATTRIBUTE_REPARSE_POINT", 0x0400)
        return bool(attributes & reparse_flag)
    except FileNotFoundError:
        return False
    except OSError:
        return True


def _path_key(path: Path) -> str:
    return os.path.normcase(os.path.normpath(str(_task_path_form(path))))


def _load_allowlist(root: Path, path: str) -> set[str]:
    try:
        allowlist_path = Path(path).resolve(strict=True)
        raw = allowlist_path.read_bytes()
    except (OSError, RuntimeError) as exc:
        raise GuardRefusal("runner allowlist could not be read") from exc
    if len(raw) > MAX_ALLOWLIST_BYTES:
        raise GuardRefusal("runner allowlist exceeds its size bound")
    try:
        payload = json.loads(raw.decode("utf-8-sig"))
    except (UnicodeDecodeError, json.JSONDecodeError) as exc:
        raise GuardRefusal("runner allowlist is not valid UTF-8 JSON") from exc
    if not isinstance(payload, list) or not all(isinstance(item, str) and item for item in payload):
        raise GuardRefusal("runner allowlist must be a JSON array of file paths")
    if not payload or len(payload) > MAX_ALLOWED_FILES or len(set(payload)) != len(payload):
        raise GuardRefusal("runner allowlist is empty, duplicated, or outside its bound")

    allowed: set[str] = set()
    for item in payload:
        canonical = _canonical_task_path(root, item, allow_missing=True)
        if canonical is None:
            raise GuardRefusal("runner allowlist contains an unsafe task path")
        allowed.add(_path_key(canonical))
    if len(allowed) != len(payload):
        raise GuardRefusal("runner allowlist contains duplicate canonical paths")
    return allowed


def _as_paths(value: object) -> list[str]:
    if value is None:
        return []
    if isinstance(value, str):
        return [value]
    if isinstance(value, (list, tuple)) and all(isinstance(item, str) for item in value):
        return list(value)
    raise GuardRefusal("Aider returned an unsupported path-argument shape")


def _trusted_support_paths(parsed: argparse.Namespace) -> tuple[set[str], set[str]]:
    reads: set[str] = set()
    writes: set[str] = set()
    for attribute in (
        "message_file",
        "model_metadata_file",
        "model_settings_file",
        "config",
        "env_file",
        "chat_history_file",
        "input_history_file",
        "llm_history_file",
    ):
        for raw in _as_paths(getattr(parsed, attribute, None)):
            try:
                reads.add(_path_key(Path(raw).resolve(strict=False)))
            except (OSError, RuntimeError):
                raise GuardRefusal("runner-owned Aider support path could not be resolved")
    for attribute in ("chat_history_file", "input_history_file", "llm_history_file"):
        for raw in _as_paths(getattr(parsed, attribute, None)):
            try:
                writes.add(_path_key(Path(raw).resolve(strict=False)))
            except (OSError, RuntimeError):
                raise GuardRefusal("runner-owned Aider history path could not be resolved")
    return reads, writes


def _validate_invocation(root: Path, allowed: set[str], aider_argv: list[str]):
    try:
        from aider.args import get_parser

        parsed = get_parser([], str(root)).parse_args(aider_argv)
    except SystemExit as exc:
        raise GuardRefusal("Aider command arguments were rejected") from exc
    except Exception as exc:
        raise GuardRefusal("Aider command arguments could not be verified") from exc

    editable = _as_paths(getattr(parsed, "files", None)) + _as_paths(
        getattr(parsed, "file", None)
    )
    read_only = _as_paths(getattr(parsed, "read", None))
    if not editable or len(editable) != len(allowed):
        raise GuardRefusal("initial editable files do not match the admitted set")
    canonical_editable = []
    for raw in editable:
        path = _canonical_task_path(root, raw, allow_missing=True)
        if path is None:
            raise GuardRefusal("initial editable file path is unsafe")
        canonical_editable.append(_path_key(path))
    if len(set(canonical_editable)) != len(canonical_editable) or set(canonical_editable) != allowed:
        raise GuardRefusal("initial editable files do not equal the admitted set")
    for raw in read_only:
        path = _canonical_task_path(root, raw, allow_missing=False)
        if path is None or _path_key(path) not in allowed:
            raise GuardRefusal("read-only files exceed the admitted set")
    if getattr(parsed, "edit_format", None) != "diff":
        raise GuardRefusal("Aider edit format must remain the reviewed diff format")
    if getattr(parsed, "map_tokens", None) != 0:
        raise GuardRefusal("Aider repository-map reads must remain disabled")
    if getattr(parsed, "yes_always", None) is True:
        raise GuardRefusal("Aider automatic confirmation is forbidden")
    if getattr(parsed, "auto_lint", True) is not False:
        raise GuardRefusal("Aider automatic linting must remain disabled")
    has_test_command = bool(getattr(parsed, "test_cmd", None))
    if bool(getattr(parsed, "auto_test", False)) != has_test_command:
        raise GuardRefusal("automatic test execution must match an explicit runner command")
    return parsed, _trusted_support_paths(parsed)


def _is_under_root(raw_path: object, root: Path) -> bool:
    try:
        path = _task_path_form(Path(raw_path)).resolve(strict=False)
        return _under(path, root)
    except (OSError, RuntimeError, TypeError, ValueError):
        return False


def _run_aider(root: Path, allowed: set[str], trusted_reads: set[str], trusted_writes: set[str], argv: list[str]) -> int:
    from aider import io as aider_io
    from aider import main as aider_main
    from aider.coders import base_coder

    original_mentions = base_coder.Coder.get_file_mentions
    original_add = base_coder.Coder.add_rel_fname
    original_edit = base_coder.Coder.allowed_to_edit
    from aider.coders import editblock_coder
    original_get_edits = editblock_coder.EditBlockCoder.get_edits
    original_read = aider_io.InputOutput.read_text
    original_write = aider_io.InputOutput.write_text

    def approved(raw_path: object, allow_missing: bool = True) -> bool:
        candidate = _canonical_task_path(root, str(raw_path), allow_missing)
        return candidate is not None and _path_key(candidate) in allowed

    def guarded_mentions(coder, content, ignore_current=False):
        mentions = original_mentions(coder, content, ignore_current=ignore_current)
        return {
            name
            for name in mentions
            if approved(coder.abs_root_path(name), allow_missing=True)
        }

    def guarded_add(coder, name):
        if not approved(coder.abs_root_path(name), allow_missing=True):
            raise GuardRefusal("Aider tried to add a file outside the admitted set")
        return original_add(coder, name)

    def guarded_edit(coder, name):
        if not approved(coder.abs_root_path(name), allow_missing=True):
            coder.io.tool_warning("Fabrica blocked an edit outside the admitted task files.")
            return False
        return original_edit(coder, name)

    def guarded_get_edits(coder):
        # Aider's diff-format apply_edits_dry_run reads targets before its
        # later prepare_to_edit/allowed_to_edit checks. Filter while get_edits
        # is still only parsing model output, before any target read.
        previous_shell_count = len(coder.shell_commands)
        edits = original_get_edits(coder)
        if len(coder.shell_commands) > previous_shell_count:
            del coder.shell_commands[previous_shell_count:]
            coder.io.tool_warning("Fabrica blocked model-proposed shell commands.")
        admitted = []
        rejected = []
        for edit in edits:
            path, original, updated = edit
            if approved(coder.abs_root_path(path), allow_missing=True):
                admitted.append(edit)
            else:
                rejected.append(path)
        if rejected:
            coder.io.tool_warning(
                "Fabrica blocked edits outside admitted task files: " + ", ".join(map(str, rejected))
            )
        return admitted

    class DenyConfirmInputOutput(aider_io.InputOutput):
        def confirm_ask(self, *args, **kwargs):
            return False

        def read_text(self, filename, silent=False):
            if _is_under_root(filename, root):
                if not approved(filename, allow_missing=True):
                    raise GuardRefusal("Aider tried to read outside the admitted task files")
            else:
                try:
                    key = _path_key(Path(filename).resolve(strict=False))
                except (OSError, RuntimeError, TypeError, ValueError) as exc:
                    raise GuardRefusal("Aider support-file read path was unsafe") from exc
                if key not in trusted_reads:
                    raise GuardRefusal("Aider tried to read an untrusted external path")
            return original_read(self, filename, silent=silent)

        def write_text(self, filename, content, max_retries=5, initial_delay=0.1):
            if _is_under_root(filename, root):
                if not approved(filename, allow_missing=True):
                    raise GuardRefusal("Aider tried to write outside the admitted task files")
            else:
                try:
                    key = _path_key(Path(filename).resolve(strict=False))
                except (OSError, RuntimeError, TypeError, ValueError) as exc:
                    raise GuardRefusal("Aider history write path was unsafe") from exc
                if key not in trusted_writes:
                    raise GuardRefusal("Aider tried to write an untrusted external path")
            return original_write(
                self, filename, content, max_retries=max_retries, initial_delay=initial_delay
            )

    base_coder.Coder.get_file_mentions = guarded_mentions
    base_coder.Coder.add_rel_fname = guarded_add
    base_coder.Coder.allowed_to_edit = guarded_edit
    editblock_coder.EditBlockCoder.get_edits = guarded_get_edits
    aider_main.InputOutput = DenyConfirmInputOutput
    base_coder.InputOutput = DenyConfirmInputOutput
    try:
        result = aider_main.main(argv=argv)
        return int(result or 0)
    except GuardRefusal as exc:
        print("Fabrica Aider file guard refused the run: " + str(exc), file=sys.stderr)
        return 2


def main(argv: list[str] | None = None) -> int:
    raw_args = list(sys.argv[1:] if argv is None else argv)
    try:
        parsed_guard, aider_argv = _parse_guard_argv(raw_args)
    except GuardRefusal as exc:
        print("Fabrica Aider file guard refused the run: " + str(exc), file=sys.stderr)
        return 2
    if parsed_guard.probe:
        return _emit_probe()

    facts = _install_facts()
    if facts.get("ok") is not True:
        print("Fabrica Aider file guard refused the run: unsupported Aider installation.", file=sys.stderr)
        return 2
    try:
        root = _canonical_root(parsed_guard.worktree)
        allowed = _load_allowlist(root, parsed_guard.allowed_files_json)
        _, (trusted_reads, trusted_writes) = _validate_invocation(root, allowed, aider_argv)
        return _run_aider(root, allowed, trusted_reads, trusted_writes, aider_argv)
    except GuardRefusal as exc:
        print("Fabrica Aider file guard refused the run: " + str(exc), file=sys.stderr)
        return 2


if __name__ == "__main__":
    raise SystemExit(main())
