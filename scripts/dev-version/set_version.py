#!/usr/bin/env python3
"""Set synchronized Tauri application versions without reformatting unrelated fields."""
from __future__ import annotations
import argparse
import json
import os
from pathlib import Path
import re

SEMVER = r"\d+\.\d+\.\d+(?:-[0-9A-Za-z.-]+)?(?:\+[0-9A-Za-z.-]+)?"
TITLE_VERSION = re.compile(rf"(?<![\w.])v?({SEMVER})(?![\w.])")


def discover(repo: Path) -> dict[str, Path]:
    candidates = []
    for root, dirs, files in os.walk(repo, followlinks=False):
        dirs[:] = sorted(d for d in dirs if d not in {".git", ".worktrees", "node_modules", "target", ".venv"}
                         and not Path(root, d).is_symlink())
        if Path(root).name == "src-tauri" and "tauri.conf.json" in files:
            candidates.append(Path(root, "tauri.conf.json"))
    if len(candidates) != 1:
        raise ValueError(f"Expected one Tauri project under {repo}; found {len(candidates)}. Pass its frontend directory for multi-app repositories.")
    config = candidates[0]
    paths = {"tauri": config, "cargo": config.with_name("Cargo.toml")}
    if not paths["cargo"].is_file():
        raise ValueError(f"Missing {paths['cargo']}")
    for key, path in (("cargo_lock", config.with_name("Cargo.lock")),
                      ("package", config.parent.parent / "package.json"),
                      ("package_lock", config.parent.parent / "package-lock.json")):
        if path.exists():
            paths[key] = path
    return paths


def package_field(text, name):
    section = re.search(r"(?ms)^\[package\][^\n]*\n(.*?)(?=^\[|\Z)", text)
    match = re.search(rf'(?m)^\s*{name}\s*=\s*"([^"]+)"', section.group(1)) if section else None
    if not match:
        raise ValueError(f"Missing Cargo [package].{name}")
    return match.group(1)


def cargo_package_name(path):
    return package_field(path.read_text(encoding="utf-8"), "name")


def lock_pattern(package_name):
    return rf'(?s)name\s*=\s*"{re.escape(package_name)}"\r?\nversion\s*=\s*"([^"]+)"'


def read_versions(paths, texts=None):
    texts = texts or {key: path.read_bytes().decode("utf-8") for key, path in paths.items()}
    config = json.loads(texts["tauri"])
    versions = {"tauri": config["version"], "cargo": package_field(texts["cargo"], "version")}
    for index, window in enumerate(config.get("app", {}).get("windows", [])):
        title = window.get("title", "")
        matches = list(TITLE_VERSION.finditer(title))
        if len(matches) > 1:
            raise ValueError(f"Ambiguous version-bearing window title: {title}")
        if matches:
            versions[f"window_title:{index}"] = matches[0].group(1)
    if "cargo_lock" in texts:
        match = re.search(lock_pattern(package_field(texts["cargo"], "name")), texts["cargo_lock"])
        if not match:
            raise ValueError("Cargo.lock has no application package entry")
        versions["cargo_lock"] = match.group(1)
    if "package" in texts:
        versions["package"] = json.loads(texts["package"])["version"]
    if "package_lock" in texts:
        lock = json.loads(texts["package_lock"])
        versions["package_lock"] = lock["version"]
        if "" in lock.get("packages", {}):
            versions["package_lock_root"] = lock["packages"][""]["version"]
    return versions


def json_string_spans(text):
    """Map JSON string value paths to source spans; preserve all other bytes."""
    decoder = json.JSONDecoder()
    spans = {}

    def space(i):
        while i < len(text) and text[i].isspace():
            i += 1
        return i

    def value(i, path):
        i = space(i)
        if text[i] == "{":
            i = space(i + 1)
            while text[i] != "}":
                key, end = decoder.raw_decode(text, i)
                i = space(end)
                if text[i] != ":":
                    raise ValueError("Invalid JSON object")
                i = space(value(i + 1, path + (key,)))
                if text[i] == ",":
                    i = space(i + 1)
                elif text[i] != "}":
                    raise ValueError("Invalid JSON separator")
            return i + 1
        if text[i] == "[":
            i, index = space(i + 1), 0
            while text[i] != "]":
                i = space(value(i, path + (index,)))
                index += 1
                if text[i] == ",":
                    i = space(i + 1)
                elif text[i] != "]":
                    raise ValueError("Invalid JSON separator")
            return i + 1
        result, end = decoder.raw_decode(text, i)
        if isinstance(result, str):
            spans[path] = (i, end, result)
        return end

    value(0, ())
    return spans


def replace_json(text, changes):
    spans = json_string_spans(text)
    edits = []
    for path, val in changes.items():
        if path not in spans:
            raise ValueError(f"Missing JSON field {path}")
        start, end, _ = spans[path]
        edits.append((start, end, json.dumps(val, ensure_ascii=False)))
    for start, end, val in sorted(edits, reverse=True):
        text = text[:start] + val + text[end:]
    return text


def set_versions(paths, version):
    if not re.fullmatch(SEMVER, version):
        raise ValueError("Expected a SemVer version")
    original = {key: path.read_bytes().decode("utf-8") for key, path in paths.items()}
    read_versions(paths, original)  # Validate every managed field before any writes.
    updated = dict(original)
    config = json.loads(original["tauri"])
    changes = {("version",): version}
    for index, window in enumerate(config.get("app", {}).get("windows", [])):
        title = window.get("title", "")
        match = TITLE_VERSION.search(title)
        if match:
            changes[("app", "windows", index, "title")] = title[:match.start(1)] + version + title[match.end(1):]
    updated["tauri"] = replace_json(original["tauri"], changes)
    updated["cargo"], count = re.subn(r'(?ms)(^\[package\][^\n]*\n(?:(?!^\[).)*?^\s*version\s*=\s*")[^"]+(".*?$)',
                                      lambda m: m[1] + version + m[2], original["cargo"], count=1)
    if count != 1:
        raise ValueError("Could not update Cargo package version")
    if "cargo_lock" in paths:
        pattern = lock_pattern(package_field(original["cargo"], "name"))
        match = re.search(pattern, original["cargo_lock"])
        updated["cargo_lock"] = original["cargo_lock"][:match.start(1)] + version + original["cargo_lock"][match.end(1):]
    if "package" in paths:
        updated["package"] = replace_json(original["package"], {("version",): version})
    if "package_lock" in paths:
        changes = {("version",): version}
        if "" in json.loads(original["package_lock"]).get("packages", {}):
            changes[("packages", "", "version")] = version
        updated["package_lock"] = replace_json(original["package_lock"], changes)
    if set(read_versions(paths, updated).values()) != {version}:
        raise ValueError("Candidate version verification failed; no files changed")
    written = []
    try:
        for key, path in paths.items():
            if original[key] != updated[key]:
                path.write_bytes(updated[key].encode("utf-8"))
                written.append(key)
    except OSError:
        for key in written:
            paths[key].write_bytes(original[key].encode("utf-8"))
        raise
    if set(read_versions(paths).values()) != {version}:
        raise ValueError("Version read-back failed")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--repo", type=Path, required=True)
    parser.add_argument("--version", required=True)
    args = parser.parse_args()
    paths = discover(args.repo.resolve())
    set_versions(paths, args.version)
    print(json.dumps(read_versions(paths), indent=2))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
