//! Versioned settings DTOs shared by the desktop shell and headless runner.

use crate::providers::{self, ProviderProfile, ProviderSelection, TypeSafeProfile};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::fs;
use std::path::{Component, Path};

pub const SETTINGS_SCHEMA_VERSION: u32 = 5;
const MAX_PROJECT_DISPLAY_IDS: usize = 10_000;
const MAX_PROJECT_ID_LENGTH: usize = 1024;
const MAX_SETTINGS_BYTES: u64 = 2 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ProjectDisplayView {
    All,
    Favorites,
    Hidden,
}

impl Default for ProjectDisplayView {
    fn default() -> Self {
        Self::All
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct ProjectDisplayPreferences {
    pub favorite_project_ids: Vec<String>,
    pub hidden_project_ids: Vec<String>,
    pub view: ProjectDisplayView,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Preferences {
    pub include_hidden: bool,
    pub scan_depth: u8,
    #[serde(default)]
    pub project_display: ProjectDisplayPreferences,
}

impl Default for Preferences {
    fn default() -> Self {
        Self {
            include_hidden: false,
            scan_depth: 3,
            project_display: ProjectDisplayPreferences::default(),
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
    pub providers: Vec<ProviderProfile>,
    pub provider_selection: ProviderSelection,
    pub github_accounts: Vec<GitHubAccount>,
    pub selected_github_account_id: Option<String>,
    pub typesafe_profile: Option<TypeSafeProfile>,
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
                ".git".into(),
                ".worktrees".into(),
                "node_modules".into(),
                "target".into(),
            ],
            preferences: Preferences::default(),
            providers: Vec::new(),
            provider_selection: ProviderSelection::default(),
            github_accounts: Vec::new(),
            selected_github_account_id: None,
            typesafe_profile: None,
        }
    }
}

/// Read and normalize the same settings document used by Fabrica. Normalization
/// is pure and read-only; forward versions remain a hard error.
pub fn load_settings(path: &Path) -> Result<Settings, String> {
    let metadata = match fs::metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(Settings::default())
        }
        Err(error) => return Err(format!("Could not read settings file: {error}")),
    };
    if metadata.len() > MAX_SETTINGS_BYTES {
        return Err("Settings file exceeds the 2 MiB limit.".into());
    }
    let bytes = fs::read(path).map_err(|e| format!("Could not read settings file: {e}"))?;
    let value: Value = match serde_json::from_slice(&bytes) {
        Ok(value) => value,
        Err(_) => return Ok(Settings::default()),
    };
    let version = value
        .get("schemaVersion")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    if version > u64::from(SETTINGS_SCHEMA_VERSION) {
        return Err("These settings were saved by a newer version of Navis-Fabrica.".into());
    }
    Ok(recover_settings(value))
}

pub fn recover_settings(value: Value) -> Settings {
    let defaults = Settings::default();
    let Some(object) = value.as_object() else {
        return defaults;
    };
    let project_root = object
        .get("projectRoot")
        .and_then(Value::as_str)
        .and_then(validate_project_root)
        .unwrap_or(defaults.project_root);
    let exclusions = match object.get("exclusions").and_then(Value::as_array) {
        Some(entries) => {
            let valid = entries
                .iter()
                .take(200)
                .filter_map(Value::as_str)
                .filter_map(|entry| validate_exclusions(vec![entry.to_owned()]).ok())
                .flatten()
                .collect();
            validate_exclusions(valid).unwrap_or(defaults.exclusions)
        }
        None => defaults.exclusions,
    };
    let prefs = object.get("preferences").and_then(Value::as_object);
    let preferences = Preferences {
        include_hidden: prefs
            .and_then(|p| p.get("includeHidden"))
            .and_then(Value::as_bool)
            .unwrap_or(defaults.preferences.include_hidden),
        scan_depth: prefs
            .and_then(|p| p.get("scanDepth"))
            .and_then(Value::as_u64)
            .filter(|depth| (1..=8).contains(depth))
            .map(|depth| depth as u8)
            .unwrap_or(defaults.preferences.scan_depth),
        project_display: recover_project_display_preferences(
            prefs.and_then(|p| p.get("projectDisplay")),
        ),
    };
    let profiles = object
        .get("providers")
        .cloned()
        .and_then(|v| serde_json::from_value::<Vec<ProviderProfile>>(v).ok())
        .unwrap_or_default();
    let selection = object
        .get("providerSelection")
        .cloned()
        .and_then(|v| serde_json::from_value::<ProviderSelection>(v).ok())
        .unwrap_or_default();
    let (providers_list, provider_selection) =
        if providers::validate_profiles(&profiles, &ProviderSelection::default()).is_ok() {
            let selected = if providers::validate_profiles(&profiles, &selection).is_ok() {
                selection
            } else {
                defaults.provider_selection
            };
            (profiles, selected)
        } else {
            (defaults.providers, defaults.provider_selection)
        };
    let github_accounts = object
        .get("githubAccounts")
        .cloned()
        .and_then(|v| serde_json::from_value::<Vec<GitHubAccount>>(v).ok())
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
        .and_then(|v| serde_json::from_value::<TypeSafeProfile>(v).ok())
        .filter(|profile| {
            providers::typesafe_credential_target(&profile.id).is_ok()
                && profile.credential_ref.as_ref().is_none_or(|r| {
                    providers::typesafe_credential_reference_matches(&profile.id, r)
                })
        });
    Settings {
        schema_version: SETTINGS_SCHEMA_VERSION,
        project_root,
        exclusions,
        preferences,
        providers: providers_list,
        provider_selection,
        github_accounts,
        selected_github_account_id,
        typesafe_profile,
    }
}

