"""Prepare, journal, and verify deterministic synthetic FAB-S001 trials.

Provider calls use only Fabrica's shared runner CLI; host checks stay separate
until a parent has reviewed the generated source hashes.
"""

from __future__ import annotations

import argparse
import ctypes
import hashlib
import json
import os
from pathlib import Path
import re
import signal
import subprocess
import sys
import tempfile
import threading
import time

CONFIG_PATH = Path(__file__).resolve().parents[1] / "config" / "economy-trial-fixtures.json"
PRIVATE_EVIDENCE_ROOT = Path("C:/Projects/Agents/private/fabrica/FAB-S001")

# Reference implementations and acceptance code are host-owned. They are not
# included in the canonical dispatch spec or candidate workspace.
HOST_VERIFIERS = {
    "small_bugfix": {
        "checks": {"test_name_utils.py": "import unittest\nfrom name_utils import normalize_name\n\nclass NormalizeNameTests(unittest.TestCase):\n    def test_trims_and_collapses_whitespace(self):\n        self.assertEqual(normalize_name('  Ada   Lovelace  '), 'Ada Lovelace')\n\n    def test_preserves_case(self):\n        self.assertEqual(normalize_name('  kEVIN '), 'kEVIN')\n\nif __name__ == '__main__':\n    unittest.main()\n"},
        "expected": {"name_utils.py": "def normalize_name(value):\n    return ' '.join(value.split())\n"}},
    "implementation": {
        "checks": {"test_limits.py": "import unittest\nfrom limits import parse_positive_limit\n\nclass LimitTests(unittest.TestCase):\n    def test_integer_and_digits(self):\n        self.assertEqual(parse_positive_limit(4), 4)\n        self.assertEqual(parse_positive_limit('12'), 12)\n\n    def test_invalid_values(self):\n        for value in (True, 0, -1, ' 2', '2 ', '0', 'x', None, 2.0):\n            with self.subTest(value=value), self.assertRaises(ValueError):\n                parse_positive_limit(value)\n\nif __name__ == '__main__':\n    unittest.main()\n"},
        "expected": {"limits.py": "def parse_positive_limit(value):\n    if isinstance(value, bool):\n        raise ValueError('limit must be a positive integer')\n    if isinstance(value, int):\n        parsed = value\n    elif isinstance(value, str) and value.isdecimal():\n        parsed = int(value)\n    else:\n        raise ValueError('limit must be a positive integer')\n    if parsed <= 0:\n        raise ValueError('limit must be a positive integer')\n    return parsed\n"}},
    "multifile_edit": {
        "checks": {"test_catalog.py": "import ast\nimport os\nimport unittest\nfrom pathlib import Path\nfrom catalog import slug_for_title\nfrom text_utils import slugify\n\nclass SlugTests(unittest.TestCase):\n    def test_shared_helper_normalizes_punctuation_digits_and_unicode(self):\n        self.assertEqual(slugify('  Hello,  World! 42—café '), 'hello-world-42-cafe')\n\n    def test_catalog_uses_shared_helper(self):\n        source = Path(os.environ['TRIAL_SOURCE_ROOT'])\n        tree = ast.parse((source / 'catalog.py').read_text(encoding='utf-8'))\n        imports = [node for node in ast.walk(tree) if isinstance(node, ast.ImportFrom)]\n        self.assertTrue(any(node.module == 'text_utils' and any(alias.name == 'slugify' for alias in node.names) for node in imports))\n        function = next(node for node in tree.body if isinstance(node, (ast.FunctionDef, ast.AsyncFunctionDef)) and node.name == 'slug_for_title')\n        calls = [node for node in ast.walk(function) if isinstance(node, ast.Call)]\n        self.assertTrue(any(isinstance(node.func, ast.Name) and node.func.id == 'slugify' for node in calls))\n        self.assertEqual(slug_for_title('Hello,  World!'), 'hello-world')\n\nif __name__ == '__main__':\n    unittest.main()\n"},
        "expected": {"text_utils.py": "import re\nimport unicodedata\n\ndef slugify(text):\n    text = ''.join(' ' if unicodedata.category(char).startswith('P') else char for char in text)\n    ascii_text = unicodedata.normalize('NFKD', text).encode('ascii', 'ignore').decode('ascii')\n    words = re.findall(r'[a-z0-9]+', ascii_text.lower())\n    return '-'.join(words)\n", "catalog.py": "from text_utils import slugify\n\ndef slug_for_title(title):\n    return slugify(title)\n"}}
}


def load_spec() -> dict:
    return json.loads(CONFIG_PATH.read_text(encoding="utf-8"))


SPEC = load_spec()
TRIAL_MODELS = tuple(SPEC["models"])
TASK_TYPES = tuple(task["id"] for task in SPEC["tasks"])
MAX_REQUESTS_PER_TASK = SPEC["limits"]["maxRequestsPerTask"]
MAX_OUTPUT_TOKENS_PER_REQUEST = SPEC["limits"]["maxOutputTokensPerRequest"]
TASK_DEADLINE_SECONDS = SPEC["limits"]["maxSecondsPerTask"]
FIXTURE_SET_HASH = hashlib.sha256(CONFIG_PATH.read_bytes()).hexdigest()
FIXTURES = {
    task["id"]: {"task_type": task["id"], "prompt": task["prompt"],
                 "files": {entry["path"]: entry["content"] for entry in task["seedFiles"]},
                 "checks": HOST_VERIFIERS[task["id"]]["checks"],
                 "expected": HOST_VERIFIERS[task["id"]]["expected"]}
    for task in SPEC["tasks"]
}

DEFAULT_TRIAL_ROOT = Path("C:/Projects/Agents/private/fabrica/FAB-S001/trials")
TRIAL_ROOT_ENV = "FABRICA_ECONOMY_TRIAL_ROOT"
TRIAL_AUTOLOAD_FILES = (".env", ".aider.conf.yml", ".aider.model.settings.yml",
                        ".aider.model.metadata.json")
RUNNER_RECEIPT_FIELDS = {"ok", "taskId", "modelId", "providerId", "runtimeId",
                         "runtimeVersion", "state", "requestCount", "tokensUsed",
                         "latencyMs", "billing", "sourceFingerprint", "schemaVersion",
                         "failureCode", "observedModelId"}
RUNNER_STATES = {"completed", "failed", "rateLimited", "timedOut", "needsInput",
                 "authenticationRequired", "unsupportedModel", "cancelled"}
RUNNER_FAILURE_CODES = {"INVALID_TRIAL_ARGUMENT", "RATE_LIMITED", "TIMED_OUT",
                        "AUTHENTICATION_REQUIRED", "UNSUPPORTED_MODEL", "PREFLIGHT_REJECTED",
                        "MODEL_IDENTITY_MISMATCH", "RUNTIME_FAILED"}
