//! Bounded, versioned local evidence for provider routing.
//!
//! A saved profile is configuration, not proof that a runtime works, a user is
//! authenticated, or a model has the requested capabilities. Only an explicit
//! refresh creates observations; previews read the persisted snapshot.

use crate::providers::{self, ProviderKind, ProviderProfile};
use crate::routing::{BillingPolicy, CandidateFacts};
use crate::settings::{self, Settings};
use serde::{Deserialize, Serialize};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

pub const PROVIDER_OBSERVATION_SCHEMA_VERSION: u32 = 1;
const MAX_SNAPSHOT_BYTES: u64 = 4 * 1024 * 1024;
const MAX_MODELS_PER_PROVIDER: usize = 500;
const OPENROUTER_AUTHENTICATED_CATALOG_SOURCE: &str = "OpenRouter authenticated user model catalog";
const MAX_REASON_CHARS: usize = 240;
static REFRESH_TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

const MAX_REFRESH_CONSENT_KINDS: usize = 5;
const FRESH_FACT_MAX_AGE_MS: u128 = 1_000;

/// Persisted opt-in for dispatch-time metadata refresh. The default is
/// intentionally disabled so old requests cannot acquire new network scope.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "camelCase", deny_unknown_fields)]
pub enum ModelFreeRefreshConsent {
    #[default]
    Disabled,
    Providers {
        kinds: Vec<ProviderKind>,
    },
}

impl ModelFreeRefreshConsent {
    pub fn validate(&self) -> Result<(), RefreshRefusal> {
        let Self::Providers { kinds } = self else {
            return Ok(());
        };
        if kinds.is_empty() || kinds.len() > MAX_REFRESH_CONSENT_KINDS {
            return Err(RefreshRefusal::InvalidConsent);
        }
        for (index, kind) in kinds.iter().enumerate() {
            if kinds[..index].contains(kind) {
                return Err(RefreshRefusal::InvalidConsent);
            }
            if !matches!(
                kind,
                ProviderKind::Codex | ProviderKind::OpenRouter | ProviderKind::Ollama
            ) {
                return Err(RefreshRefusal::UnsupportedProvider);
            }
        }
        Ok(())
    }

    fn permits(&self, kind: ProviderKind) -> bool {
        matches!(self, Self::Providers { kinds } if kinds.contains(&kind))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FreshCandidateObservation {
    pub facts: CandidateFacts,
    pub snapshot_fingerprint: String,
    pub settings_fingerprint: String,
    pub runtime_identity_fingerprint: String,
    pub observed_at_ms: u128,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RefreshRefusal {
    NotConsented,
    Deadline,
    UnsupportedProvider,
    UnknownProfile,
    AuthenticationUnknown,
    ModelUnavailable,
    RuntimeChanged,
    InvalidConsent,
    ProbeFailed(String),
}

impl std::fmt::Display for RefreshRefusal {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let message = match self {
            Self::NotConsented => {
                "Model-free refresh was not explicitly authorized for this provider kind."
            }
            Self::Deadline => "Provider metadata refresh exceeded its dispatch deadline.",
            Self::UnsupportedProvider => {
                "This provider kind does not support bounded model-free refresh."
            }
            Self::UnknownProfile => "The selected provider profile or model is not configured.",
            Self::AuthenticationUnknown => {
                "Provider execution authentication could not be verified."
            }
            Self::ModelUnavailable => {
                "The exact selected model is not available for this provider profile."
            }
            Self::RuntimeChanged => {
                "The selected provider runtime identity changed during metadata refresh."
            }
            Self::InvalidConsent => {
                "Provider refresh consent is malformed or exceeds its supported bounds."
            }
            Self::ProbeFailed(reason) => reason,
        };
        formatter.write_str(message)
    }
}

impl std::error::Error for RefreshRefusal {}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ObservationState {
    Known,
    Unknown,
    Unavailable,
}

/// `value` is present only for `Known`. Unknown and unavailable evidence must
/// never be converted into a positive routing fact.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Observation<T> {
    pub state: ObservationState,
    pub value: Option<T>,
    pub reason: Option<String>,
    pub source: Option<String>,
    pub observed_at_ms: Option<u128>,
}

impl<T> Observation<T> {
    pub fn known(value: T, source: impl Into<String>, observed_at_ms: u128) -> Self {
        Self {
            state: ObservationState::Known,
            value: Some(value),
            reason: None,
            source: Some(source.into()),
            observed_at_ms: Some(observed_at_ms),
        }
    }

    pub fn unknown(reason: impl Into<String>) -> Self {
        Self::absent(ObservationState::Unknown, reason)
    }

    pub fn unavailable(reason: impl Into<String>) -> Self {
        Self::absent(ObservationState::Unavailable, reason)
    }

    fn absent(state: ObservationState, reason: impl Into<String>) -> Self {
        Self {
            state,
            value: None,
            reason: Some(bound_reason(reason.into())),
            source: None,
            observed_at_ms: None,
        }
    }

