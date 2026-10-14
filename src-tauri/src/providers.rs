use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::mpsc::Receiver;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ProviderKind {
    Codex,
    Antigravity,
    Ollama,
    Custom,
}

impl ProviderKind {
    pub fn label(self) -> &'static str {
        match self {
            Self::Codex => "Codex",
            Self::Antigravity => "Antigravity",
            Self::Ollama => "Ollama",
            Self::Custom => "Custom runtime",
        }
    }

    pub fn default_executable(self) -> &'static str {
        match self {
            Self::Codex => "codex",
            Self::Antigravity => "antigravity",
            Self::Ollama => "ollama",
            Self::Custom => "",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelProfile {
    pub id: String,
    pub name: String,
    pub reasoning_efforts: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderProfile {
    pub id: String,
    pub kind: ProviderKind,
    pub name: String,
    pub executable: String,
    pub secret_ref: Option<String>,
    pub models: Vec<ModelProfile>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderSelection {
    pub provider_id: Option<String>,
    pub model_id: Option<String>,
    pub reasoning_effort: Option<String>,
    pub runtime_id: Option<String>,
}

impl Default for ProviderSelection {
    fn default() -> Self {
        Self {
            provider_id: None,
            model_id: None,
            reasoning_effort: None,
            runtime_id: None,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AdapterCapabilities {
    pub discovery: bool,
    pub health: bool,
    pub model_listing: bool,
    pub run_start: bool,
    pub run_cancel: bool,
    pub event_stream: bool,
    pub checkpoints: bool,
}

impl AdapterCapabilities {
    pub fn local_preflight() -> Self {
        Self {
            discovery: true,
            ..Self::default()
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderAvailability {
    pub provider_id: String,
    pub executable_found: bool,
    pub executable_path: Option<String>,
    pub authentication_reference: AuthenticationReferenceStatus,
    pub capabilities: AdapterCapabilities,
    pub note: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", tag = "state")]
pub enum ProviderHealth {
    Unknown,
    Ready,
    MissingExecutable,
    AuthenticationRequired,
    Unhealthy { message: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum AuthenticationReferenceStatus {
    Missing,
    ReferenceConfigured,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", tag = "kind")]
pub enum ProviderEvent {
    Output {
        sequence: u64,
        text: String,
    },
    StateChanged {
        sequence: u64,
        state: String,
    },
    NeedsInput {
        sequence: u64,
        prompt: String,
    },
    Checkpoint {
        sequence: u64,
        checkpoint: RunCheckpoint,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RunCheckpoint {
    pub provider_id: String,
    pub runtime_id: String,
    pub checkpoint_ref: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderRunRequest {
    pub run_id: String,
    pub model_id: String,
    pub reasoning_effort: Option<String>,
    pub runtime_id: String,
    pub prompt: String,
    pub checkpoint: Option<RunCheckpoint>,
}

/// Provider implementations must explicitly declare each operation they support.
/// The initial phase ships the contract and local executable preflight only; no
/// provider process is launched until a concrete adapter is implemented.
pub trait ProviderAdapter: Send + Sync {
    fn kind(&self) -> ProviderKind;
    fn capabilities(&self) -> AdapterCapabilities;
    fn discover(&self, profile: &ProviderProfile) -> Result<ProviderAvailability, String>;
    fn health(&self, profile: &ProviderProfile) -> Result<ProviderHealth, String>;
    fn list_models(&self, profile: &ProviderProfile) -> Result<Vec<ModelProfile>, String>;
    fn start_run(
        &self,
        profile: &ProviderProfile,
        request: ProviderRunRequest,
    ) -> Result<(), String>;
    fn cancel_run(&self, profile: &ProviderProfile, run_id: &str) -> Result<bool, String>;
    fn stream_events(
        &self,
        profile: &ProviderProfile,
        run_id: &str,
    ) -> Result<Receiver<ProviderEvent>, String>;
    fn resume_from_checkpoint(
        &self,
        profile: &ProviderProfile,
        checkpoint: RunCheckpoint,
    ) -> Result<(), String>;
}

pub fn find_executable(executable: &str, path_value: Option<&std::ffi::OsStr>) -> Option<PathBuf> {
    if executable.trim().is_empty() {
        return None;
    }
    let candidate = Path::new(executable);
    if candidate.components().count() > 1 || candidate.is_absolute() {
        return candidate.is_file().then(|| candidate.to_path_buf());
    }
    let path_value = path_value?;
    for directory in std::env::split_paths(path_value) {
        let direct = directory.join(executable);
        if direct.is_file() {
            return Some(direct);
        }
        #[cfg(windows)]
        for extension in [".exe", ".cmd", ".bat"] {
            let with_extension = directory.join(format!("{executable}{extension}"));
            if with_extension.is_file() {
                return Some(with_extension);
            }
        }
    }
    None
}

pub fn inspect(
    profile: &ProviderProfile,
    path_value: Option<&std::ffi::OsStr>,
) -> ProviderAvailability {
    let executable_path = find_executable(&profile.executable, path_value);
    ProviderAvailability {
        provider_id: profile.id.clone(),
        executable_found: executable_path.is_some(),
        executable_path: executable_path.map(|path| path.to_string_lossy().into_owned()),
        authentication_reference: if profile.secret_ref.as_deref().is_some_and(|value| !value.trim().is_empty()) {
            AuthenticationReferenceStatus::ReferenceConfigured
        } else {
            AuthenticationReferenceStatus::Missing
        },
        capabilities: AdapterCapabilities::local_preflight(),
        note: "Executable presence is checked without launching it. Authentication validity and provider operations are not implemented by this adapter.".into(),
    }
}

pub fn validate_profiles(
    profiles: &[ProviderProfile],
    selection: &ProviderSelection,
) -> Result<(), String> {
    if profiles.len() > 16 {
        return Err("Configure 16 or fewer providers.".into());
    }
    let mut ids = Vec::new();
    for profile in profiles {
        if profile.id.trim().is_empty()
            || profile.id.len() > 80
            || profile.name.trim().is_empty()
            || profile.name.len() > 100
        {
            return Err(
                "Provider IDs and names must be non-empty and within their length limits.".into(),
            );
        }
        if profile.id.chars().any(char::is_control) || profile.name.chars().any(char::is_control) {
            return Err("Provider IDs and names cannot contain control characters.".into());
        }
        if ids.iter().any(|id: &String| id == &profile.id) {
            return Err("Provider IDs must be unique.".into());
        }
        ids.push(profile.id.clone());
        if profile.executable.len() > 260 || profile.executable.chars().any(char::is_control) {
            return Err("Executable paths must be 260 characters or fewer and contain no control characters.".into());
        }
        if profile.secret_ref.as_ref().is_some_and(|value| {
            let reference_key = value
                .strip_prefix("credential-store:")
                .or_else(|| value.strip_prefix("provider-managed:"));
            value.len() > 260
                || value.chars().any(char::is_control)
                || reference_key.map_or(true, |key| {
                    key.is_empty()
                        || !key.chars().all(|character| {
                            character.is_ascii_alphanumeric() || ".:/_-".contains(character)
                        })
                })
        }) {
            return Err("Use an opaque reference such as credential-store:provider-name; secret values are not accepted.".into());
        }
        if profile.models.len() > 200 {
            return Err("Configure 200 or fewer manual model profiles per provider.".into());
        }
        let mut model_ids = Vec::new();
        for model in &profile.models {
            if model.id.trim().is_empty()
                || model.id.len() > 160
                || model.name.trim().is_empty()
                || model.name.len() > 100
                || model.id.chars().any(char::is_control)
                || model.name.chars().any(char::is_control)
            {
                return Err(
                    "Model IDs and names must be valid and within their length limits.".into(),
                );
            }
            if model_ids.iter().any(|id: &String| id == &model.id) {
                return Err("Model IDs must be unique within a provider.".into());
            }
            model_ids.push(model.id.clone());
            if model.reasoning_efforts.len() > 8
                || model.reasoning_efforts.iter().any(|effort| {
                    effort.is_empty() || effort.len() > 40 || effort.chars().any(char::is_control)
                })
            {
                return Err(
                    "Reasoning effort options must be valid and limited to eight per model.".into(),
                );
            }
        }
    }
    if let Some(provider_id) = &selection.provider_id {
        let provider = profiles
            .iter()
            .find(|profile| &profile.id == provider_id)
            .ok_or("Select a configured provider.")?;
        if selection
            .model_id
            .as_ref()
            .is_some_and(|model_id| !provider.models.iter().any(|model| &model.id == model_id))
        {
            return Err("Select a model configured for the selected provider.".into());
        }
        if selection.reasoning_effort.as_ref().is_some_and(|effort| {
            !provider.models.iter().any(|model| {
                selection
                    .model_id
                    .as_ref()
                    .is_some_and(|model_id| &model.id == model_id)
                    && model.reasoning_efforts.contains(effort)
            })
        }) {
            return Err("Select a reasoning effort supported by the selected model.".into());
        }
    } else if selection.model_id.is_some() || selection.reasoning_effort.is_some() {
        return Err("Choose a provider before selecting its model or reasoning effort.".into());
    }
    for value in [
        &selection.provider_id,
        &selection.model_id,
        &selection.reasoning_effort,
        &selection.runtime_id,
    ]
    .into_iter()
    .flatten()
    {
        if value.len() > 160 || value.chars().any(char::is_control) {
            return Err("Provider selections must be 160 characters or fewer and contain no control characters.".into());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsStr;

    fn profile() -> ProviderProfile {
        ProviderProfile {
            id: "codex-local".into(),
            kind: ProviderKind::Codex,
            name: "Codex Local".into(),
            executable: "codex".into(),
            secret_ref: Some("credential-store:codex-session".into()),
            models: vec![ModelProfile {
                id: "model-a".into(),
                name: "Model A".into(),
                reasoning_efforts: vec!["low".into(), "high".into()],
            }],
        }
    }

    #[test]
    fn executable_discovery_uses_only_the_supplied_path() {
        let folder = tempfile::tempdir().unwrap();
        let executable = folder.path().join("codex");
        std::fs::write(&executable, "fixture executable").unwrap();
        let path = std::env::join_paths([folder.path()]).unwrap();
        let result = find_executable("codex", Some(path.as_os_str()));
        assert_eq!(result.as_deref(), Some(executable.as_path()));
        assert!(find_executable("missing", Some(path.as_os_str())).is_none());
        assert!(find_executable("codex", Some(OsStr::new(""))).is_none());
    }

    #[test]
    fn inspection_reports_reference_and_unsupported_operations_without_running_a_process() {
        let profile = profile();
        let status = inspect(&profile, None);
        assert!(!status.executable_found);
        assert_eq!(
            status.authentication_reference,
            AuthenticationReferenceStatus::ReferenceConfigured
        );
        assert!(status.capabilities.discovery);
        assert!(!status.capabilities.run_start);
        assert!(!status.capabilities.checkpoints);
        assert!(status.note.contains("without launching"));
    }

    #[test]
    fn profiles_reject_secret_values_and_invalid_selection() {
        let mut profile = profile();
        profile.secret_ref = Some("sk-secret-token-value".into());
        assert!(validate_profiles(&[profile.clone()], &ProviderSelection::default()).is_err());
        profile.secret_ref = Some("credential-store:codex-session".into());
        let invalid = ProviderSelection {
            provider_id: Some(profile.id.clone()),
            model_id: Some("missing".into()),
            ..ProviderSelection::default()
        };
        assert!(validate_profiles(&[profile], &invalid).is_err());
    }

    #[test]
    fn provider_profiles_serialize_only_a_secret_reference_field() {
        let value = serde_json::to_value(profile()).unwrap();
        assert_eq!(value["secretRef"], "credential-store:codex-session");
        assert!(value.get("secret").is_none());
    }
}