MAX_RUNNER_RECEIPT_BYTES = 64 * 1024
OUTER_RUNNER_TIMEOUT_SECONDS = 130


class AttemptAlreadyReserved(RuntimeError):
    """This exact model/task pair has already been admitted and cannot retry."""


def trial_plan() -> list[dict]:
    """Return the fixed nine authorized model/task pairs, in reproducible order."""
    return [{"model_id": model, "task_type": task, "fixture_set_hash": FIXTURE_SET_HASH}
            for model in TRIAL_MODELS for task in TASK_TYPES]


def candidate_inventory(workspace: Path) -> dict[str, str]:
    """Hash a candidate tree; reject links and non-regular files."""
    if _is_link_or_reparse(workspace):
        raise ValueError("candidate workspace root is a link or reparse point")
    root = workspace.resolve(strict=True)
    result: dict[str, str] = {}
    for path in root.rglob("*"):
        if _is_link_or_reparse(path):
            raise ValueError(f"candidate workspace contains a symlink: {path.relative_to(root)}")
        if path.is_dir():
            continue
        if not path.is_file():
            raise ValueError(f"candidate workspace contains a non-file entry: {path.relative_to(root)}")
        relative = path.relative_to(root).as_posix()
        result[relative] = hashlib.sha256(path.read_bytes()).hexdigest()
    return dict(sorted(result.items()))


def _is_link_or_reparse(path: Path) -> bool:
    if path.is_symlink():
        return True
    junction_check = getattr(path, "is_junction", None)
    return bool(junction_check and junction_check())


def _check_ids(task_type: str) -> list[str]:
    return sorted(Path(name).stem for name in FIXTURES[task_type]["checks"])


def run_host_checks(task_type: str, candidate_workspace: Path,
                    verifier_root: Path, timeout_seconds: int = 10) -> dict:
    """Run the fixed host-owned unittest suite and return facts without output text."""
    if not 1 <= timeout_seconds <= 10:
        raise ValueError("host check timeout must be between 1 and 10 seconds")
    candidate = candidate_workspace.resolve(strict=True)
    inventory = candidate_inventory(candidate)
    suite = prepare_check_suite(task_type, verifier_root)
    env = {key: os.environ[key] for key in ("SystemRoot", "WINDIR", "PATH", "TEMP", "TMP")
           if key in os.environ}
    env.update({"PYTHONPATH": str(candidate), "TRIAL_SOURCE_ROOT": str(candidate),
                "PYTHONNOUSERSITE": "1", "PYTHONDONTWRITEBYTECODE": "1"})
    started = time.monotonic()
    try:
        result = subprocess.run(suite["command"], cwd=verifier_root, env=env,
                                stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL,
                                stderr=subprocess.DEVNULL, timeout=timeout_seconds)
    except subprocess.TimeoutExpired:
        status, exit_code = "blocked", None
    else:
        status = "passed" if result.returncode == 0 else "failed"
        exit_code = result.returncode
    return {"status": status, "exit_code": exit_code,
            "duration_seconds": min(time.monotonic() - started, float(timeout_seconds)),
            "candidate_inventory": inventory, "check_hashes": suite["hashes"],
            "check_ids": _check_ids(task_type)}


def parse_runner_receipt(stdout: bytes, exit_code: int, model_id: str,
                         task_type: str,
                         expected_canonical_model_id: str | None = None) -> dict:
    """Validate the shared runner's bounded, sanitized JSON receipt."""
    if len(stdout) > MAX_RUNNER_RECEIPT_BYTES:
        raise ValueError("runner receipt exceeds the 64 KiB contract")

    def no_duplicate_keys(pairs):
        output = {}
        for key, value in pairs:
            if key in output:
                raise ValueError("runner receipt contains duplicate JSON fields")
            output[key] = value
        return output

    try:
        receipt = json.loads(stdout.decode("utf-8"), object_pairs_hook=no_duplicate_keys)
    except (UnicodeDecodeError, json.JSONDecodeError) as exc:
        raise ValueError("runner returned no valid sanitized JSON receipt") from exc
    if not isinstance(receipt, dict) or set(receipt) != RUNNER_RECEIPT_FIELDS:
        raise ValueError("runner receipt fields do not match the supported schema")
    if (not isinstance(receipt["schemaVersion"], int) or
            isinstance(receipt["schemaVersion"], bool) or receipt["schemaVersion"] != 1):
        raise ValueError("unsupported runner receipt schema")
    if receipt["taskId"] != task_type or receipt["modelId"] != model_id:
        raise ValueError("runner receipt selected identity differs from the admitted pair")
    if not isinstance(receipt["ok"], bool) or receipt["state"] not in RUNNER_STATES:
        raise ValueError("runner receipt has invalid status")
    if receipt["ok"] != (receipt["state"] == "completed") or (exit_code == 0) != receipt["ok"]:
        raise ValueError("runner receipt state disagrees with process exit status")
    failure_code = receipt["failureCode"]
    if failure_code is not None and failure_code not in RUNNER_FAILURE_CODES:
        raise ValueError("runner receipt contains an invalid failure code")
    for key in ("providerId", "runtimeId", "runtimeVersion"):
        if not isinstance(receipt[key], str) or len(receipt[key]) > 128:
            raise ValueError("runner receipt contains invalid runtime identity")
    for key, maximum in (("requestCount", MAX_REQUESTS_PER_TASK),
                         ("latencyMs", TASK_DEADLINE_SECONDS * 1000)):
        value = receipt[key]
        if not isinstance(value, int) or isinstance(value, bool) or not 0 <= value <= maximum:
            raise ValueError(f"runner receipt exceeds {key} bound")
    tokens = receipt["tokensUsed"]
    if tokens is not None and (not isinstance(tokens, int) or isinstance(tokens, bool) or tokens < 0):
        raise ValueError("runner receipt contains invalid token usage")
    if receipt["state"] == "completed" and (receipt["requestCount"] < 1 or failure_code is not None):
        raise ValueError("completed runner task has no observed inference requests")
    if receipt["state"] != "completed" and failure_code is None:
        raise ValueError("failed runner task has no bounded failure code")
    observed_model = receipt["observedModelId"]
    if observed_model is not None and (not isinstance(observed_model, str) or len(observed_model) > 256):
        raise ValueError("runner receipt contains invalid observed model identity")
    if receipt["billing"] != "zeroCost":
        raise ValueError("runner reported nonzero-cost billing")
    fingerprint = receipt["sourceFingerprint"]
    if fingerprint is not None and (not isinstance(fingerprint, str) or
                                    re.fullmatch(r"[0-9a-f]{64}", fingerprint) is None):
        raise ValueError("runner receipt contains invalid source fingerprint")
    if receipt["requestCount"] == 0:
        if receipt["state"] == "completed" or failure_code is None:
            raise ValueError("zero-request receipt is not an explicit pre-dispatch failure")
    else:
        if not receipt["providerId"] or receipt["runtimeId"] != "aider" or receipt["runtimeVersion"] != "0.86.2":
            raise ValueError("runner did not use the authorized OpenRouter and Aider 0.86.2 route")
    allowed_observed_models = {model_id}
    if expected_canonical_model_id is not None:
        allowed_observed_models.add(expected_canonical_model_id)
    if receipt["state"] == "completed" and (
            observed_model not in allowed_observed_models or
            fingerprint != seed_fingerprint(task_type)):
        raise ValueError("completed receipt does not match selected model and canonical source")
    return receipt