fn recover_project_display_preferences(value: Option<&Value>) -> ProjectDisplayPreferences {
    let Some(object) = value.and_then(Value::as_object) else {
        return ProjectDisplayPreferences::default();
    };
    let view = match object.get("view").and_then(Value::as_str) {
        Some("favorites") => ProjectDisplayView::Favorites,
        Some("hidden") => ProjectDisplayView::Hidden,
        _ => ProjectDisplayView::All,
    };
    fn ids(value: Option<&Value>) -> Vec<String> {
        let Some(values) = value.and_then(Value::as_array) else {
            return Vec::new();
        };
        let mut output = Vec::new();
        let mut unique = std::collections::HashSet::new();
        for id in values
            .iter()
            .take(MAX_PROJECT_DISPLAY_IDS)
            .filter_map(Value::as_str)
        {
            if !id.is_empty()
                && id.len() <= MAX_PROJECT_ID_LENGTH
                && !id.chars().any(char::is_control)
                && unique.insert(id.to_owned())
            {
                output.push(id.to_owned());
            }
        }
        output
    }
    ProjectDisplayPreferences {
        favorite_project_ids: ids(object.get("favoriteProjectIds")),
        hidden_project_ids: ids(object.get("hiddenProjectIds")),
        view,
    }
}

fn validate_project_root(root: &str) -> Option<String> {
    let path = Path::new(root.trim());
    if root.trim().is_empty()
        || root.len() > 32_767
        || !path.is_absolute()
        || path
            .components()
            .any(|part| matches!(part, Component::ParentDir))
        || root.chars().any(char::is_control)
    {
        return None;
    }
    #[cfg(windows)]
    if path
        .components()
        .any(|part| matches!(part, Component::Prefix(prefix) if prefix.kind().is_verbatim()))
    {
        return None;
    }
    if path.exists() && !path.is_dir() {
        return None;
    }
    Some(path.to_string_lossy().into_owned())
}

fn validate_exclusions(values: Vec<String>) -> Result<Vec<String>, String> {
    if values.len() > 200 {
        return Err("Use 200 or fewer excluded folders.".into());
    }
    let mut out: Vec<String> = Vec::new();
    for value in values {
        let entry = value.trim();
        if entry.is_empty() {
            continue;
        }
        let path = Path::new(entry);
        if entry.len() > 260
            || entry.chars().any(char::is_control)
            || path.is_absolute()
            || path.components().any(|p| {
                matches!(
                    p,
                    Component::ParentDir | Component::RootDir | Component::Prefix(_)
                )
            })
        {
            return Err(
                "Excluded folders must be relative paths without parent-folder segments.".into(),
            );
        }
        if !out
            .iter()
            .any(|existing| existing.eq_ignore_ascii_case(entry))
        {
            out.push(entry.to_owned());
        }
    }
    Ok(out)
}

