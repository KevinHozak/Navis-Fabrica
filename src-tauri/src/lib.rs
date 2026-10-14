use notify::{Event, RecursiveMode, Watcher};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::fs;
use std::io::Write;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant};
use tauri::{AppHandle, Emitter, Manager};
use tempfile::NamedTempFile;

pub mod providers;
mod scanner;

static ACTIVE_SCANS: OnceLock<Mutex<HashMap<String, Arc<AtomicBool>>>> = OnceLock::new();
static PROJECT_FACT_CACHE: OnceLock<Mutex<HashMap<String, scanner::ProjectEntry>>> =
    OnceLock::new();
static PROJECT_CACHE_EPOCH: AtomicU64 = AtomicU64::new(0);
static WATCH_COMMANDS: OnceLock<Sender<WatchCommand>> = OnceLock::new();

enum WatchCommand {
    Event(Result<Event, String>),
    Root {
        path: PathBuf,
        exclusions: Vec<String>,
    },
}

fn active_scans() -> &'static Mutex<HashMap<String, Arc<AtomicBool>>> {
    ACTIVE_SCANS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn project_fact_cache() -> &'static Mutex<HashMap<String, scanner::ProjectEntry>> {
    PROJECT_FACT_CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

const SETTINGS_SCHEMA_VERSION: u32 = 3;
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
    pub github_accounts: Vec<GitHubAccount>,
    pub selected_github_account_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GitHubAccount {
    pub id: String,
    pub login: String,
    pub credential_ref: Option<String>,
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
            github_accounts: Vec::new(),
            selected_github_account_id: None,
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
    #[serde(default)]
    github_accounts: Vec<GitHubAccount>,
    #[serde(default)]
    selected_github_account_id: Option<String>,
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
    // Profile validity is independent of the remembered selection. Preserve
    // usable profiles when a stale provider/model selection needs to recover.
    let profiles_valid =
        providers::validate_profiles(&provider_profiles, &providers::ProviderSelection::default())
            .is_ok();
    let (providers, provider_selection) = if profiles_valid {
        let selection =
            if providers::validate_profiles(&provider_profiles, &provider_selection).is_ok() {
                provider_selection
            } else {
                defaults.provider_selection
            };
        (provider_profiles, selection)
    } else {
        (defaults.providers, defaults.provider_selection)
    };

    let github_accounts = object
        .get("githubAccounts")
        .cloned()
        .and_then(|value| serde_json::from_value::<Vec<GitHubAccount>>(value).ok())
        .filter(|accounts| validate_github_accounts(accounts, None).is_ok())
        .unwrap_or_default();
    let selected_github_account_id = object
        .get("selectedGithubAccountId")
        .and_then(Value::as_str)
        .filter(|id| github_accounts.iter().any(|account| &account.id == *id))
        .map(str::to_owned);

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
        github_accounts,
        selected_github_account_id,
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
    validate_github_accounts(
        &settings.github_accounts,
        settings.selected_github_account_id.as_deref(),
    )?;
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
        github_accounts: settings.github_accounts,
        selected_github_account_id: settings.selected_github_account_id,
    };
    if !(1..=8).contains(&settings.preferences.scan_depth) {
        return Err("Scan depth must be between 1 and 8.".into());
    }

    write_settings_atomic(&settings_path(&app)?, &settings)?;
    if let Some(sender) = WATCH_COMMANDS.get() {
        let _ = sender.send(WatchCommand::Root {
            path: PathBuf::from(&settings.project_root),
            exclusions: settings.exclusions.clone(),
        });
    }
    Ok(settings)
}