def validate_runner_cli(runner: Path, settings: Path, trial_root: Path,
                        model_id: str, task_type: str) -> None:
    """Strictly probe an unsupported check flag before journal admission.

    Consent is deliberately omitted, and both paths are nonexistent private
    sentinels. The backend must reject the unknown option without side effects.
    """
    root = _require_trial_root(trial_root)
    data_probe = root / ".cli-preflight-data-must-not-exist"
    fixture_probe = root / ".cli-preflight-fixture-must-not-exist"
    if data_probe.exists() or fixture_probe.exists():
        raise ValueError("CLI preflight sentinel path already exists")
    args = [str(runner), "economy-trial-run", "--settings", str(settings),
            "--data-root", str(data_probe), "--model-id", model_id,
            "--task-id", task_type, "--fixture-dir", str(fixture_probe),
            "--test-command", "must-not-be-accepted"]
    stdout, exit_code = _run_bounded(args, timeout_seconds=5)
    if data_probe.exists() or fixture_probe.exists():
        raise ValueError("CLI preflight touched its no-dispatch sentinel paths")
    if len(stdout) > MAX_RUNNER_RECEIPT_BYTES:
        raise ValueError("CLI preflight output exceeded its bound")
    response = parse_runner_preflight(stdout, exit_code, model_id, task_type)
    if data_probe.exists() or fixture_probe.exists():
        raise ValueError("CLI preflight touched its no-dispatch sentinel paths")
    if response["failureCode"] != "INVALID_TRIAL_ARGUMENT":
        raise ValueError("runner CLI did not prove strict no-command trial admission")


def parse_runner_preflight(stdout: bytes, exit_code: int,
                           model_id: str, task_type: str) -> dict:
    """Validate the CLI's intentional unsupported-option response without dispatch."""
    if len(stdout) > MAX_RUNNER_RECEIPT_BYTES:
        raise ValueError("CLI preflight output exceeded its bound")
    try:
        response = json.loads(stdout.decode("utf-8"))
    except (UnicodeDecodeError, json.JSONDecodeError) as exc:
        raise ValueError("CLI preflight returned invalid JSON") from exc
    if (not isinstance(response, dict) or set(response) != RUNNER_RECEIPT_FIELDS or
            not isinstance(response.get("schemaVersion"), int) or isinstance(response.get("schemaVersion"), bool) or
            response.get("schemaVersion") != 1 or
            response.get("modelId") != model_id or response.get("taskId") != task_type or
            response.get("ok") is not False or response.get("state") != "failed" or
            response.get("requestCount") != 0 or
            response.get("failureCode") != "INVALID_TRIAL_ARGUMENT" or
            response.get("billing") != "zeroCost" or exit_code != 2):
        raise ValueError("runner CLI did not return the expected no-dispatch argument rejection")
    if response.get("observedModelId") is not None or response.get("sourceFingerprint") is not None:
        raise ValueError("CLI preflight returned model-call evidence despite rejecting arguments")
    if response.get("runtimeVersion") not in {"", "0.86.2"}:
        raise ValueError("CLI preflight reported an unsupported runtime version")
    return response


def _run_bounded(argv: list[str], timeout_seconds: int = OUTER_RUNNER_TIMEOUT_SECONDS) -> tuple[bytes, int]:
    """Drain runner stdout while retaining at most one bounded receipt plus a sentinel byte."""
    if not 1 <= timeout_seconds <= OUTER_RUNNER_TIMEOUT_SECONDS:
        raise ValueError("outer runner timeout exceeds the bounded trial window")
    process = subprocess.Popen(argv, stdin=subprocess.DEVNULL, stdout=subprocess.PIPE,
                               stderr=subprocess.DEVNULL,
                               start_new_session=(os.name != "nt"))
    captured = bytearray()
    overflow = threading.Event()

    def drain():
        assert process.stdout is not None
        try:
            while True:
                chunk = process.stdout.read(4096)
                if not chunk:
                    break
                remaining = MAX_RUNNER_RECEIPT_BYTES + 1 - len(captured)
                if remaining > 0:
                    captured.extend(chunk[:remaining])
                if len(chunk) > max(remaining, 0):
                    overflow.set()
        finally:
            process.stdout.close()

    reader = threading.Thread(target=drain, name="fabrica-trial-receipt-reader", daemon=True)
    reader.start()
    deadline = time.monotonic() + timeout_seconds
    try:
        while process.poll() is None:
            if overflow.is_set():
                _terminate_process_tree(process)
                reader.join(timeout=2)
                raise ValueError("runner receipt exceeds the 64 KiB contract")
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                _terminate_process_tree(process)
                reader.join(timeout=2)
                raise subprocess.TimeoutExpired(argv, timeout_seconds)
            try:
                process.wait(timeout=min(0.1, remaining))
            except subprocess.TimeoutExpired:
                continue
        exit_code = process.returncode
    except BaseException:
        if process.poll() is None:
            _terminate_process_tree(process)
        reader.join(timeout=2)
        raise
    reader.join(timeout=2)
    if reader.is_alive():
        process.kill()
        raise ValueError("runner stdout did not close after process exit")
    if overflow.is_set() or len(captured) > MAX_RUNNER_RECEIPT_BYTES:
        raise ValueError("runner receipt exceeds the 64 KiB contract")
    return bytes(captured), exit_code


def _terminate_process_tree(process: subprocess.Popen) -> None:
    """Terminate only the still-running process tree started by this harness."""
    if process.poll() is not None:
        return
    if os.name == "nt":
        _terminate_windows_owned_tree(process)
    else:
        try:
            os.killpg(process.pid, signal.SIGTERM)
        except ProcessLookupError:
            return
        try:
            process.wait(timeout=2)
        except subprocess.TimeoutExpired:
            if process.poll() is None:
                os.killpg(process.pid, signal.SIGKILL)
    if process.poll() is None:
        process.wait(timeout=5)


