//! Admission evidence and fixed-route policy for optional OpenRouter Economy
//! runs. Catalog presence and model self-reports are deliberately insufficient
//! to create a tested candidate.

use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

#[cfg(test)]
thread_local! {
    static TEST_EVALUATION_EVIDENCE: std::cell::RefCell<Option<CodingEvaluationEvidence>> = const { std::cell::RefCell::new(None) };
    static TEST_ENDPOINT_POLICIES: std::cell::RefCell<Vec<ReviewedEndpointPolicy>> = const { std::cell::RefCell::new(Vec::new()) };
}
use std::fs::{self, Metadata};
use std::path::{Component, Path, PathBuf};

pub const MAX_EVIDENCE_AGE_MS: u64 = 30 * 24 * 60 * 60 * 1000;

/// Provider router IDs are not fixed model identities and are never accepted
/// as a coding run pin.
pub fn is_router_model_id(model_id: &str) -> bool {
    matches!(model_id, "openrouter/free" | "openrouter/auto")
        || model_id.ends_with("/free")
        || model_id.ends_with("/auto")
}

/// Free variants must use the separate evidence-gated Economy path, regardless
/// of the user-selected pace or positive paid cost ceiling.
pub fn is_free_variant_id(model_id: &str) -> bool {
    model_id.ends_with(":free") || model_id.ends_with("/free")
}

const TRIAL_REGISTRY_JSON: &str = include_str!("../../config/economy-trial-fixtures.json");
const REVIEWED_RESULTS_JSON: &[u8] = include_bytes!("../config/economy-reviewed-results.json");
const REVIEWED_RESULTS_SHA256: &str =
    "b7bd936dd0ba60e3dcee400683b6ca28ce341fb3978667403eb4ea6d938354d2";
const REVIEWED_ENDPOINT_POLICIES_JSON: &[u8] =
    include_bytes!("../config/economy-endpoint-policies.json");
const REVIEWED_ENDPOINT_POLICIES_SHA256: &str =
    "65757ee5733e7c371c77fc5efc87ba6fd957c485b201ee978f695cd224d2dcb8";
const REVIEWED_PAID_ENDPOINT_POLICIES_JSON: &[u8] =
    include_bytes!("../config/paid-endpoint-policies.json");
const REVIEWED_PAID_ENDPOINT_POLICIES_SHA256: &str =
    "b6ffb294af8897ca2f61bdd0f00b05c7cf563075e130f9303f6f8dd86687d4ca";
const PAID_ENDPOINT_POLICY_MAX_AGE_MS: u64 = 30 * 24 * 60 * 60 * 1000;

