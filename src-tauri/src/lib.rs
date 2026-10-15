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
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tauri::{AppHandle, Emitter, Manager};
use tempfile::NamedTempFile;

mod github;
mod inventory;
pub mod jev;
mod process;
pub mod providers;
mod routing;
mod runs;
mod scanner;
mod usage;

static ACTIVE_SCANS: OnceLock<Mutex<HashMap<String, Arc<AtomicBool>>>> = OnceLock::new();
static PROJECT_FACT_CACHE: OnceLock<Mutex<HashMap<String, scanner::ProjectEntry>>> =
    OnceLock::new();
static PROJECT_CACHE_EPOCH: AtomicU64 = AtomicU64::new(0);
static PROJECT_CACHE_RESTORED: AtomicBool = AtomicBool::new(false);
static WATCH_COMMANDS: OnceLock<Sender<WatchCommand>> = OnceLock::new();
static GITHUB_ACTIVITY_CACHE: OnceLock<Mutex<HashMap<String, github::GitHubActivity>>> =
    OnceLock::new();
static APP_STARTUP_MS: AtomicU64 = AtomicU64::new(0);

pub fn startup_measurement_ms() -> Option<u64> {
    match APP_STARTUP_MS.load(Ordering::Acquire) {
        0 => None,
        value => Some(value),
    }
}

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

fn github_activity_cache() -> &'static Mutex<HashMap<String, github::GitHubActivity>> {
    GITHUB_ACTIVITY_CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

fn github_cache_key(
    owner: &str,
    repository: &str,
    account_id: Option<&str>,
    credential_ref: Option<&str>,
) -> String {
    format!(
        "{}/{}@{}:{}",
        owner.to_ascii_lowercase(),
        repository.to_ascii_lowercase(),
        account_id.unwrap_or("public"),
        credential_ref.unwrap_or("none")
    )
}

fn can_restore_github_cache(authentication: &str, credential_available: bool) -> bool {
    authentication != "available" || credential_available
}

#[tauri::command]
fn get_cached_github_activity(
    app: AppHandle,
    owner: String,
    repository: String,
) -> Result<github::GitHubActivity, String> {
    if !github::valid_repository_name(&owner, &repository) {
        return Err("The GitHub repository identity is invalid.".into());
    }
    let settings = load_settings(&settings_path(&app)?)?;
    let account = settings
        .selected_github_account_id
        .as_deref()
        .and_then(|id| {
            settings
                .github_accounts
                .iter()
                .find(|account| account.id == id)
        });
    let account_id = account.map(|item| item.id.as_str());
    let credential_ref = account.and_then(|item| item.credential_ref.as_deref());
    let credential_available = credential_ref.and_then(resolve_github_credential).is_some();
    let cache = github_activity_cache()
        .lock()
        .map_err(|_| "Could not access GitHub activity cache.".to_string())?;
    let cached = cache.get(&github_cache_key(
        &owner,
        &repository,
        account_id,
        credential_ref,
    ));
    let recent = cached.is_some_and(|entry| {
        entry
            .checked_at
            .as_deref()
            .and_then(|value| value.parse::<u64>().ok())
            .is_some_and(|checked_at| {
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs()
                    .saturating_sub(checked_at)
                    <= 300
            })
    });
    Ok(cached
        .filter(|entry| {
            recent && can_restore_github_cache(&entry.authentication, credential_available)
        })
        .cloned()
        .unwrap_or_else(|| github::GitHubActivity::unchecked(&owner, &repository)))
}

#[tauri::command]
async fn refresh_github_activity(
    app: AppHandle,
    owner: String,
    repository: String,
) -> Result<github::GitHubActivity, String> {
    if !github::valid_repository_name(&owner, &repository) {
        return Err("The GitHub repository identity is invalid.".into());
    }
    let settings = load_settings(&settings_path(&app)?)?;
    let selected_account = settings
        .selected_github_account_id
        .as_deref()
        .and_then(|id| {
            settings
                .github_accounts
                .iter()
                .find(|account| account.id == id)
        });
    let account_id = selected_account.map(|account| account.id.as_str());
    let credential_ref = selected_account.and_then(|account| account.credential_ref.as_deref());
    let token = credential_ref.and_then(resolve_github_credential);
    let key = github_cache_key(&owner, &repository, account_id, credential_ref);
    let mut activity = github::fetch_activity(&owner, &repository, token.as_deref()).await;
    if activity.remote_status != "fresh" {
        let mut cache = github_activity_cache()
            .lock()
            .map_err(|_| "Could not access GitHub activity cache.".to_string())?;
        if activity.remote_status == "unauthorized" || activity.remote_status == "authExpired" {
            cache.remove(&key);
            return Ok(activity);
        }
        if let Some(previous) = cache
            .get(&key)
            .filter(|entry| can_restore_github_cache(&entry.authentication, token.is_some()))
        {
            activity.issue_count = previous.issue_count;
            activity.pull_request_count = previous.pull_request_count;
            activity.recent_commits = previous.recent_commits.clone();
            activity.checked_at = previous.checked_at.clone();
        }
        return Ok(activity);
    }
    let mut cache = github_activity_cache()
        .lock()
        .map_err(|_| "Could not access GitHub activity cache.".to_string())?;
    if cache.len() >= 256 && !cache.contains_key(&key) {
        if let Some(first) = cache.keys().next().cloned() {
            cache.remove(&first);
        }
    }
    cache.insert(key, activity.clone());
    Ok(activity)
}

fn github_token_for_app(app: &AppHandle) -> Result<Option<String>, String> {
    let settings = load_settings(&settings_path(app)?)?;
    let reference = settings
        .selected_github_account_id
        .as_deref()
        .and_then(|id| {
            settings
                .github_accounts
                .iter()
                .find(|account| account.id == id)
        })
        .and_then(|account| account.credential_ref.as_deref());
    Ok(reference.and_then(resolve_github_credential))
}