def _terminate_windows_owned_tree(process: subprocess.Popen) -> None:
    """Use Toolhelp ancestry plus creation-time checks; never kill by process name."""
    from ctypes import wintypes

    class ProcessEntry(ctypes.Structure):
        _fields_ = [("dwSize", wintypes.DWORD), ("cntUsage", wintypes.DWORD),
                    ("th32ProcessID", wintypes.DWORD), ("th32DefaultHeapID", ctypes.c_size_t),
                    ("th32ModuleID", wintypes.DWORD), ("cntThreads", wintypes.DWORD),
                    ("th32ParentProcessID", wintypes.DWORD), ("pcPriClassBase", wintypes.LONG),
                    ("dwFlags", wintypes.DWORD), ("szExeFile", wintypes.WCHAR * 260)]

    kernel32 = ctypes.WinDLL("kernel32", use_last_error=True)
    snapshot = kernel32.CreateToolhelp32Snapshot
    snapshot.argtypes = [wintypes.DWORD, wintypes.DWORD]
    snapshot.restype = wintypes.HANDLE
    first = kernel32.Process32FirstW
    first.argtypes = [wintypes.HANDLE, ctypes.POINTER(ProcessEntry)]
    first.restype = wintypes.BOOL
    following = kernel32.Process32NextW
    following.argtypes = [wintypes.HANDLE, ctypes.POINTER(ProcessEntry)]
    following.restype = wintypes.BOOL
    open_process = kernel32.OpenProcess
    open_process.argtypes = [wintypes.DWORD, wintypes.BOOL, wintypes.DWORD]
    open_process.restype = wintypes.HANDLE
    get_times = kernel32.GetProcessTimes
    get_times.argtypes = [wintypes.HANDLE, ctypes.POINTER(wintypes.FILETIME),
                          ctypes.POINTER(wintypes.FILETIME), ctypes.POINTER(wintypes.FILETIME),
                          ctypes.POINTER(wintypes.FILETIME)]
    get_times.restype = wintypes.BOOL
    terminate = kernel32.TerminateProcess
    terminate.argtypes = [wintypes.HANDLE, wintypes.UINT]
    terminate.restype = wintypes.BOOL
    wait = kernel32.WaitForSingleObject
    wait.argtypes = [wintypes.HANDLE, wintypes.DWORD]
    wait.restype = wintypes.DWORD
    close = kernel32.CloseHandle
    close.argtypes = [wintypes.HANDLE]
    close.restype = wintypes.BOOL

    def created_at(handle) -> int | None:
        created, exited, kernel, user = (wintypes.FILETIME() for _ in range(4))
        if not get_times(handle, ctypes.byref(created), ctypes.byref(exited),
                         ctypes.byref(kernel), ctypes.byref(user)):
            return None
        return (created.dwHighDateTime << 32) | created.dwLowDateTime

    root_handle = wintypes.HANDLE(process._handle)
    root_created = created_at(root_handle)
    if root_created is None or process.poll() is not None:
        return
    invalid = ctypes.c_void_p(-1).value
    snap = snapshot(0x00000002, 0)
    if not snap or ctypes.cast(snap, ctypes.c_void_p).value == invalid:
        raise ctypes.WinError(ctypes.get_last_error())
    entries: dict[int, tuple[int, int, object]] = {}
    handles = []
    try:
        entry = ProcessEntry()
        entry.dwSize = ctypes.sizeof(ProcessEntry)
        present = first(snap, ctypes.byref(entry))
        while present:
            pid = int(entry.th32ProcessID)
            parent_pid = int(entry.th32ParentProcessID)
            handle = open_process(0x0001 | 0x1000 | 0x00100000, False, pid)
            if handle:
                handles.append(handle)
                created = created_at(handle)
                if created is not None:
                    entries[pid] = (parent_pid, created, handle)
            present = following(snap, ctypes.byref(entry))
    finally:
        close(snap)

    tree: dict[int, tuple[int, int]] = {process.pid: (root_created, 0)}
    changed = True
    while changed:
        changed = False
        for pid, (parent_pid, created, _) in entries.items():
            if pid in tree or parent_pid not in tree:
                continue
            parent_created, depth = tree[parent_pid]
            if created >= parent_created:
                tree[pid] = (created, depth + 1)
                changed = True
    try:
        for pid, (_, depth) in sorted(tree.items(), key=lambda row: row[1][1], reverse=True):
            if pid == process.pid:
                continue
            handle = entries[pid][2]
            if wait(handle, 0) == 0x00000102 and terminate(handle, 1):
                wait(handle, 2000)
        if process.poll() is None:
            process.kill()
            process.wait(timeout=5)
    finally:
        for handle in handles:
            close(handle)