fn paid_endpoint_policy_digest(bytes: &[u8]) -> String {
    // This is a new registry with a line-ending-independent semantic hash.
    // Existing free-economy registry hashes intentionally retain their prior
    // byte-for-byte behavior.
    let mut normalized = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'\r' && bytes.get(index + 1) == Some(&b'\n') {
            normalized.push(b'\n');
            index += 2;
        } else {
            normalized.push(bytes[index]);
            index += 1;
        }
    }
    sha256_hex(&normalized)
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ReviewedPaidEndpointPolicies {
    schema_version: u32,
    policies: Vec<ReviewedPaidEndpointPolicy>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ReviewedPaidEndpointPolicy {
    model_id: String,
    canonical_model_id: String,
    provider_slug: String,
    endpoint_tag: String,
    observed_at_ms: u64,
    source_urls: Vec<String>,
    processing_mode: String,
    no_training: bool,
    persistent_input_storage: bool,
    input_content_logging: bool,
    transient_content_processing: bool,
    operational_metadata_may_be_logged: bool,
    legal_process_exceptions_may_apply: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TrialRegistry {
    pub schema_version: u32,
    pub models: Vec<String>,
    pub limits: TrialLimits,
    pub tasks: Vec<TrialTask>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TrialLimits {
    pub max_requests_per_task: u32,
    pub max_output_tokens_per_request: u64,
    pub max_seconds_per_task: u64,
    pub max_cost_usd: u64,
    pub retries: u32,
    pub fallbacks: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TrialTask {
    pub id: String,
    pub prompt: String,
    #[serde(rename = "seedFiles")]
    pub seed_files: Vec<TrialSeedFile>,
    pub checks: Vec<TrialCheck>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TrialSeedFile {
    pub path: String,
    pub content: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TrialCheck {
    pub id: String,
}

#[derive(Debug, Clone)]
pub struct VerifiedTrialFixture {
    pub(crate) task: TrialTask,
    pub(crate) root: PathBuf,
    pub(crate) seed_inventory_fingerprint: String,
    pub(crate) fixture_set_sha256: String,
}

impl VerifiedTrialFixture {
    pub(crate) fn matches_trial_pair(&self, model_id: &str, task_id: &str) -> bool {
        let Ok(registry) = load_trial_registry() else {
            return false;
        };
        self.task.id == task_id
            && registry.models.iter().any(|model| model == model_id)
            && registry
                .tasks
                .iter()
                .any(|task| task.id == task_id && task == &self.task)
            && self.fixture_set_sha256 == trial_fixture_set_sha256()
            && is_sha256(&self.seed_inventory_fingerprint)
    }
}

#[derive(Debug, Clone)]
pub struct TrialRunArguments<'a> {
    pub settings_path: &'a Path,
    pub data_root: &'a Path,
    pub model_id: &'a str,
    pub task_id: &'a str,
    pub fixture_dir: &'a Path,
    pub synthetic_policy_consent: bool,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TrialReceipt {
    pub schema_version: u32,
    pub ok: bool,
    pub task_id: String,
    pub model_id: String,
    pub provider_id: String,
    pub runtime_id: String,
    pub runtime_version: String,
    pub state: String,
    pub request_count: u32,
    pub tokens_used: Option<u64>,
    pub latency_ms: u64,
    pub billing: String,
    pub source_fingerprint: Option<String>,
    pub failure_code: Option<String>,
    pub observed_model_id: Option<String>,
}

pub fn trial_fixture_set_sha256() -> String {
    sha256_hex(TRIAL_REGISTRY_JSON.as_bytes())
}

pub fn load_trial_registry() -> Result<TrialRegistry, String> {
    let registry: TrialRegistry = serde_json::from_str(TRIAL_REGISTRY_JSON)
        .map_err(|_| "The built-in Economy trial fixture registry is invalid.".to_string())?;
    if registry.schema_version != 1
        || registry.models.len() != 3
        || registry.tasks.len() != 3
        || registry.limits.max_requests_per_task != 3
        || registry.limits.max_output_tokens_per_request != 8_192
        || registry.limits.max_seconds_per_task != 120
        || registry.limits.max_cost_usd != 0
        || registry.limits.retries != 0
        || registry.limits.fallbacks
    {
        return Err(
            "The built-in Economy trial fixture registry violates its fixed limits.".into(),
        );
    }
    Ok(registry)
}

/// Validate exact synthetic inputs before any content leaves the fixture
/// workspace. Extra, hidden, symlinked, reparse-point, and modified files fail.
pub(crate) fn verify_trial_fixture(
    task_id: &str,
    fixture_dir: &Path,
) -> Result<VerifiedTrialFixture, String> {
    let registry = load_trial_registry()?;
    let task = registry
        .tasks
        .into_iter()
        .find(|task| task.id == task_id)
        .ok_or_else(|| "Unknown synthetic trial task.".to_string())?;
    if !fixture_dir.is_absolute() {
        return Err("Synthetic trial fixture path must be absolute.".into());
    }
    let root_meta = fs::symlink_metadata(fixture_dir)
        .map_err(|_| "Synthetic trial fixture directory is unavailable.".to_string())?;
    if !root_meta.is_dir() || is_reparse_point(&root_meta) {
        return Err("Synthetic trial fixture root must be a regular directory.".into());
    }
    let root = fixture_dir
        .canonicalize()
        .map_err(|_| "Synthetic trial fixture directory could not be resolved.".to_string())?;
    let expected = task
        .seed_files
        .iter()
        .map(|file| file.path.clone())
        .collect::<BTreeSet<_>>();
    if expected.len() != task.seed_files.len() || expected.is_empty() {
        return Err("Synthetic trial fixture registry has duplicate or empty file paths.".into());
    }
    for file in &task.seed_files {
        let relative = Path::new(&file.path);
        if relative.is_absolute()
            || relative
                .components()
                .any(|component| !matches!(component, Component::Normal(_)))
        {
            return Err("Synthetic trial registry contains an unsafe file path.".into());
        }
    }
    let mut actual = BTreeSet::new();
    for entry in fs::read_dir(&root)
        .map_err(|_| "Synthetic trial fixture inventory could not be read.".to_string())?
    {
        let entry = entry
            .map_err(|_| "Synthetic trial fixture inventory could not be read.".to_string())?;
        let path = entry.path();
        let metadata = fs::symlink_metadata(&path)
            .map_err(|_| "Synthetic trial fixture inventory could not be read.".to_string())?;
        if is_reparse_point(&metadata) {
            return Err("Synthetic trial fixture contains a symlink or reparse point.".into());
        }
        if metadata.is_dir() {
            return Err("Synthetic trial fixture may not contain subdirectories.".into());
        } else if metadata.is_file() {
            let relative = path
                .strip_prefix(&root)
                .map_err(|_| "Synthetic trial fixture escaped its root.".to_string())?
                .to_string_lossy()
                .replace('\\', "/");
            actual.insert(relative);
            if actual.len() > expected.len() {
                return Err("Synthetic trial fixture contains extra files.".into());
            }
        } else {
            return Err("Synthetic trial fixture contains a non-regular file.".into());
        }
    }
    if actual != expected {
        return Err(
            "Synthetic trial fixture file inventory does not exactly match the approved seed."
                .into(),
        );
    }
    let mut fingerprint_input = Vec::new();
    for file in &task.seed_files {
        let bytes = fs::read(root.join(&file.path))
            .map_err(|_| "Synthetic trial seed file is unavailable.".to_string())?;
        if bytes.len() != file.content.len() || bytes != file.content.as_bytes() {
            return Err("Synthetic trial seed content differs from the approved fixture.".into());
        }
        fingerprint_input.extend_from_slice(file.path.as_bytes());
        fingerprint_input.push(0);
        fingerprint_input.extend_from_slice(&bytes);
        fingerprint_input.push(0);
    }
    Ok(VerifiedTrialFixture {
        task,
        root,
        seed_inventory_fingerprint: sha256_hex(&fingerprint_input),
        fixture_set_sha256: trial_fixture_set_sha256(),
    })
}

#[cfg(windows)]
fn is_reparse_point(metadata: &Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    metadata.file_attributes() & 0x400 != 0
}

#[cfg(not(windows))]
fn is_reparse_point(metadata: &Metadata) -> bool {
    metadata.file_type().is_symlink()
}

/// Execute one fixed synthetic fixture pair with the configured shared runtime.
/// Result output intentionally excludes prompts, provider responses and source.
pub fn execute_trial(arguments: TrialRunArguments<'_>) -> TrialReceipt {
    let trial_started = std::time::Instant::now();
    let deadline = trial_started + std::time::Duration::from_secs(120);
    let mut receipt = TrialReceipt {
        schema_version: 1,
        ok: false,
        task_id: arguments.task_id.to_owned(),
        model_id: arguments.model_id.to_owned(),
        provider_id: String::new(),
        runtime_id: "aider".into(),
        runtime_version: String::new(),
        state: "failed".into(),
        request_count: 0,
        tokens_used: None,
        latency_ms: 0,
        billing: "zeroCost".into(),
        source_fingerprint: None,
        failure_code: None,
        observed_model_id: None,
    };
    let run = (|| -> Result<TrialReceipt, String> {
        if !arguments.synthetic_policy_consent {
            return Err("Explicit synthetic-only policy consent is required.".into());
        }
        let registry = load_trial_registry()?;
        if !registry
            .models
            .iter()
            .any(|model| model == arguments.model_id)
        {
            return Err("Model is outside the exact approved trial shortlist.".into());
        }
        let fixture = verify_trial_fixture(arguments.task_id, arguments.fixture_dir)?;
        receipt.source_fingerprint = Some(fixture.seed_inventory_fingerprint.clone());
        let mut settings = crate::settings::load_settings(arguments.settings_path)?;
        let selected_provider = settings
            .provider_selection
            .provider_id
            .as_deref()
            .ok_or_else(|| {
                "Select a configured OpenRouter provider before the trial.".to_string()
            })?;
        let profile = settings
            .providers
            .iter()
            .find(|provider| provider.id == selected_provider)
            .filter(|provider| provider.kind == crate::providers::ProviderKind::OpenRouter)
            .cloned()
            .ok_or_else(|| {
                "The selected provider is not a configured OpenRouter profile.".to_string()
            })?;
        receipt.provider_id = profile.id.clone();
        let runtime_version =
            crate::providers::probe_aider_runtime_version(Path::new(&profile.executable))
                .ok_or_else(|| {
                    "The configured Aider runtime is unavailable or unsupported.".to_string()
                })?;
        receipt.runtime_version = runtime_version.clone();
        if runtime_version != "0.86.2" {
            return Err("The synthetic trial is bound to Aider 0.86.2.".into());
        }
        let secret_ref = profile
            .secret_ref
            .as_deref()
            .filter(|reference| {
                crate::providers::openrouter_credential_reference_matches(&profile.id, reference)
            })
            .ok_or_else(|| {
                "A valid saved OpenRouter credential reference is required.".to_string()
            })?;
        let api_key = crate::credentials::resolve(secret_ref)
            .filter(|key| crate::providers::valid_openrouter_key(key))
            .ok_or_else(|| {
                "The saved OpenRouter credential is unavailable or invalid.".to_string()
            })?;
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        if remaining.is_zero() {
            return Err("The fixed trial deadline expired during admission.".into());
        }
        let metadata =
            crate::providers::openrouter_metadata_http_probe_with_timeout(&api_key, remaining)?;
        if metadata.authenticated_for_execution != Some(true) {
            return Err("OpenRouter execution authentication could not be verified.".into());
        }
        let model = metadata
            .models
            .iter()
            .find(|model| model.id == arguments.model_id)
            .ok_or_else(|| {
                "The exact free model is not available to this authenticated key.".to_string()
            })?;
        let input_price = model
            .prompt_price
            .as_deref()
            .and_then(crate::providers::openrouter_price_micros_per_million);
        let output_price = model
            .completion_price
            .as_deref()
            .and_then(crate::providers::openrouter_price_micros_per_million);
        let context = model
            .context_length
            .ok_or_else(|| "The exact model context capacity is unknown.".to_string())?;
        if input_price != Some(0) || output_price != Some(0) || context < 1024 {
            return Err(
                "The selected model's authenticated free pricing or context eligibility changed."
                    .into(),
            );
        }
        // A bounded input reservation gives Aider room for normal system and
        // framing tokens while remaining far below the candidate's verified
        // context window. Output stays independently capped at 8,192.
        let context_tokens = context.min(32_768);
        let output_tokens = context_tokens.min(registry.limits.max_output_tokens_per_request);
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        if remaining.is_zero() {
            return Err("The fixed trial deadline expired during admission.".into());
        }
        let endpoints = crate::providers::openrouter_endpoints_http_probe_with_timeout(
            &api_key,
            arguments.model_id,
            remaining,
        )?;
        let eligible = endpoints
            .iter()
            .filter(|endpoint| {
                endpoint.model_id == arguments.model_id
                    && endpoint.status == Some(0)
                    && endpoint
                        .context_length
                        .is_some_and(|value| value >= context_tokens)
                    && endpoint
                        .prompt_price
                        .as_deref()
                        .and_then(crate::providers::openrouter_price_micros_per_million)
                        == Some(0)
                    && endpoint
                        .completion_price
                        .as_deref()
                        .and_then(crate::providers::openrouter_price_micros_per_million)
                        == Some(0)
                    && endpoint.request_price.as_deref().is_none_or(|price| {
                        crate::providers::openrouter_price_micros_per_million(price) == Some(0)
                    })
            })
            .collect::<Vec<_>>();
        if eligible.len() != 1 {
            return Err(
                "The exact free model does not have one unambiguous active zero-price endpoint."
                    .into(),
            );
        }
        let endpoint_id = eligible[0].tag.clone();
        let canonical_slug = model
            .canonical_slug
            .clone()
            .unwrap_or_else(|| model.id.clone());
        let now_ms = crate::usage::now_ms().min(u64::MAX as u128) as u64;
        let pricing = crate::paid_gateway::PaidPricingCeiling {
            input_price_micros_per_1m: 0,
            output_price_micros_per_1m: 0,
            request_fee_micros: 0,
            source: "openrouter-authenticated-model-and-endpoint-metadata".into(),
            observed_at_ms: now_ms,
        };
        let max_calls = registry.limits.max_requests_per_task;
        let max_tokens = context_tokens
            .checked_add(output_tokens)
            .and_then(|value| value.checked_mul(u64::from(max_calls)))
            .ok_or_else(|| "The synthetic trial token reservation overflowed.".to_string())?;
        let policy = crate::paid_gateway::PaidModelPolicy {
            mode: crate::paid_gateway::GatewayMode::SyntheticTrial {
                endpoint_id,
                allow_training: true,
            },
            provider_id: profile.id.clone(),
            model_id: arguments.model_id.to_owned(),
            runtime_id: "aider".into(),
            model_context_window_tokens: context_tokens,
            max_output_tokens_per_call: output_tokens,
            max_calls,
            max_cost_micros: 0,
            pricing: pricing.clone(),
            endpoint_pin: None,
        };
        let max_seconds = registry.limits.max_seconds_per_task;
        let budget_policy = crate::budgets::BudgetPolicy {
            max_tokens,
            max_cost_micros: 0,
            max_seconds,
            max_concurrency: 1,
        };
        let data_root = arguments.data_root;
        if !data_root.is_absolute()
            || !data_root.components().any(|component| {
                component
                    .as_os_str()
                    .to_string_lossy()
                    .eq_ignore_ascii_case("private")
            })
        {
            return Err("Trial data must be stored under a private data directory.".into());
        }
        if data_root.exists() {
            return Err("A trial data root must be new for every model/task pair.".into());
        }
        let parent_directory = data_root
            .parent()
            .ok_or_else(|| "Trial data root has no parent directory.".to_string())?;
        fs::create_dir_all(parent_directory)
            .map_err(|_| "Private trial parent directory is unavailable.".to_string())?;
        fs::create_dir(data_root)
            .map_err(|_| "A fresh private trial data root could not be created.".to_string())?;
        let store = crate::budgets::BudgetStore::open(data_root.join("budgets.sqlite3"))
            .map_err(|_| "The private trial budget ledger could not be initialized.".to_string())?;
        let batch_id = format!(
            "economy-trial-{}",
            sha256_hex(
                format!(
                    "{}:{}:{}",
                    profile.id, arguments.model_id, arguments.task_id
                )
                .as_bytes()
            )
        );
        let job_id = format!(
            "{}-{}",
            arguments.model_id.replace(['/', ':'], "-"),
            arguments.task_id
        );
        store
            .configure(&batch_id, &job_id, &budget_policy, &budget_policy)
            .map_err(|_| "The private trial budget policy could not be initialized.".to_string())?;
        let reservation_id = format!(
            "trial-{}",
            sha256_hex(format!("{}:{}:{}", batch_id, job_id, now_ms).as_bytes())
        );
        let _reservation = store
            .reserve(
                &crate::budgets::ReservationRequest {
                    reservation_id: reservation_id.clone(),
                    batch_id: batch_id.clone(),
                    job_id: job_id.clone(),
                    task_id: arguments.task_id.to_owned(),
                    provider_id: profile.id.clone(),
                    model_id: arguments.model_id.to_owned(),
                    runtime_id: "aider".into(),
                    max_tokens,
                    max_cost_micros: Some(0),
                    max_seconds,
                    is_paid: false,
                    pricing_source: Some(pricing.source.clone()),
                    pricing_verified_at_ms: Some(pricing.observed_at_ms),
                },
                now_ms,
            )
            .map_err(|_| "The zero-cost trial token/time reservation was rejected.".to_string())?;
        let intent = serde_json::json!({"schemaVersion":1,"state":"intent_persisted","taskId":arguments.task_id,"modelId":arguments.model_id,"providerId":profile.id,"runtimeId":"aider","runtimeVersion":runtime_version,"fixtureSetSha256":fixture.fixture_set_sha256,"sourceFingerprint":fixture.seed_inventory_fingerprint,"syntheticPolicyConsent":true,"createdAtMs":now_ms});
        fs::write(
            data_root.join("trial-attempt.json"),
            serde_json::to_vec(&intent).unwrap_or_default(),
        )
        .map_err(|_| "The durable trial intent could not be recorded.".to_string())?;
        if !settings.providers.iter().any(|provider| {
            provider.id == profile.id
                && provider
                    .models
                    .iter()
                    .any(|configured| configured.id == arguments.model_id)
        }) {
            if let Some(configured_profile) = settings
                .providers
                .iter_mut()
                .find(|provider| provider.id == profile.id)
            {
                configured_profile
                    .models
                    .push(crate::providers::ModelProfile {
                        id: arguments.model_id.to_owned(),
                        name: model.name.clone(),
                        reasoning_efforts: Vec::new(),
                        context_window_tokens: Some(context),
                        supports_tools: model
                            .supported_parameters
                            .as_ref()
                            .map(|values| values.iter().any(|value| value == "tools")),
                        quality_rating: None,
                    });
            }
        }
        let cancel = std::sync::atomic::AtomicBool::new(false);
        struct TrialSink<'a> {
            budgets: &'a crate::budgets::BudgetStore,
            reservation_id: &'a str,
            dispatched: bool,
            stderr_tail: String,
        }
        impl crate::runtime::RuntimeEventSink for TrialSink<'_> {
            fn on_event(
                &mut self,
                event: crate::runtime::RuntimeEvent,
            ) -> crate::runtime::RuntimeControl {
                if event.kind == crate::runtime::RuntimeEventKind::ProcessStarted
                    && !self.dispatched
                {
                    if self
                        .budgets
                        .mark_dispatched(
                            self.reservation_id,
                            crate::usage::now_ms().min(u64::MAX as u128) as u64,
                        )
                        .is_err()
                    {
                        return crate::runtime::RuntimeControl::Cancel;
                    }
                    self.dispatched = true;
                }
                if event.kind == crate::runtime::RuntimeEventKind::Output
                    && event.stream.as_deref() == Some("stderr")
                {
                    append_bounded_diagnostic(&mut self.stderr_tail, &event.text);
                }
                crate::runtime::RuntimeControl::Continue
            }
        }
        let allowed_files = fixture
            .task
            .seed_files
            .iter()
            .map(|file| file.path.clone())
            .collect::<Vec<_>>();
        let mut sink = TrialSink {
            budgets: &store,
            reservation_id: &reservation_id,
            dispatched: false,
            stderr_tail: String::new(),
        };
        let runtime_remaining = deadline
            .saturating_duration_since(std::time::Instant::now())
            .as_secs();
        if runtime_remaining == 0 {
            return Err("The fixed trial deadline expired during admission.".into());
        }
        let runtime_request = crate::runtime::RuntimeRequest {
            provider_id: Some(profile.id.clone()),
            model_id: Some(arguments.model_id.to_owned()),
            cwd: fixture.root.clone(),
            prompt: fixture.task.prompt.clone(),
            timeout_seconds: runtime_remaining,
            reasoning_effort: None,
            read_only: false,
            test_command: None,
            allow_paid: true,
        };
        let outcome = crate::runtime::run_budgeted(
            &settings,
            &runtime_request,
            &cancel,
            &mut sink,
            &crate::runtime::PaidDispatchContext {
                budgets: &store,
                parent_reservation_id: &reservation_id,
                policy: &policy,
                allowed_files: &allowed_files,
                authenticated_canonical_slug: Some(&canonical_slug),
                economy_trial_task_id: Some(arguments.task_id),
                synthetic_trial_consent: arguments.synthetic_policy_consent,
                synthetic_trial_fixture: Some(&fixture),
                economy_admission: None,
                cancel: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            },
        );
        let elapsed = trial_started
            .elapsed()
            .as_millis()
            .min(u128::from(u64::MAX)) as u64;
        let mut diagnostic = None;
        let (state, usage) = match outcome {
            Ok(outcome) => {
                let calls = store
                    .free_calls_for_parent(&reservation_id)
                    .unwrap_or_default();
                receipt.observed_model_id = calls
                    .iter()
                    .find_map(|call| call.settlement_source.as_deref())
                    .and_then(|source| source.strip_prefix("openrouter-response-usage|model="))
                    .or_else(|| {
                        calls.iter().find_map(|call| {
                            call.settlement_source.as_deref().and_then(|source| {
                                source.strip_prefix("openrouter-model-mismatch|model=")
                            })
                        })
                    })
                    .filter(|value| is_safe_model_identity(value))
                    .map(str::to_owned);
                let mismatch = calls.iter().any(|call| {
                    call.settlement_source
                        .as_deref()
                        .is_some_and(|source| source.starts_with("openrouter-model-mismatch"))
                });
                let state = if mismatch {
                    "failed".to_owned()
                } else {
                    trial_runtime_state(outcome.state)
                };
                if state != "completed" {
                    receipt.failure_code = Some(
                        if mismatch {
                            "MODEL_IDENTITY_MISMATCH"
                        } else {
                            runtime_failure_code(outcome.state)
                        }
                        .into(),
                    );
                    diagnostic = Some(serde_json::json!({
                        "schemaVersion": 1,
                        "runtimeMessage": outcome.message.as_str(),
                        "runtimeExitCode": outcome.exit_code,
                        "processStarted": sink.dispatched,
                    }));
                }
                (state, Some(outcome.usage))
            }
            Err(error) => {
                receipt.failure_code = Some(runtime_error_code(error.code).into());
                diagnostic = Some(serde_json::json!({
                    "schemaVersion": 1,
                    "runtimeErrorCode": safe_runtime_error_code(error.code),
                    "runtimeMessage": redact_trial_diagnostic(
                        &error.message,
                        &fixture.root,
                        arguments.data_root,
                        arguments.settings_path,
                        &api_key,
                    ),
                    "runtimeExitCode": null,
                    "processStarted": sink.dispatched,
                }));
                ("failed".to_owned(), None)
            }
        };
        let calls = store
            .free_calls_for_parent(&reservation_id)
            .map_err(|_| "The free call ledger could not be read after the trial.".to_string())?;
        receipt.request_count = u32::try_from(calls.len()).unwrap_or(u32::MAX);
        if let Some(usage) = usage {
            receipt.tokens_used = usage.total_tokens;
        }
        receipt.latency_ms = elapsed;
        receipt.state = state.clone();
        receipt.ok = state == "completed";
        if !receipt.ok {
            let request_count = u32::try_from(calls.len()).unwrap_or(u32::MAX);
            let safe_stderr = if request_count == 0 {
                Some(redact_trial_diagnostic(
                    &sink.stderr_tail,
                    &fixture.root,
                    arguments.data_root,
                    arguments.settings_path,
                    &api_key,
                ))
            } else {
                None
            };
            let mut diagnostic = diagnostic.unwrap_or_else(|| {
                serde_json::json!({
                    "schemaVersion": 1,
                    "runtimeMessage": null,
                    "runtimeExitCode": null,
                    "processStarted": sink.dispatched,
                })
            });
            if let Some(object) = diagnostic.as_object_mut() {
                object.insert("requestCount".into(), serde_json::json!(request_count));
                object.insert("stderrTail".into(), serde_json::json!(safe_stderr));
            }
            fs::write(
                data_root.join("trial-diagnostics.json"),
                serde_json::to_vec(&diagnostic).unwrap_or_default(),
            )
            .map_err(|_| {
                "The bounded private trial diagnostics could not be recorded.".to_string()
            })?;
        }
        let current_reservation = store.get(&reservation_id).ok();
        if current_reservation
            .as_ref()
            .is_some_and(|current| current.state == crate::budgets::ReservationState::Reserved)
        {
            let _ = store.release_before_dispatch(&reservation_id);
        } else if current_reservation
            .as_ref()
            .is_some_and(|current| current.state == crate::budgets::ReservationState::Dispatched)
        {
            let _ = store.settle(
                &reservation_id,
                receipt.tokens_used,
                Some(0),
                Some(elapsed / 1000),
                "openrouter-zero-price-policy-and-token-usage",
                crate::usage::now_ms().min(u64::MAX as u128) as u64,
            );
        }
        fs::write(
            data_root.join("trial-result.json"),
            serde_json::to_vec(&receipt).unwrap_or_default(),
        )
        .map_err(|_| "The sanitized trial result could not be recorded.".to_string())?;
        Ok(receipt.clone())
    })();
    match run {
        Ok(result) => result,
        Err(error) => {
            receipt.failure_code = Some(failure_code(&error).into());
            receipt.latency_ms = trial_started
                .elapsed()
                .as_millis()
                .min(u128::from(u64::MAX)) as u64;
            receipt
        }
    }
}

fn failure_code(message: &str) -> &'static str {
    if message.contains("rate limit") {
        "RATE_LIMITED"
    } else if message.contains("deadline") {
        "TIMED_OUT"
    } else if message.contains("credential")
        || message.contains("authentication")
        || message.contains("API key")
    {
        "AUTHENTICATION_REQUIRED"
    } else if message.contains("model")
        || message.contains("endpoint")
        || message.contains("pricing")
    {
        "UNSUPPORTED_MODEL"
    } else {
        "PREFLIGHT_REJECTED"
    }
}

fn is_safe_model_identity(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 256
        && value
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || "/:._-".contains(character))
}

fn append_bounded_diagnostic(target: &mut String, chunk: &str) {
    const MAX_DIAGNOSTIC_BYTES: usize = 4_096;
    let clean = chunk
        .chars()
        .filter(|character| !character.is_control() || matches!(character, '\n' | '\r' | '\t'))
        .collect::<String>();
    target.push_str(&clean);
    if target.len() > MAX_DIAGNOSTIC_BYTES {
        let excess = target.len() - MAX_DIAGNOSTIC_BYTES;
        let boundary = target
            .char_indices()
            .find(|(index, _)| *index >= excess)
            .map(|(index, _)| index)
            .unwrap_or(0);
        target.drain(..boundary);
    }
}

fn redact_trial_diagnostic(
    text: &str,
    fixture_root: &Path,
    data_root: &Path,
    settings_path: &Path,
    api_key: &str,
) -> String {
    let mut value = text.replace(
        &fixture_root.to_string_lossy().to_string(),
        "[synthetic-fixture]",
    );
    value = value.replace(&data_root.to_string_lossy().to_string(), "[private-trial]");
    value = value.replace(
        &settings_path.to_string_lossy().to_string(),
        "[private-settings]",
    );
    if !api_key.is_empty() {
        value = value.replace(api_key, "[REDACTED]");
    }
    let mut bounded = String::new();
    append_bounded_diagnostic(&mut bounded, &value);
    bounded
}

fn runtime_failure_code(state: crate::runtime::RuntimeState) -> &'static str {
    match state {
        crate::runtime::RuntimeState::RateLimited => "RATE_LIMITED",
        crate::runtime::RuntimeState::TimedOut => "TIMED_OUT",
        crate::runtime::RuntimeState::AuthenticationRequired => "AUTHENTICATION_REQUIRED",
        crate::runtime::RuntimeState::UnsupportedModel => "UNSUPPORTED_MODEL",
        _ => "RUNTIME_FAILED",
    }
}

fn runtime_error_code(code: &str) -> &'static str {
    match code {
        "RATE_LIMITED" => "RATE_LIMITED",
        "TIMEOUT" | "TIMED_OUT" => "TIMED_OUT",
        "AUTHENTICATION_REQUIRED" => "AUTHENTICATION_REQUIRED",
        "UNSUPPORTED_MODEL" => "UNSUPPORTED_MODEL",
        "MODEL_IDENTITY_MISMATCH" => "MODEL_IDENTITY_MISMATCH",
        _ => "RUNTIME_FAILED",
    }
}

