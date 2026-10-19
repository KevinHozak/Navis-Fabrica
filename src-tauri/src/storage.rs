use serde::Deserialize;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use tauri::{AppHandle, Manager};

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Location {
    schema_version: u32,
    data_root: PathBuf,
}

fn configured_root(config: &Path) -> Result<Option<PathBuf>, String> {
    let file = match fs::File::open(config.join("storage-location.json")) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(format!("Could not read storage location: {error}")),
    };
    let mut bytes = Vec::new();
    file.take(16_385)
        .read_to_end(&mut bytes)
        .map_err(|e| e.to_string())?;
    if bytes.len() > 16_384 {
        return Err("Storage location exceeds 16 KiB.".into());
    }
    let location: Location =
        serde_json::from_slice(&bytes).map_err(|e| format!("Invalid storage location: {e}"))?;
    if location.schema_version != 1 || !location.data_root.is_absolute() {
        return Err("Storage location requires schemaVersion 1 and an absolute dataRoot.".into());
    }
    let root = location
        .data_root
        .canonicalize()
        .map_err(|e| format!("Configured Fabrica data folder is unavailable: {e}"))?;
    if !root.is_dir() {
        return Err("Configured Fabrica data root is not a directory.".into());
    }
    Ok(Some(root))
}

pub fn config_root(app: &AppHandle) -> Result<PathBuf, String> {
    let legacy = app.path().app_config_dir().map_err(|e| e.to_string())?;
    Ok(configured_root(&legacy)?.unwrap_or(legacy))
}

pub fn data_root(app: &AppHandle) -> Result<PathBuf, String> {
    let config = app.path().app_config_dir().map_err(|e| e.to_string())?;
    match configured_root(&config)? {
        Some(root) => Ok(root),
        None => app.path().app_data_dir().map_err(|e| e.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_external_storage_and_preserves_legacy_without_locator() {
        let config = tempfile::tempdir().unwrap();
        let data = tempfile::tempdir().unwrap();
        assert_eq!(configured_root(config.path()).unwrap(), None);
        fs::write(
            config.path().join("storage-location.json"),
            serde_json::to_vec(&serde_json::json!({"schemaVersion": 1, "dataRoot": data.path()}))
                .unwrap(),
        )
        .unwrap();
        assert_eq!(
            configured_root(config.path()).unwrap(),
            Some(data.path().canonicalize().unwrap())
        );
    }

    #[test]
    fn rejects_invalid_or_unavailable_location_without_falling_back() {
        let config = tempfile::tempdir().unwrap();
        for value in [
            serde_json::json!({"schemaVersion": 1, "dataRoot": "relative"}),
            serde_json::json!({"schemaVersion": 2, "dataRoot": config.path()}),
            serde_json::json!({"schemaVersion": 1, "dataRoot": config.path().join("missing")}),
        ] {
            fs::write(
                config.path().join("storage-location.json"),
                serde_json::to_vec(&value).unwrap(),
            )
            .unwrap();
            assert!(configured_root(config.path()).is_err());
        }
    }
}
