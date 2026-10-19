# Desktop storage and card logos

Fabrica keeps desktop state outside its source repository. By default it uses the platform application config/data directories. To relocate it, create `storage-location.json` in the original app config directory:

```json
{
  "schemaVersion": 1,
  "dataRoot": "D:/Private/Fabrica/Desktop"
}
```

The configured directory must already exist. An invalid or unavailable location produces an error, rather than silently creating a second data store. Restart the desktop app after changing the locator. Settings and project inventory live at the data root, workflow history uses `workflow-runs.sqlite3`, and runner state lives under `runner/`. Provider credentials remain in OS/provider-managed credential storage; the locator is the only bootstrap configuration left in the platform folder. Other tools' explicit CLI data-root arguments remain authoritative.

Use `scripts/migrate-desktop-storage.py --source <old-app-folder> --target <private-desktop-folder> --logo <image> --project <project-path> --project <second-project-path>` with Fabrica and workers closed. It copies and verifies existing state before writing the locator, uses SQLite backup/integrity checks for databases, refuses to overwrite conflicting destination files, and retains originals for rollback. Do not run an old desktop executable against the legacy state after switching: it does not understand the locator. Delete legacy copies only after the new desktop build has been verified with the migrated state.

## Logo overrides

`logo-overrides.json` in the selected data root maps project paths to relative image paths:

```json
{
  "schemaVersion": 1,
  "projects": {
    "D:/Projects/Example": "logos/example.png",
    "D:/Projects/Example-Data": "logos/example.png"
  }
}
```

An override takes precedence over discovery. Without one, the app searches conventional paths inside the project: `assets/logo.png`, `public/logo.png`, Tauri icons, favicons, and supported nested frontend roots. Images must be supported, no larger than 512 KiB, and confined to the selected data root (or project root for discovery). SVG validation remains in effect. Logos and mappings are private data, never bundled frontend assets. Overrides are read when cards load; restart or reload the view after editing them.