#[tauri::command]
async fn list_github_issues(
    app: AppHandle,
    owner: String,
    repository: String,
) -> Result<Vec<github::IssueSummary>, String> {
    let token = github_token_for_app(&app)?;
    github::list_issues(&owner, &repository, token.as_deref()).await
}

#[tauri::command]
async fn get_github_issue(
    app: AppHandle,
    owner: String,
    repository: String,
    reference: String,
) -> Result<github::RepositoryIssue, String> {
    let number = github::parse_issue_reference(&owner, &repository, &reference)?;
    let token = github_token_for_app(&app)?;
    github::fetch_issue(&owner, &repository, number, token.as_deref()).await
}

fn resolve_github_credential(reference: &str) -> Option<String> {
    let key = reference.strip_prefix("credential-store:")?;
    if key.is_empty()
        || !key
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || ".:/_-".contains(character))
    {
        return None;
    }
    #[cfg(windows)]
    {
        use std::slice;
        use windows_sys::Win32::Security::Credentials::{
            CredFree, CredReadW, CREDENTIALW, CRED_TYPE_GENERIC,
        };
        let target: Vec<u16> = key.encode_utf16().chain(std::iter::once(0)).collect();
        let mut credential: *mut CREDENTIALW = std::ptr::null_mut();
        let read = unsafe { CredReadW(target.as_ptr(), CRED_TYPE_GENERIC, 0, &mut credential) };
        if read == 0 || credential.is_null() {
            return None;
        }
        let entry = unsafe { &*credential };
        let token = if entry.CredentialBlob.is_null()
            || entry.CredentialBlobSize == 0
            || entry.CredentialBlobSize > 4096
        {
            None
        } else {
            let bytes = unsafe {
                slice::from_raw_parts(entry.CredentialBlob, entry.CredentialBlobSize as usize)
            };
            std::str::from_utf8(bytes)
                .ok()
                .map(str::trim)
                .filter(|value| !value.is_empty() && !value.chars().any(char::is_control))
                .map(str::to_owned)
        };
        unsafe {
            CredFree(credential.cast());
        }
        token
    }
    #[cfg(not(windows))]
    {
        None
    }
}

fn resolve_openrouter_credential(provider_id: &str, reference: Option<&str>) -> Option<String> {
    let reference = reference?;
    if !providers::openrouter_credential_reference_matches(provider_id, reference) {
        return None;
    }
    resolve_github_credential(reference)
}

fn store_openrouter_credential(target: &str, secret: &str) -> Result<(), String> {
    if !providers::valid_openrouter_key(secret) {
        return Err("Enter a valid OpenRouter API key.".into());
    }
    #[cfg(windows)]
    {
        use windows_sys::Win32::Security::Credentials::{
            CredWriteW, CREDENTIALW, CRED_PERSIST_LOCAL_MACHINE, CRED_TYPE_GENERIC,
        };
        let target_wide: Vec<u16> = target.encode_utf16().chain(std::iter::once(0)).collect();
        let mut bytes = secret.trim().as_bytes().to_vec();
        let credential = CREDENTIALW {
            Flags: 0,
            Type: CRED_TYPE_GENERIC,
            TargetName: target_wide.as_ptr() as *mut u16,
            Comment: std::ptr::null_mut(),
            LastWritten: unsafe { std::mem::zeroed() },
            CredentialBlobSize: bytes.len() as u32,
            CredentialBlob: bytes.as_mut_ptr(),
            Persist: CRED_PERSIST_LOCAL_MACHINE,
            AttributeCount: 0,
            Attributes: std::ptr::null_mut(),
            TargetAlias: std::ptr::null_mut(),
            UserName: std::ptr::null_mut(),
        };
        let written = unsafe { CredWriteW(&credential, 0) };
        bytes.fill(0);
        if written == 0 {
            return Err("Windows Credential Manager could not save the OpenRouter key.".into());
        }
        Ok(())
    }
    #[cfg(not(windows))]
    {
        let _ = target;
        Err("OpenRouter credential storage is available only in the Windows desktop app.".into())
    }
}

fn resolve_typesafe_credential(profile_id: &str, reference: Option<&str>) -> Option<String> {
    let reference = reference?;
    if !providers::typesafe_credential_reference_matches(profile_id, reference) {
        return None;
    }
    resolve_github_credential(reference)
}

fn store_typesafe_credential(target: &str, secret: &str) -> Result<(), String> {
    if !providers::valid_typesafe_key(secret) {
        return Err("Enter a valid TypeSafe API key.".into());
    }
    #[cfg(windows)]
    {
        use windows_sys::Win32::Security::Credentials::{
            CredWriteW, CREDENTIALW, CRED_PERSIST_LOCAL_MACHINE, CRED_TYPE_GENERIC,
        };
        let target_wide: Vec<u16> = target.encode_utf16().chain(std::iter::once(0)).collect();
        let mut bytes = secret.trim().as_bytes().to_vec();
        let credential = CREDENTIALW {
            Flags: 0,
            Type: CRED_TYPE_GENERIC,
            TargetName: target_wide.as_ptr() as *mut u16,
            Comment: std::ptr::null_mut(),
            LastWritten: unsafe { std::mem::zeroed() },
            CredentialBlobSize: bytes.len() as u32,
            CredentialBlob: bytes.as_mut_ptr(),
            Persist: CRED_PERSIST_LOCAL_MACHINE,
            AttributeCount: 0,
            Attributes: std::ptr::null_mut(),
            TargetAlias: std::ptr::null_mut(),
            UserName: std::ptr::null_mut(),
        };
        let written = unsafe { CredWriteW(&credential, 0) };
        bytes.fill(0);
        if written == 0 {
            return Err("Windows Credential Manager could not save the TypeSafe key.".into());
        }
        Ok(())
    }
    #[cfg(not(windows))]
    {
        let _ = target;
        Err("TypeSafe credential storage is available only in the Windows desktop app.".into())
    }
}