fn validate_github_accounts(
    accounts: &[GitHubAccount],
    selected: Option<&str>,
) -> Result<(), String> {
    if accounts.len() > 20 {
        return Err("Configure 20 or fewer GitHub accounts.".into());
    }
    let mut ids = Vec::new();
    for account in accounts {
        if account.id.trim().is_empty()
            || account.id.len() > 80
            || account.login.trim().is_empty()
            || account.login.len() > 100
            || account.id.chars().any(char::is_control)
            || account.login.chars().any(char::is_control)
            || ids.iter().any(|id: &String| id == &account.id)
        {
            return Err("GitHub account IDs and logins must be valid and unique.".into());
        }
        ids.push(account.id.clone());
        if account.credential_ref.as_ref().is_some_and(|value| {
            let key = value
                .strip_prefix("credential-store:")
                .or_else(|| value.strip_prefix("provider-managed:"));
            value.len() > 260
                || value.chars().any(char::is_control)
                || key.is_none_or(|key| {
                    key.is_empty()
                        || !key
                            .chars()
                            .all(|c| c.is_ascii_alphanumeric() || ".:/_-".contains(c))
                })
        }) {
            return Err("Use an opaque credential-store: or provider-managed: reference; tokens are not accepted.".into());
        }
    }
    if selected.is_some_and(|id| !accounts.iter().any(|account| account.id == id)) {
        return Err("Select a configured GitHub account.".into());
    }
    Ok(())
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
fn start_project_scan(app: AppHandle, scan_id: String, force_refresh: bool) -> Result<(), String> {
    if scan_id.is_empty()
        || scan_id.len() > 80
        || !scan_id
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || character == '-')
    {
        return Err("The scan identifier is invalid.".into());
    }
    let settings = load_settings(&settings_path(&app)?)?;
    if force_refresh {
        PROJECT_CACHE_EPOCH.fetch_add(1, Ordering::Relaxed);
        project_fact_cache()
            .lock()
            .map_err(|_| "Could not access cached project facts.".to_string())?
            .clear();
    }
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
        let cache_epoch = PROJECT_CACHE_EPOCH.load(Ordering::Relaxed);
        let cached_projects = project_fact_cache()
            .lock()
            .map(|cache| cache.clone())
            .unwrap_or_default();
        let result = scanner::scan(
            &scan_id,
            Path::new(&settings.project_root),
            &options,
            &cancellation,
            &cached_projects,
            |progress| {
                let _ = app.emit("project-scan-progress", progress);
            },
        );
        if !result.cancelled && result.fatal_error.is_none() {
            if let Ok(mut cache) = project_fact_cache().lock() {
                if PROJECT_CACHE_EPOCH.load(Ordering::Relaxed) == cache_epoch {
                    if result.errors.is_empty() {
                        cache.clear();
                    }
                    for project in &result.projects {
                        cache.insert(project.id.clone(), project.clone());
                    }
                }
            }
        }
        if let Ok(mut scans) = active_scans().lock() {
            scans.remove(&scan_id);
        }
        let _ = app.emit("project-scan-finished", result);
    });
    Ok(())
}

fn invalidate_project_facts(paths: &[PathBuf]) {
    PROJECT_CACHE_EPOCH.fetch_add(1, Ordering::Relaxed);
    let Ok(mut cache) = project_fact_cache().lock() else {
        return;
    };
    cache.retain(|_, project| {
        let project_path = normalize_path(&PathBuf::from(&project.path));
        !paths.iter().any(|changed| {
            let changed = normalize_path(changed);
            paths_overlap(&changed, &project_path)
        })
    });
}

fn normalize_path(path: &Path) -> String {
    let value = path.to_string_lossy().replace('\\', "/");
    let value = value.strip_prefix("//?/").unwrap_or(&value);
    if cfg!(windows) {
        value.to_lowercase()
    } else {
        value.to_owned()
    }
}

fn watcher_path_excluded(path: &Path, root: &Path, exclusions: &[String]) -> bool {
    let path = normalize_path(path);
    let root = normalize_path(root).trim_end_matches('/').to_owned();
    let Some(relative) = path.strip_prefix(&format!("{root}/")) else {
        return false;
    };
    let relative = relative.to_lowercase();
    exclusions.iter().any(|excluded| {
        let excluded = excluded.replace('\\', "/").trim_matches('/').to_lowercase();
        // Git metadata changes must refresh repository facts even though the walker skips .git.
        if excluded.is_empty() || excluded == ".git" {
            return false;
        }
        relative == excluded
            || relative
                .strip_prefix(&excluded)
                .is_some_and(|suffix| suffix.starts_with('/'))
            || relative.split('/').any(|component| component == excluded)
    })
}

fn paths_overlap(first: &str, second: &str) -> bool {
    let first = first.trim_end_matches('/');
    let second = second.trim_end_matches('/');
    first == second
        || first
            .strip_prefix(second)
            .is_some_and(|suffix| suffix.starts_with('/'))
        || second
            .strip_prefix(first)
            .is_some_and(|suffix| suffix.starts_with('/'))
}