    fn is_fresh_known(&self, now_ms: u128, max_age_ms: u128) -> bool {
        self.state == ObservationState::Known
            && self.value.is_some()
            && self
                .source
                .as_deref()
                .is_some_and(|source| !source.trim().is_empty())
            && self
                .observed_at_ms
                .is_some_and(|time| time <= now_ms && now_ms.saturating_sub(time) <= max_age_ms)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ProviderModelObservation {
    pub model_id: String,
    pub model_available: Observation<bool>,
    pub context_tokens: Observation<u64>,
    pub supports_tools: Observation<bool>,
    pub local_only: Observation<bool>,
    pub billing: Observation<BillingPolicy>,
    pub input_micros_per_million_tokens: Observation<u64>,
    pub output_micros_per_million_tokens: Observation<u64>,
    pub pricing_source: Observation<String>,
    pub pricing_verified_at_ms: Option<u128>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ProviderObservation {
    pub provider_id: String,
    pub kind: ProviderKind,
    /// Hash of the configured executable identity, never the path itself.
    pub runtime_identity_fingerprint: String,
    pub runtime_installed: Observation<bool>,
    pub authenticated: Observation<bool>,
    pub healthy: Observation<bool>,
    pub health_checked_at_ms: Option<u128>,
    pub models: Vec<ProviderModelObservation>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ProviderObservationSnapshot {
    pub schema_version: u32,
    pub settings_fingerprint: String,
    pub snapshot_fingerprint: String,
    pub observed_at_ms: u128,
    pub providers: Vec<ProviderObservation>,
}

pub struct ProviderObservationStore {
    path: PathBuf,
}

impl ProviderObservationStore {
    /// Opens a store without creating directories or files. Preview is a pure
    /// read and therefore cannot accidentally trigger probes or writes.
    pub fn open_read_only(data_root: &Path) -> Result<Self, String> {
        if !data_root.is_absolute() {
            return Err("Provider observation data root must be an absolute path.".into());
        }
        Ok(Self {
            path: data_root.join("provider-observations.json"),
        })
    }

    pub fn snapshot(
        &self,
        settings: &Settings,
        now_ms: u128,
    ) -> Result<ProviderObservationSnapshot, String> {
        let metadata = fs::metadata(&self.path)
            .map_err(|_| "Provider observations have not been refreshed yet.".to_string())?;
        if metadata.len() > MAX_SNAPSHOT_BYTES {
            return Err("Provider observation snapshot exceeds its size limit.".into());
        }
        let bytes = fs::read(&self.path)
            .map_err(|_| "Provider observation snapshot could not be read.".to_string())?;
        let snapshot: ProviderObservationSnapshot = serde_json::from_slice(&bytes)
            .map_err(|_| "Provider observation snapshot is invalid.".to_string())?;
        snapshot.validate(settings, now_ms)?;
        Ok(snapshot)
    }

    /// Explicitly refreshes local evidence and, only when `allow_live_probe` is
    /// true, the OpenRouter catalog/authentication observation. This never runs
    /// model inference. A false flag leaves remote-provider evidence unknown.
    pub fn refresh(
        data_root: &Path,
        settings_path: &Path,
        allow_live_probe: bool,
        now_ms: u128,
    ) -> Result<ProviderObservationSnapshot, String> {
        let settings = settings::load_settings(settings_path)?;
        let mut providers_out = Vec::new();
        for profile in &settings.providers {
            providers_out.push(refresh_provider(profile, allow_live_probe, now_ms));
        }
        let snapshot = seal_snapshot(&settings, now_ms, providers_out)?;
        persist_snapshot(data_root, &snapshot)?;
        Ok(snapshot)
    }
}

/// Refresh one explicitly selected configured provider/model for a dispatch
/// boundary. The resulting facts are ephemeral: this function never writes or
/// replaces the shared observation snapshot and never selects a fallback.
pub fn fresh_candidate_facts(
    settings: &Settings,
    provider_id: &str,
    model_id: &str,
    deadline: Instant,
    consent: &ModelFreeRefreshConsent,
) -> Result<FreshCandidateObservation, RefreshRefusal> {
    consent.validate()?;
    if Instant::now() >= deadline {
        return Err(RefreshRefusal::Deadline);
    }
    let profile = settings
        .providers
        .iter()
        .find(|profile| profile.id == provider_id)
        .ok_or(RefreshRefusal::UnknownProfile)?;
    let _model = profile
        .models
        .iter()
        .find(|model| model.id == model_id)
        .ok_or(RefreshRefusal::UnknownProfile)?;
    if !consent.permits(profile.kind) {
        return Err(RefreshRefusal::NotConsented);
    }
    if !matches!(
        profile.kind,
        ProviderKind::Codex | ProviderKind::OpenRouter | ProviderKind::Ollama
    ) {
        return Err(RefreshRefusal::UnsupportedProvider);
    }

    let runtime_identity = providers::runtime_identity_fingerprint_until(profile, Some(deadline))
        .map_err(|reason| {
        if Instant::now() >= deadline || reason.contains("deadline") {
            RefreshRefusal::Deadline
        } else {
            RefreshRefusal::RuntimeChanged
        }
    })?;
    let runtime_installed =
        Observation::known(true, "configured-executable-inspection", unix_time_ms());
    let mut refreshed = match profile.kind {
        ProviderKind::Codex => refresh_selected_codex(
            profile,
            model_id,
            runtime_identity.clone(),
            runtime_installed,
            deadline,
        )?,
        ProviderKind::OpenRouter => refresh_selected_openrouter(
            profile,
            model_id,
            runtime_identity.clone(),
            runtime_installed,
            deadline,
        )?,
        ProviderKind::Ollama => refresh_selected_ollama(
            profile,
            model_id,
            runtime_identity.clone(),
            runtime_installed,
            deadline,
        )?,
        _ => return Err(RefreshRefusal::UnsupportedProvider),
    };
    if Instant::now() >= deadline {
        return Err(RefreshRefusal::Deadline);
    }
    let identity_after = providers::runtime_identity_fingerprint_until(profile, Some(deadline))
        .map_err(|reason| {
            if Instant::now() >= deadline || reason.contains("deadline") {
                RefreshRefusal::Deadline
            } else {
                RefreshRefusal::RuntimeChanged
            }
        })?;
    if identity_after != runtime_identity {
        return Err(RefreshRefusal::RuntimeChanged);
    }
    let observed_at_ms = unix_time_ms();
    refreshed.runtime_installed =
        Observation::known(true, "configured-executable-inspection", observed_at_ms);
    refreshed.health_checked_at_ms = refreshed.healthy.observed_at_ms;
    let settings_fingerprint =
        settings_fingerprint(settings).map_err(|reason| RefreshRefusal::ProbeFailed(reason))?;
    let snapshot = seal_snapshot(settings, observed_at_ms, vec![refreshed.clone()])
        .map_err(RefreshRefusal::ProbeFailed)?;
    let mut facts = snapshot
        .candidate_facts(settings, observed_at_ms, FRESH_FACT_MAX_AGE_MS)
        .map_err(RefreshRefusal::ProbeFailed)?;
    let index = facts
        .iter()
        .position(|candidate| {
            candidate.provider_id == provider_id && candidate.model_id == model_id
        })
        .ok_or(RefreshRefusal::ModelUnavailable)?;
    let facts = facts.swap_remove(index);
    if !facts.model_available {
        return Err(RefreshRefusal::ModelUnavailable);
    }
    if !facts.authenticated {
        return Err(RefreshRefusal::AuthenticationUnknown);
    }
    Ok(FreshCandidateObservation {
        facts,
        snapshot_fingerprint: snapshot.snapshot_fingerprint,
        settings_fingerprint,
        runtime_identity_fingerprint: runtime_identity,
        observed_at_ms,
    })
}

fn refresh_selected_codex(
    profile: &ProviderProfile,
    model_id: &str,
    runtime_identity_fingerprint: String,
    runtime_installed: Observation<bool>,
    deadline: Instant,
) -> Result<ProviderObservation, RefreshRefusal> {
    let remaining = deadline.saturating_duration_since(Instant::now());
    if remaining.is_zero() {
        return Err(RefreshRefusal::Deadline);
    }
    let probe = crate::runtime::probe_codex_account_and_models_until(profile, deadline)
        .map_err(RefreshRefusal::ProbeFailed)?;
    let observed_at_ms = unix_time_ms();
    let authenticated_value = probe
        .authenticated
        .ok_or(RefreshRefusal::AuthenticationUnknown)?;
    if !authenticated_value {
        return Err(RefreshRefusal::AuthenticationUnknown);
    }
    if !probe.model_list_verified {
        return Err(RefreshRefusal::ProbeFailed(
            "Codex model/list did not return a verified inventory.".into(),
        ));
    }
    let live = probe
        .models
        .iter()
        .find(|model| model.id == model_id)
        .ok_or(RefreshRefusal::ModelUnavailable)?;
    let mut observed_model = unknown_model(model_id, "Codex model capability was not reported.");
    observed_model.model_available =
        Observation::known(true, "codex-app-server-model-list", observed_at_ms);
    observed_model.local_only = Observation::known(
        false,
        "codex-app-server-provider-classification",
        observed_at_ms,
    );
    observed_model.billing = Observation::known(
        BillingPolicy::Subscription,
        "codex-chatgpt-account-read",
        observed_at_ms,
    );
    observed_model.supports_tools = live
        .supports_tools
        .map(|value| Observation::known(value, "codex-app-server-model-list", observed_at_ms))
        .unwrap_or_else(|| {
            Observation::known(
                true,
                "codex-app-server-command-execution-contract",
                observed_at_ms,
            )
        });
    observed_model.context_tokens = live
        .context_tokens
        .map(|value| Observation::known(value, "codex-app-server-model-list", observed_at_ms))
        .unwrap_or_else(|| Observation::unknown("Codex model/list omitted context size."));
    Ok(ProviderObservation {
        provider_id: profile.id.clone(),
        kind: profile.kind,
        runtime_identity_fingerprint,
        runtime_installed,
        authenticated: Observation::known(true, "codex-chatgpt-account-read", observed_at_ms),
        healthy: Observation::known(
            true,
            "codex-app-server-initialize-model-list",
            observed_at_ms,
        ),
        health_checked_at_ms: Some(observed_at_ms),
        models: vec![observed_model],
    })
}

fn refresh_selected_openrouter(
    profile: &ProviderProfile,
    model_id: &str,
    runtime_identity_fingerprint: String,
    runtime_installed: Observation<bool>,
    deadline: Instant,
) -> Result<ProviderObservation, RefreshRefusal> {
    let secret_ref = profile
        .secret_ref
        .as_deref()
        .filter(|reference| {
            providers::openrouter_credential_reference_matches(&profile.id, reference)
        })
        .ok_or(RefreshRefusal::AuthenticationUnknown)?;
    let key = crate::credentials::resolve(secret_ref)
        .filter(|value| providers::valid_openrouter_key(value))
        .ok_or(RefreshRefusal::AuthenticationUnknown)?;
    let timeout = deadline.saturating_duration_since(Instant::now());
    if timeout.is_zero() {
        return Err(RefreshRefusal::Deadline);
    }
    let probe = providers::openrouter_metadata_http_probe_until(&key, deadline)
        .map_err(RefreshRefusal::ProbeFailed)?;
    if probe.authenticated_for_execution != Some(true) {
        return Err(RefreshRefusal::AuthenticationUnknown);
    }
    let model = probe
        .models
        .iter()
        .find(|model| model.id == model_id)
        .ok_or(RefreshRefusal::ModelUnavailable)?;
    let observed_at_ms = unix_time_ms();
    let mut observed_model = unknown_model(model_id, "OpenRouter omitted required model metadata.");
    observed_model.model_available =
        Observation::known(true, "openrouter-user-model-catalog", observed_at_ms);
    observed_model.context_tokens = model
        .context_length
        .map(|value| Observation::known(value, "openrouter-user-model-catalog", observed_at_ms))
        .unwrap_or_else(|| Observation::unknown("OpenRouter user catalog omitted context size."));
    let tool_support = model.supported_parameters.as_ref().map(|parameters| {
        parameters
            .iter()
            .any(|parameter| matches!(parameter.as_str(), "tools" | "tool_choice"))
    });
    observed_model.supports_tools = tool_support
        .map(|value| Observation::known(value, "openrouter-user-model-catalog", observed_at_ms))
        .unwrap_or_else(|| Observation::unknown("OpenRouter user catalog omitted tool metadata."));
    observed_model.local_only =
        Observation::known(false, "openrouter-user-model-catalog", observed_at_ms);
    observed_model.billing = Observation::known(
        BillingPolicy::Paid,
        "openrouter-user-model-catalog",
        observed_at_ms,
    );
    apply_authenticated_openrouter_pricing(&mut observed_model, model, observed_at_ms);
    Ok(ProviderObservation {
        provider_id: profile.id.clone(),
        kind: profile.kind,
        runtime_identity_fingerprint,
        runtime_installed,
        authenticated: Observation::known(true, "openrouter-key-and-user-catalog", observed_at_ms),
        healthy: Observation::known(true, "openrouter-key-and-user-catalog", observed_at_ms),
        health_checked_at_ms: Some(observed_at_ms),
        models: vec![observed_model],
    })
}

fn refresh_selected_ollama(
    profile: &ProviderProfile,
    model_id: &str,
    runtime_identity_fingerprint: String,
    runtime_installed: Observation<bool>,
    deadline: Instant,
) -> Result<ProviderObservation, RefreshRefusal> {
    let endpoint = profile
        .endpoint
        .as_deref()
        .ok_or(RefreshRefusal::UnsupportedProvider)?;
    let model = providers::ollama_selected_model_until(endpoint, model_id, deadline)
        .map_err(RefreshRefusal::ProbeFailed)?
        .ok_or(RefreshRefusal::ModelUnavailable)?;
    let observed_at_ms = unix_time_ms();
    let mut observed_model = unknown_model(
        model_id,
        "Ollama model capability evidence was not reported.",
    );
    observed_model.model_available =
        Observation::known(true, "ollama-loopback-model-list", observed_at_ms);
    observed_model.context_tokens = model
        .context_tokens
        .map(|value| Observation::known(value, "ollama-loopback-model-details", observed_at_ms))
        .unwrap_or_else(|| Observation::unknown("Ollama omitted context size."));
    observed_model.supports_tools = Observation::known(
        model.tools_supported,
        "ollama-loopback-model-details",
        observed_at_ms,
    );
    observed_model.local_only =
        Observation::known(true, "ollama-numeric-loopback-endpoint", observed_at_ms);
    observed_model.billing = Observation::known(
        BillingPolicy::Local,
        "ollama-numeric-loopback-endpoint",
        observed_at_ms,
    );
    Ok(ProviderObservation {
        provider_id: profile.id.clone(),
        kind: profile.kind,
        runtime_identity_fingerprint,
        runtime_installed,
        authenticated: Observation::known(true, "ollama-loopback-health", observed_at_ms),
        healthy: Observation::known(true, "ollama-loopback-model-details", observed_at_ms),
        health_checked_at_ms: Some(observed_at_ms),
        models: vec![observed_model],
    })
}

fn unix_time_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
}

fn refresh_provider(
    profile: &ProviderProfile,
    allow_live_probe: bool,
    now_ms: u128,
) -> ProviderObservation {
    let identity = providers::runtime_identity_fingerprint(profile);
    let availability = providers::inspect(profile, std::env::var_os("PATH").as_deref());
    let runtime_installed = match (&identity, availability.executable_found) {
        (Ok(_), true) => Observation::known(true, "configured-executable-inspection", now_ms),
        (Ok(_), false) => Observation::known(false, "configured-executable-inspection", now_ms),
        (Err(reason), _) => Observation::unavailable(reason.clone()),
    };
    let identity_fingerprint =
        identity.unwrap_or_else(|_| stable_fingerprint(profile.executable.as_bytes()));

    if profile.kind == ProviderKind::Codex {
        return refresh_codex_provider(profile, identity_fingerprint, runtime_installed, now_ms);
    }

    if profile.kind == ProviderKind::OpenRouter && allow_live_probe {
        return refresh_openrouter_provider(
            profile,
            identity_fingerprint,
            runtime_installed,
            Instant::now() + std::time::Duration::from_secs(15),
        );
    }

    if !allow_live_probe && profile.kind == ProviderKind::OpenRouter {
        return ProviderObservation {
            provider_id: profile.id.clone(),
            kind: profile.kind,
            runtime_identity_fingerprint: identity_fingerprint,
            runtime_installed,
            authenticated: Observation::unknown(
                "OpenRouter live catalog/auth probe was not requested.",
            ),
            healthy: Observation::unknown("OpenRouter live catalog/auth probe was not requested."),
            health_checked_at_ms: None,
            models: profile
                .models
                .iter()
                .take(MAX_MODELS_PER_PROVIDER)
                .map(|model| {
                    unknown_model(&model.id, "OpenRouter catalog probe was not requested.")
                })
                .collect(),
        };
    }

    let probe = providers::probe_installed(profile, allow_live_probe);
    let state = probe.get("state").and_then(serde_json::Value::as_str);
    let provider_ready = state == Some("ready");
    let authentication = match (profile.kind, state) {
        (ProviderKind::OpenRouter, Some("ready")) => {
            Observation::known(true, "openrouter-authenticated-catalog", now_ms)
        }
        (ProviderKind::OpenRouter, Some("authenticationRequired")) => {
            Observation::known(false, "openrouter-authenticated-catalog", now_ms)
        }
        (ProviderKind::Ollama, Some("ready")) => {
            Observation::known(true, "ollama-loopback-health", now_ms)
        }
        (ProviderKind::Antigravity, Some("ready")) => Observation::unknown(
            "Antigravity model listing does not independently verify account authentication.",
        ),
        (ProviderKind::Codex, _) => Observation::unknown(
            "Codex account state requires a model-free app-server account/read probe.",
        ),
        (_, Some("authenticationRequired")) => {
            Observation::known(false, "provider-refresh", now_ms)
        }
        _ => Observation::unknown("Provider authentication was not verified."),
    };
    let healthy = match state {
        Some("ready") => Observation::known(true, "provider-refresh", now_ms),
        Some("unavailable" | "unsupportedRuntime" | "missingRuntime") => {
            Observation::known(false, "provider-refresh", now_ms)
        }
        _ => Observation::unknown("Provider health was not verified."),
    };
    let models = profile
        .models
        .iter()
        .take(MAX_MODELS_PER_PROVIDER)
        .map(|model| refresh_model(profile, model, &probe, provider_ready, now_ms))
        .collect();
    ProviderObservation {
        provider_id: profile.id.clone(),
        kind: profile.kind,
        runtime_identity_fingerprint: identity_fingerprint,
        runtime_installed,
        authenticated: authentication,
        healthy,
        health_checked_at_ms: provider_ready.then_some(now_ms),
        models,
    }
}

fn refresh_openrouter_provider(
    profile: &ProviderProfile,
    runtime_identity_fingerprint: String,
    runtime_installed: Observation<bool>,
    deadline: Instant,
) -> ProviderObservation {
    let secret = profile
        .secret_ref
        .as_deref()
        .filter(|reference| {
            providers::openrouter_credential_reference_matches(&profile.id, reference)
        })
        .and_then(crate::credentials::resolve)
        .filter(|value| providers::valid_openrouter_key(value));
    let Some(secret) = secret else {
        return ProviderObservation {
            provider_id: profile.id.clone(),
            kind: profile.kind,
            runtime_identity_fingerprint,
            runtime_installed,
            authenticated: Observation::unknown("A valid OpenRouter key reference is unavailable."),
            healthy: Observation::unknown("OpenRouter user-scoped catalog was not checked."),
            health_checked_at_ms: None,
            models: profile
                .models
                .iter()
                .take(MAX_MODELS_PER_PROVIDER)
                .map(|model| unknown_model(&model.id, "OpenRouter authentication is unknown."))
                .collect(),
        };
    };
    let probe = providers::openrouter_metadata_http_probe_until(&secret, deadline);
    match probe {
        Ok(probe) if probe.authenticated_for_execution == Some(true) => {
            let now_ms = unix_time_ms();
            let models = refresh_authenticated_openrouter_models(profile, &probe.models, now_ms);
            ProviderObservation {
                provider_id: profile.id.clone(),
                kind: profile.kind,
                runtime_identity_fingerprint,
                runtime_installed,
                authenticated: Observation::known(true, "openrouter-key-and-user-catalog", now_ms),
                healthy: Observation::known(true, "openrouter-key-and-user-catalog", now_ms),
                health_checked_at_ms: Some(now_ms),
                models,
            }
        }
        Ok(probe) if probe.authenticated_for_execution == Some(false) => {
            let now_ms = unix_time_ms();
            ProviderObservation {
                provider_id: profile.id.clone(),
                kind: profile.kind,
                runtime_identity_fingerprint,
                runtime_installed,
                authenticated: Observation::known(false, "openrouter-key-metadata", now_ms),
                healthy: Observation::unknown(
                    "OpenRouter user catalog was not checked for this key.",
                ),
                health_checked_at_ms: None,
                models: profile
                    .models
                    .iter()
                    .take(MAX_MODELS_PER_PROVIDER)
                    .map(|model| {
                        unknown_model(
                            &model.id,
                            "OpenRouter key is not eligible for model execution.",
                        )
                    })
                    .collect(),
            }
        }
        _ => ProviderObservation {
            provider_id: profile.id.clone(),
            kind: profile.kind,
            runtime_identity_fingerprint,
            runtime_installed,
            authenticated: Observation::unknown(
                "OpenRouter execution authentication could not be verified.",
            ),
            healthy: Observation::unknown("OpenRouter user-scoped catalog could not be verified."),
            health_checked_at_ms: None,
            models: profile
                .models
                .iter()
                .take(MAX_MODELS_PER_PROVIDER)
                .map(|model| {
                    unknown_model(
                        &model.id,
                        "OpenRouter user-scoped model evidence is unavailable.",
                    )
                })
                .collect(),
        },
    }
}

fn refresh_codex_provider(
    profile: &ProviderProfile,
    identity_fingerprint: String,
    runtime_installed: Observation<bool>,
    now_ms: u128,
) -> ProviderObservation {
    let probe =
        crate::runtime::probe_codex_account_and_models(profile, std::time::Duration::from_secs(10));
    let (authenticated, healthy, models) = match probe {
        Ok(probe) => {
            let authenticated = probe
                .authenticated
                .map(|value| Observation::known(value, "codex-app-server-account-read", now_ms))
                .unwrap_or_else(|| {
                    Observation::unknown("Codex account/read did not establish login state.")
                });
            let healthy = if probe.model_list_verified {
                Observation::known(
                    true,
                    "codex-app-server-initialize-account-model-list",
                    now_ms,
                )
            } else {
                Observation::unknown(
                    "Codex model/list did not return a verifiable model inventory.",
                )
            };
            let models = profile
                .models
                .iter()
                .take(MAX_MODELS_PER_PROVIDER)
                .map(|configured| {
                    let mut observation = unknown_model(
                        &configured.id,
                        "Codex model capabilities were not present in model/list.",
                    );
                    if probe.model_list_verified {
                        let live = probe.models.iter().find(|model| model.id == configured.id);
                        observation.model_available = Observation::known(
                            live.is_some(),
                            "codex-app-server-model-list",
                            now_ms,
                        );
                        observation.local_only = Observation::known(
                            false,
                            "codex-app-server-provider-classification",
                            now_ms,
                        );
                        if authenticated.value == Some(true) {
                            observation.billing = Observation::known(
                                BillingPolicy::Subscription,
                                "codex-chatgpt-account-read",
                                now_ms,
                            );
                        }
                        if let Some(live) = live {
                            observation.supports_tools = live
                                .supports_tools
                                .map(|value| {
                                    Observation::known(value, "codex-app-server-model-list", now_ms)
                                })
                                .unwrap_or_else(|| {
                                    Observation::known(
                                        true,
                                        "codex-app-server-command-execution-contract",
                                        now_ms,
                                    )
                                });
                            observation.context_tokens = live
                                .context_tokens
                                .map(|value| {
                                    Observation::known(value, "codex-app-server-model-list", now_ms)
                                })
                                .unwrap_or_else(|| {
                                    Observation::unknown("Codex model/list omitted context size.")
                                });
                        }
                    }
                    observation
                })
                .collect();
            (authenticated, healthy, models)
        }
        Err(reason) => {
            let unavailable = Observation::unavailable(reason);
            (
                Observation::unknown("Codex account state could not be verified."),
                unavailable,
                profile
                    .models
                    .iter()
                    .take(MAX_MODELS_PER_PROVIDER)
                    .map(|model| {
                        unknown_model(&model.id, "Codex app-server model/list probe failed.")
                    })
                    .collect(),
            )
        }
    };
    let health_checked_at_ms = healthy.observed_at_ms;
    ProviderObservation {
        provider_id: profile.id.clone(),
        kind: profile.kind,
        runtime_identity_fingerprint: identity_fingerprint,
        runtime_installed,
        authenticated,
        healthy,
        health_checked_at_ms,
        models,
    }
}

fn refresh_authenticated_openrouter_models(
    profile: &ProviderProfile,
    catalog_models: &[providers::OpenRouterModel],
    now_ms: u128,
) -> Vec<ProviderModelObservation> {
    let catalog = serde_json::json!({"models": catalog_models});
    profile
        .models
        .iter()
        .take(MAX_MODELS_PER_PROVIDER)
        .map(|model| {
            let mut observed = refresh_model(profile, model, &catalog, true, now_ms);
            if let Some(catalog_model) = catalog_models.iter().find(|entry| entry.id == model.id) {
                apply_authenticated_openrouter_pricing(&mut observed, catalog_model, now_ms);
            }
            observed
        })
        .collect()
}
fn apply_authenticated_openrouter_pricing(
    observation: &mut ProviderModelObservation,
    model: &providers::OpenRouterModel,
    observed_at_ms: u128,
) {
    observation.input_micros_per_million_tokens = model
        .prompt_price
        .as_deref()
        .and_then(price_per_token_to_micros_per_million)
        .map(|value| Observation::known(value, "openrouter-user-model-catalog", observed_at_ms))
        .unwrap_or_else(|| Observation::unknown("OpenRouter omitted valid input pricing."));
    observation.output_micros_per_million_tokens = model
        .completion_price
        .as_deref()
        .and_then(price_per_token_to_micros_per_million)
        .map(|value| Observation::known(value, "openrouter-user-model-catalog", observed_at_ms))
        .unwrap_or_else(|| Observation::unknown("OpenRouter omitted valid output pricing."));
    observation.pricing_source = Observation::known(
        OPENROUTER_AUTHENTICATED_CATALOG_SOURCE.to_owned(),
        "openrouter-user-model-catalog",
        observed_at_ms,
    );
    observation.pricing_verified_at_ms = Some(observed_at_ms);
}
fn refresh_model(
    profile: &ProviderProfile,
    model: &providers::ModelProfile,
    probe: &serde_json::Value,
    provider_ready: bool,
    now_ms: u128,
) -> ProviderModelObservation {
    let mut result = unknown_model(
        &model.id,
        "Model capability evidence was not returned by the provider.",
    );
    if !provider_ready {
        result.model_available =
            Observation::unknown("Provider did not return a ready model inventory.");
        return result;
    }
    match profile.kind {
        ProviderKind::OpenRouter => {
            let catalog = probe
                .get("models")
                .and_then(serde_json::Value::as_array)
                .into_iter()
                .flatten()
                .find(|entry| {
                    entry.get("id").and_then(serde_json::Value::as_str) == Some(&model.id)
                });
            let Some(entry) = catalog else {
                result.model_available =
                    Observation::known(false, "openrouter-user-model-catalog", now_ms);
                result.billing = Observation::known(
                    BillingPolicy::Paid,
                    "openrouter-user-model-catalog",
                    now_ms,
                );
                result.local_only =
                    Observation::known(false, "openrouter-provider-classification", now_ms);
                return result;
            };
            result.model_available =
                Observation::known(true, "openrouter-user-model-catalog", now_ms);
            result.local_only =
                Observation::known(false, "openrouter-provider-classification", now_ms);
            result.billing =
                Observation::known(BillingPolicy::Paid, "openrouter-user-model-catalog", now_ms);
            result.context_tokens = entry
                .get("contextLength")
                .and_then(serde_json::Value::as_u64)
                .map(|value| Observation::known(value, "openrouter-user-model-catalog", now_ms))
                .unwrap_or_else(|| Observation::unknown("Catalog omitted model context length."));
            let tool_support = entry
                .get("supportedParameters")
                .and_then(serde_json::Value::as_array)
                .map(|parameters| {
                    parameters
                        .iter()
                        .any(|value| matches!(value.as_str(), Some("tools" | "tool_choice")))
                });
            result.supports_tools = tool_support
                .map(|value| Observation::known(value, "openrouter-user-model-catalog", now_ms))
                .unwrap_or_else(|| Observation::unknown("Catalog omitted tool support metadata."));
            let pricing = entry.get("promptPrice").and_then(serde_json::Value::as_str);
            result.input_micros_per_million_tokens = pricing
                .and_then(price_per_token_to_micros_per_million)
                .map(|value| Observation::known(value, "openrouter-user-model-catalog", now_ms))
                .unwrap_or_else(|| Observation::unknown("Catalog omitted valid input pricing."));
            let pricing = entry
                .get("completionPrice")
                .and_then(serde_json::Value::as_str);
            result.output_micros_per_million_tokens = pricing
                .and_then(price_per_token_to_micros_per_million)
                .map(|value| Observation::known(value, "openrouter-user-model-catalog", now_ms))
                .unwrap_or_else(|| Observation::unknown("Catalog omitted valid output pricing."));
            result.pricing_source = Observation::known(
                "OpenRouter /models catalog".into(),
                "openrouter-user-model-catalog",
                now_ms,
            );
            result.pricing_verified_at_ms = Some(now_ms);
        }
        ProviderKind::Ollama => {
            let local = probe
                .get("models")
                .and_then(serde_json::Value::as_array)
                .into_iter()
                .flatten()
                .find(|entry| {
                    entry.get("id").and_then(serde_json::Value::as_str) == Some(&model.id)
                });
            let Some(entry) = local else {
                result.model_available =
                    Observation::known(false, "ollama-loopback-model-list", now_ms);
                result.local_only = Observation::known(true, "ollama-loopback-endpoint", now_ms);
                result.billing =
                    Observation::known(BillingPolicy::Local, "ollama-loopback-endpoint", now_ms);
                return result;
            };
            result.model_available = Observation::known(true, "ollama-loopback-model-list", now_ms);
            result.local_only = Observation::known(true, "ollama-loopback-endpoint", now_ms);
            result.billing =
                Observation::known(BillingPolicy::Local, "ollama-loopback-endpoint", now_ms);
            result.context_tokens = entry
                .get("contextTokens")
                .and_then(serde_json::Value::as_u64)
                .map(|value| Observation::known(value, "ollama-model-details", now_ms))
                .unwrap_or_else(|| Observation::unknown("Local provider omitted context size."));
            result.supports_tools = entry
                .get("toolsSupported")
                .and_then(serde_json::Value::as_bool)
                .map(|value| Observation::known(value, "ollama-model-details", now_ms))
                .unwrap_or_else(|| Observation::unknown("Local provider omitted tool capability."));
        }
        ProviderKind::Codex => {
            result.model_available = Observation::unknown(
                "Codex model/list evidence is not available from the version-only probe.",
            );
        }
        ProviderKind::Antigravity => {
            let listed = probe
                .get("models")
                .and_then(serde_json::Value::as_array)
                .into_iter()
                .flatten()
                .any(|entry| entry.as_str() == Some(model.id.as_str()));
            result.model_available = Observation::known(listed, "antigravity-model-list", now_ms);
            result.local_only =
                Observation::known(false, "antigravity-provider-classification", now_ms);
        }
        ProviderKind::Custom => {
            result.model_available = Observation::unknown(
                "Provider model inventory does not include verified capability metadata.",
            );
        }
    }
    result
}

fn unknown_model(model_id: &str, reason: &str) -> ProviderModelObservation {
    ProviderModelObservation {
        model_id: model_id.to_owned(),
        model_available: Observation::unknown(reason),
        context_tokens: Observation::unknown(reason),
        supports_tools: Observation::unknown(reason),
        local_only: Observation::unknown(reason),
        billing: Observation::unknown(reason),
        input_micros_per_million_tokens: Observation::unknown(reason),
        output_micros_per_million_tokens: Observation::unknown(reason),
        pricing_source: Observation::unknown(reason),
        pricing_verified_at_ms: None,
    }
}

/// OpenRouter reports USD per token. Convert decimal values without floating
/// point rounding to integer micro-USD per million tokens.
fn price_per_token_to_micros_per_million(value: &str) -> Option<u64> {
    let trimmed = value.trim();
    if trimmed.is_empty() || trimmed.starts_with('-') {
        return None;
    }
    let (whole, fraction) = trimmed.split_once('.').unwrap_or((trimmed, ""));
    if whole.is_empty()
        || !whole.bytes().all(|byte| byte.is_ascii_digit())
        || !fraction.bytes().all(|byte| byte.is_ascii_digit())
        || fraction.len() > 12
    {
        return None;
    }
    let whole = whole.parse::<u128>().ok()?;
    let scale = 10u128.checked_pow(fraction.len() as u32)?;
    let fraction = if fraction.is_empty() {
        0
    } else {
        fraction.parse::<u128>().ok()?
    };
    let numerator = whole.checked_mul(scale)?.checked_add(fraction)?;
    let scaled = numerator.checked_mul(1_000_000_000_000u128)? / scale;
    u64::try_from(scaled).ok()
}

impl ProviderObservationSnapshot {
    pub fn validate(&self, settings: &Settings, now_ms: u128) -> Result<(), String> {
        if self.schema_version != PROVIDER_OBSERVATION_SCHEMA_VERSION {
            return Err("Provider observation snapshot version is unsupported.".into());
        }
        if self.observed_at_ms > now_ms
            || self.settings_fingerprint != settings_fingerprint(settings)?
            || self.snapshot_fingerprint != snapshot_fingerprint(self)?
        {
            return Err(
                "Provider observation snapshot is stale or does not match current settings.".into(),
            );
        }
        if self.providers.len() > 16
            || self.providers.iter().any(|provider| {
                provider.models.len() > MAX_MODELS_PER_PROVIDER
                    || provider.provider_id.len() > 96
                    || provider.provider_id.chars().any(char::is_control)
                    || provider.models.iter().any(|model| {
                        model.model_id.trim().is_empty()
                            || model.model_id.len() > 160
                            || model.model_id.chars().any(char::is_control)
                    })
            })
        {
            return Err("Provider observation snapshot exceeds its bounded shape.".into());
        }
        Ok(())
    }

    /// Converts persisted evidence into router inputs. Stale or unknown fields
    /// block only their own provider/model candidate; independent fresh rows
    /// remain available for preview.
    pub fn candidate_facts(
        &self,
        settings: &Settings,
        now_ms: u128,
        max_age_ms: u128,
    ) -> Result<Vec<CandidateFacts>, String> {
        self.validate(settings, now_ms)?;
        let mut facts = Vec::new();
        for observed in &self.providers {
            let Some(profile) = settings.providers.iter().find(|profile| {
                profile.id == observed.provider_id && profile.kind == observed.kind
            }) else {
                continue;
            };
            let runtime_matches = providers::runtime_identity_fingerprint(profile)
                .is_ok_and(|value| value == observed.runtime_identity_fingerprint);
            for model in &observed.models {
                let mut reasons = Vec::new();
                if !runtime_matches {
                    reasons.push(
                        "Configured runtime identity changed; refresh provider evidence."
                            .to_owned(),
                    );
                }
                let installed = known_fresh(&observed.runtime_installed, now_ms, max_age_ms);
                let authenticated = known_fresh(&observed.authenticated, now_ms, max_age_ms);
                let healthy = known_fresh(&observed.healthy, now_ms, max_age_ms);
                let available = known_fresh(&model.model_available, now_ms, max_age_ms);
                let context = known_fresh(&model.context_tokens, now_ms, max_age_ms);
                let tools = known_fresh(&model.supports_tools, now_ms, max_age_ms);
                let privacy = known_fresh(&model.local_only, now_ms, max_age_ms);
                let billing = known_fresh(&model.billing, now_ms, max_age_ms);
                for (ready, label, observation_reason) in [
                    (
                        installed,
                        "runtime installation",
                        observation_block_reason(&observed.runtime_installed, now_ms, max_age_ms),
                    ),
                    (
                        authenticated,
                        "authentication",
                        observation_block_reason(&observed.authenticated, now_ms, max_age_ms),
                    ),
                    (
                        healthy,
                        "provider health",
                        observation_block_reason(&observed.healthy, now_ms, max_age_ms),
                    ),
                    (
                        available,
                        "model availability",
                        observation_block_reason(&model.model_available, now_ms, max_age_ms),
                    ),
                    (
                        privacy,
                        "privacy classification",
                        observation_block_reason(&model.local_only, now_ms, max_age_ms),
                    ),
                    (
                        billing,
                        "billing policy",
                        observation_block_reason(&model.billing, now_ms, max_age_ms),
                    ),
                ] {
                    if !ready {
                        reasons.push(format!("{label}: {observation_reason}"));
                    }
                }
                let input_price =
                    fresh_optional(&model.input_micros_per_million_tokens, now_ms, max_age_ms);
                let output_price =
                    fresh_optional(&model.output_micros_per_million_tokens, now_ms, max_age_ms);
                let pricing_source = fresh_optional(&model.pricing_source, now_ms, max_age_ms);
                let pricing_verified_at_ms = model.pricing_verified_at_ms.filter(|value| {
                    *value <= now_ms && now_ms.saturating_sub(*value) <= max_age_ms
                });
                let billing_value = model.billing.value.unwrap_or(BillingPolicy::Unknown);
                if billing_value == BillingPolicy::Paid
                    && (input_price.is_none()
                        || output_price.is_none()
                        || pricing_source.is_none()
                        || pricing_verified_at_ms.is_none())
                {
                    reasons.push("Verified current per-token pricing is unavailable.".into());
                }
                facts.push(CandidateFacts {
                    provider_id: observed.provider_id.clone(),
                    model_id: model.model_id.clone(),
                    runtime_id: format!(
                        "{}:{}",
                        kind_id(observed.kind),
                        observed.runtime_identity_fingerprint
                    ),
                    runtime_installed: installed
                        && runtime_matches
                        && observed.runtime_installed.value == Some(true),
                    authenticated: authenticated && observed.authenticated.value == Some(true),
                    healthy: healthy && observed.healthy.value == Some(true),
                    health_checked_at_ms: observed.health_checked_at_ms,
                    model_available: available && model.model_available.value == Some(true),
                    local_only: privacy && model.local_only.value == Some(true),
                    context_tokens: if context {
                        model.context_tokens.value
                    } else {
                        None
                    },
                    supports_tools: if tools {
                        model.supports_tools.value
                    } else {
                        None
                    },
                    evidence_ready: reasons.is_empty(),
                    evidence_reasons: reasons.into_iter().take(8).collect(),
                    billing: billing_value,
                    input_micros_per_million_tokens: input_price,
                    output_micros_per_million_tokens: output_price,
                    pricing_source,
                    pricing_verified_at_ms,
                });
            }
        }
        Ok(facts)
    }
}

fn known_fresh<T>(observation: &Observation<T>, now_ms: u128, max_age_ms: u128) -> bool {
    observation.is_fresh_known(now_ms, max_age_ms)
}

fn observation_block_reason<T>(
    observation: &Observation<T>,
    now_ms: u128,
    max_age_ms: u128,
) -> String {
    if observation.state == ObservationState::Known {
        if observation
            .observed_at_ms
            .is_some_and(|time| time > now_ms || now_ms.saturating_sub(time) > max_age_ms)
        {
            return "evidence is stale; refresh provider observations".into();
        }
        if observation.value.is_none() {
            return "known evidence omitted its value".into();
        }
        if observation
            .source
            .as_deref()
            .is_none_or(|source| source.trim().is_empty())
        {
            return "evidence source is missing".into();
        }
    }
    observation
        .reason
        .as_deref()
        .map(bound_reason_str)
        .unwrap_or_else(|| "no verified observation is available".into())
}

fn bound_reason_str(reason: &str) -> String {
    reason.chars().take(MAX_REASON_CHARS).collect()
}

fn fresh_optional<T: Clone>(
    observation: &Observation<T>,
    now_ms: u128,
    max_age_ms: u128,
) -> Option<T> {
    observation
        .is_fresh_known(now_ms, max_age_ms)
        .then(|| observation.value.clone())
        .flatten()
}

fn bound_reason(reason: String) -> String {
    reason.chars().take(MAX_REASON_CHARS).collect()
}

fn settings_fingerprint(settings: &Settings) -> Result<String, String> {
    let bytes = serde_json::to_vec(settings)
        .map_err(|_| "Could not fingerprint provider settings.".to_string())?;
    Ok(stable_fingerprint(&bytes))
}

fn snapshot_fingerprint(snapshot: &ProviderObservationSnapshot) -> Result<String, String> {
    let mut copy = snapshot.clone();
    copy.snapshot_fingerprint.clear();
    let bytes = serde_json::to_vec(&copy)
        .map_err(|_| "Could not fingerprint provider observations.".to_string())?;
    Ok(stable_fingerprint(&bytes))
}

pub(crate) fn seal_snapshot(
    settings: &Settings,
    observed_at_ms: u128,
    providers: Vec<ProviderObservation>,
) -> Result<ProviderObservationSnapshot, String> {
    let mut snapshot = ProviderObservationSnapshot {
        schema_version: PROVIDER_OBSERVATION_SCHEMA_VERSION,
        settings_fingerprint: settings_fingerprint(settings)?,
        snapshot_fingerprint: String::new(),
        observed_at_ms,
        providers,
    };
    snapshot.snapshot_fingerprint = snapshot_fingerprint(&snapshot)?;
    Ok(snapshot)
}

pub(crate) fn persist_snapshot(
    data_root: &Path,
    snapshot: &ProviderObservationSnapshot,
) -> Result<(), String> {
    fs::create_dir_all(data_root)
        .map_err(|_| "Could not create provider observation storage.".to_string())?;
    let bytes = serde_json::to_vec(snapshot)
        .map_err(|_| "Could not encode provider observation snapshot.".to_string())?;
    if bytes.len() as u64 > MAX_SNAPSHOT_BYTES {
        return Err("Provider observation snapshot exceeds its size limit.".into());
    }
    let target = data_root.join("provider-observations.json");
    let temporary = data_root.join(format!(
        "provider-observations.{}.{}.tmp",
        std::process::id(),
        REFRESH_TEMP_COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temporary)
        .map_err(|_| {
            "Could not create provider observation snapshot temporary file.".to_string()
        })?;
    if file.write_all(&bytes).is_err() || file.sync_all().is_err() {
        let _ = fs::remove_file(&temporary);
        return Err("Could not write provider observation snapshot.".into());
    }
    drop(file);
    if replace_snapshot_file(&temporary, &target).is_err() {
        let _ = fs::remove_file(&temporary);
        return Err("Could not atomically replace provider observation snapshot.".into());
    }
    Ok(())
}

#[cfg(windows)]
fn replace_snapshot_file(source: &Path, destination: &Path) -> std::io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    let source = source
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    let destination = destination
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    let succeeded = unsafe {
        windows_sys::Win32::Storage::FileSystem::MoveFileExW(
            source.as_ptr(),
            destination.as_ptr(),
            windows_sys::Win32::Storage::FileSystem::MOVEFILE_REPLACE_EXISTING
                | windows_sys::Win32::Storage::FileSystem::MOVEFILE_WRITE_THROUGH,
        )
    };
    if succeeded == 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(not(windows))]
fn replace_snapshot_file(source: &Path, destination: &Path) -> std::io::Result<()> {
    fs::rename(source, destination)
}

fn stable_fingerprint(bytes: &[u8]) -> String {
    fn fnv(bytes: &[u8], seed: u64) -> u64 {
        bytes.iter().fold(seed, |mut hash, byte| {
            hash ^= u64::from(*byte);
            hash.wrapping_mul(0x100000001b3)
        })
    }
    format!(
        "{:016x}{:016x}",
        fnv(bytes, 0xcbf29ce484222325),
        fnv(bytes, 0x84222325cbf29ce4)
    )
}

fn kind_id(kind: ProviderKind) -> &'static str {
    match kind {
        ProviderKind::Codex => "codex",
        ProviderKind::Antigravity => "antigravity",
        ProviderKind::Ollama => "ollama",
        ProviderKind::OpenRouter => "openrouter",
        ProviderKind::Custom => "custom",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture_settings(executable: &Path) -> Settings {
        let mut settings = Settings::default();
        settings.providers.push(ProviderProfile {
            id: "codex-test".into(),
            kind: ProviderKind::Codex,
            name: "Synthetic Codex".into(),
            executable: executable.to_string_lossy().into_owned(),
            endpoint: None,
            secret_ref: Some("credential-store:synthetic".into()),
            models: vec![providers::ModelProfile {
                id: "model-test".into(),
                name: "Synthetic model".into(),
                reasoning_efforts: vec!["low".into()],
                context_window_tokens: None,
                supports_tools: None,
                quality_rating: None,
            }],
        });
        settings
    }

    fn fixture_provider(settings: &Settings, observed_at_ms: u128) -> ProviderObservation {
        let profile = &settings.providers[0];
        ProviderObservation {
            provider_id: profile.id.clone(),
            kind: profile.kind,
            runtime_identity_fingerprint: providers::runtime_identity_fingerprint(profile)
                .expect("synthetic executable bytes are inspectable"),
            runtime_installed: Observation::known(
                true,
                "fixture-runtime-inspection",
                observed_at_ms,
            ),
            authenticated: Observation::known(true, "fixture-account-read", observed_at_ms),
            healthy: Observation::known(true, "fixture-app-server", observed_at_ms),
            health_checked_at_ms: Some(observed_at_ms),
            models: vec![ProviderModelObservation {
                model_id: profile.models[0].id.clone(),
                model_available: Observation::known(true, "fixture-model-list", observed_at_ms),
                context_tokens: Observation::unknown("fixture omitted context"),
                supports_tools: Observation::unknown("fixture omitted tool metadata"),
                local_only: Observation::known(
                    false,
                    "fixture-provider-classification",
                    observed_at_ms,
                ),
                billing: Observation::known(
                    BillingPolicy::Subscription,
                    "fixture-account-type",
                    observed_at_ms,
                ),
                input_micros_per_million_tokens: Observation::unknown("not a paid model"),
                output_micros_per_million_tokens: Observation::unknown("not a paid model"),
                pricing_source: Observation::unknown("subscription billing has no API price"),
                pricing_verified_at_ms: None,
            }],
        }
    }

    fn synthetic_executable(directory: &Path) -> PathBuf {
        let path = directory.join("codex-fixture.exe");
        fs::write(
            &path,
            b"synthetic executable identity bytes; never executed",
        )
        .unwrap();
        path
    }

    #[test]
    fn non_known_observations_never_carry_values_or_become_fresh() {
        let unknown: Observation<bool> = Observation::unknown("not probed");
        assert_eq!(unknown.state, ObservationState::Unknown);
        assert!(unknown.value.is_none());
        assert!(!unknown.is_fresh_known(100, 500));
        let known = Observation::known(true, "synthetic", 90);
        assert!(known.is_fresh_known(100, 20));
        assert!(!known.is_fresh_known(100, 9));
    }

    #[test]
    fn model_free_refresh_consent_defaults_disabled_and_rejects_ambiguous_scope() {
        let disabled = ModelFreeRefreshConsent::default();
        assert_eq!(disabled, ModelFreeRefreshConsent::Disabled);
        assert!(disabled.validate().is_ok());
        assert_eq!(
            serde_json::to_value(&disabled).unwrap(),
            serde_json::json!({"mode": "disabled"})
        );

        let duplicate = ModelFreeRefreshConsent::Providers {
            kinds: vec![ProviderKind::Codex, ProviderKind::Codex],
        };
        assert_eq!(duplicate.validate(), Err(RefreshRefusal::InvalidConsent));
        let unsupported = ModelFreeRefreshConsent::Providers {
            kinds: vec![ProviderKind::Custom],
        };
        assert_eq!(
            unsupported.validate(),
            Err(RefreshRefusal::UnsupportedProvider)
        );
        let unbounded = ModelFreeRefreshConsent::Providers {
            kinds: vec![
                ProviderKind::Codex,
                ProviderKind::OpenRouter,
                ProviderKind::Ollama,
                ProviderKind::Antigravity,
                ProviderKind::Custom,
                ProviderKind::Codex,
            ],
        };
        assert_eq!(unbounded.validate(), Err(RefreshRefusal::InvalidConsent));
    }

    #[test]
    fn expired_dispatch_refresh_refuses_before_runtime_or_provider_access() {
        let directory = tempfile::tempdir().unwrap();
        let executable = directory.path().join("absent.exe");
        let settings = fixture_settings(&executable);
        let consent = ModelFreeRefreshConsent::Providers {
            kinds: vec![ProviderKind::Codex],
        };
        let refusal = fresh_candidate_facts(
            &settings,
            "codex-test",
            "model-test",
            Instant::now(),
            &consent,
        )
        .unwrap_err();
        assert_eq!(refusal, RefreshRefusal::Deadline);
        assert!(!executable.exists());
    }

    #[test]
    fn stable_fingerprint_is_deterministic_and_sensitive_to_bytes() {
        assert_eq!(
            stable_fingerprint(b"profile"),
            stable_fingerprint(b"profile")
        );
        assert_ne!(
            stable_fingerprint(b"profile"),
            stable_fingerprint(b"profile2")
        );
    }

    #[test]
    fn openrouter_decimal_prices_convert_without_float_rounding() {
        assert_eq!(
            price_per_token_to_micros_per_million("0.000001"),
            Some(1_000_000)
        );
        assert_eq!(
            price_per_token_to_micros_per_million("0.0000005"),
            Some(500_000)
        );
        assert_eq!(price_per_token_to_micros_per_million("-0.1"), None);
        assert_eq!(price_per_token_to_micros_per_million("NaN"), None);
    }

    #[test]
    fn authenticated_openrouter_preview_and_dispatch_share_quote_provenance() {
        let profile = ProviderProfile {
            id: "openrouter-test".into(),
            kind: ProviderKind::OpenRouter,
            name: "Synthetic OpenRouter".into(),
            executable: "aider".into(),
            endpoint: None,
            secret_ref: Some("credential-store:synthetic".into()),
            models: vec![providers::ModelProfile {
                id: "vendor/model".into(),
                name: "Synthetic model".into(),
                reasoning_efforts: vec!["low".into()],
                context_window_tokens: None,
                supports_tools: None,
                quality_rating: None,
            }],
        };
        let catalog_model = providers::OpenRouterModel {
            id: "vendor/model".into(),
            name: "Synthetic model".into(),
            context_length: Some(32_000),
            prompt_price: Some("0.000002".into()),
            completion_price: Some("0.000006".into()),
            input_modalities: None,
            output_modalities: None,
            supported_parameters: Some(vec!["tools".into()]),
        };
        let catalog = serde_json::json!({"models": [&catalog_model]});
        let now_ms = 1_000;

        let generic = refresh_model(&profile, &profile.models[0], &catalog, true, now_ms);
        assert_eq!(
            generic.input_micros_per_million_tokens.value,
            Some(2_000_000)
        );
        assert_eq!(
            generic.output_micros_per_million_tokens.value,
            Some(6_000_000)
        );
        assert_eq!(
            generic.pricing_source.value.as_deref(),
            Some("OpenRouter /models catalog")
        );
        let preview = refresh_authenticated_openrouter_models(
            &profile,
            std::slice::from_ref(&catalog_model),
            now_ms,
        )
        .remove(0);

        let mut dispatch = unknown_model("vendor/model", "fixture");
        apply_authenticated_openrouter_pricing(&mut dispatch, &catalog_model, now_ms);
        assert_eq!(preview.pricing_source, dispatch.pricing_source);
        assert_eq!(
            preview.input_micros_per_million_tokens,
            dispatch.input_micros_per_million_tokens
        );
        assert_eq!(
            preview.output_micros_per_million_tokens,
            dispatch.output_micros_per_million_tokens
        );
        assert_eq!(
            preview.pricing_verified_at_ms,
            dispatch.pricing_verified_at_ms
        );
        assert_eq!(
            preview.pricing_source.value.as_deref(),
            Some(OPENROUTER_AUTHENTICATED_CATALOG_SOURCE)
        );

        let changed_model = providers::OpenRouterModel {
            prompt_price: Some("0.000003".into()),
            ..catalog_model.clone()
        };
        let mut changed_rates = dispatch.clone();
        apply_authenticated_openrouter_pricing(&mut changed_rates, &changed_model, now_ms);
        assert_ne!(
            changed_rates.input_micros_per_million_tokens,
            dispatch.input_micros_per_million_tokens
        );
        let mut changed_source = dispatch.clone();
        changed_source.pricing_source = Observation::known(
            "Different authenticated catalog".into(),
            "openrouter-user-model-catalog",
            now_ms,
        );
        assert_ne!(changed_source.pricing_source, dispatch.pricing_source);
    }
    #[test]
    fn persisted_snapshot_can_be_read_and_replaced_at_the_same_path() {
        let directory = tempfile::tempdir().unwrap();
        let executable = synthetic_executable(directory.path());
        let data_root = directory.path().join("data");
        let settings = fixture_settings(&executable);
        let first = seal_snapshot(&settings, 100, vec![fixture_provider(&settings, 100)]).unwrap();
        persist_snapshot(&data_root, &first).unwrap();

        let store = ProviderObservationStore::open_read_only(&data_root).unwrap();
        let read_first = store.snapshot(&settings, 100).unwrap();
        assert_eq!(read_first, first);

        let mut replacement_provider = fixture_provider(&settings, 200);
        replacement_provider.health_checked_at_ms = Some(200);
        let replacement = seal_snapshot(&settings, 200, vec![replacement_provider]).unwrap();
        persist_snapshot(&data_root, &replacement).unwrap();

        assert_eq!(store.snapshot(&settings, 200).unwrap(), replacement);
        let files = fs::read_dir(&data_root)
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect::<Vec<_>>();
        assert_eq!(
            files,
            vec![std::ffi::OsString::from("provider-observations.json")]
        );
    }

    #[test]
    fn settings_and_executable_changes_invalidate_saved_evidence() {
        let directory = tempfile::tempdir().unwrap();
        let executable = synthetic_executable(directory.path());
        let data_root = directory.path().join("data");
        let settings = fixture_settings(&executable);
        let snapshot =
            seal_snapshot(&settings, 100, vec![fixture_provider(&settings, 100)]).unwrap();
        persist_snapshot(&data_root, &snapshot).unwrap();
        let store = ProviderObservationStore::open_read_only(&data_root).unwrap();

        let mut changed_settings = settings.clone();
        changed_settings.project_root = "C:/different-project".into();
        assert!(store.snapshot(&changed_settings, 100).is_err());

        fs::write(&executable, b"replacement executable bytes").unwrap();
        let facts = store
            .snapshot(&settings, 100)
            .unwrap()
            .candidate_facts(&settings, 100, 1_000)
            .unwrap();
        assert_eq!(facts.len(), 1);
        assert!(!facts[0].runtime_installed);
        assert!(!facts[0].evidence_ready);
        assert!(facts[0]
            .evidence_reasons
            .iter()
            .any(|reason| reason.contains("runtime identity changed")));
    }

    #[test]
    fn candidate_facts_keep_optional_capabilities_unknown_but_block_bad_required_evidence() {
        let directory = tempfile::tempdir().unwrap();
        let executable = synthetic_executable(directory.path());
        let settings = fixture_settings(&executable);
        let baseline = fixture_provider(&settings, 1_000);

        let fresh_snapshot = seal_snapshot(&settings, 1_000, vec![baseline.clone()]).unwrap();
        let facts = fresh_snapshot
            .candidate_facts(&settings, 1_000, 100)
            .unwrap();
        assert_eq!(facts.len(), 1);
        assert!(facts[0].evidence_ready);
        assert_eq!(facts[0].context_tokens, None);
        assert_eq!(facts[0].supports_tools, None);

        let mut stale_auth = baseline.clone();
        stale_auth.authenticated = Observation::known(true, "fixture-account-read", 800);
        let stale = seal_snapshot(&settings, 1_000, vec![stale_auth]).unwrap();
        let stale_facts = stale.candidate_facts(&settings, 1_000, 100).unwrap();
        assert!(!stale_facts[0].evidence_ready);
        assert!(!stale_facts[0].authenticated);
        assert!(stale_facts[0]
            .evidence_reasons
            .iter()
            .any(|reason| reason.contains("authentication")));

        let mut future_health = baseline.clone();
        future_health.healthy = Observation::known(true, "fixture-app-server", 1_001);
        let future = seal_snapshot(&settings, 1_000, vec![future_health]).unwrap();
        let future_facts = future.candidate_facts(&settings, 1_000, 100).unwrap();
        assert!(!future_facts[0].evidence_ready);
        assert!(!future_facts[0].healthy);

        let mut missing_availability = baseline;
        missing_availability.models[0].model_available =
            Observation::unknown("model-list evidence was omitted");
        let missing = seal_snapshot(&settings, 1_000, vec![missing_availability]).unwrap();
        let missing_facts = missing.candidate_facts(&settings, 1_000, 100).unwrap();
        assert!(!missing_facts[0].evidence_ready);
        assert!(!missing_facts[0].model_available);
        assert!(missing_facts[0]
            .evidence_reasons
            .iter()
            .any(|reason| reason.contains("model availability")));
    }
}