def run_trial(runner: Path, settings: Path, state_dir: Path,
              data_root: Path, model_id: str, task_type: str,
              workspace: Path, verifier_root: Path,
              canonical_prerequisites_path: Path | None = None) -> dict:
    """Run exactly one journaled pair through Fabrica's supported runner CLI."""
    if model_id not in TRIAL_MODELS or task_type not in TASK_TYPES:
        raise ValueError("trial selection is outside the authorized 3 by 3 matrix")
    if runner.name.lower() not in {"fabrica-runner", "fabrica-runner.exe"}:
        raise ValueError("runner must be the Fabrica shared runner executable")
    runner_path = runner.resolve(strict=True)
    if not runner_path.is_file():
        raise ValueError("runner path is not a regular executable file")
    settings_path = settings.resolve(strict=True)
    if not settings_path.is_file():
        raise ValueError("settings path is not a regular file")
    state_dir = _require_trial_root(state_dir)
    data_root = _require_trial_root(data_root)
    workspace = _require_trial_root(workspace)
    verifier_root = _require_trial_root(verifier_root)
    if canonical_prerequisites_path is None:
        raise ValueError("authenticated canonical model prerequisites are required before admission")
    canonical_prerequisites_path = canonical_prerequisites_path.resolve()
    canonical_association = load_canonical_prerequisite(
        canonical_prerequisites_path, model_id, settings_path)
    expected_canonical_model_id = canonical_association["expected_canonical_model_id"]
    validate_runner_cli(runner_path, settings_path, _configured_trial_root(), model_id, task_type)
    if (not canonical_prerequisites_path.is_file() or
            _is_link_or_reparse(canonical_prerequisites_path) or
            hashlib.sha256(canonical_prerequisites_path.read_bytes()).hexdigest() !=
            canonical_association["prerequisites_sha256"]):
        raise ValueError("canonical model prerequisite snapshot changed before admission")
    private_data = data_root
    if private_data.exists():
        raise FileExistsError("runner data root must be new for every model/task pair")
    private_data.parent.mkdir(parents=True, exist_ok=True)
    candidate = workspace
    if not candidate.exists():
        prepare_fixture(task_type, candidate)
    if not candidate.is_dir():
        raise ValueError("candidate workspace must be a directory")
    if not any(candidate.iterdir()):
        prepare_fixture(task_type, candidate)
    journal = reserve_attempt(state_dir, model_id, task_type, candidate,
                              canonical_association)
    try:
        snapshot_still_matches = (
            not _is_link_or_reparse(canonical_prerequisites_path) and
            hashlib.sha256(canonical_prerequisites_path.read_bytes()).hexdigest() ==
            canonical_association["prerequisites_sha256"])
    except OSError:
        snapshot_still_matches = False
    if not snapshot_still_matches:
        record = finalize_attempt(state_dir, model_id, task_type, {
            "state": "failed", "request_count": 0, "checks": {},
            "failure_code": "canonical_snapshot_changed",
            "host_check_status": "not_run",
            "host_check_hashes": check_hashes(task_type),
        })
        return {"record": record, "runner_receipt": None,
                "candidate_workspace": str(candidate),
                "candidate_inventory": candidate_inventory(candidate),
                "next_step": "canonical prerequisite snapshot changed; pair consumed without dispatch"}
    args = [str(runner_path), "economy-trial-run", "--settings", str(settings_path),
            "--data-root", str(private_data), "--model-id", model_id,
            "--task-id", task_type, "--fixture-dir", str(candidate),
            "--synthetic-policy-consent"]
    try:
        stdout, exit_code = _run_bounded(args)
        receipt = parse_runner_receipt(stdout, exit_code, model_id, task_type,
                                       expected_canonical_model_id)
        allowed_observed_models = {model_id, expected_canonical_model_id}
        terminal_state = ("completed" if receipt["state"] == "completed" and
                          receipt["observedModelId"] in allowed_observed_models and
                          receipt["sourceFingerprint"] == seed_fingerprint(task_type) else "failed")
        failure_code = (None if terminal_state == "completed" else
                        receipt["failureCode"].lower() if receipt["requestCount"] == 0 and receipt["failureCode"] else
                        "model_identity_mismatch" if receipt["observedModelId"] not in allowed_observed_models else
                        "source_fingerprint_mismatch" if receipt["sourceFingerprint"] != seed_fingerprint(task_type) else
                        receipt["failureCode"].lower() if receipt["failureCode"] else
                        "runtime_state_" + receipt["state"].lower())
    except subprocess.TimeoutExpired:
        receipt = None
        terminal_state = "ambiguous"
        failure_code = "runner_timeout"
    except OSError:
        receipt = None
        terminal_state = "ambiguous"
        failure_code = "runner_process_error"
    except ValueError:
        receipt = None
        terminal_state = "ambiguous"
        failure_code = "invalid_runner_receipt"

    try:
        inventory_after = candidate_inventory(candidate)
    except (OSError, ValueError):
        inventory_after = {}
    # Generated source is untrusted. Host-side checks require a separate,
    # hash-bound call after independent source review.
    outcome = {"state": terminal_state,
               "request_count": receipt["requestCount"] if receipt else None,
               "output_tokens_per_request": [],
               "duration_seconds": receipt["latencyMs"] / 1000 if receipt else None,
               "observed_model_id": receipt["observedModelId"] if receipt else None,
               "candidate_inventory_after": inventory_after, "checks": {},
               "runner_receipt": receipt, "host_check_hashes": check_hashes(task_type),
               "host_check_status": "not_run", "verification_state": "not_started",
               "failure_code": failure_code}
    record = finalize_attempt(state_dir, model_id, task_type, outcome)
    return {"record": record, "runner_receipt": receipt,
            "candidate_workspace": str(candidate),
            "candidate_inventory": inventory_after,
            "next_step": "independent source review, then verify_trial with reviewed inventory hashes"}


def _attempt_path(state_dir: Path, model_id: str, task_type: str) -> Path:
    if model_id not in TRIAL_MODELS or task_type not in TASK_TYPES:
        raise ValueError("attempt is outside the approved model/task matrix")
    key = hashlib.sha256(f"{model_id}\0{task_type}".encode("utf-8")).hexdigest()
    return state_dir.resolve() / "attempts" / f"{key}.json"


def _require_trial_root(path: Path) -> Path:
    root = _configured_trial_root()
    resolved = path.resolve()
    try:
        resolved.relative_to(root)
    except ValueError as exc:
        raise ValueError("trial artifacts must stay under the approved private trial root") from exc
    return resolved


def _configured_trial_root() -> Path:
    """Resolve an absolute private artifact root that cannot inherit Aider config."""
    raw = os.environ.get(TRIAL_ROOT_ENV)
    candidate = Path(raw) if raw is not None else DEFAULT_TRIAL_ROOT
    if not candidate.is_absolute():
        raise ValueError(f"{TRIAL_ROOT_ENV} must be an absolute path")
    ancestor = candidate
    while True:
        if _is_link_or_reparse(ancestor):
            raise ValueError("trial artifact root cannot traverse a symlink or reparse point")
        parent = ancestor.parent
        if parent == ancestor:
            break
        ancestor = parent
    resolved = candidate.resolve()
    lexical = Path(os.path.normpath(str(candidate)))
    if os.path.normcase(str(lexical)) != os.path.normcase(str(resolved)):
        raise ValueError("trial artifact root must already be canonical")
    public_repo = CONFIG_PATH.resolve().parents[1]
    if resolved == public_repo or public_repo in resolved.parents:
        raise ValueError("trial artifact root must be outside the public source repository")
    ancestor = resolved
    git_root = None
    while True:
        if (ancestor / ".git").exists():
            git_root = ancestor
            break
        parent = ancestor.parent
        if parent == ancestor:
            break
        ancestor = parent
    if git_root is not None:
        raise ValueError("trial artifact root cannot be inside a Git repository")
    home = os.environ.get("USERPROFILE") or os.environ.get("HOME")
    roots = [resolved, Path(resolved.anchor)]
    if home:
        roots.append(Path(home))
    for root in roots:
        for name in TRIAL_AUTOLOAD_FILES:
            candidate_file = root / name
            if candidate_file.exists() or candidate_file.is_symlink():
                raise ValueError("trial artifact root is blocked by an Aider auto-load configuration file")
    return resolved


def _atomic_store_record(target: Path, record: dict) -> None:
    encoded = (json.dumps(record, sort_keys=True, separators=(",", ":")) + "\n").encode("utf-8")
    fd, temporary_name = tempfile.mkstemp(prefix=target.stem + ".", suffix=".tmp", dir=target.parent)
    try:
        with os.fdopen(fd, "wb") as stream:
            stream.write(encoded)
            stream.flush()
            os.fsync(stream.fileno())
        os.replace(temporary_name, target)
    finally:
        if os.path.exists(temporary_name):
            os.unlink(temporary_name)