fn start_project_watcher(app: AppHandle, root: PathBuf, exclusions: Vec<String>) {
    let (sender, receiver) = mpsc::channel::<WatchCommand>();
    if WATCH_COMMANDS.set(sender.clone()).is_err() {
        return;
    }
    let event_sender = sender.clone();
    thread::spawn(move || {
        let mut watcher = match notify::recommended_watcher(move |event: notify::Result<Event>| {
            let message = event.map_err(|error| error.to_string());
            let _ = event_sender.send(WatchCommand::Event(message));
        }) {
            Ok(watcher) => watcher,
            Err(error) => {
                let _ = app.emit("project-watch-error", error.to_string());
                return;
            }
        };
        let mut watched_exclusions = exclusions;
        let mut watched_root = None;
        let set_root = |watcher: &mut notify::RecommendedWatcher,
                        watched_root: &mut Option<PathBuf>,
                        next: PathBuf| {
            if let Some(previous) = watched_root.take() {
                let _ = watcher.unwatch(&previous);
            }
            match watcher.watch(&next, RecursiveMode::Recursive) {
                Ok(()) => {
                    *watched_root = Some(next);
                    let _ = app.emit("project-watch-error", "");
                }
                Err(error) => {
                    let _ = app.emit("project-watch-error", error.to_string());
                }
            }
        };
        set_root(&mut watcher, &mut watched_root, root);

        while let Ok(command) = receiver.recv() {
            match command {
                WatchCommand::Root { path, exclusions } => {
                    watched_exclusions = exclusions;
                    set_root(&mut watcher, &mut watched_root, path);
                }
                WatchCommand::Event(Err(error)) => {
                    let _ = app.emit("project-watch-error", error);
                }
                WatchCommand::Event(Ok(event)) => {
                    let mut changed_paths = event.paths;
                    let mut deadline = Instant::now() + Duration::from_millis(350);
                    loop {
                        let remaining = deadline.saturating_duration_since(Instant::now());
                        if remaining.is_zero() {
                            break;
                        }
                        match receiver.recv_timeout(remaining) {
                            Ok(WatchCommand::Event(Ok(next))) => {
                                changed_paths.extend(next.paths);
                                deadline = Instant::now() + Duration::from_millis(350);
                            }
                            Ok(WatchCommand::Event(Err(error))) => {
                                let _ = app.emit("project-watch-error", error);
                            }
                            Ok(WatchCommand::Root { path, exclusions }) => {
                                watched_exclusions = exclusions;
                                set_root(&mut watcher, &mut watched_root, path);
                                changed_paths.clear();
                                break;
                            }
                            Err(RecvTimeoutError::Timeout) => break,
                            Err(RecvTimeoutError::Disconnected) => return,
                        }
                    }
                    if let Some(root) = watched_root.as_deref() {
                        changed_paths
                            .retain(|path| !watcher_path_excluded(path, root, &watched_exclusions));
                    }
                    if !changed_paths.is_empty() {
                        invalidate_project_facts(&changed_paths);
                        let _ = app.emit("project-scan-invalidated", ());
                    }
                }
            }
        }
    });
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
        .setup(|app| {
            let handle = app.handle().clone();
            let settings = settings_path(&handle)
                .ok()
                .and_then(|path| load_settings(&path).ok())
                .unwrap_or_default();
            start_project_watcher(
                handle,
                PathBuf::from(settings.project_root),
                settings.exclusions,
            );
            Ok(())
        })
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
    fn watcher_ignores_excluded_folders_but_keeps_git_metadata_events() {
        let root = Path::new(r"C:\Projects");
        let exclusions = vec![".git".into(), "node_modules".into(), "target".into()];
        assert!(watcher_path_excluded(
            Path::new(r"C:\Projects\app\node_modules\pkg\index.js"),
            root,
            &exclusions
        ));
        assert!(watcher_path_excluded(
            Path::new(r"\\?\C:\Projects\app\target\debug\app.exe"),
            root,
            &exclusions
        ));
        assert!(!watcher_path_excluded(
            Path::new(r"C:\Projects\app\.git\index"),
            root,
            &exclusions
        ));
        assert!(!watcher_path_excluded(
            Path::new(r"C:\Projects\app\src\main.rs"),
            root,
            &exclusions
        ));
    }

    #[test]
    fn defaults_are_schema_versioned_and_windows_friendly() {
        let settings = Settings::default();
        assert_eq!(settings.schema_version, 3);
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
    fn preserves_valid_provider_profiles_when_selection_is_stale() {
        let value = serde_json::json!({
            "schemaVersion": 2,
            "providers": [{
                "id": "codex-local",
                "kind": "codex",
                "name": "Codex",
                "executable": "codex",
                "secretRef": "credential-store:codex-session",
                "models": [{ "id": "gpt", "name": "GPT", "reasoningEfforts": ["medium"] }]
            }],
            "providerSelection": {
                "providerId": "removed-provider",
                "modelId": "removed-model",
                "reasoningEffort": "high",
                "runtimeId": null
            }
        });

        let settings = recover_settings(value);
        assert_eq!(settings.providers.len(), 1);
        assert_eq!(settings.providers[0].id, "codex-local");
        assert_eq!(
            settings.provider_selection,
            providers::ProviderSelection::default()
        );
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
        fs::write(&path, r#"{"schemaVersion":4,"projectRoot":"C:\\Projects"}"#).unwrap();
        assert!(load_settings(&path).unwrap_err().contains("newer version"));
        assert_eq!(
            fs::read_to_string(path).unwrap(),
            r#"{"schemaVersion":4,"projectRoot":"C:\\Projects"}"#
        );
    }

    #[test]
    fn github_account_settings_accept_only_opaque_references_and_valid_selection() {
        let accounts = vec![GitHubAccount {
            id: "personal".into(),
            login: "kevin".into(),
            credential_ref: Some("credential-store:github-personal".into()),
        }];
        assert!(validate_github_accounts(&accounts, Some("personal")).is_ok());
        assert!(validate_github_accounts(&accounts, Some("missing")).is_err());
        let mut secret_value = accounts.clone();
        secret_value[0].credential_ref = Some("ghp_exampletoken".into());
        assert!(validate_github_accounts(&secret_value, None).is_err());

        let mut settings = Settings::default();
        settings.github_accounts = accounts;
        settings.selected_github_account_id = Some("personal".into());
        let recovered = recover_settings(serde_json::to_value(settings).unwrap());
        assert_eq!(recovered.github_accounts[0].login, "kevin");
        assert_eq!(
            recovered.selected_github_account_id.as_deref(),
            Some("personal")
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
