use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::fs;
use std::io::Write;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex, OnceLock};
use tauri::{AppHandle, Emitter, Manager};
use tempfile::NamedTempFile;

pub mod providers;
mod scanner;

static ACTIVE_SCANS: OnceLock<Mutex<HashMap<String, Arc<AtomicBool>>>> = OnceLock::new();

fn active_scans() -> &'static Mutex<HashMap<String, Arc<AtomicBool>>> {
    ACTIVE_SCANS.get_or_init(|| Mutex::new(HashMap::new()))
}

const SETTINGS_SCHEMA_VERSION: u32 = 2;
const SETTINGS_FILE: &str = "settings.json";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Preferences {
    pub include_hidden: bool,
    pub scan_depth: u8,
}

impl Default for Preferences {
    fn default() -> Self {
        Self {
            include_hidden: false,
            scan_depth: 3,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Settings {
    pub schema_version: u32,
    pub project_root: String,
    pub exclusions: Vec<String>,
    pub preferences: Preferences,
    pub providers: Vec<providers::ProviderProfile>,
    pub provider_selection: providers::ProviderSelection,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            schema_version: SETTINGS_SCHEMA_VERSION,
            project_root: String::from(r"C:\Projects"),
            exclusions: vec![
                String::from(".git"),
                String::from(".worktrees"),
                String::from("node_modules"),
                String::from("target"),
            ],
            preferences: Preferences::default(),
            providers: Vec::new(),
            provider_selection: providers::ProviderSelection::default(),
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct SettingsInput {
    project_root: String,
    exclusions: Vec<String>,
    preferences: PreferencesInput,
    #[serde(default)]
    providers: Vec<providers::ProviderProfile>,
    #[serde(default)]
    provider_selection: providers::ProviderSelection,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct PreferencesInput {
    include_hidden: bool,
    scan_depth: u8,
}

fn validate_project_root(value: &str) -> Result<String, String> {
    let root = value.trim();
    if root.is_empty() || root.len() > 32_767 || root.chars().any(char::is_control) {
        return Err("Enter a valid absolute folder path.".into());
    }

    let path = Path::new(root);
    if !path.is_absolute()
        || path
            .components()
            .any(|part| matches!(part, Component::ParentDir))
    {
        return Err(
            "The project root must be an absolute path without parent-folder segments.".into(),
        );
    }

    if let Some(prefix) = path.components().find_map(|part| match part {
        Component::Prefix(prefix) => Some(prefix),
        _ => None,
    }) {
        if prefix.kind().is_verbatim() {
            return Err("Device namespace paths are not supported.".into());
        }
    }

    if path.exists() && !path.is_dir() {
        return Err("The project root must point to a folder.".into());
    }

    Ok(path.to_string_lossy().into_owned())
}

fn validate_exclusions(values: Vec<String>) -> Result<Vec<String>, String> {
    if values.len() > 200 {
        return Err("Use 200 or fewer excluded folders.".into());
    }

    let mut exclusions = Vec::with_capacity(values.len());
    for value in values {
        let entry = value.trim();
        if entry.is_empty() {
            continue;
        }
        let path = Path::new(entry);
        if entry.len() > 260
            || entry.chars().any(char::is_control)
            || path.is_absolute()
            || path.components().any(|part| {
                matches!(
                    part,
                    Component::ParentDir | Component::RootDir | Component::Prefix(_)
                )
            })
        {
            return Err(
                "Excluded folders must be relative paths without parent-folder segments.".into(),
            );
        }
        if !exclusions
            .iter()
            .any(|existing: &String| existing.eq_ignore_ascii_case(entry))
        {
            exclusions.push(entry.to_owned());
        }
    }
    Ok(exclusions)
}

fn recover_settings(value: Value) -> Settings {
    let defaults = Settings::default();
    let Some(object) = value.as_object() else {
        return defaults;
    };

    let project_root = object
        .get("projectRoot")
        .and_then(Value::as_str)
        .and_then(|candidate| validate_project_root(candidate).ok())
        .unwrap_or(defaults.project_root);

    let exclusions = match object.get("exclusions").and_then(Value::as_array) {
        Some(entries) => {
            let mut valid_entries = Vec::with_capacity(entries.len().min(200));
            for entry in entries.iter().take(200).filter_map(Value::as_str) {
                if let Ok(mut validated) = validate_exclusions(vec![entry.to_owned()]) {
                    if let Some(entry) = validated.pop() {
                        valid_entries.push(entry);
                    }
                }
            }
            validate_exclusions(valid_entries).unwrap_or(defaults.exclusions)
        }
        None => defaults.exclusions,
    };

    let preference_value = object.get("preferences").and_then(Value::as_object);
    let include_hidden = preference_value
        .and_then(|preferences| preferences.get("includeHidden"))
        .and_then(Value::as_bool)
        .unwrap_or(defaults.preferences.include_hidden);
    let scan_depth = preference_value
        .and_then(|preferences| preferences.get("scanDepth"))
        .and_then(Value::as_u64)
        .filter(|depth| (1..=8).contains(depth))
        .map(|depth| depth as u8)
        .unwrap_or(defaults.preferences.scan_depth);

    let provider_profiles = object
        .get("providers")
        .cloned()
        .and_then(|value| serde_json::from_value::<Vec<providers::ProviderProfile>>(value).ok())
        .unwrap_or_default();
    let provider_selection = object
        .get("providerSelection")
        .cloned()
        .and_then(|value| serde_json::from_value::<providers::ProviderSelection>(value).ok())
        .unwrap_or_default();
    let (providers, provider_selection) =
        if providers::validate_profiles(&provider_profiles, &provider_selection).is_ok() {
            (provider_profiles, provider_selection)
        } else {
            (defaults.providers, defaults.provider_selection)
        };

    Settings {
        schema_version: SETTINGS_SCHEMA_VERSION,
        project_root,
        exclusions,
        preferences: Preferences {
            include_hidden,
            scan_depth,
        },
        providers,
        provider_selection,
    }
}

fn load_settings(path: &Path) -> Result<Settings, String> {
    match fs::read(path) {
        Ok(contents) => match serde_json::from_slice::<Value>(&contents) {
            Ok(value) => {
                let stored_version = value
                    .get("schemaVersion")
                    .and_then(Value::as_u64)
                    .unwrap_or(0);
                if stored_version > u64::from(SETTINGS_SCHEMA_VERSION) {
                    return Err(
                        "These settings were saved by a newer version of Navis-Fabrica.".into(),
                    );
                }
                Ok(recover_settings(value))
            }
            Err(_) => Ok(Settings::default()),
        },
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Settings::default()),
        Err(error) => Err(format!("Could not read settings: {error}")),
    }
}

fn write_settings_atomic(path: &Path, settings: &Settings) -> Result<(), String> {
    let parent = path
        .parent()
        .ok_or_else(|| "Settings location has no parent folder.".to_string())?;
    fs::create_dir_all(parent)
        .map_err(|error| format!("Could not create settings folder: {error}"))?;

    let contents = serde_json::to_vec_pretty(settings)
        .map_err(|error| format!("Could not encode settings: {error}"))?;
    let mut temporary = NamedTempFile::new_in(parent)
        .map_err(|error| format!("Could not prepare settings file: {error}"))?;
    temporary
        .write_all(&contents)
        .and_then(|()| temporary.as_file().sync_all())
        .map_err(|error| format!("Could not write settings: {error}"))?;
    replace_settings_file(temporary, path)?;
    Ok(())
}

#[cfg(windows)]
fn replace_settings_file(temporary: NamedTempFile, destination: &Path) -> Result<(), String> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::ReplaceFileW;

    let (file, temporary_path) = temporary
        .keep()
        .map_err(|error| format!("Could not prepare settings replacement: {}", error.error))?;
    drop(file);
    let source: Vec<u16> = temporary_path
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect();
    let target: Vec<u16> = destination
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect();

    let result = match fs::rename(&temporary_path, destination) {
        Ok(()) => Ok(()),
        Err(_rename_error) if destination.exists() => {
            let replaced = unsafe {
                ReplaceFileW(
                    target.as_ptr(),
                    source.as_ptr(),
                    std::ptr::null(),
                    0,
                    std::ptr::null(),
                    std::ptr::null_mut(),
                )
            };
            if replaced != 0 {
                Ok(())
            } else {
                Err(std::io::Error::last_os_error())
            }
        }
        Err(rename_error) => Err(rename_error),
    };

    if let Err(error) = result {
        let _ = fs::remove_file(&temporary_path);
        return Err(format!("Could not replace settings file: {error}"));
    }
    Ok(())
}

#[cfg(not(windows))]
fn replace_settings_file(temporary: NamedTempFile, destination: &Path) -> Result<(), String> {
    temporary
        .persist(destination)
        .map_err(|error| format!("Could not replace settings file: {}", error.error))?;
    Ok(())
}

fn settings_path(app: &AppHandle) -> Result<PathBuf, String> {
    let folder = app
        .path()
        .app_config_dir()
        .map_err(|error| format!("Could not locate app settings folder: {error}"))?;
    Ok(folder.join(SETTINGS_FILE))
}

#[tauri::command]
fn get_settings(app: AppHandle) -> Result<Settings, String> {
    load_settings(&settings_path(&app)?)
}

#[tauri::command]
fn save_settings(app: AppHandle, settings: SettingsInput) -> Result<Settings, String> {
    providers::validate_profiles(&settings.providers, &settings.provider_selection)?;
    let settings = Settings {
        schema_version: SETTINGS_SCHEMA_VERSION,
        project_root: validate_project_root(&settings.project_root)?,
        exclusions: validate_exclusions(settings.exclusions)?,
        preferences: Preferences {
            include_hidden: settings.preferences.include_hidden,
            scan_depth: settings.preferences.scan_depth,
        },
        providers: settings.providers,
        provider_selection: settings.provider_selection,
    };
    if !(1..=8).contains(&settings.preferences.scan_depth) {
        return Err("Scan depth must be between 1 and 8.".into());
    }

    write_settings_atomic(&settings_path(&app)?, &settings)?;
    Ok(settings)
}

#[tauri::command]
fn inspect_providers(
    profiles: Vec<providers::ProviderProfile>,
) -> Result<Vec<providers::ProviderAvailability>, String> {
    providers::validate_profiles(&profiles, &providers::ProviderSelection::default())?;
    let path = std::env::var_os("PATH");
    Ok(profiles
        .iter()
        .map(|profile| providers::inspect(profile, path.as_deref()))
        .collect())
}

#[tauri::command]
fn start_project_scan(app: AppHandle, scan_id: String) -> Result<(), String> {
    if scan_id.is_empty()
        || scan_id.len() > 80
        || !scan_id
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || character == '-')
    {
        return Err("The scan identifier is invalid.".into());
    }
    let settings = load_settings(&settings_path(&app)?)?;
    let cancellation = Arc::new(AtomicBool::new(false));
    {
        let mut scans = active_scans()
            .lock()
            .map_err(|_| "Could not access active scans.".to_string())?;
        if !scans.is_empty() {
            return Err("A project scan is already running.".into());
        }
        scans.insert(scan_id.clone(), cancellation.clone());
    }

    std::thread::spawn(move || {
        let options = scanner::ScanOptions {
            depth: settings.preferences.scan_depth,
            include_hidden: settings.preferences.include_hidden,
            exclusions: settings.exclusions,
        };
        let result = scanner::scan(
            &scan_id,
            Path::new(&settings.project_root),
            &options,
            &cancellation,
            |progress| {
                let _ = app.emit("project-scan-progress", progress);
            },
        );
        if let Ok(mut scans) = active_scans().lock() {
            scans.remove(&scan_id);
        }
        let _ = app.emit("project-scan-finished", result);
    });
    Ok(())
}

#[tauri::command]
fn cancel_project_scan(scan_id: String) -> Result<bool, String> {
    let scans = active_scans()
        .lock()
        .map_err(|_| "Could not access active scans.".to_string())?;
    if let Some(cancellation) = scans.get(&scan_id) {
        cancellation.store(true, std::sync::atomic::Ordering::Relaxed);
        Ok(true)
    } else {
        Ok(false)
    }
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .invoke_handler(tauri::generate_handler![
            get_settings,
            save_settings,
            inspect_providers,
            start_project_scan,
            cancel_project_scan
        ])
        .run(tauri::generate_context!())
        .expect("error while running Navis-Fabrica");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_schema_versioned_and_windows_friendly() {
        let settings = Settings::default();
        assert_eq!(settings.schema_version, 2);
        assert_eq!(settings.project_root, r"C:\Projects");
        assert_eq!(settings.preferences.scan_depth, 3);
    }

    #[test]
    fn rejects_relative_and_parent_segment_roots() {
        assert!(validate_project_root("projects").is_err());
        assert!(validate_project_root(r"C:\Projects\..\Users").is_err());
    }

    #[test]
    fn recovers_valid_fields_when_other_fields_are_malformed() {
        let value = serde_json::json!({
            "schemaVersion": 1,
            "projectRoot": r"D:\Workspaces",
            "exclusions": [".git", { "invalid": true }, "..\\outside"],
            "preferences": { "includeHidden": true, "scanDepth": 99 }
        });
        let settings = recover_settings(value);
        assert_eq!(settings.project_root, r"D:\Workspaces");
        assert_eq!(settings.exclusions, vec![".git"]);
        assert!(settings.preferences.include_hidden);
        assert_eq!(settings.preferences.scan_depth, 3);
    }

    #[test]
    fn invalid_exclusions_cannot_escape_the_root() {
        assert!(validate_exclusions(vec![String::from(r"..\outside")]).is_err());
        assert_eq!(
            validate_exclusions(vec![String::from(".git"), String::from(".GIT")]).unwrap(),
            vec![".git"]
        );
    }

    #[test]
    fn settings_write_is_reopenable_and_replaces_prior_values() {
        let folder = tempfile::tempdir_in(env!("CARGO_MANIFEST_DIR")).unwrap();
        let path = folder.path().join(SETTINGS_FILE);
        let mut settings = Settings::default();
        write_settings_atomic(&path, &settings).unwrap();
        settings.preferences.include_hidden = true;
        write_settings_atomic(&path, &settings).unwrap();
        assert_eq!(load_settings(&path).unwrap(), settings);
    }

    #[test]
    fn leaves_settings_from_a_newer_schema_untouched() {
        let folder = tempfile::tempdir_in(env!("CARGO_MANIFEST_DIR")).unwrap();
        let path = folder.path().join(SETTINGS_FILE);
        fs::write(&path, r#"{"schemaVersion":3,"projectRoot":"C:\\Projects"}"#).unwrap();
        assert!(load_settings(&path).unwrap_err().contains("newer version"));
        assert_eq!(
            fs::read_to_string(path).unwrap(),
            r#"{"schemaVersion":3,"projectRoot":"C:\\Projects"}"#
        );
    }

    #[test]
    fn rejects_invalid_scan_identifiers() {
        assert!(!"bad/scan"
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || character == '-'));
        assert!("a"
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || character == '-'));
    }
}