pub fn validate_github_accounts(
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
        if account.credential_ref.as_ref().is_some_and(|r| {
            r.strip_prefix("credential-store:")
                .or_else(|| r.strip_prefix("provider-managed:"))
                .is_none_or(|key| {
                    key.is_empty()
                        || key.len() > 260
                        || !key
                            .chars()
                            .all(|c| c.is_ascii_alphanumeric() || ".:/_-".contains(c))
                })
        }) {
            return Err(
                "GitHub accounts must use opaque credential-store or provider-managed references."
                    .into(),
            );
        }
    }
    if selected.is_some_and(|id| !ids.iter().any(|known| known == id)) {
        return Err("Choose a saved GitHub account.".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_are_shared_versioned_and_never_accept_raw_provider_credentials() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        let settings = Settings::default();
        fs::write(&path, serde_json::to_vec(&settings).unwrap()).unwrap();
        assert_eq!(load_settings(&path).unwrap(), settings);
        let mut raw = serde_json::to_value(settings).unwrap();
        raw["providers"] = serde_json::json!([{"id":"or","kind":"openRouter","name":"OR","executable":"aider","secretRef":"sk-or-secret","models":[]}]);
        fs::write(&path, serde_json::to_vec(&raw).unwrap()).unwrap();
        let recovered = load_settings(&path).unwrap();
        assert!(recovered.providers.is_empty());
        assert!(!serde_json::to_string(&recovered)
            .unwrap()
            .contains("sk-or-secret"));
    }

    #[test]
    fn settings_loader_recovers_old_schema_read_only_and_rejects_future_versions() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        let old = serde_json::json!({"schemaVersion":3,"projectRoot":r"C:\Projects","providers":[],"providerSelection":{},"preferences":{"includeHidden":true,"scanDepth":4}});
        let old_bytes = serde_json::to_vec(&old).unwrap();
        fs::write(&path, &old_bytes).unwrap();
        let recovered = load_settings(&path).unwrap();
        assert_eq!(recovered.schema_version, SETTINGS_SCHEMA_VERSION);
        assert!(recovered.preferences.include_hidden);
        assert_eq!(
            fs::read(&path).unwrap(),
            old_bytes,
            "read-only CLI recovery must not rewrite settings"
        );
        fs::write(&path, r#"{"schemaVersion":99}"#).unwrap();
        assert!(load_settings(&path).unwrap_err().contains("newer version"));
    }

    #[test]
    fn schema_four_settings_migrate_display_defaults_without_losing_existing_preferences() {
        let recovered = recover_settings(serde_json::json!({
            "schemaVersion": 4,
            "projectRoot": r"C:\Projects",
            "exclusions": [".git"],
            "preferences": {"includeHidden": true, "scanDepth": 6},
            "providers": [],
            "providerSelection": {}
        }));
        assert_eq!(recovered.schema_version, 5);
        assert!(recovered.preferences.include_hidden);
        assert_eq!(recovered.preferences.scan_depth, 6);
        assert_eq!(
            recovered.preferences.project_display,
            ProjectDisplayPreferences::default()
        );
    }

    #[test]
    fn schema_five_display_preferences_round_trip_and_recover_bounded_ids() {
        let value = serde_json::json!({
            "schemaVersion": 5,
            "projectRoot": r"C:\Projects",
            "preferences": {
                "includeHidden": false,
                "scanDepth": 3,
                "projectDisplay": {
                    "favoriteProjectIds": ["repo-a", "repo-a", "bad\nrepo", "repo-b"],
                    "hiddenProjectIds": ["repo-b"],
                    "view": "hidden"
                }
            }
        });
        let recovered = recover_settings(value);
        assert_eq!(
            recovered.preferences.project_display.view,
            ProjectDisplayView::Hidden
        );
        assert_eq!(
            recovered.preferences.project_display.favorite_project_ids,
            vec!["repo-a", "repo-b"]
        );
        assert_eq!(
            recovered.preferences.project_display.hidden_project_ids,
            vec!["repo-b"]
        );
        assert_eq!(
            serde_json::from_value::<Settings>(serde_json::to_value(&recovered).unwrap()).unwrap(),
            recovered
        );
    }
}
