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
  "exclude_dirs": ["node_modules", ".venv", "venv", "__pycache__", "target", "dist", "build", ".pytest_cache", ".mypy_cache", ".ruff_cache", ".worktrees"]
}
```

The destination must already exist and must be outside each source repository. Discovery examines only immediate children of `source_root`; it does not recursively walk unrelated project data. Symlinks, junctions, and reparse points are never traversed. Linked worktrees selected as a source are rejected because their Git metadata is external.

## Use

```powershell
python C:\Projects\Navis-Fabrica\scripts\repo_backup.py --config C:\Projects\Agents\private\fabrica\repo-backups.json plan
python C:\Projects\Navis-Fabrica\scripts\repo_backup.py --config C:\Projects\Agents\private\fabrica\repo-backups.json backup
```

`plan` is read-only and lists eligible repositories and approximate uncompressed sizes. `backup` creates `NAME_YYYY-MM-DD.zip` archives with a `NAME/` top-level folder. Existing same-day archives are never overwritten. Each archive contains `NAME/BACKUP-MANIFEST.json` with file sizes, SHA-256 hashes, exclusions, and skipped links/subrepositories. The helper checks source stability, ZIP CRCs, and archived hashes before publishing the final name. A failed run leaves a `.partial` file for inspection, never a successful-looking dated archive. No retention deletion is performed.

Use `--repo NAME` before the mode to limit a run. Example: `python ... --config ... --repo Agents backup`. Scheduling is intentionally managed outside this helper.

## Verification

Run the synthetic regression checks with:

```powershell
python C:\Projects\Navis-Fabrica\scripts\tests\test_repo_backup.py
```