fn safe_runtime_error_code(code: &str) -> String {
    if !code.is_empty()
        && code.len() <= 64
        && code
            .bytes()
            .all(|byte| byte.is_ascii_uppercase() || byte.is_ascii_digit() || byte == b'_')
    {
        code.to_owned()
    } else {
        "RUNTIME_ERROR".into()
    }
}

fn trial_runtime_state(state: crate::runtime::RuntimeState) -> String {
    match state {
        crate::runtime::RuntimeState::Completed => "completed",
        crate::runtime::RuntimeState::RateLimited => "rateLimited",
        crate::runtime::RuntimeState::TimedOut => "timedOut",
        crate::runtime::RuntimeState::NeedsInput => "needsInput",
        crate::runtime::RuntimeState::AuthenticationRequired => "authenticationRequired",
        crate::runtime::RuntimeState::UnsupportedModel => "unsupportedModel",
        crate::runtime::RuntimeState::Cancelled => "cancelled",
        _ => "failed",
    }
    .to_owned()
}

fn sha256_hex(bytes: &[u8]) -> String {
    ring::digest::digest(&ring::digest::SHA256, bytes)
        .as_ref()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum EvidenceStatus {
    NotEvaluated,
    Passed,
    Failed,
    Stale,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum PrivacyStatus {
    Eligible,
    Unknown,
    Incompatible,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EconomyModelStatus {
    pub provider_id: String,
    pub model_id: String,
    pub context_tokens: Option<u64>,
    pub input_price_micros_per_1m: Option<u64>,
    pub output_price_micros_per_1m: Option<u64>,
    pub catalog_observed_at_ms: u64,
    pub evaluation_status: EvidenceStatus,
    pub evaluation_runtime_version: Option<String>,
    pub current_runtime_version: Option<String>,
    pub evaluation_at_ms: Option<u64>,
    pub evaluation_passed: u32,
    pub evaluation_cases: u32,
    pub evaluation_reason_codes: Vec<String>,
    pub catalog_present: bool,
    pub catalog_fresh: bool,
    pub current_zero_priced: bool,
    pub current_context_tokens: Option<u64>,
    pub capability_reason: Option<String>,
    pub privacy_status: PrivacyStatus,
    pub privacy_source: Option<String>,
    pub privacy_observed_at_ms: Option<u64>,
    pub selectable: bool,
    pub reasons: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ReviewedResults {
    schema_version: u32,
    evidence_kind: String,
    evaluated_at_ms: u64,
    fixture_set_sha256: String,
    runtime: ReviewedRuntime,
    cases: Vec<ReviewedCase>,
    models: Vec<ReviewedModel>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ReviewedRuntime {
    id: String,
    version: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ReviewedCase {
    model_id: String,
    task_id: String,
    status: String,
    reason_code: Option<String>,
    runtime: ReviewedReceipt,
    source: ReviewedSource,
    host_check: ReviewedHostCheck,
    independent_review: ReviewedReview,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ReviewedReceipt {
    ok: bool,
    state: String,
    failure_code: Option<String>,
    observed_model_id: Option<String>,
    request_count: u32,
    tokens_used: u64,
    latency_ms: u64,
    receipt_sha256: String,
    source_fingerprint: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ReviewedSource {
    inventory_matches_review: bool,
    source_journal_sha256: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ReviewedHostCheck {
    status: String,
    source: String,
    check_hashes: BTreeMap<String, String>,
    candidate_inventory_matches_source: bool,
    check_files_match_recorded_hashes: bool,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ReviewedReview {
    status: String,
    reviewed_inventory_matches_source: bool,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ReviewedModel {
    model_id: String,
    status: String,
    passed_cases: u32,
    total_cases: u32,
    reason_codes: Vec<String>,
}

fn trusted_reviewed_results() -> Option<ReviewedResults> {
    if sha256_hex(REVIEWED_RESULTS_JSON) != REVIEWED_RESULTS_SHA256 {
        return None;
    }
    let registry: ReviewedResults = serde_json::from_slice(REVIEWED_RESULTS_JSON).ok()?;
    if registry.schema_version != 1
        || registry.evidence_kind != "syntheticTrialMatrix"
        || registry.runtime.id != "aider"
        || registry.runtime.version.is_empty()
        || registry.fixture_set_sha256 != sha256_hex(TRIAL_REGISTRY_JSON.as_bytes())
        || registry.evaluated_at_ms == 0
        || registry.models.len() != 3
        || registry.cases.len() != 9
    {
        return None;
    }
    Some(registry)
}

fn valid_reviewed_endpoint_policies() -> bool {
    sha256_hex(REVIEWED_ENDPOINT_POLICIES_JSON) == REVIEWED_ENDPOINT_POLICIES_SHA256
        && serde_json::from_slice::<ReviewedEndpointPolicies>(REVIEWED_ENDPOINT_POLICIES_JSON)
            .is_ok_and(|registry| registry.schema_version == 1)
}

/// The endpoint privacy evidence is compiled into the runner and hash-bound.
/// Its content-retention wording is intentionally scoped: temporary content
/// processing to generate output is recorded, and operational metadata or
/// legal-process exceptions are not represented as impossible.
pub(crate) fn reviewed_paid_endpoint_policy_matches(
    model_id: &str,
    canonical_model_id: &str,
    endpoint_id: &str,
    now_ms: u64,
) -> bool {
    if paid_endpoint_policy_digest(REVIEWED_PAID_ENDPOINT_POLICIES_JSON)
        != REVIEWED_PAID_ENDPOINT_POLICIES_SHA256
    {
        return false;
    }
    let Ok(registry) = serde_json::from_slice::<ReviewedPaidEndpointPolicies>(
        REVIEWED_PAID_ENDPOINT_POLICIES_JSON,
    ) else {
        return false;
    };
    if registry.schema_version != 1 || registry.policies.len() > 64 {
        return false;
    }
    registry.policies.iter().any(|policy| {
        policy.model_id == model_id
            && policy.canonical_model_id == canonical_model_id
            && policy.endpoint_tag == endpoint_id
            && policy.provider_slug == endpoint_id.split('/').next().unwrap_or_default()
            && policy.processing_mode == "serverless"
            && policy.no_training
            && !policy.persistent_input_storage
            && !policy.input_content_logging
            && policy.transient_content_processing
            && policy.operational_metadata_may_be_logged
            && policy.legal_process_exceptions_may_apply
            && !policy.source_urls.is_empty()
            && policy.source_urls.len() <= 8
            && policy.source_urls.iter().all(|url| valid_source_url(url))
            && policy.observed_at_ms <= now_ms
            && now_ms.saturating_sub(policy.observed_at_ms) <= PAID_ENDPOINT_POLICY_MAX_AGE_MS
    })
}

fn reviewed_endpoint_policy(model_id: &str, endpoint_id: &str) -> Option<EndpointPrivacyEvidence> {
    if !valid_reviewed_endpoint_policies() {
        return None;
    }
    let registry: ReviewedEndpointPolicies =
        serde_json::from_slice(REVIEWED_ENDPOINT_POLICIES_JSON).ok()?;
    let policy = registry.policies.iter().find(|policy| {
        policy.model_id == model_id
            && policy.endpoint_tag == endpoint_id
            && policy.provider_slug == endpoint_id.split('/').next().unwrap_or_default()
    })?;
    if !valid_endpoint_tag(&policy.endpoint_tag) || !valid_source_url(&policy.source_url) {
        return None;
    }
    Some(EndpointPrivacyEvidence {
        // This evidence binds the OpenRouter service slug. The configured
        // profile ID is user-local and is bound separately at admission.
        provider_id: "openrouter".into(),
        provider_slug: policy.provider_slug.clone(),
        model_id: policy.model_id.clone(),
        endpoint_id: policy.endpoint_tag.clone(),
        observed_at_ms: policy.observed_at_ms,
        source_url: policy.source_url.clone(),
        no_training: policy.no_training == Some(true),
        no_retention: policy.no_retention == Some(true),
        no_content_logging: policy.no_content_logging == Some(true),
    })
}

fn reviewed_policy_any_endpoint(model_id: &str) -> Option<ReviewedEndpointPolicy> {
    if !valid_reviewed_endpoint_policies() {
        return None;
    }
    let registry: ReviewedEndpointPolicies =
        serde_json::from_slice(REVIEWED_ENDPOINT_POLICIES_JSON).ok()?;
    registry
        .policies
        .into_iter()
        .find(|policy| policy.model_id == model_id)
}

fn trusted_passing_evaluation(
    model_id: &str,
    runtime_id: &str,
    runtime_version: &str,
    now_ms: u64,
) -> Option<CodingEvaluationEvidence> {
    #[cfg(test)]
    if let Some(evidence) = TEST_EVALUATION_EVIDENCE.with(|slot| slot.borrow().clone()) {
        if evidence.model_id == model_id
            && evidence.runtime_id == runtime_id
            && evidence.runtime_version == runtime_version
            && evidence.independent_review
            && evidence.cases == 3
            && evidence.passed == 3
            && evidence.evaluated_at_ms <= now_ms
            && now_ms.saturating_sub(evidence.evaluated_at_ms) <= MAX_EVIDENCE_AGE_MS
            && is_sha256(&evidence.fixture_set_sha256)
            && is_sha256(&evidence.result_sha256)
        {
            return Some(evidence);
        }
    }
    let registry = trusted_reviewed_results()?;
    if registry.runtime.id != runtime_id
        || registry.runtime.version != runtime_version
        || registry.evaluated_at_ms > now_ms
        || now_ms.saturating_sub(registry.evaluated_at_ms) > MAX_EVIDENCE_AGE_MS
        || registry.fixture_set_sha256 != sha256_hex(TRIAL_REGISTRY_JSON.as_bytes())
    {
        return None;
    }
    let model = registry
        .models
        .iter()
        .find(|row| row.model_id == model_id)?;
    let cases = registry
        .cases
        .iter()
        .filter(|row| row.model_id == model_id)
        .collect::<Vec<_>>();
    if model.status != "passed"
        || model.total_cases != 3
        || model.passed_cases != 3
        || cases.len() != 3
        || model.reason_codes.len() != 0
        || cases
            .iter()
            .map(|row| row.task_id.as_str())
            .collect::<BTreeSet<_>>()
            .len()
            != 3
        || cases.iter().any(|case| {
            case.status != "passed"
                || !case.runtime.ok
                || case.runtime.state != "completed"
                || case.runtime.failure_code.is_some()
                || case.runtime.observed_model_id.as_deref() != Some(model_id)
                || case.runtime.request_count == 0
                || case.runtime.request_count > 3
                || case.runtime.tokens_used == 0
                || case.runtime.tokens_used > 122_880
                || case.runtime.latency_ms == 0
                || case.runtime.latency_ms > 120_000
                || !is_sha256(&case.runtime.receipt_sha256)
                || !is_sha256(&case.runtime.source_fingerprint)
                || !is_sha256(&case.source.source_journal_sha256)
                || !case.source.inventory_matches_review
                || case.host_check.status != "passed"
                || !case.host_check.candidate_inventory_matches_source
                || !case.host_check.check_files_match_recorded_hashes
                || !matches!(
                    case.host_check.source.as_str(),
                    "independent-host-check.json"
                        | "journal-verification"
                        | "verification-reconciliation"
                )
                || case.host_check.check_hashes.is_empty()
                || case
                    .host_check
                    .check_hashes
                    .values()
                    .any(|hash| !is_sha256(hash))
                || case.independent_review.status != "passed"
                || !case.independent_review.reviewed_inventory_matches_source
        })
    {
        return None;
    }
    Some(CodingEvaluationEvidence {
        model_id: model_id.into(),
        runtime_id: runtime_id.into(),
        runtime_version: runtime_version.into(),
        evaluated_at_ms: registry.evaluated_at_ms,
        fixture_set_id: "FAB-S001-synthetic-matrix-v1".into(),
        fixture_set_sha256: registry.fixture_set_sha256,
        result_sha256: sha256_hex(REVIEWED_RESULTS_JSON),
        cases: model.total_cases,
        passed: model.passed_cases,
        independent_review: true,
    })
}

/// Exact models with hash-verified passing coding evidence for a currently
/// probed runtime. Callers may use this bounded list to decide which model
/// endpoint metadata needs a network refresh for status display.
pub fn trusted_economy_candidate_ids(
    runtime_id: &str,
    runtime_version: &str,
    now_ms: u64,
) -> Vec<String> {
    let Some(registry) = trusted_reviewed_results() else {
        return Vec::new();
    };
    registry
        .models
        .iter()
        .filter(|entry| {
            trusted_passing_evaluation(&entry.model_id, runtime_id, runtime_version, now_ms)
                .is_some()
        })
        .map(|entry| entry.model_id.clone())
        .take(16)
        .collect()
}

/// Turn fresh authenticated OpenRouter metadata into an opaque dispatch proof.
/// Caller-supplied evidence and serialized route flags cannot instantiate this.
#[allow(clippy::too_many_arguments)]
pub(crate) fn admit_authenticated_candidate(
    provider_profile_id: &str,
    model_id: &str,
    runtime_id: &str,
    runtime_version: &str,
    metadata: &crate::providers::OpenRouterMetadataObservation,
    endpoints: &[crate::providers::OpenRouterEndpoint],
    zdr_endpoints: &BTreeSet<(String, String)>,
    required_input_tokens: Option<u64>,
    required_output_tokens: Option<u64>,
    require_tools: bool,
    task_max_tokens: u64,
    task_max_seconds: u64,
    now_ms: u64,
) -> Result<VerifiedEconomyAdmission, String> {
    if metadata.authenticated_for_execution != Some(true) {
        return Err("OpenRouter execution authentication is not verified.".into());
    }
    let evaluation = trusted_passing_evaluation(model_id, runtime_id, runtime_version, now_ms)
        .ok_or_else(|| {
            "This exact model has no fresh trusted passing coding evaluation.".to_string()
        })?;
    #[cfg(test)]
    let test_policies = TEST_ENDPOINT_POLICIES.with(|slot| slot.borrow().clone());
    #[cfg(test)]
    let policies = if !test_policies.is_empty() {
        test_policies
    } else if valid_reviewed_endpoint_policies() {
        serde_json::from_slice::<ReviewedEndpointPolicies>(REVIEWED_ENDPOINT_POLICIES_JSON)
            .map(|registry| registry.policies)
            .unwrap_or_default()
    } else {
        Vec::new()
    };
    #[cfg(not(test))]
    let policies = if valid_reviewed_endpoint_policies() {
        serde_json::from_slice::<ReviewedEndpointPolicies>(REVIEWED_ENDPOINT_POLICIES_JSON)
            .map(|registry| registry.policies)
            .unwrap_or_default()
    } else {
        Vec::new()
    };
    admit_authenticated_candidate_with_evidence(
        provider_profile_id,
        model_id,
        runtime_id,
        runtime_version,
        metadata,
        endpoints,
        zdr_endpoints,
        required_input_tokens,
        required_output_tokens,
        require_tools,
        task_max_tokens,
        task_max_seconds,
        now_ms,
        evaluation,
        &policies,
    )
}

/// Supplies evidence only to the current unit-test thread. The exact production
/// admission validator still checks identity, pricing, capabilities, endpoint,
/// ZDR membership, privacy facts and all budget bounds.
#[cfg(test)]
pub(crate) fn with_test_admission_evidence<T>(
    evaluation: CodingEvaluationEvidence,
    model_id: &str,
    endpoint_id: &str,
    observed_at_ms: u64,
    run: impl FnOnce() -> T,
) -> T {
    with_test_admission_privacy(
        evaluation,
        model_id,
        endpoint_id,
        observed_at_ms,
        true,
        true,
        true,
        run,
    )
}

#[cfg(test)]
pub(crate) fn with_test_admission_privacy<T>(
    evaluation: CodingEvaluationEvidence,
    model_id: &str,
    endpoint_id: &str,
    observed_at_ms: u64,
    no_training: bool,
    no_retention: bool,
    no_content_logging: bool,
    run: impl FnOnce() -> T,
) -> T {
    let policy = ReviewedEndpointPolicy {
        model_id: model_id.into(),
        provider_slug: endpoint_id.split('/').next().unwrap_or_default().into(),
        endpoint_tag: endpoint_id.into(),
        source_url: "https://openrouter.ai/docs/guides/privacy/provider-logging".into(),
        observed_at_ms,
        no_training: Some(no_training),
        no_retention: Some(no_retention),
        no_content_logging: Some(no_content_logging),
    };
    TEST_EVALUATION_EVIDENCE.with(|slot| *slot.borrow_mut() = Some(evaluation));
    TEST_ENDPOINT_POLICIES.with(|slot| *slot.borrow_mut() = vec![policy]);
    let result = run();
    TEST_EVALUATION_EVIDENCE.with(|slot| *slot.borrow_mut() = None);
    TEST_ENDPOINT_POLICIES.with(|slot| slot.borrow_mut().clear());
    result
}

#[allow(clippy::too_many_arguments)]
fn admit_authenticated_candidate_with_evidence(
    provider_profile_id: &str,
    model_id: &str,
    runtime_id: &str,
    runtime_version: &str,
    metadata: &crate::providers::OpenRouterMetadataObservation,
    endpoints: &[crate::providers::OpenRouterEndpoint],
    zdr_endpoints: &BTreeSet<(String, String)>,
    required_input_tokens: Option<u64>,
    required_output_tokens: Option<u64>,
    require_tools: bool,
    task_max_tokens: u64,
    task_max_seconds: u64,
    now_ms: u64,
    evaluation: CodingEvaluationEvidence,
    reviewed_policies: &[ReviewedEndpointPolicy],
) -> Result<VerifiedEconomyAdmission, String> {
    if metadata.authenticated_for_execution != Some(true) {
        return Err("OpenRouter execution authentication is not verified.".into());
    }
    let model = metadata
        .models
        .iter()
        .find(|row| row.id == model_id)
        .ok_or_else(|| {
            "This exact model is absent from authenticated user availability.".to_string()
        })?;
    let canonical_slug = model
        .canonical_slug
        .as_deref()
        .filter(|slug| !slug.trim().is_empty() && valid_authenticated_model_slug(slug))
        .ok_or_else(|| "Authenticated canonical model identity is unavailable.".to_string())?;
    let input_price = model
        .prompt_price
        .as_deref()
        .and_then(crate::providers::openrouter_price_micros_per_million);
    let output_price = model
        .completion_price
        .as_deref()
        .and_then(crate::providers::openrouter_price_micros_per_million);
    if input_price != Some(0) || output_price != Some(0) {
        return Err("Current authenticated model pricing is not exactly zero.".into());
    }
    if !model
        .input_modalities
        .as_ref()
        .is_some_and(|values| values.iter().any(|value| value == "text"))
        || !model
            .output_modalities
            .as_ref()
            .is_some_and(|values| values.iter().any(|value| value == "text"))
    {
        return Err("Authenticated model metadata does not confirm text input and output.".into());
    }
    if require_tools
        && !model.supported_parameters.as_ref().is_some_and(|values| {
            values
                .iter()
                .any(|value| matches!(value.as_str(), "tools" | "tool_choice"))
        })
    {
        return Err("Authenticated model metadata does not confirm required tool support.".into());
    }
    let model_context = model
        .context_length
        .ok_or_else(|| "Model input context capacity is unknown.".to_string())?;
    let endpoints_matching_model = endpoints
        .iter()
        .filter(|endpoint| endpoint.model_id == model_id)
        .collect::<Vec<_>>();
    let eligible = endpoints_matching_model
        .into_iter()
        .filter_map(|endpoint| {
            let policy = reviewed_policies.iter().find(|policy| {
                policy.model_id == model_id
                    && policy.endpoint_tag == endpoint.tag
                    && policy.provider_slug == endpoint.tag.split('/').next().unwrap_or_default()
                    && valid_endpoint_tag(&policy.endpoint_tag)
                    && valid_source_url(&policy.source_url)
                    && policy.observed_at_ms <= now_ms
                    && now_ms.saturating_sub(policy.observed_at_ms) <= MAX_EVIDENCE_AGE_MS
            })?;
            let privacy = EndpointPrivacyEvidence {
                provider_id: "openrouter".into(),
                provider_slug: policy.provider_slug.clone(),
                model_id: policy.model_id.clone(),
                endpoint_id: policy.endpoint_tag.clone(),
                observed_at_ms: policy.observed_at_ms,
                source_url: policy.source_url.clone(),
                no_training: policy.no_training == Some(true),
                no_retention: policy.no_retention == Some(true),
                no_content_logging: policy.no_content_logging == Some(true),
            };
            if privacy.no_training != true
                || privacy.no_retention != true
                || privacy.no_content_logging != true
                || endpoint.status != Some(0)
                || endpoint.zdr == Some(false)
                || !zdr_endpoints.contains(&(model_id.to_owned(), endpoint.tag.clone()))
                || endpoint
                    .prompt_price
                    .as_deref()
                    .and_then(crate::providers::openrouter_price_micros_per_million)
                    != Some(0)
                || endpoint
                    .completion_price
                    .as_deref()
                    .and_then(crate::providers::openrouter_price_micros_per_million)
                    != Some(0)
                || endpoint
                    .request_price
                    .as_deref()
                    .and_then(crate::providers::openrouter_price_micros_per_million)
                    != Some(0)
            {
                return None;
            }
            let endpoint_context = endpoint.context_length?;
            let max_prompt = endpoint.max_prompt_tokens?;
            let max_completion = endpoint.max_completion_tokens?;
            let input = required_input_tokens.unwrap_or(32_768);
            if input > model_context || input > endpoint_context || input > max_prompt {
                return None;
            }
            if !request_capacity_fits(
                input,
                1,
                model_context,
                endpoint_context,
                max_prompt,
                max_completion,
            ) {
                return None;
            }
            if !endpoint
                .supported_parameters
                .as_ref()
                .is_some_and(|parameters| parameters.iter().any(|v| v == "max_tokens"))
            {
                return None;
            }
            if require_tools
                && !endpoint
                    .supported_parameters
                    .as_ref()
                    .is_some_and(|parameters| {
                        parameters
                            .iter()
                            .any(|v| matches!(v.as_str(), "tools" | "tool_choice"))
                    })
            {
                return None;
            }
            if required_output_tokens.is_some_and(|required| required > max_completion.min(8_192)) {
                return None;
            }
            let output = required_output_tokens
                .unwrap_or(8_192)
                .min(max_completion)
                .min(8_192);
            if !request_capacity_fits(
                input,
                output,
                model_context,
                endpoint_context,
                max_prompt,
                max_completion,
            ) {
                return None;
            }
            let calls = 3u32;
            let token_envelope = input.checked_add(output)?.checked_mul(u64::from(calls))?;
            let seconds = task_max_seconds.min(120);
            if token_envelope > task_max_tokens || seconds == 0 {
                return None;
            }
            Some((
                endpoint,
                privacy,
                input,
                output,
                calls,
                token_envelope,
                seconds,
            ))
        })
        .collect::<Vec<_>>();
    if eligible.len() != 1 {
        return Err("No single endpoint has fresh zero-price, ZDR, capability, and reviewed privacy evidence.".into());
    }
    let (endpoint, privacy, context_tokens, output, calls, tokens, seconds) = &eligible[0];
    if provider_profile_id.trim().is_empty()
        || provider_profile_id.len() > 96
        || runtime_id != "aider"
        || runtime_version != evaluation.runtime_version
        || !valid_model_id(model_id)
    {
        return Err(
            "The selected provider, model, or runtime is not bound to its evaluation.".into(),
        );
    }
    let semantic_evidence = serde_json::to_vec(&(
        "economy-admission-v2",
        (
            provider_profile_id,
            model_id,
            canonical_slug,
            endpoint.tag.as_str(),
        ),
        ("aider", runtime_version, evaluation.result_sha256.as_str()),
        (
            model_context,
            endpoint.context_length,
            endpoint.max_prompt_tokens,
            endpoint.max_completion_tokens,
        ),
        (
            model.supported_parameters.as_ref().is_some_and(|values| {
                values
                    .iter()
                    .any(|value| matches!(value.as_str(), "tools" | "tool_choice"))
            }),
            endpoint
                .supported_parameters
                .as_ref()
                .is_some_and(|values| {
                    values
                        .iter()
                        .any(|value| matches!(value.as_str(), "tools" | "tool_choice"))
                }),
            endpoint
                .supported_parameters
                .as_ref()
                .is_some_and(|values| values.iter().any(|value| value == "max_tokens")),
        ),
        (
            privacy.source_url.as_str(),
            privacy.observed_at_ms,
            privacy.no_training,
            privacy.no_retention,
            privacy.no_content_logging,
        ),
    ))
    .map_err(|_| "Economy admission evidence could not be bound.".to_string())?;
    Ok(VerifiedEconomyAdmission {
        provider_profile_id: provider_profile_id.into(),
        provider_slug: endpoint.tag.split('/').next().unwrap_or_default().into(),
        model_id: model_id.into(),
        canonical_slug: canonical_slug.into(),
        endpoint_id: endpoint.tag.clone(),
        runtime_id: runtime_id.into(),
        runtime_version: runtime_version.into(),
        context_tokens: *context_tokens,
        max_output_tokens_per_call: *output,
        max_calls: *calls,
        max_tokens: *tokens,
        max_seconds: *seconds,
        requires_tools: require_tools,
        catalog_observed_at_ms: now_ms,
        endpoint_observed_at_ms: now_ms,
        evaluation,
        privacy: privacy.clone(),
        evidence_digest: sha256_hex(&semantic_evidence),
    })
}

fn request_capacity_fits(
    input_tokens: u64,
    output_tokens: u64,
    model_context: u64,
    endpoint_context: u64,
    max_prompt_tokens: u64,
    max_completion_tokens: u64,
) -> bool {
    input_tokens > 0
        && output_tokens > 0
        && input_tokens <= max_prompt_tokens
        && output_tokens <= max_completion_tokens
        && input_tokens
            .checked_add(output_tokens)
            .is_some_and(|total| total <= model_context && total <= endpoint_context)
}

/// Refresh all backend observations needed to mint an ordinary Economy proof.
/// The helper returns `None` without network access unless the immutable trial
/// registry contains a passing record for this exact Aider version.
#[allow(clippy::too_many_arguments)]
pub(crate) fn refresh_profile_admission(
    profile: &crate::providers::ProviderProfile,
    model_id: &str,
    required_input_tokens: Option<u64>,
    required_output_tokens: Option<u64>,
    require_tools: bool,
    task_max_tokens: u64,
    task_max_seconds: u64,
    timeout: std::time::Duration,
    now_ms: u64,
) -> Result<VerifiedEconomyAdmission, String> {
    if profile.kind != crate::providers::ProviderKind::OpenRouter {
        return Err("Economy admission requires an OpenRouter provider profile.".into());
    }
    if timeout.is_zero() || timeout > std::time::Duration::from_secs(30) {
        return Err("Economy admission refresh deadline is invalid.".into());
    }
    let executable =
        crate::providers::find_executable(&profile.executable, std::env::var_os("PATH").as_deref())
            .ok_or_else(|| "The configured Aider runtime is unavailable.".to_string())?;
    let runtime_version = crate::providers::probe_aider_runtime_version(&executable)
        .ok_or_else(|| "The configured Aider runtime version could not be verified.".to_string())?;
    if trusted_passing_evaluation(model_id, "aider", &runtime_version, now_ms).is_none() {
        return Err("This exact model has no fresh trusted passing coding evaluation for the configured runtime.".into());
    }
    let secret_ref = profile
        .secret_ref
        .as_deref()
        .filter(|reference| {
            crate::providers::openrouter_credential_reference_matches(&profile.id, reference)
        })
        .ok_or_else(|| "A valid OpenRouter credential reference is required.".to_string())?;
    let key = crate::credentials::resolve(secret_ref)
        .filter(|key| crate::providers::valid_openrouter_key(key))
        .ok_or_else(|| "The saved OpenRouter credential is unavailable or invalid.".to_string())?;
    let deadline = std::time::Instant::now() + timeout;
    let metadata = crate::providers::openrouter_metadata_http_probe_until(&key, deadline)?;
    let remaining = deadline.saturating_duration_since(std::time::Instant::now());
    if remaining.is_zero() {
        return Err("Economy metadata refresh exceeded its deadline.".into());
    }
    let endpoints =
        crate::providers::openrouter_endpoints_http_probe_with_timeout(&key, model_id, remaining)?;
    let remaining = deadline.saturating_duration_since(std::time::Instant::now());
    if remaining.is_zero() {
        return Err("Economy endpoint refresh exceeded its deadline.".into());
    }
    let zdr = crate::providers::openrouter_zdr_endpoints_http_probe_with_timeout(&key, remaining)?;
    admit_authenticated_candidate(
        &profile.id,
        model_id,
        "aider",
        &runtime_version,
        &metadata,
        &endpoints,
        &zdr,
        required_input_tokens,
        required_output_tokens,
        require_tools,
        task_max_tokens,
        task_max_seconds,
        now_ms,
    )
}

fn valid_authenticated_model_slug(value: &str) -> bool {
    value.len() <= 200
        && value.contains('/')
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"/:-_.".contains(&byte))
        && !is_router_model_id(value)
}

/// Build Economy UI rows from an authenticated catalog and the immutable
/// compile-time evaluation bundle. Endpoint privacy is always revalidated at
/// dispatch; absent a trusted serving-endpoint proof, rows remain unselectable.
pub fn statuses_from_catalog(
    provider_id: &str,
    models: &[crate::providers::OpenRouterModel],
    endpoints_by_model: &BTreeMap<String, Vec<crate::providers::OpenRouterEndpoint>>,
    zdr_endpoints: &BTreeSet<(String, String)>,
    catalog_observed_at_ms: u64,
    runtime_id: &str,
    runtime_version: &str,
    now_ms: u64,
) -> Vec<EconomyModelStatus> {
    let registry = trusted_reviewed_results();
    let Some(registry) = registry else {
        return Vec::new();
    };
    let model_map = models
        .iter()
        .filter(|model| valid_model_id(&model.id))
        .map(|model| (model.id.as_str(), model))
        .collect::<BTreeMap<_, _>>();
    let mut ids = registry
        .models
        .iter()
        .map(|row| row.model_id.clone())
        .collect::<BTreeSet<_>>();
    ids.extend(
        models
            .iter()
            .filter(|model| {
                valid_model_id(&model.id)
                    && (model.id.ends_with(":free")
                        || model
                            .prompt_price
                            .as_deref()
                            .and_then(crate::providers::openrouter_price_micros_per_million)
                            == Some(0)
                        || model
                            .completion_price
                            .as_deref()
                            .and_then(crate::providers::openrouter_price_micros_per_million)
                            == Some(0))
            })
            .map(|model| model.id.clone()),
    );
    ids.into_iter().map(|model_id| {
        let model = model_map.get(model_id.as_str()).copied();
        let catalog_fresh = catalog_observed_at_ms <= now_ms && now_ms.saturating_sub(catalog_observed_at_ms) <= 24 * 60 * 60 * 1000;
        let input = model.and_then(|model| model.prompt_price.as_deref()).and_then(crate::providers::openrouter_price_micros_per_million);
        let output = model.and_then(|model| model.completion_price.as_deref()).and_then(crate::providers::openrouter_price_micros_per_million);
        let current_zero_priced = input == Some(0) && output == Some(0);
        let entry = registry.models.iter().find(|entry| entry.model_id == model_id);
        let codes = entry.map_or_else(Vec::new, |entry| entry.reason_codes.clone());
        let eval = trusted_passing_evaluation(&model_id, runtime_id, runtime_version, now_ms);
        let evaluation_status = if entry.is_none() { EvidenceStatus::NotEvaluated }
            else if runtime_id != registry.runtime.id || runtime_version != registry.runtime.version
                || registry.evaluated_at_ms > now_ms || now_ms.saturating_sub(registry.evaluated_at_ms) > MAX_EVIDENCE_AGE_MS { EvidenceStatus::Stale }
            else if eval.is_some() { EvidenceStatus::Passed } else { EvidenceStatus::Failed };
        let policy = reviewed_policy_any_endpoint(&model_id);
        let privacy_status = match &policy {
            None => PrivacyStatus::Unknown,
            Some(policy) if policy.no_training == Some(false) || policy.no_retention == Some(false) || policy.no_content_logging == Some(false) => PrivacyStatus::Incompatible,
            Some(policy) if policy.no_training == Some(true) && policy.no_retention == Some(true) && policy.no_content_logging == Some(true)
                && policy.observed_at_ms <= now_ms && now_ms.saturating_sub(policy.observed_at_ms) <= MAX_EVIDENCE_AGE_MS => PrivacyStatus::Eligible,
            _ => PrivacyStatus::Unknown,
        };
        let mut capability_reason = None;
        let selected_admission = if let (Some(model), Some(eval)) = (model, eval) {
            let endpoints = endpoints_by_model.get(&model_id).map(Vec::as_slice).unwrap_or(&[]);
            let observation = crate::providers::OpenRouterMetadataObservation { authenticated_for_execution: Some(true), models: vec![model.clone()], free_model_daily_requests: None };
            match admit_authenticated_candidate(provider_id, &model_id, runtime_id, runtime_version, &observation, endpoints, zdr_endpoints,
                Some(8_192), Some(8_192), true, 49_152, 120, now_ms) {
                Ok(admission) => Some(admission),
                Err(reason) => { capability_reason = Some(reason); let _ = eval; None }
            }
        } else { None };
        let mut reasons = Vec::new();
        if model.is_none() { reasons.push("Model is no longer present in the authenticated catalog.".into()); }
        if !catalog_fresh { reasons.push("Authenticated model catalog is stale.".into()); }
        if model.is_some() && !current_zero_priced { reasons.push("Current authenticated prompt and completion prices are not both zero.".into()); }
        match evaluation_status {
            EvidenceStatus::Failed if codes.iter().any(|code| code == "independent_review_failed_or_inventory_mismatch") => reasons.push("Synthetic coding evaluation failed: the multi-file task did not pass independent host review or its source inventory did not match.".into()),
            EvidenceStatus::Failed if codes.iter().any(|code| code == "RUNTIME_FAILED") => reasons.push("Synthetic coding evaluation failed: all three tasks ended with runtime failures.".into()),
            EvidenceStatus::Failed => reasons.push("Synthetic coding evaluation did not pass all required tasks.".into()),
            EvidenceStatus::Stale => reasons.push("Coding evaluation is stale or does not match the current Aider runtime.".into()),
            EvidenceStatus::NotEvaluated => reasons.push("No trusted coding evaluation is recorded for this model.".into()),
            EvidenceStatus::Passed => {},
        }
        match privacy_status {
            PrivacyStatus::Unknown => reasons.push("Serving endpoint training, retention, or logging policy is unknown.".into()),
            PrivacyStatus::Incompatible => reasons.push("Reviewed provider policy does not guarantee no training, retention, and content logging for this endpoint.".into()),
            PrivacyStatus::Eligible => {},
        }
        if let Some(reason) = capability_reason.clone() { reasons.push(reason); }
        let selectable = selected_admission.is_some() && catalog_fresh && current_zero_priced;
        EconomyModelStatus {
            provider_id: provider_id.into(), model_id: model_id.clone(),
            context_tokens: model.and_then(|model| model.context_length),
            input_price_micros_per_1m: input, output_price_micros_per_1m: output,
            catalog_observed_at_ms, evaluation_status,
            evaluation_runtime_version: entry.map(|_| registry.runtime.version.clone()),
            current_runtime_version: Some(runtime_version.to_owned()),
            evaluation_at_ms: entry.map(|_| registry.evaluated_at_ms),
            evaluation_passed: entry.map_or(0, |entry| entry.passed_cases),
            evaluation_cases: entry.map_or(0, |entry| entry.total_cases),
            evaluation_reason_codes: codes,
            catalog_present: model.is_some(), catalog_fresh,
            current_zero_priced, current_context_tokens: model.and_then(|model| model.context_length),
            capability_reason,
            privacy_status,
            privacy_source: policy.as_ref().map(|policy| policy.source_url.clone()),
            privacy_observed_at_ms: policy.map(|policy| policy.observed_at_ms),
            selectable, reasons,
        }
    }).collect()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodingEvaluationEvidence {
    pub model_id: String,
    pub runtime_id: String,
    pub runtime_version: String,
    pub evaluated_at_ms: u64,
    pub fixture_set_id: String,
    pub fixture_set_sha256: String,
    pub result_sha256: String,
    pub cases: u32,
    pub passed: u32,
    pub independent_review: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EndpointPrivacyEvidence {
    /// Service kind (OpenRouter), independent from its configured profile ID.
    pub provider_id: String,
    /// Exact OpenRouter provider slug bound by the reviewed policy record.
    pub provider_slug: String,
    pub model_id: String,
    pub endpoint_id: String,
    pub observed_at_ms: u64,
    pub source_url: String,
    pub no_training: bool,
    pub no_retention: bool,
    pub no_content_logging: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EconomyModelPolicy {
    pub provider_id: String,
    pub model_id: String,
    pub runtime_id: String,
    pub context_tokens: u64,
    pub max_output_tokens_per_call: u64,
    pub max_calls: u32,
    pub max_tokens: u64,
    pub max_seconds: u64,
    pub catalog_source: String,
    pub catalog_observed_at_ms: u64,
    pub context_snapshot: u64,
    pub evaluation: CodingEvaluationEvidence,
    pub privacy: EndpointPrivacyEvidence,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ReviewedEndpointPolicies {
    schema_version: u32,
    policies: Vec<ReviewedEndpointPolicy>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ReviewedEndpointPolicy {
    model_id: String,
    provider_slug: String,
    endpoint_tag: String,
    source_url: String,
    observed_at_ms: u64,
    no_training: Option<bool>,
    no_retention: Option<bool>,
    no_content_logging: Option<bool>,
}

/// A proof minted only by the backend from a passing, hash-bound evaluation,
/// current authenticated model/endpoint metadata and reviewed endpoint policy.
/// This type intentionally has private fields and no serde implementation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifiedEconomyAdmission {
    provider_profile_id: String,
    provider_slug: String,
    model_id: String,
    canonical_slug: String,
    endpoint_id: String,
    runtime_id: String,
    runtime_version: String,
    context_tokens: u64,
    max_output_tokens_per_call: u64,
    max_calls: u32,
    max_tokens: u64,
    max_seconds: u64,
    requires_tools: bool,
    catalog_observed_at_ms: u64,
    endpoint_observed_at_ms: u64,
    evaluation: CodingEvaluationEvidence,
    privacy: EndpointPrivacyEvidence,
    evidence_digest: String,
}

impl VerifiedEconomyAdmission {
    pub(crate) fn gateway_policy(&self) -> crate::paid_gateway::PaidModelPolicy {
        crate::paid_gateway::PaidModelPolicy {
            mode: crate::paid_gateway::GatewayMode::Economy {
                endpoint_id: self.endpoint_id.clone(),
            },
            provider_id: self.provider_profile_id.clone(),
            model_id: self.model_id.clone(),
            runtime_id: self.runtime_id.clone(),
            model_context_window_tokens: self.context_tokens,
            max_output_tokens_per_call: self.max_output_tokens_per_call,
            max_calls: self.max_calls,
            max_cost_micros: 0,
            pricing: crate::paid_gateway::PaidPricingCeiling {
                input_price_micros_per_1m: 0,
                output_price_micros_per_1m: 0,
                request_fee_micros: 0,
                source: "authenticated-openrouter-model-and-zdr-endpoint-metadata".into(),
                observed_at_ms: self.catalog_observed_at_ms,
            },
            endpoint_pin: None,
        }
    }

    pub(crate) fn endpoint_id(&self) -> &str {
        &self.endpoint_id
    }
    pub(crate) fn model_id(&self) -> &str {
        &self.model_id
    }
    pub(crate) fn provider_profile_id(&self) -> &str {
        &self.provider_profile_id
    }
    pub(crate) fn canonical_slug(&self) -> &str {
        &self.canonical_slug
    }
    pub(crate) fn runtime_version(&self) -> &str {
        &self.runtime_version
    }
    pub(crate) fn provider_slug(&self) -> &str {
        &self.provider_slug
    }
    pub(crate) fn context_tokens(&self) -> u64 {
        self.context_tokens
    }
    pub(crate) fn max_output_tokens_per_call(&self) -> u64 {
        self.max_output_tokens_per_call
    }
    pub(crate) fn evidence_digest(&self) -> &str {
        &self.evidence_digest
    }
    pub(crate) fn max_tokens(&self) -> u64 {
        self.max_tokens
    }
    pub(crate) fn max_seconds(&self) -> u64 {
        self.max_seconds
    }
    pub(crate) fn requires_tools(&self) -> bool {
        self.requires_tools
    }

    pub(crate) fn validate_pinned_policy(
        &self,
        provider_profile_id: &str,
        policy: &crate::paid_gateway::PaidModelPolicy,
        now_ms: u64,
    ) -> Result<(), EconomyAdmissionError> {
        if provider_profile_id != self.provider_profile_id
            || policy.provider_id != self.provider_profile_id
            || policy.model_id != self.model_id
            || policy.runtime_id != self.runtime_id
            || policy.model_context_window_tokens != self.context_tokens
            || policy.max_output_tokens_per_call != self.max_output_tokens_per_call
            || policy.max_calls != self.max_calls
            || policy.max_cost_micros != 0
            || policy.pricing.input_price_micros_per_1m != 0
            || policy.pricing.output_price_micros_per_1m != 0
            || policy.pricing.request_fee_micros != 0
            || !matches!(&policy.mode, crate::paid_gateway::GatewayMode::Economy { endpoint_id } if endpoint_id == &self.endpoint_id)
        {
            return Err(EconomyAdmissionError::InvalidIdentity);
        }
        if now_ms > self.endpoint_observed_at_ms
            && now_ms - self.endpoint_observed_at_ms > 10 * 60 * 1000
        {
            return Err(EconomyAdmissionError::StaleCatalog);
        }
        if self.catalog_observed_at_ms > now_ms
            || now_ms.saturating_sub(self.catalog_observed_at_ms) > 24 * 60 * 60 * 1000
        {
            return Err(EconomyAdmissionError::StaleCatalog);
        }
        if self.evaluation.evaluated_at_ms > now_ms
            || now_ms.saturating_sub(self.evaluation.evaluated_at_ms) > MAX_EVIDENCE_AGE_MS
        {
            return Err(EconomyAdmissionError::StaleEvaluation);
        }
        self.privacy_current(now_ms)
    }

    pub(crate) fn privacy_current(&self, now_ms: u64) -> Result<(), EconomyAdmissionError> {
        if !valid_reviewed_endpoint_policies() {
            return Err(EconomyAdmissionError::PrivacyEvidenceMissing);
        }
        if self.privacy.provider_id != "openrouter"
            || self.privacy.model_id != self.model_id
            || self.privacy.endpoint_id != self.endpoint_id
            || self.privacy.provider_slug != self.provider_slug
            || self.privacy.provider_slug != self.endpoint_id.split('/').next().unwrap_or_default()
            || !valid_endpoint_tag(&self.endpoint_id)
            || !valid_source_url(&self.privacy.source_url)
        {
            return Err(EconomyAdmissionError::PrivacyEvidenceMissing);
        }
        if self.privacy.observed_at_ms > now_ms
            || now_ms.saturating_sub(self.privacy.observed_at_ms) > MAX_EVIDENCE_AGE_MS
        {
            return Err(EconomyAdmissionError::PrivacyEvidenceStale);
        }
        if !self.privacy.no_training
            || !self.privacy.no_retention
            || !self.privacy.no_content_logging
        {
            return Err(EconomyAdmissionError::PrivacyPolicyIncompatible);
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EconomyAdmissionError {
    InvalidIdentity,
    StaleCatalog,
    StaleEvaluation,
    EvaluationDidNotPass,
    RuntimeChanged,
    PrivacyEvidenceMissing,
    PrivacyEvidenceStale,
    PrivacyPolicyIncompatible,
    PricingChanged,
    ContextChanged,
    LimitInvalid,
}

impl EconomyModelPolicy {
    /// Validate the immutable run pin against fresh observations immediately
    /// before each provider request. `None` prices are unknown, never free.
    pub fn validate_live(
        &self,
        now_ms: u64,
        provider_id: &str,
        model_id: &str,
        runtime_id: &str,
        runtime_version: &str,
        current_context_tokens: Option<u64>,
        current_input_price_micros: Option<u64>,
        current_output_price_micros: Option<u64>,
        current_privacy: Option<&EndpointPrivacyEvidence>,
    ) -> Result<(), EconomyAdmissionError> {
        if !valid_id(&self.provider_id)
            || !valid_model_id(&self.model_id)
            || !valid_id(&self.runtime_id)
        {
            return Err(EconomyAdmissionError::InvalidIdentity);
        }
        if self.provider_id != provider_id || self.model_id != model_id {
            return Err(EconomyAdmissionError::InvalidIdentity);
        }
        if self.runtime_id != runtime_id || self.evaluation.runtime_id != runtime_id {
            return Err(EconomyAdmissionError::RuntimeChanged);
        }
        if self.evaluation.runtime_version != runtime_version {
            return Err(EconomyAdmissionError::RuntimeChanged);
        }
        if self.catalog_observed_at_ms > now_ms
            || now_ms - self.catalog_observed_at_ms > 24 * 60 * 60 * 1000
        {
            return Err(EconomyAdmissionError::StaleCatalog);
        }
        if self.evaluation.evaluated_at_ms > now_ms
            || now_ms - self.evaluation.evaluated_at_ms > MAX_EVIDENCE_AGE_MS
        {
            return Err(EconomyAdmissionError::StaleEvaluation);
        }
        if self.evaluation.model_id != self.model_id
            || self.evaluation.cases == 0
            || self.evaluation.passed != self.evaluation.cases
            || !self.evaluation.independent_review
            || !is_sha256(&self.evaluation.fixture_set_sha256)
            || !is_sha256(&self.evaluation.result_sha256)
            || self.evaluation.fixture_set_id.trim().is_empty()
            || self.evaluation.runtime_version.trim().is_empty()
        {
            return Err(EconomyAdmissionError::EvaluationDidNotPass);
        }
        if self.max_calls == 0
            || self.max_tokens == 0
            || self.max_seconds == 0
            || self.context_tokens == 0
            || self.max_output_tokens_per_call == 0
            || self.max_output_tokens_per_call > self.context_tokens
            || self
                .context_tokens
                .saturating_add(self.max_output_tokens_per_call)
                > self.max_tokens
        {
            return Err(EconomyAdmissionError::LimitInvalid);
        }
        if current_input_price_micros != Some(0) || current_output_price_micros != Some(0) {
            return Err(EconomyAdmissionError::PricingChanged);
        }
        if current_context_tokens.is_none_or(|context| context < self.context_tokens)
            || current_context_tokens != Some(self.context_snapshot)
        {
            return Err(EconomyAdmissionError::ContextChanged);
        }
        let privacy = current_privacy.ok_or(EconomyAdmissionError::PrivacyEvidenceMissing)?;
        if privacy.provider_id != "openrouter"
            || self.privacy.provider_id != "openrouter"
            || privacy.provider_slug != self.privacy.provider_slug
            || privacy.model_id != self.model_id
            || self.privacy.model_id != self.model_id
            || privacy.endpoint_id != self.privacy.endpoint_id
            || !valid_endpoint_tag(&privacy.endpoint_id)
            || !valid_source_url(&privacy.source_url)
            || !valid_source_url(&self.privacy.source_url)
            || privacy.observed_at_ms > now_ms
            || now_ms - privacy.observed_at_ms > 24 * 60 * 60 * 1000
        {
            return Err(EconomyAdmissionError::PrivacyEvidenceStale);
        }
        if !privacy.no_training || !privacy.no_retention || !privacy.no_content_logging {
            return Err(EconomyAdmissionError::PrivacyPolicyIncompatible);
        }
        if !self.privacy.no_training
            || !self.privacy.no_retention
            || !self.privacy.no_content_logging
        {
            return Err(EconomyAdmissionError::PrivacyPolicyIncompatible);
        }
        Ok(())
    }
}

fn valid_id(value: &str) -> bool {
    !value.trim().is_empty() && value.len() <= 200 && !value.chars().any(char::is_control)
}

fn valid_model_id(value: &str) -> bool {
    valid_id(value)
        && !matches!(value, "openrouter/free" | "openrouter/auto")
        && !value.ends_with("/free")
        && !value.ends_with("/auto")
}

fn valid_endpoint_tag(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value.split('/').all(|part| {
            !part.is_empty()
                && part
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
        })
}

fn valid_source_url(value: &str) -> bool {
    value.starts_with("https://") && value.len() <= 2048 && !value.chars().any(char::is_control)
}

fn is_sha256(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paid_endpoint_policy_is_exact_fresh_and_records_processing_scope() {
        let now = 1793145600000;
        assert!(reviewed_paid_endpoint_policy_matches(
            "qwen/qwen3-coder-next",
            "qwen/qwen3-coder-next-2025-02-03",
            "parasail/bf16",
            now,
        ));
        assert!(!reviewed_paid_endpoint_policy_matches(
            "qwen/qwen3-coder-next",
            "qwen/qwen3-coder-next-2025-02-03",
            "parasail/other",
            now,
        ));
        assert!(!reviewed_paid_endpoint_policy_matches(
            "qwen/qwen3-coder-next",
            "qwen/qwen3-coder-next-other",
            "parasail/bf16",
            now,
        ));
        assert!(!reviewed_paid_endpoint_policy_matches(
            "qwen/qwen3-coder-next",
            "qwen/qwen3-coder-next-2025-02-03",
            "parasail/bf16",
            now + PAID_ENDPOINT_POLICY_MAX_AGE_MS + 1,
        ));
        assert!(!reviewed_paid_endpoint_policy_matches(
            "qwen/qwen3-coder-next",
            "qwen/qwen3-coder-next-2025-02-03",
            "parasail/bf16",
            now - 1,
        ));
    }

    #[test]
    fn paid_endpoint_policy_digest_is_stable_across_windows_line_endings() {
        let lf = b"{\n  \"schemaVersion\": 1\n}\n";
        let crlf = b"{\r\n  \"schemaVersion\": 1\r\n}\r\n";
        let changed = b"{\n  \"schemaVersion\": 2\n}\n";

        assert_eq!(
            paid_endpoint_policy_digest(lf),
            paid_endpoint_policy_digest(crlf)
        );
        assert_ne!(
            paid_endpoint_policy_digest(lf),
            paid_endpoint_policy_digest(changed)
        );
        assert_eq!(
            paid_endpoint_policy_digest(REVIEWED_PAID_ENDPOINT_POLICIES_JSON),
            REVIEWED_PAID_ENDPOINT_POLICIES_SHA256
        );
    }

    #[test]
    fn economy_request_envelope_obeys_model_and_endpoint_context_boundaries() {
        assert!(request_capacity_fits(
            7_000, 1_192, 8_192, 8_192, 7_000, 1_192
        ));
        assert!(request_capacity_fits(
            6_999, 1_192, 8_192, 8_192, 7_000, 1_192
        ));
        assert!(!request_capacity_fits(
            7_001, 1_192, 8_192, 8_192, 7_000, 1_192
        ));
        assert!(!request_capacity_fits(
            7_000, 1_193, 8_192, 8_192, 7_000, 1_192
        ));
        assert!(!request_capacity_fits(
            7_000, 1_192, 8_191, 8_192, 7_000, 1_192
        ));
        assert!(!request_capacity_fits(
            7_000, 1_192, 8_192, 8_191, 7_000, 1_192
        ));
    }

    #[test]
    fn embedded_trial_results_are_digest_bound_and_fail_closed() {
        assert_eq!(sha256_hex(REVIEWED_RESULTS_JSON), REVIEWED_RESULTS_SHA256);
        let parsed: ReviewedResults = serde_json::from_slice(REVIEWED_RESULTS_JSON)
            .expect("sanitized evidence schema parses");
        assert_eq!(
            parsed.fixture_set_sha256,
            sha256_hex(TRIAL_REGISTRY_JSON.as_bytes())
        );
        let registry = trusted_reviewed_results().expect("reviewed bundle has the pinned digest");
        assert_eq!(registry.models.len(), 3);
        assert_eq!(registry.cases.len(), 9);
        assert!(registry.models.iter().all(|model| model.status == "failed"));
        assert!(registry
            .models
            .iter()
            .all(|model| model.passed_cases < model.total_cases));
        assert!(registry
            .cases
            .iter()
            .all(|case| case.runtime.observed_model_id.as_deref() == Some(case.model_id.as_str())));
        assert!(registry
            .cases
            .iter()
            .all(|case| case.runtime.request_count <= 3));
        assert!(registry
            .cases
            .iter()
            .all(|case| case.runtime.latency_ms <= 120_000));
        assert_eq!(
            registry.fixture_set_sha256,
            sha256_hex(TRIAL_REGISTRY_JSON.as_bytes())
        );
        assert_ne!(registry.evaluated_at_ms, 0);
    }

    #[test]
    fn trusted_trial_statuses_are_model_specific_and_never_selectable() {
        let models = trusted_reviewed_results()
            .unwrap()
            .models
            .into_iter()
            .map(|entry| crate::providers::OpenRouterModel {
                id: entry.model_id,
                canonical_slug: None,
                name: "fixture model".into(),
                context_length: Some(32_768),
                prompt_price: Some("0".into()),
                completion_price: Some("0".into()),
                input_modalities: Some(vec!["text".into()]),
                output_modalities: Some(vec!["text".into()]),
                supported_parameters: None,
            })
            .collect::<Vec<_>>();
        let statuses = statuses_from_catalog(
            "openrouter",
            &models,
            &BTreeMap::new(),
            &BTreeSet::new(),
            1_793_059_200_000,
            "aider",
            "0.86.2",
            1_793_059_200_000,
        );
        assert_eq!(statuses.len(), 3);
        assert!(statuses
            .iter()
            .all(|status| status.evaluation_status == EvidenceStatus::Failed));
        assert!(statuses.iter().all(|status| !status.selectable));
        let laguna = statuses
            .iter()
            .find(|status| status.model_id == "poolside/laguna-s-2.1:free")
            .unwrap();
        assert_eq!(laguna.privacy_status, PrivacyStatus::Unknown);
        assert_eq!((laguna.evaluation_passed, laguna.evaluation_cases), (2, 3));
        assert!(laguna
            .reasons
            .iter()
            .any(|reason| reason.contains("multi-file")));
        for status in statuses
            .iter()
            .filter(|status| status.model_id.starts_with("nvidia/"))
        {
            assert_eq!(status.privacy_status, PrivacyStatus::Incompatible);
            assert_eq!((status.evaluation_passed, status.evaluation_cases), (0, 3));
            assert!(status
                .reasons
                .iter()
                .any(|reason| reason.contains("runtime failures")));
        }
        let stale = statuses_from_catalog(
            "openrouter",
            &models,
            &BTreeMap::new(),
            &BTreeSet::new(),
            1_793_059_200_000,
            "aider",
            "0.86.3",
            1_793_059_200_000,
        );
        assert!(stale
            .iter()
            .all(|status| status.evaluation_status == EvidenceStatus::Stale));
    }

    #[test]
    fn free_and_router_ids_are_never_paid_identities() {
        assert!(is_router_model_id("openrouter/free"));
        assert!(is_router_model_id("openrouter/auto"));
        assert!(is_router_model_id("vendor/model/free"));
        assert!(!is_router_model_id("vendor/model:free"));
        assert!(is_free_variant_id("vendor/model:free"));
        assert!(is_free_variant_id("vendor/model/free"));
        assert!(!is_free_variant_id("vendor/model"));
    }

    fn evidence(now: u64) -> (EconomyModelPolicy, EndpointPrivacyEvidence) {
        let privacy = EndpointPrivacyEvidence {
            provider_id: "openrouter".into(),
            provider_slug: "endpoint-zdr-1".into(),
            model_id: "vendor/free-model".into(),
            endpoint_id: "endpoint-zdr-1".into(),
            observed_at_ms: now,
            source_url: "https://example.invalid/policy".into(),
            no_training: true,
            no_retention: true,
            no_content_logging: true,
        };
        let policy = EconomyModelPolicy {
            provider_id: "openrouter-main".into(),
            model_id: "vendor/free-model".into(),
            runtime_id: "aider".into(),
            context_tokens: 16_384,
            max_output_tokens_per_call: 2_048,
            max_calls: 8,
            max_tokens: 200_000,
            max_seconds: 1_800,
            catalog_source: "authenticated-model-catalog".into(),
            catalog_observed_at_ms: now,
            context_snapshot: 16_384,
            evaluation: CodingEvaluationEvidence {
                model_id: "vendor/free-model".into(),
                runtime_id: "aider".into(),
                runtime_version: "0.86.2".into(),
                evaluated_at_ms: now,
                fixture_set_id: "public-fixtures-1".into(),
                fixture_set_sha256: "a".repeat(64),
                result_sha256: "b".repeat(64),
                cases: 3,
                passed: 3,
                independent_review: true,
            },
            privacy: privacy.clone(),
        };
        (policy, privacy)
    }

    #[test]
    fn admission_requires_exact_zero_prices_and_pinned_identity() {
        let now = 1_000_000;
        let (policy, privacy) = evidence(now);
        assert_eq!(
            policy.validate_live(
                now,
                "openrouter-main",
                "vendor/free-model",
                "aider",
                "0.86.2",
                Some(16_384),
                Some(0),
                Some(0),
                Some(&privacy)
            ),
            Ok(())
        );
        assert_eq!(
            policy.validate_live(
                now,
                "openrouter-main",
                "openrouter/free",
                "aider",
                "0.86.2",
                Some(16_384),
                Some(0),
                Some(0),
                Some(&privacy)
            ),
            Err(EconomyAdmissionError::InvalidIdentity)
        );
        assert_eq!(
            policy.validate_live(
                now,
                "openrouter-main",
                "vendor/free-model",
                "aider",
                "0.86.2",
                Some(16_384),
                None,
                Some(0),
                Some(&privacy)
            ),
            Err(EconomyAdmissionError::PricingChanged)
        );
    }

    #[test]
    fn admission_rejects_stale_or_incompatible_privacy_and_changed_context() {
        let now = 1_000_000;
        let (policy, privacy) = evidence(now);
        assert_eq!(
            policy.validate_live(
                now,
                "openrouter-main",
                "vendor/free-model",
                "aider",
                "0.86.2",
                Some(16_384),
                Some(0),
                Some(0),
                None
            ),
            Err(EconomyAdmissionError::PrivacyEvidenceMissing)
        );
        let mut retained = privacy.clone();
        retained.no_retention = false;
        assert_eq!(
            policy.validate_live(
                now,
                "openrouter-main",
                "vendor/free-model",
                "aider",
                "0.86.2",
                Some(16_384),
                Some(0),
                Some(0),
                Some(&retained)
            ),
            Err(EconomyAdmissionError::PrivacyPolicyIncompatible)
        );
        assert_eq!(
            policy.validate_live(
                now,
                "openrouter-main",
                "vendor/free-model",
                "aider",
                "0.86.2",
                Some(32_768),
                Some(0),
                Some(0),
                Some(&privacy)
            ),
            Err(EconomyAdmissionError::ContextChanged)
        );
    }

    #[test]
    fn catalog_model_or_self_report_cannot_replace_dated_passing_trial() {
        let now = 1_000_000;
        let (mut policy, privacy) = evidence(now);
        policy.evaluation.passed = 2;
        assert_eq!(
            policy.validate_live(
                now,
                "openrouter-main",
                "vendor/free-model",
                "aider",
                "0.86.2",
                Some(16_384),
                Some(0),
                Some(0),
                Some(&privacy)
            ),
            Err(EconomyAdmissionError::EvaluationDidNotPass)
        );
    }

    #[test]
    fn fixture_admission_requires_exact_flat_seed_inventory_and_bytes() {
        let task = load_trial_registry().unwrap().tasks.remove(0);
        let dir = tempfile::tempdir().unwrap();
        for seed in &task.seed_files {
            fs::write(dir.path().join(&seed.path), seed.content.as_bytes()).unwrap();
        }
        assert!(verify_trial_fixture(&task.id, dir.path()).is_ok());
        fs::write(dir.path().join(".hidden-extra"), b"extra").unwrap();
        assert!(verify_trial_fixture(&task.id, dir.path()).is_err());
        fs::remove_file(dir.path().join(".hidden-extra")).unwrap();
        let seed = &task.seed_files[0];
        fs::write(dir.path().join(&seed.path), b"modified").unwrap();
        assert!(verify_trial_fixture(&task.id, dir.path()).is_err());
    }

    #[test]
    fn trial_registry_is_exact_and_zero_spend_bounded() {
        let registry = load_trial_registry().unwrap();
        assert_eq!(registry.models.len(), 3);
        assert_eq!(registry.tasks.len(), 3);
        assert_eq!(registry.limits.max_requests_per_task, 3);
        assert_eq!(registry.limits.max_output_tokens_per_request, 8_192);
        assert_eq!(registry.limits.max_seconds_per_task, 120);
        assert_eq!(registry.limits.max_cost_usd, 0);
        assert_eq!(registry.limits.retries, 0);
        assert!(!registry.limits.fallbacks);
    }

    #[test]
    fn trial_diagnostic_redacts_private_roots_and_bounds_output() {
        let fixture = Path::new("C:/private/fixture");
        let data = Path::new("C:/private/trial");
        let settings = Path::new("C:/private/settings.json");
        let value = format!(
            "{} {} {} sk-secret",
            fixture.display(),
            data.display(),
            settings.display()
        );
        let redacted = redact_trial_diagnostic(&value, fixture, data, settings, "sk-secret");
        assert_eq!(
            redacted,
            "[synthetic-fixture] [private-trial] [private-settings] [REDACTED]"
        );
        let mut bounded = String::new();
        append_bounded_diagnostic(&mut bounded, &"x".repeat(8_000));
        assert_eq!(bounded.len(), 4_096);
        assert!(!bounded.chars().any(char::is_control));
    }
}