const SETTINGS_SCHEMA_VERSION: u32 = 4;
const SETTINGS_FILE: &str = "settings.json";
const PROJECT_INVENTORY_FILE: &str = "project-inventory.json";

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
    pub typesafe_profile: Option<providers::TypeSafeProfile>,
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
            typesafe_profile: None,
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
    #[serde(default)]
    typesafe_profile: Option<providers::TypeSafeProfile>,
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
    let typesafe_profile = object
        .get("typesafeProfile")
        .cloned()
        .and_then(|value| serde_json::from_value::<providers::TypeSafeProfile>(value).ok())
        .filter(|profile| {
            providers::typesafe_credential_target(&profile.id).is_ok()
                && profile.credential_ref.as_ref().is_none_or(|reference| {
                    providers::typesafe_credential_reference_matches(&profile.id, reference)
                })
        });

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
        typesafe_profile,
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

fn project_inventory_path(app: &AppHandle) -> Result<PathBuf, String> {
    let folder = app
        .path()
        .app_config_dir()
        .map_err(|error| format!("Could not locate project inventory folder: {error}"))?;
    Ok(folder.join(PROJECT_INVENTORY_FILE))
}

#[tauri::command]
fn get_project_inventory(app: AppHandle) -> Result<Vec<scanner::ProjectEntry>, String> {
    let settings = load_settings(&settings_path(&app)?)?;
    Ok(inventory::load(
        &project_inventory_path(&app)?,
        &settings.project_root,
    ))
}

#[tauri::command]
fn get_settings(app: AppHandle) -> Result<Settings, String> {
    load_settings(&settings_path(&app)?)
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct WorkflowSkillStatus {
    id: String,
    status: &'static str,
    compatible_agents: Vec<&'static str>,
}

const WORKFLOW_SKILL_IDS: [&str; 3] = ["dev-work", "dev-push", "dev-build"];
const WORKFLOW_SKILL_COMPATIBILITY: [(&str, &[&str]); 3] = [
    ("dev-work", &["Codex"]),
    ("dev-push", &["Codex"]),
    ("dev-build", &["Codex"]),
];

fn has_valid_skill_contract(contents: &str, expected_id: &str) -> bool {
    let contents = contents.strip_prefix('\u{feff}').unwrap_or(contents);
    let mut lines = contents.lines();
    if lines.next().map(str::trim) != Some("---") {
        return false;
    }

    let mut name_matches = false;
    let mut has_description = false;
    for line in lines {
        let line = line.trim();
        if line == "---" {
            return name_matches && has_description;
        }
        if let Some((key, value)) = line.split_once(':') {
            match key.trim() {
                "name" => name_matches = value.trim() == expected_id,
                "description" => has_description = !value.trim().is_empty(),
                _ => {}
            }
        }
    }
    false
}

fn workflow_skills_root() -> Option<PathBuf> {
    if let Some(root) = std::env::var_os("CODEX_HOME").filter(|value| !value.is_empty()) {
        return Some(PathBuf::from(root).join("skills"));
    }
    std::env::var_os("USERPROFILE")
        .filter(|value| !value.is_empty())
        .map(|home| PathBuf::from(home).join(".codex").join("skills"))
}

fn workflow_skill_status(skills_root: &Path, id: &str) -> &'static str {
    if !WORKFLOW_SKILL_IDS.contains(&id) {
        return "missing";
    }
    let root = match fs::canonicalize(skills_root) {
        Ok(root) => root,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return "missing",
        Err(_) => return "unavailable",
    };
    let skill_file = skills_root.join(id).join("SKILL.md");
    match fs::metadata(&skill_file) {
        Ok(metadata) if metadata.is_file() => {}
        Ok(_) => return "missing",
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return "missing",
        Err(_) => return "unavailable",
    }
    match fs::canonicalize(&skill_file) {
        Ok(resolved) if resolved.starts_with(root) => match fs::read_to_string(resolved) {
            Ok(contents) if has_valid_skill_contract(&contents, id) => "available",
            Ok(_) => "incompatible",
            Err(_) => "unavailable",
        },
        Ok(_) => "unavailable",
        Err(_) => "unavailable",
    }
}

#[tauri::command]
fn discover_workflow_skills() -> Vec<WorkflowSkillStatus> {
    let root = workflow_skills_root();
    WORKFLOW_SKILL_IDS
        .iter()
        .map(|id| WorkflowSkillStatus {
            id: (*id).to_owned(),
            compatible_agents: WORKFLOW_SKILL_COMPATIBILITY
                .iter()
                .find(|(skill_id, _)| skill_id == id)
                .map(|(_, agents)| agents.to_vec())
                .unwrap_or_default(),
            status: root
                .as_deref()
                .map(|path| workflow_skill_status(path, id))
                .unwrap_or("unavailable"),
        })
        .collect()
}

fn resolved_workflow_skill_path(id: &str) -> Result<PathBuf, String> {
    let skills_root = workflow_skills_root().ok_or("The Codex skill location is unavailable.")?;
    if workflow_skill_status(&skills_root, id) != "available" {
        return Err(format!(
            "The {id} skill is missing or incompatible with Codex."
        ));
    }
    fs::canonicalize(skills_root.join(id).join("SKILL.md"))
        .map_err(|_| format!("The {id} skill file could not be resolved."))
}

