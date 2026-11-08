# Repository ZIP backups

This helper plans or creates a dated ZIP snapshot for each immediate child of a configured project root that is a Git repository and has no GitHub remote. GitHub detection is read-only and local; the helper never fetches or contacts GitHub. Dirty tracked files, ignored authored/private files, untracked files, and the repository's `.git` directory are included, so local history and unpushed-only projects remain recoverable. Nested repositories are included in their parent snapshot; nested GitHub-backed repositories and nested linked worktrees are excluded and listed in the manifest.

## Private configuration

Keep the JSON outside this repository. Example shape (replace paths locally):

```json
{
  "schema_version": 1,
  "destination": "G:\\My Drive\\Backups\\Projects",
  "source_root": "C:\\Projects",
  "selection": "no-github-remote",
  "exclude_repositories": ["GP-Krita"],
  "exclude_dirs": ["node_modules", ".venv", "venv", "__pycache__", "target", "dist", "build", ".pytest_cache", ".mypy_cache", ".ruff_cache", ".worktrees"],
  "snapshot_paths": {
    "Agents": ["private/fabrica/usage-summary/latest.json", "private/fabrica/usage-summary/usage.sqlite3"]
  }
}
```

`snapshot_paths` is optional and maps repository names to exact repository-relative paths. Only `.json`, `.sqlite3`, and `.db` files are supported. JSON is copied only after a stable before/read/after check and valid JSON parse. SQLite files are captured through the SQLite online backup API from a read-only source connection, then quick-checked. Snapshot bytes are stored under the original path in the ZIP; the manifest records method, source path, size, and SHA-256. The source file is never modified. Only configured paths bypass ordinary source-mutation checks. Configured SQLite `-wal`, `-shm`, and `-journal` sidecars are omitted because the online snapshot incorporates their transaction state. For configured `latest.json`, only `latest.<UUID>.tmp` atomic-replacement siblings are omitted. Each such omission is named in the manifest. All other source files retain the normal strict stability checks.

`exclude_repositories` is an optional, case-insensitive list of discovered/configured repository names. Excluded repositories are skipped before remote inspection and remain excluded even when named with `--repo`.

The destination must already exist and must be outside each source repository. Discovery examines only immediate children of `source_root`; it does not recursively walk unrelated project data. Symlinks, junctions, and reparse points are never traversed. Linked worktrees selected as a source are rejected because their Git metadata is external.

## Use

```powershell
python C:\Projects\Navis-Fabrica\scripts\repo_backup.py --config C:\Projects\Agents\private\fabrica\repo-backups.json plan
python C:\Projects\Navis-Fabrica\scripts\repo_backup.py --config C:\Projects\Agents\private\fabrica\repo-backups.json backup
```

`plan` is read-only and lists eligible repositories and approximate uncompressed sizes. `backup` creates `NAME_YYYY-MM-DD.zip` archives with a `NAME/` top-level folder. Existing same-day archives are never overwritten. Each archive contains `NAME/BACKUP-MANIFEST.json` with file sizes, SHA-256 hashes, snapshot methods, exclusions, and skipped links/subrepositories. The helper checks source stability, ZIP CRCs, and archived hashes before publishing the final name. A failed run leaves a `.partial` file for inspection, never a successful-looking dated archive. No retention deletion is performed.

Use `--repo NAME` before the mode to limit a run. Example: `python ... --config ... --repo Agents backup`. Scheduling is managed outside this helper.

## Verification

Run the synthetic regression checks with:

```powershell
python C:\Projects\Navis-Fabrica\scripts\tests\test_repo_backup.py
```