def reserve_attempt(state_dir: Path, model_id: str, task_type: str,
                    workspace: Path,
                    canonical_association: dict | None = None) -> dict:
    """Durably reserve a unique pair before dispatch; existing records never retry."""
    validate_fixture_contract()
    repo_root = CONFIG_PATH.resolve().parents[1]
    candidate_root = workspace.resolve(strict=True)
    if candidate_root == repo_root or repo_root in candidate_root.parents:
        raise ValueError("trial workspace must be outside the repository")
    fixture = FIXTURES.get(task_type)
    if fixture is None:
        raise ValueError("unknown trial fixture")
    observed = candidate_inventory(candidate_root)
    wanted = seed_hashes(task_type)
    if observed != wanted:
        raise ValueError("pre-dispatch workspace does not exactly match registered fixture seeds")
    if canonical_association is None:
        raise ValueError("authenticated canonical model association is required before reservation")
    if (canonical_association.get("selected_model_id") != model_id or
            not isinstance(canonical_association.get("expected_canonical_model_id"), str) or
            re.fullmatch(r"[A-Za-z0-9_.:-]+/[A-Za-z0-9_.:-]+",
                         canonical_association["expected_canonical_model_id"]) is None or
            not isinstance(canonical_association.get("provider_profile"), str) or
            re.fullmatch(r"[A-Za-z0-9._:-]{1,128}", canonical_association["provider_profile"]) is None or
            not isinstance(canonical_association.get("at_ms"), int) or
            isinstance(canonical_association.get("at_ms"), bool) or
            not isinstance(canonical_association.get("prerequisites_sha256"), str) or
            re.fullmatch(r"[0-9a-f]{64}", canonical_association["prerequisites_sha256"]) is None):
        raise ValueError("invalid authenticated canonical model association")
    target = _attempt_path(state_dir, model_id, task_type)
    target.parent.mkdir(parents=True, exist_ok=True)
    record = {"schema_version": 1, "state": "attempted", "model_id": model_id,
              "task_type": task_type, "fixture_set_hash": FIXTURE_SET_HASH,
              "expected_canonical_model_id": canonical_association["expected_canonical_model_id"],
              "canonical_provider_profile": canonical_association["provider_profile"],
              "canonical_prerequisites_at_ms": canonical_association["at_ms"],
              "canonical_prerequisites_sha256": canonical_association["prerequisites_sha256"],
              "candidate_inventory": observed, "check_hashes": check_hashes(task_type)}
    encoded = (json.dumps(record, sort_keys=True, separators=(",", ":")) + "\n").encode("utf-8")
    try:
        fd = os.open(target, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
    except FileExistsError as exc:
        raise AttemptAlreadyReserved(f"trial pair already reserved: {model_id} / {task_type}") from exc
    try:
        with os.fdopen(fd, "wb") as stream:
            stream.write(encoded)
            stream.flush()
            os.fsync(stream.fileno())
    except Exception:
        # Keep an incomplete marker if a write fails; it still prevents retry.
        raise
    return record


def finalize_attempt(state_dir: Path, model_id: str, task_type: str,
                     outcome: dict) -> dict:
    """Record bounded metadata only; never persist model output or raw logs."""
    target = _attempt_path(state_dir, model_id, task_type)
    if not target.is_file():
        raise FileNotFoundError("attempt was not reserved")
    record = json.loads(target.read_text(encoding="utf-8"))
    if record.get("state") != "attempted":
        raise AttemptAlreadyReserved("attempt already has a terminal observation")
    state = outcome.get("state")
    if state not in {"completed", "ambiguous", "failed"}:
        raise ValueError("invalid terminal attempt state")
    requests = outcome.get("request_count")
    if requests is not None and (not isinstance(requests, int) or isinstance(requests, bool) or
                                 not 0 <= requests <= MAX_REQUESTS_PER_TASK):
        raise ValueError("observed request count exceeds trial bound")
    outputs = outcome.get("output_tokens_per_request", [])
    if not isinstance(outputs, list) or len(outputs) > MAX_REQUESTS_PER_TASK or any(
            not isinstance(count, int) or isinstance(count, bool) or
            count < 0 or count > MAX_OUTPUT_TOKENS_PER_REQUEST
            for count in outputs):
        raise ValueError("observed output tokens exceed trial bound")
    duration = outcome.get("duration_seconds")
    if duration is not None and (not isinstance(duration, (int, float)) or isinstance(duration, bool) or
                                 duration < 0 or duration > TASK_DEADLINE_SECONDS):
        raise ValueError("observed duration exceeds task deadline")
    observed_model = outcome.get("observed_model_id")
    checks = outcome.get("checks", {})
    valid_check_ids = set(_check_ids(task_type))
    if (not isinstance(checks, dict) or set(checks) - valid_check_ids or
            any(value not in {"passed", "failed", "blocked"} for value in checks.values())):
        raise ValueError("invalid check outcomes")
    host_check_status = outcome.get("host_check_status")
    if host_check_status is not None and host_check_status not in {"not_run", "passed", "failed", "blocked"}:
        raise ValueError("invalid host check status")
    failure_code = outcome.get("failure_code")
    if failure_code is not None and (not isinstance(failure_code, str) or
                                     re.fullmatch(r"[a-z][a-z0-9_]{0,63}", failure_code) is None):
        raise ValueError("invalid sanitized failure code")
    verification_state = outcome.get("verification_state", "not_started")
    if verification_state != "not_started":
        raise ValueError("invalid initial verification state")
    inventory = outcome.get("candidate_inventory_after", {})
    if (not isinstance(inventory, dict) or any(
            not isinstance(path, str) or Path(path).is_absolute() or ".." in Path(path).parts or
            not isinstance(digest, str) or re.fullmatch(r"[0-9a-f]{64}", digest) is None
            for path, digest in inventory.items())):
        raise ValueError("invalid candidate inventory hashes")
    host_hashes = outcome.get("host_check_hashes", check_hashes(task_type))
    if host_hashes != check_hashes(task_type):
        raise ValueError("host check hashes differ from registered verifier source")
    runner_receipt = outcome.get("runner_receipt")
    safe_receipt = None
    if runner_receipt is not None:
        if not isinstance(runner_receipt, dict) or set(runner_receipt) != RUNNER_RECEIPT_FIELDS:
            raise ValueError("invalid runner receipt")
        safe_receipt = {key: runner_receipt[key] for key in sorted(RUNNER_RECEIPT_FIELDS)}
    record.update({"state": state, "request_count": requests,
                   "output_tokens_per_request": outputs, "duration_seconds": duration,
                   "observed_model_id": observed_model,
                   "observed_model_matches": observed_model == model_id if observed_model is not None else None,
                   "candidate_inventory_after": inventory, "checks": checks,
                   "host_check_status": host_check_status, "host_check_hashes": host_hashes,
                   "runner_receipt": safe_receipt,
                   "verification_state": outcome.get("verification_state", "not_started"),
                   "failure_code": failure_code})
    _atomic_store_record(target, record)
    return record


def verify_trial(state_dir: Path, model_id: str, task_type: str,
                 workspace: Path, verifier_root: Path,
                 reviewed_inventory: dict[str, str]) -> dict:
    """Run fixed host checks only after source review hashes still match."""
    state_dir = _require_trial_root(state_dir)
    workspace = _require_trial_root(workspace)
    verifier_root = _require_trial_root(verifier_root)
    target = _attempt_path(state_dir, model_id, task_type)
    if not target.is_file():
        raise FileNotFoundError("trial attempt was not recorded")
    record = json.loads(target.read_text(encoding="utf-8"))
    if record.get("state") != "completed":
        raise ValueError("only completed runtime trials can be verified")
    if (record.get("host_check_status") != "not_run" or
            record.get("verification_state") != "not_started" or "verification" in record):
        raise AttemptAlreadyReserved("host verification was already attempted")
    current = candidate_inventory(workspace)
    expected = record.get("candidate_inventory_after")
    if not isinstance(reviewed_inventory, dict) or current != reviewed_inventory or current != expected:
        raise ValueError("candidate files changed since captured source review hashes")
    expected_checks = check_hashes(task_type)
    if record.get("host_check_hashes") != expected_checks:
        raise ValueError("host verifier source changed since trial admission")
    verification_lock = target.with_suffix(".verification.lock")
    try:
        lock_fd = os.open(verification_lock, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
    except FileExistsError as exc:
        raise AttemptAlreadyReserved("host verification intent already exists") from exc
    with os.fdopen(lock_fd, "wb") as lock_stream:
        lock_stream.write((FIXTURE_SET_HASH + "\n").encode("ascii"))
        lock_stream.flush()
        os.fsync(lock_stream.fileno())
    # Persist one-shot intent before executing untrusted candidate code.
    record["verification_state"] = "started"
    _atomic_store_record(target, record)
    result = run_host_checks(task_type, workspace, verifier_root, timeout_seconds=10)
    if result["check_hashes"] != expected_checks or result["candidate_inventory"] != current:
        raise ValueError("verification source or candidate inventory changed during checks")
    record["host_check_status"] = result["status"]
    record["checks"] = {item: result["status"] for item in result["check_ids"]}
    record["verification"] = {"candidate_inventory": current,
                              "check_hashes": expected_checks,
                              "status": result["status"],
                              "exit_code": result["exit_code"],
                              "duration_seconds": result["duration_seconds"]}
    record["verification_state"] = "completed"
    _atomic_store_record(target, record)
    return {"status": result["status"], "checks": record["checks"],
            "verification": record["verification"]}


def fixture_ids() -> tuple[str, ...]:
    return TASK_TYPES


def validate_fixture_contract() -> None:
    """Guard the immutable experiment bounds and fixture shape."""
    spec = load_spec()
    if spec.get("schemaVersion") != 1:
        raise ValueError("unsupported fixture spec version")
    limits = spec.get("limits", {})
    if (limits.get("maxRequestsPerTask") != 3 or
            limits.get("maxOutputTokensPerRequest") != 8192 or
            limits.get("maxSecondsPerTask") != 120 or
            limits.get("maxCostUsd") != 0 or limits.get("retries") != 0 or
            limits.get("fallbacks") is not False):
        raise ValueError("trial bounds changed")
    if tuple(FIXTURES) != TASK_TYPES:
        raise ValueError("fixture task types must match the declared trial matrix")
    if len(TRIAL_MODELS) != 3 or len(set(TRIAL_MODELS)) != 3:
        raise ValueError("trial model allowlist must contain exactly three unique IDs")
    if set(HOST_VERIFIERS) != set(TASK_TYPES):
        raise ValueError("host verifier IDs must exactly match canonical task IDs")
    for name, fixture in FIXTURES.items():
        if fixture["task_type"] != name or not fixture["files"] or not fixture["checks"]:
            raise ValueError(f"incomplete fixture: {name}")
        if not set(fixture["expected"]).issubset(fixture["files"]):
            raise ValueError(f"expected outputs must target seeded files: {name}")
        if not fixture["prompt"]:
            raise ValueError(f"missing task prompt: {name}")
        entry = next(task for task in spec["tasks"] if task["id"] == name)
        check_ids = {item["id"] for item in entry["checks"]}
        host_ids = {Path(item).stem for item in fixture["checks"]}
        if check_ids != host_ids:
            raise ValueError(f"canonical check IDs differ from host verifier files: {name}")
        for relative in fixture["files"]:
            candidate = Path(relative)
            if candidate.is_absolute() or ".." in candidate.parts or "\\" in relative:
                raise ValueError(f"unsafe canonical seed path: {relative}")


def prepare_fixture(task_type: str, workspace: Path) -> dict:
    """Create candidate-writable source files only; keep checks/answers host-side."""
    validate_fixture_contract()
    if task_type not in FIXTURES:
        raise ValueError(f"unknown task type: {task_type}")
    root = workspace.resolve()
    if root.exists() and any(root.iterdir()):
        raise FileExistsError(f"workspace is not empty: {root}")
    root.mkdir(parents=True, exist_ok=True)
    fixture = FIXTURES[task_type]
    for relative, content in fixture["files"].items():
        target = (root / relative).resolve()
        if root not in target.parents:
            raise ValueError(f"fixture path escapes workspace: {relative}")
        target.parent.mkdir(parents=True, exist_ok=True)
        target.write_text(content, encoding="utf-8", newline="\n")
    return {"schema_version": SPEC["schemaVersion"], "fixture_set_hash": FIXTURE_SET_HASH,
            "fixture_id": task_type,
            "seed_hashes": seed_hashes(task_type),
            "prompt": fixture["prompt"],
            "check_hashes": check_hashes(task_type)}


def seed_hashes(task_type: str) -> dict[str, str]:
    """Return registered SHA-256 values for the candidate-writable seed files."""
    fixture = FIXTURES[task_type]
    return {name: hashlib.sha256(content.encode("utf-8")).hexdigest()
            for name, content in fixture["files"].items()}


def seed_fingerprint(task_type: str) -> str:
    """Match Rust's ordered path-NUL-content-NUL registry source digest."""
    task = next((item for item in SPEC["tasks"] if item["id"] == task_type), None)
    if task is None:
        raise ValueError(f"unknown trial fixture: {task_type}")
    digest = hashlib.sha256()
    for entry in task["seedFiles"]:
        digest.update(entry["path"].encode("utf-8"))
        digest.update(b"\0")
        digest.update(entry["content"].encode("utf-8"))
        digest.update(b"\0")
    return digest.hexdigest()


def load_canonical_prerequisite(path: Path, model_id: str, settings_path: Path,
                                now_ms: int | None = None) -> dict:
    """Validate the private, authenticated zero-call model identity snapshot."""
    if model_id not in TRIAL_MODELS:
        raise ValueError("model is outside the fixed trial shortlist")
    if _is_link_or_reparse(path):
        raise ValueError("canonical model prerequisites cannot be a link or reparse point")
    if _is_link_or_reparse(settings_path):
        raise ValueError("private trial settings cannot be a link or reparse point")
    evidence_root = PRIVATE_EVIDENCE_ROOT.resolve()
    snapshot_path = path.resolve()
    try:
        snapshot_path.relative_to(evidence_root)
    except ValueError as exc:
        raise ValueError("canonical model prerequisites must be in the approved private evidence root") from exc
    if not snapshot_path.is_file() or _is_link_or_reparse(snapshot_path):
        raise ValueError("canonical model prerequisites must be a regular private file")
    settings_path = settings_path.resolve()
    try:
        settings_path.relative_to(evidence_root)
    except ValueError as exc:
        raise ValueError("trial settings must be a private copy under the approved evidence root") from exc
    try:
        settings = json.loads(settings_path.read_text(encoding="utf-8"))
    except (OSError, UnicodeDecodeError, json.JSONDecodeError) as exc:
        raise ValueError("private trial settings are unavailable or invalid") from exc
    selection = settings.get("providerSelection") if isinstance(settings, dict) else None
    selected_profile = selection.get("providerId") if isinstance(selection, dict) else None
    if not isinstance(selected_profile, str) or not selected_profile:
        raise ValueError("private trial settings must select a configured provider profile")
    profiles = settings.get("providers") if isinstance(settings, dict) else None
    profile_entry = next((entry for entry in profiles if isinstance(entry, dict) and
                          entry.get("id") == selected_profile), None) if isinstance(profiles, list) else None
    kind = profile_entry.get("kind") if profile_entry else None
    if not isinstance(kind, str) or re.sub(r"[^a-z]", "", kind.lower()) != "openrouter":
        raise ValueError("private trial settings must select an existing OpenRouter profile")
    raw = snapshot_path.read_bytes()
    if len(raw) > 256 * 1024:
        raise ValueError("canonical model prerequisites exceed their size bound")
    try:
        snapshot = json.loads(raw.decode("utf-8"))
    except (UnicodeDecodeError, json.JSONDecodeError) as exc:
        raise ValueError("canonical model prerequisites are invalid JSON") from exc
    if not isinstance(snapshot, dict):
        raise ValueError("canonical model prerequisites must be an object")
    at_ms = snapshot.get("at_ms")
    observed_now = int(time.time() * 1000) if now_ms is None else now_ms
    if (not isinstance(at_ms, int) or isinstance(at_ms, bool) or at_ms > observed_now or
            observed_now - at_ms > 24 * 60 * 60 * 1000):
        raise ValueError("canonical model prerequisite snapshot is future-dated or stale")
    if snapshot.get("inference_calls") != 0 or isinstance(snapshot.get("inference_calls"), bool):
        raise ValueError("canonical model prerequisites must record zero inference calls")
    provider_profile = snapshot.get("provider_profile")
    if provider_profile != selected_profile:
        raise ValueError("canonical model snapshot profile differs from private trial settings")
    aider_exit = snapshot.get("aider_exit")
    if (snapshot.get("aider_version") != "aider 0.86.2" or
            not isinstance(aider_exit, int) or isinstance(aider_exit, bool) or aider_exit != 0):
        raise ValueError("canonical model snapshot lacks the authorized Aider 0.86.2 probe")
    models = snapshot.get("models")
    if (not isinstance(models, list) or len(models) != len(TRIAL_MODELS) or
            snapshot.get("model_count") != len(TRIAL_MODELS)):
        raise ValueError("canonical model snapshot does not contain the exact candidate set")
    by_id = {}
    for item in models:
        if not isinstance(item, dict) or not isinstance(item.get("id"), str):
            raise ValueError("canonical model snapshot contains an invalid model entry")
        if item["id"] in by_id:
            raise ValueError("canonical model snapshot contains duplicate IDs")
        by_id[item["id"]] = item
    if set(by_id) != set(TRIAL_MODELS):
        raise ValueError("canonical model snapshot IDs differ from the authorized shortlist")
    canonical_values = set()
    for selected_id, item in by_id.items():
        canonical = item.get("canonical_slug")
        if (not isinstance(canonical, str) or
                re.fullmatch(r"[A-Za-z0-9_.:-]+/[A-Za-z0-9_.:-]+", canonical) is None or
                canonical.lower().startswith("openrouter/")):
            raise ValueError("canonical model identity is invalid or router-based")
        if canonical in canonical_values:
            raise ValueError("canonical model identities are not unique")
        canonical_values.add(canonical)
        pricing = item.get("pricing")
        if not isinstance(pricing, dict):
            raise ValueError("canonical model pricing is missing")
        prompt_price = pricing.get("prompt")
        completion_price = pricing.get("completion")
        if prompt_price != "0" or completion_price != "0":
            raise ValueError("canonical model pricing is not zero for prompt and completion")
        context = item.get("context_length")
        if not isinstance(context, int) or isinstance(context, bool) or context < 1024:
            raise ValueError("canonical model context is unavailable or too small")
    selected = by_id[model_id]
    return {"selected_model_id": model_id,
            "expected_canonical_model_id": selected["canonical_slug"],
            "provider_profile": provider_profile, "at_ms": at_ms,
            "prerequisites_sha256": hashlib.sha256(raw).hexdigest()}


def check_hashes(task_type: str) -> dict[str, str]:
    """Return SHA-256 hashes of host-owned verifier files, never source answers."""
    fixture = FIXTURES[task_type]
    return {name: hashlib.sha256(content.encode("utf-8")).hexdigest()
            for name, content in fixture["checks"].items()}


def prepare_check_suite(task_type: str, verifier_root: Path) -> dict:
    """Write verifier files outside the candidate workspace, refusing overwrite."""
    if task_type not in FIXTURES:
        raise ValueError(f"unknown task type: {task_type}")
    root = verifier_root.resolve()
    if root.exists() and any(root.iterdir()):
        raise FileExistsError(f"verifier directory is not empty: {root}")
    root.mkdir(parents=True, exist_ok=True)
    fixture = FIXTURES[task_type]
    for relative, content in fixture["checks"].items():
        (root / relative).write_text(content, encoding="utf-8", newline="\n")
    command = [sys.executable, "-m", "unittest", "-v", *fixture["checks"].keys()]
    return {"command": command, "hashes": check_hashes(task_type)}


def main() -> int:
    parser = argparse.ArgumentParser(description="Prepare synthetic FAB-S001 task fixtures (no dispatch).")
    parser.add_argument("task_type", choices=TASK_TYPES)
    parser.add_argument("workspace", type=Path)
    args = parser.parse_args()
    prepare_fixture(args.task_type, args.workspace)
    print(f"Prepared {args.task_type} synthetic fixture at {args.workspace.resolve()}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