#[tauri::command]
fn save_settings(app: AppHandle, settings: SettingsInput) -> Result<Settings, String> {
    providers::validate_profiles(&settings.providers, &settings.provider_selection)?;
    validate_github_accounts(
        &settings.github_accounts,
        settings.selected_github_account_id.as_deref(),
    )?;
    validate_typesafe_profile(settings.typesafe_profile.as_ref())?;
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
        typesafe_profile: settings.typesafe_profile,
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

fn validate_typesafe_profile(profile: Option<&providers::TypeSafeProfile>) -> Result<(), String> {
    if let Some(profile) = profile {
        providers::typesafe_credential_target(&profile.id)?;
        if profile.credential_ref.as_ref().is_some_and(|reference| {
            !providers::typesafe_credential_reference_matches(&profile.id, reference)
        }) {
            return Err("The TypeSafe credential reference does not match this profile.".into());
        }
    }
    Ok(())
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
fn probe_antigravity_cli(
    app: AppHandle,
    provider_id: String,
) -> Result<providers::AntigravityProbe, String> {
    let settings = load_settings(&settings_path(&app)?)?;
    let profile = settings
        .providers
        .iter()
        .find(|profile| profile.id == provider_id)
        .ok_or("The Antigravity provider profile no longer exists.")?;
    if profile.kind != providers::ProviderKind::Antigravity {
        return Err("Choose an Antigravity provider to probe.".into());
    }
    let path = std::env::var_os("PATH");
    let executable = providers::find_executable(&profile.executable, path.as_deref()).ok_or(
        "The configured Antigravity executable is not available on PATH or at its saved path.",
    )?;
    Ok(providers::probe_antigravity(&executable))
}

#[tauri::command]
fn probe_ollama(app: AppHandle, provider_id: String) -> Result<providers::OllamaProbe, String> {
    let settings = load_settings(&settings_path(&app)?)?;
    let profile = settings
        .providers
        .iter()
        .find(|profile| profile.id == provider_id)
        .ok_or("The Ollama provider profile no longer exists.")?;
    if profile.kind != providers::ProviderKind::Ollama {
        return Err("Choose a local Ollama provider to probe.".into());
    }
    providers::validate_profiles(&settings.providers, &settings.provider_selection)?;
    Ok(providers::ollama_probe(
        profile
            .endpoint
            .as_deref()
            .unwrap_or(providers::OLLAMA_DEFAULT_ENDPOINT),
    ))
}

#[tauri::command]
fn save_openrouter_credential(
    app: AppHandle,
    provider_id: String,
    api_key: String,
) -> Result<Settings, String> {
    let mut settings = load_settings(&settings_path(&app)?)?;
    let profile = settings
        .providers
        .iter_mut()
        .find(|profile| profile.id == provider_id)
        .ok_or("The OpenRouter provider profile no longer exists.")?;
    if profile.kind != providers::ProviderKind::OpenRouter {
        return Err("Choose an OpenRouter provider profile.".into());
    }
    let target = providers::openrouter_credential_target(&provider_id)?;
    store_openrouter_credential(&target, &api_key)?;
    profile.secret_ref = Some(format!("credential-store:{target}"));
    write_settings_atomic(&settings_path(&app)?, &settings)?;
    Ok(settings)
}

#[tauri::command]
fn probe_openrouter(
    app: AppHandle,
    provider_id: String,
) -> Result<providers::OpenRouterProbe, String> {
    let settings = load_settings(&settings_path(&app)?)?;
    let profile = settings
        .providers
        .iter()
        .find(|profile| profile.id == provider_id)
        .ok_or("The OpenRouter provider profile no longer exists.")?;
    if profile.kind != providers::ProviderKind::OpenRouter {
        return Err("Choose an OpenRouter provider to probe.".into());
    }
    let key = resolve_openrouter_credential(&profile.id, profile.secret_ref.as_deref())
        .unwrap_or_default();
    Ok(providers::openrouter_http_probe(&key))
}

#[tauri::command]
fn save_typesafe_credential(
    app: AppHandle,
    profile_id: String,
    api_key: String,
) -> Result<Settings, String> {
    let mut settings = load_settings(&settings_path(&app)?)?;
    let profile = settings
        .typesafe_profile
        .as_mut()
        .filter(|profile| profile.id == profile_id)
        .ok_or("The TypeSafe profile no longer exists.")?;
    let target = providers::typesafe_credential_target(&profile_id)?;
    store_typesafe_credential(&target, &api_key)?;
    profile.credential_ref = Some(format!("credential-store:{target}"));
    write_settings_atomic(&settings_path(&app)?, &settings)?;
    Ok(settings)
}

#[tauri::command]
fn probe_typesafe(app: AppHandle) -> Result<providers::TypeSafeProbe, String> {
    let settings = load_settings(&settings_path(&app)?)?;
    let Some(profile) = settings.typesafe_profile.as_ref() else {
        return Ok(providers::TypeSafeProbe {
            state: "unconfigured".into(),
            model: providers::TYPESAFE_JEV_MODEL.into(),
            note: "Add the optional TypeSafe profile to configure connection health.".into(),
        });
    };
    validate_typesafe_profile(Some(profile))?;
    let credential = profile
        .enabled
        .then(|| resolve_typesafe_credential(&profile.id, profile.credential_ref.as_deref()))
        .flatten();
    Ok(providers::typesafe_http_probe(
        profile,
        credential.as_deref(),
    ))
}

#[tauri::command]
fn start_openrouter_workflow_run(
    app: AppHandle,
    request: runs::OpenRouterStartRequest,
) -> Result<runs::RunRecord, String> {
    let settings = load_settings(&settings_path(&app)?)?;
    providers::validate_profiles(&settings.providers, &settings.provider_selection)?;
    let selection = &settings.provider_selection;
    let provider_id = selection
        .provider_id
        .as_deref()
        .ok_or("Choose an OpenRouter provider in Providers first.")?;
    let profile = settings
        .providers
        .iter()
        .find(|profile| profile.id == provider_id)
        .ok_or("The selected provider is no longer configured.")?;
    if profile.kind != providers::ProviderKind::OpenRouter {
        return Err("The selected provider is not OpenRouter.".into());
    }
    if selection.reasoning_effort.is_some() {
        return Err(
            "The Aider OpenRouter adapter does not expose verified reasoning-effort options."
                .into(),
        );
    }
    if selection
        .runtime_id
        .as_deref()
        .is_some_and(|runtime| !runtime.is_empty() && runtime != "aider-openrouter")
    {
        return Err("This phase supports the Aider OpenRouter runtime only.".into());
    }
    let model_id = selection
        .model_id
        .as_deref()
        .ok_or("Discover OpenRouter models and select a model first.")?;
    let probe = probe_openrouter(app.clone(), provider_id.to_owned())?;
    if probe.state != "ready" {
        return Err(probe.note);
    }
    let model = probe.models.iter().find(|model| model.id == model_id).ok_or("The selected model is no longer available from OpenRouter. Refresh the catalog and choose a model explicitly.")?;
    let key = resolve_openrouter_credential(&profile.id, profile.secret_ref.as_deref()).ok_or("The OpenRouter credential reference is missing, mismatched, or unavailable. Save the key again in Providers.")?;
    let path = std::env::var_os("PATH");
    let executable = providers::find_executable(&profile.executable, path.as_deref())
        .ok_or("The configured Aider executable is not available on PATH or at its saved path.")?;
    let handoff = request
        .handoff_source_run_id
        .as_deref()
        .map(|source_id| {
            runs::saved_provider_handoff(&app, source_id, provider_id, model_id, false)
        })
        .transpose()?;
    let root = Path::new(&settings.project_root)
        .canonicalize()
        .map_err(|_| "The configured project root could not be resolved.".to_string())?;
    let cwd = Path::new(&request.cwd)
        .canonicalize()
        .map_err(|_| "The selected project folder could not be resolved.".to_string())?;
    if !cwd.is_dir() || !cwd.starts_with(&root) {
        return Err(
            "OpenRouter can only run in a project folder inside the configured project root."
                .into(),
        );
    }
    runs::start_openrouter_workflow_run(
        app,
        runs::OpenRouterRunConfig {
            executable,
            cwd,
            model_id: model.id.clone(),
            provider_profile_id: profile.id.clone(),
            api_key: key,
            prompt: request.prompt,
            test_command: request.test_command,
            timeout_seconds: request.timeout_seconds,
        },
        handoff,
    )
}

#[tauri::command]
fn create_provider_handoff(
    app: AppHandle,
    input: runs::HandoffInput,
) -> Result<runs::ProviderHandoff, String> {
    let settings = load_settings(&settings_path(&app)?)?;
    if !settings
        .providers
        .iter()
        .any(|profile| profile.id == input.target_provider_id)
    {
        return Err("Select a configured target provider for the handoff.".into());
    }
    runs::create_provider_handoff(app, input)
}

#[tauri::command]
fn start_codex_workflow_run(
    app: AppHandle,
    request: runs::CodexStartRequest,
) -> Result<runs::RunRecord, String> {
    let settings = load_settings(&settings_path(&app)?)?;
    providers::validate_profiles(&settings.providers, &settings.provider_selection)?;
    let selection = &settings.provider_selection;
    let provider_id = selection
        .provider_id
        .as_deref()
        .ok_or("Choose a Codex provider in Providers first.")?;
    let provider = settings
        .providers
        .iter()
        .find(|provider| provider.id == provider_id)
        .ok_or("The selected provider is no longer configured.")?;
    if provider.kind != providers::ProviderKind::Codex {
        return Err("The selected provider is not Codex.".into());
    }
    if selection.runtime_id.as_deref().is_some_and(|runtime| {
        !runtime.is_empty() && !matches!(runtime, "codex-cli" | "codex-app-server")
    }) {
        return Err("This phase supports the installed Codex app-server runtime only.".into());
    }
    let model_id = selection
        .model_id
        .as_deref()
        .ok_or("Choose a configured Codex model in Providers first.")?;
    let model = provider
        .models
        .iter()
        .find(|model| model.id == model_id)
        .ok_or("The selected Codex model is not configured for this provider.")?;
    if selection.reasoning_effort.as_deref().is_some_and(|effort| {
        !model
            .reasoning_efforts
            .iter()
            .any(|supported| supported == effort)
    }) {
        return Err("The selected reasoning effort is not supported by this model profile.".into());
    }
    let root = Path::new(&settings.project_root)
        .canonicalize()
        .map_err(|_| "The configured project root could not be resolved.".to_string())?;
    let cwd = Path::new(&request.cwd)
        .canonicalize()
        .map_err(|_| "The selected project folder could not be resolved.".to_string())?;
    if !cwd.is_dir() || !cwd.starts_with(&root) {
        return Err(
            "Codex can only run in a project folder inside the configured project root.".into(),
        );
    }
    let skill_path = request
        .skill_id
        .as_deref()
        .map(resolved_workflow_skill_path)
        .transpose()?;
    let path = std::env::var_os("PATH");
    let executable = providers::find_executable(&provider.executable, path.as_deref())
        .ok_or("The configured Codex executable is not available on PATH or at its saved path.")?;
    let handoff = request
        .handoff_source_run_id
        .as_deref()
        .map(|source_id| {
            runs::saved_provider_handoff(&app, source_id, provider_id, &model.id, false)
        })
        .transpose()?;
    runs::start_codex_workflow_run(
        app,
        runs::CodexRunConfig {
            executable,
            cwd,
            model_id: model.id.clone(),
            provider_profile_id: provider.id.clone(),
            reasoning_effort: selection.reasoning_effort.clone(),
            prompt: request.prompt,
            skill_id: request.skill_id,
            skill_path,
            timeout_seconds: request.timeout_seconds,
            workflow: request.workflow,
            handoff,
        },
    )
}

#[tauri::command]
fn start_antigravity_workflow_run(
    app: AppHandle,
    request: runs::AntigravityStartRequest,
) -> Result<runs::RunRecord, String> {
    let settings = load_settings(&settings_path(&app)?)?;
    providers::validate_profiles(&settings.providers, &settings.provider_selection)?;
    let selection = &settings.provider_selection;
    let provider_id = selection
        .provider_id
        .as_deref()
        .ok_or("Choose an Antigravity provider in Providers first.")?;
    let provider = settings
        .providers
        .iter()
        .find(|provider| provider.id == provider_id)
        .ok_or("The selected provider is no longer configured.")?;
    if provider.kind != providers::ProviderKind::Antigravity {
        return Err("The selected provider is not Antigravity.".into());
    }
    if selection.reasoning_effort.is_some() {
        return Err("The installed Antigravity CLI does not expose a reasoning-effort flag; clear the selected effort to run.".into());
    }
    if selection.runtime_id.as_deref().is_some_and(|runtime| {
        !runtime.is_empty() && runtime != "agy-cli" && runtime != "antigravity-cli"
    }) {
        return Err("This phase supports the Antigravity CLI runtime only.".into());
    }
    let model_id = selection
        .model_id
        .as_deref()
        .ok_or("Choose a configured Antigravity model in Providers first.")?;
    let model = provider
        .models
        .iter()
        .find(|model| model.id == model_id)
        .ok_or("The selected model is not configured for this Antigravity provider.")?;
    if selection.reasoning_effort.as_deref().is_some_and(|effort| {
        !model
            .reasoning_efforts
            .iter()
            .any(|supported| supported == effort)
    }) {
        return Err("The selected reasoning effort is not supported by this model profile.".into());
    }
    let root = Path::new(&settings.project_root)
        .canonicalize()
        .map_err(|_| "The configured project root could not be resolved.".to_string())?;
    let cwd = Path::new(&request.cwd)
        .canonicalize()
        .map_err(|_| "The selected project folder could not be resolved.".to_string())?;
    if !cwd.is_dir() || !cwd.starts_with(&root) {
        return Err(
            "Antigravity can only run in a project folder inside the configured project root."
                .into(),
        );
    }
    let path = std::env::var_os("PATH");
    let executable = providers::find_executable(&provider.executable, path.as_deref()).ok_or(
        "The configured Antigravity executable is not available on PATH or at its saved path.",
    )?;
    let handoff = request
        .handoff_source_run_id
        .as_deref()
        .map(|source_id| {
            runs::saved_provider_handoff(&app, source_id, provider_id, &model.id, false)
        })
        .transpose()?;
    runs::start_antigravity_workflow_run(
        app,
        runs::AntigravityRunConfig {
            executable,
            cwd,
            model_id: model.id.clone(),
            provider_profile_id: provider.id.clone(),
            prompt: request.prompt,
            timeout_seconds: request.timeout_seconds,
        },
        handoff,
    )
}

#[tauri::command]
fn start_ollama_workflow_run(
    app: AppHandle,
    request: runs::OllamaStartRequest,
) -> Result<runs::RunRecord, String> {
    let settings = load_settings(&settings_path(&app)?)?;
    providers::validate_profiles(&settings.providers, &settings.provider_selection)?;
    let selection = &settings.provider_selection;
    let provider_id = selection
        .provider_id
        .as_deref()
        .ok_or("Choose a local Ollama provider in Providers first.")?;
    let provider = settings
        .providers
        .iter()
        .find(|profile| profile.id == provider_id)
        .ok_or("The selected provider is no longer configured.")?;
    if provider.kind != providers::ProviderKind::Ollama {
        return Err("The selected provider is not local Ollama.".into());
    }
    if selection.reasoning_effort.is_some() {
        return Err(
            "The Aider Ollama adapter does not expose configured reasoning-effort labels.".into(),
        );
    }
    if selection
        .runtime_id
        .as_deref()
        .is_some_and(|runtime| !runtime.is_empty() && runtime != "aider-ollama")
    {
        return Err("This phase supports the Aider Ollama Chat runtime only.".into());
    }
    let model_id = selection
        .model_id
        .as_deref()
        .ok_or("Discover local models and select a tool-capable model first.")?;
    let endpoint_url = providers::validate_ollama_endpoint(
        provider
            .endpoint
            .as_deref()
            .unwrap_or(providers::OLLAMA_DEFAULT_ENDPOINT),
    )?;
    let endpoint = endpoint_url.as_str().trim_end_matches('/').to_owned();
    let probe = providers::ollama_probe(&endpoint);
    if probe.state != "ready" {
        return Err(probe.note);
    }
    let model = probe
        .models
        .iter()
        .find(|model| model.id == model_id)
        .ok_or("The selected model is no longer available from local Ollama.")?;
    if !model.eligible {
        return Err(format!("Selected model is not eligible: {}", model.note));
    }
    let timeout_seconds = providers::ollama_timeout_seconds(
        model.size_bytes,
        probe.available_memory_bytes,
    )
    .ok_or(
        "Available system memory is unknown; a hardware-aware Ollama timeout cannot be calculated.",
    )?;
    let path = std::env::var_os("PATH");
    let executable = providers::find_executable(&provider.executable, path.as_deref())
        .ok_or("The configured Aider executable is not available on PATH or at its saved path.")?;
    let handoff = request
        .handoff_source_run_id
        .as_deref()
        .map(|source_id| {
            runs::saved_provider_handoff(&app, source_id, provider_id, &model.id, true)
        })
        .transpose()?;
    let root = Path::new(&settings.project_root)
        .canonicalize()
        .map_err(|_| "The configured project root could not be resolved.".to_string())?;
    let cwd = Path::new(&request.cwd)
        .canonicalize()
        .map_err(|_| "The selected project folder could not be resolved.".to_string())?;
    if !cwd.is_dir() || !cwd.starts_with(&root) {
        return Err(
            "Ollama can only run in a project folder inside the configured project root.".into(),
        );
    }
    runs::start_ollama_workflow_run(
        app,
        runs::OllamaRunConfig {
            executable,
            cwd,
            endpoint,
            model_id: model.id.clone(),
            provider_profile_id: provider.id.clone(),
            prompt: request.prompt,
            test_command: request.test_command,
            timeout_seconds,
        },
        handoff,
    )
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
    let inventory_path = project_inventory_path(&app)?;
    let prior_inventory = inventory::load(&inventory_path, &settings.project_root);
    let mut refresh_restored_facts = false;
    if force_refresh {
        PROJECT_CACHE_RESTORED.store(true, Ordering::Release);
        PROJECT_CACHE_EPOCH.fetch_add(1, Ordering::Relaxed);
        project_fact_cache()
            .lock()
            .map_err(|_| "Could not access cached project facts.".to_string())?
            .clear();
    } else if !PROJECT_CACHE_RESTORED.swap(true, Ordering::AcqRel) {
        // Keep the persisted projects available for immediate rendering, but do
        // not let their Git facts short-circuit the first background scan.
        refresh_restored_facts = true;
        if !prior_inventory.is_empty() {
            let mut cache = project_fact_cache()
                .lock()
                .map_err(|_| "Could not access cached project facts.".to_string())?;
            if cache.is_empty() {
                cache.extend(
                    prior_inventory
                        .iter()
                        .cloned()
                        .into_iter()
                        .map(|project| (project.id.clone(), project)),
                );
            }
        }
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
        let prior_inventory = prior_inventory;
        let cache_epoch = PROJECT_CACHE_EPOCH.load(Ordering::Relaxed);
        let cached_projects = if refresh_restored_facts {
            HashMap::new()
        } else {
            project_fact_cache()
                .lock()
                .map(|cache| cache.clone())
                .unwrap_or_default()
        };
        let mut result = scanner::scan(
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
                    } else {
                        for project in prior_inventory {
                            cache.entry(project.id.clone()).or_insert(project);
                        }
                    }
                    for project in &result.projects {
                        cache.insert(project.id.clone(), project.clone());
                    }
                    if !result.errors.is_empty() {
                        result.projects = cache.values().cloned().collect();
                        result.projects.sort_by_key(|project| {
                            (project.name.to_lowercase(), project.path.to_lowercase())
                        });
                    }
                    if let Err(error) =
                        inventory::save(&inventory_path, &settings.project_root, &result.projects)
                    {
                        if result.errors.len() < 501 {
                            result.errors.push(error);
                        }
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
    let startup_started = Instant::now();
    tauri::Builder::default()
        .on_page_load(move |webview, payload| {
            if webview.label() == "main"
                && payload.event() == tauri::webview::PageLoadEvent::Finished
            {
                let elapsed = startup_started
                    .elapsed()
                    .as_millis()
                    .clamp(1, u64::MAX as u128) as u64;
                let _ = APP_STARTUP_MS.compare_exchange(
                    0,
                    elapsed,
                    Ordering::AcqRel,
                    Ordering::Acquire,
                );
            }
        })
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
            get_project_inventory,
            discover_workflow_skills,
            save_settings,
            inspect_providers,
            probe_antigravity_cli,
            probe_ollama,
            save_openrouter_credential,
            probe_openrouter,
            save_typesafe_credential,
            probe_typesafe,
            create_provider_handoff,
            start_codex_workflow_run,
            start_antigravity_workflow_run,
            start_ollama_workflow_run,
            start_openrouter_workflow_run,
            get_cached_github_activity,
            refresh_github_activity,
            list_github_issues,
            get_github_issue,
            start_project_scan,
            cancel_project_scan,
            runs::list_workflow_runs,
            runs::export_workflow_diagnostics,
            runs::delete_finished_workflow_runs,
            runs::start_workflow_run,
            runs::cancel_workflow_run,
            runs::mark_workflow_run_needs_input,
            runs::write_workflow_run_input,
            runs::respond_codex_approval,
            runs::respond_codex_user_input,
            list_usage_signals,
            record_manual_usage_estimate,
            evaluate_routing,
            evaluate_provider_fallback,
            calibrate_routing
        ])
        .run(tauri::generate_context!())
        .expect("error while running Navis-Fabrica");
}

#[tauri::command]
fn list_usage_signals(app: AppHandle) -> Result<Vec<usage::UsageSignal>, String> {
    usage::list_signals(&runs::history_path(&app)?, usage::now_ms())
}

#[tauri::command]
fn evaluate_routing(mut request: routing::Request) -> Result<routing::Decision, String> {
    if request.candidates.len() > 3_200 || request.usage_signals.len() > 500 {
        return Err("Routing input exceeds the supported candidate or signal limit.".into());
    }
    if request
        .desired_completion_minutes
        .is_some_and(|minutes| !(1..=1_440).contains(&minutes))
    {
        return Err("Desired completion time must be between 1 minute and 24 hours.".into());
    }
    let mut candidate_keys = std::collections::HashSet::new();
    if request.candidates.iter().any(|candidate| {
        candidate.provider_id.trim().is_empty()
            || candidate.provider_id.len() > 80
            || candidate.telemetry_provider_id.trim().is_empty()
            || candidate.telemetry_provider_id.len() > 80
            || candidate.model_id.trim().is_empty()
            || candidate.model_id.len() > 160
            || candidate.provider_name.len() > 100
            || candidate.model_name.len() > 160
            || [
                &candidate.provider_id,
                &candidate.telemetry_provider_id,
                &candidate.model_id,
                &candidate.provider_name,
                &candidate.model_name,
            ]
            .iter()
            .any(|value| value.chars().any(char::is_control))
            || candidate
                .quality_rating
                .is_some_and(|rating| !(1..=5).contains(&rating))
            || candidate.reasoning_efforts.len() > 8
            || candidate
                .reasoning_efforts
                .iter()
                .any(|effort| effort.len() > 40 || effort.chars().any(char::is_control))
            || !candidate_keys.insert((candidate.provider_id.as_str(), candidate.model_id.as_str()))
    }) {
        return Err("Routing candidate identities or quality ratings are invalid.".into());
    }
    request.now_ms = usage::now_ms();
    Ok(routing::decide(request))
}

#[tauri::command]
fn evaluate_provider_fallback(
    app: AppHandle,
    mut request: routing::FallbackRequest,
) -> Result<routing::FallbackDecision, String> {
    if request.routing.candidates.is_empty()
        || request.routing.candidates.len() > 200
        || request.routing.usage_signals.len() > 500
        || request
            .routing
            .desired_completion_minutes
            .is_some_and(|minutes| !(1..=1_440).contains(&minutes))
    {
        return Err("Fallback routing requires between one and 200 candidates and no more than 500 usage signals.".into());
    }
    let settings = load_settings(&settings_path(&app)?)?;
    let mut candidate_keys = std::collections::HashSet::new();
    for candidate in &mut request.routing.candidates {
        let profile = settings
            .providers
            .iter()
            .find(|profile| profile.id == candidate.provider_id)
            .ok_or("Fallback candidates must be configured provider profiles.")?;
        if !profile
            .models
            .iter()
            .any(|model| model.id == candidate.model_id)
            || !profile.kind.supports_workflow_run()
            || profile.kind.telemetry_id() != candidate.telemetry_provider_id
            || candidate.provider_id.len() > 80
            || candidate.model_id.len() > 160
            || candidate.provider_name.len() > 100
            || candidate.model_name.len() > 160
            || candidate
                .quality_rating
                .is_some_and(|rating| !(1..=5).contains(&rating))
            || candidate.reasoning_efforts.len() > 8
            || candidate
                .reasoning_efforts
                .iter()
                .any(|effort| effort.len() > 40 || effort.chars().any(char::is_control))
            || [
                &candidate.provider_id,
                &candidate.telemetry_provider_id,
                &candidate.model_id,
                &candidate.provider_name,
                &candidate.model_name,
            ]
            .iter()
            .any(|value| value.chars().any(char::is_control))
            || !candidate_keys.insert((candidate.provider_id.as_str(), candidate.model_id.as_str()))
        {
            return Err(
                "Fallback candidate identities must match configured provider models.".into(),
            );
        }
        candidate.is_local = profile.kind == providers::ProviderKind::Ollama;
    }
    request.routing.usage_signals =
        usage::list_signals(&runs::history_path(&app)?, usage::now_ms())?;
    request.routing.now_ms = usage::now_ms();
    runs::evaluate_provider_fallback(&app, request)
}

#[tauri::command]
fn calibrate_routing(
    samples: Vec<routing::CalibrationSample>,
) -> Result<Vec<routing::CalibrationSummary>, String> {
    if samples.len() > 2_000
        || samples.iter().any(|sample| {
            sample.provider_id.trim().is_empty()
                || sample.provider_id.len() > 80
                || sample.model_id.trim().is_empty()
                || sample.model_id.len() > 160
                || sample.provider_id.chars().any(char::is_control)
                || sample.model_id.chars().any(char::is_control)
                || sample
                    .measured_usage
                    .is_some_and(|value| !value.is_finite() || value < 0.0)
        })
    {
        return Err("Calibration accepts up to 2,000 valid local run samples.".into());
    }
    Ok(routing::calibrate(samples))
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ManualUsageEstimateInput {
    category: usage::UsageCategory,
    value: f64,
    unit: String,
    window_duration_seconds: Option<u64>,
    reset_at_ms: Option<u128>,
}

#[tauri::command]
fn record_manual_usage_estimate(
    app: AppHandle,
    estimate: ManualUsageEstimateInput,
) -> Result<Vec<usage::UsageSignal>, String> {
    let signal = usage::manual_estimate(
        estimate.category,
        estimate.value,
        estimate.unit,
        estimate.window_duration_seconds,
        estimate.reset_at_ms,
        usage::now_ms(),
    )?;
    let path = runs::history_path(&app)?;
    usage::record_signals(&path, &[signal])?;
    usage::list_signals(&path, usage::now_ms())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn openrouter_credential_resolution_rejects_another_profiles_reference() {
        assert!(resolve_openrouter_credential(
            "openRouter-a1b2",
            Some("credential-store:github-session")
        )
        .is_none());
        assert!(resolve_openrouter_credential("openRouter-a1b2", None).is_none());
    }

    #[test]
    fn refuses_to_restore_authenticated_activity_without_a_usable_credential() {
        assert!(!can_restore_github_cache("available", false));
        assert!(can_restore_github_cache("available", true));
        assert!(can_restore_github_cache("notConfigured", false));
    }

    #[test]
    fn discovers_only_known_skill_references_inside_the_codex_skills_root() {
        let folder = tempfile::tempdir().unwrap();
        let skill = folder.path().join("dev-work");
        fs::create_dir_all(&skill).unwrap();
        fs::write(
            skill.join("SKILL.md"),
            "---\nname: dev-work\ndescription: Work on a GitHub issue.\n---\n",
        )
        .unwrap();

        assert_eq!(
            workflow_skill_status(folder.path(), "dev-work"),
            "available"
        );
        assert_eq!(workflow_skill_status(folder.path(), "dev-push"), "missing");
        assert_eq!(
            workflow_skill_status(folder.path(), "../../outside"),
            "missing"
        );

        fs::write(skill.join("SKILL.md"), "not frontmatter\n").unwrap();
        assert_eq!(
            workflow_skill_status(folder.path(), "dev-work"),
            "incompatible"
        );
        fs::write(
            skill.join("SKILL.md"),
            "---\nname: another-skill\ndescription: Wrong skill.\n---\n",
        )
        .unwrap();
        assert_eq!(
            workflow_skill_status(folder.path(), "dev-work"),
            "incompatible"
        );
    }

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
        assert_eq!(settings.schema_version, 4);
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
    fn typesafe_profile_settings_keep_only_matching_opaque_references() {
        let valid = recover_settings(serde_json::json!({
            "schemaVersion": 3,
            "typesafeProfile": {
                "id": "jev-main",
                "enabled": true,
                "credentialRef": "credential-store:navis-fabrica-typesafe-jev-main"
            }
        }));
        assert_eq!(valid.typesafe_profile.as_ref().unwrap().id, "jev-main");
        assert_eq!(valid.schema_version, 4);

        let invalid = recover_settings(serde_json::json!({
            "schemaVersion": 4,
            "typesafeProfile": {
                "id": "jev-main",
                "enabled": true,
                "credentialRef": "credential-store:github-token"
            }
        }));
        assert!(invalid.typesafe_profile.is_none());
        assert!(validate_typesafe_profile(Some(&providers::TypeSafeProfile {
            id: "jev-main".into(),
            enabled: true,
            credential_ref: Some("raw-api-key-value".into()),
        }))
        .is_err());
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
        fs::write(&path, r#"{"schemaVersion":5,"projectRoot":"C:\\Projects"}"#).unwrap();
        assert!(load_settings(&path).unwrap_err().contains("newer version"));
        assert_eq!(
            fs::read_to_string(path).unwrap(),
            r#"{"schemaVersion":5,"projectRoot":"C:\\Projects"}"#
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
