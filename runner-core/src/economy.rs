//! Admission evidence and fixed-route policy for optional OpenRouter Economy
//! runs. Catalog presence and model self-reports are deliberately insufficient
//! to create a tested candidate.

use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
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

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TrialRegistry {
    pub schema_version: u32,
    pub models: Vec<String>,
    pub limits: TrialLimits,
    pub tasks: Vec<TrialTask>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TrialLimits {
    pub max_requests_per_task: u32,
    pub max_output_tokens_per_request: u64,
    pub max_seconds_per_task: u64,
    pub max_cost_usd: u64,
    pub retries: u32,
    pub fallbacks: bool,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TrialTask {
    pub id: String,
    pub prompt: String,
    #[serde(rename = "seedFiles")]
    pub seed_files: Vec<TrialSeedFile>,
    pub checks: Vec<TrialCheck>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TrialSeedFile {
    pub path: String,
    pub content: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TrialCheck {
    pub id: String,
}

#[derive(Debug, Clone)]
pub struct VerifiedTrialFixture {
    pub task: TrialTask,
    pub root: PathBuf,
    pub seed_inventory_fingerprint: String,
    pub fixture_set_sha256: String,
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
pub fn verify_trial_fixture(
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

/// Build Economy UI rows from an authenticated catalog and the immutable
/// compile-time evaluation bundle. Endpoint privacy is always revalidated at
/// dispatch; absent a trusted serving-endpoint proof, rows remain unselectable.
pub fn statuses_from_catalog(
    provider_id: &str,
    models: &[crate::providers::OpenRouterModel],
    catalog_observed_at_ms: u64,
    runtime_id: &str,
    runtime_version: &str,
    now_ms: u64,
) -> Vec<EconomyModelStatus> {
    let registry = trusted_reviewed_results();
    models
        .iter()
        .filter(|model| {
            model.prompt_price.as_deref().and_then(crate::providers::openrouter_price_micros_per_million) == Some(0)
                && model.completion_price.as_deref().and_then(crate::providers::openrouter_price_micros_per_million) == Some(0)
                && valid_model_id(&model.id)
        })
        .map(|model| {
            let model_input = model.prompt_price.as_deref().and_then(crate::providers::openrouter_price_micros_per_million);
            let model_output = model.completion_price.as_deref().and_then(crate::providers::openrouter_price_micros_per_million);
            let evaluation = registry.as_ref().and_then(|r| r.models.iter().find(|v| v.model_id == model.id));
            let cases = registry.as_ref().map(|r| r.cases.iter().filter(|v| v.model_id == model.id).collect::<Vec<_>>()).unwrap_or_default();
            let evaluation_passed = evaluation.map_or(0, |v| v.passed_cases);
            let evaluation_cases = evaluation.map_or(0, |v| v.total_cases);
            let reason_codes = evaluation.map(|v| v.reason_codes.clone()).unwrap_or_default();
            let derived_passed = cases.iter().filter(|c| {
                c.status == "passed" && c.runtime.ok && c.runtime.state == "completed"
                    && c.host_check.status == "passed" && c.host_check.candidate_inventory_matches_source
                    && c.host_check.check_files_match_recorded_hashes
                    && c.independent_review.status == "passed"
                    && c.independent_review.reviewed_inventory_matches_source && c.source.inventory_matches_review
            }).count() as u32;
            let registry_case_integrity = evaluation.is_some_and(|ev| {
                cases.len() == ev.total_cases as usize && ev.total_cases == 3
                    && ev.passed_cases == derived_passed
                    && cases.iter().map(|c| c.task_id.as_str()).collect::<BTreeSet<_>>().len() == 3
                    && cases.iter().all(|c| c.runtime.request_count <= 3
                        && c.runtime.tokens_used <= 122_880
                        && c.runtime.latency_ms <= 120_000
                        && c.runtime.observed_model_id.as_deref() == Some(c.model_id.as_str())
                        && ((c.status == "passed") == (c.runtime.ok && c.runtime.state == "completed"
                            && c.host_check.status == "passed" && c.host_check.candidate_inventory_matches_source
                            && c.host_check.check_files_match_recorded_hashes && c.independent_review.status == "passed"
                            && c.independent_review.reviewed_inventory_matches_source && c.source.inventory_matches_review))
                        && c.host_check.candidate_inventory_matches_source
                        && c.host_check.check_files_match_recorded_hashes
                        && is_sha256(&c.runtime.receipt_sha256)
                        && is_sha256(&c.runtime.source_fingerprint)
                        && is_sha256(&c.source.source_journal_sha256)
                        && c.host_check.check_hashes.values().all(|h| is_sha256(h)))
            });
            let evidence_status = match evaluation {
                None => EvidenceStatus::NotEvaluated,
                Some(_ev) if runtime_id != registry.as_ref().map_or("", |r| r.runtime.id.as_str())
                    || runtime_version != registry.as_ref().map_or("", |r| r.runtime.version.as_str())
                    || registry.as_ref().is_some_and(|r| r.evaluated_at_ms > now_ms
                        || now_ms.saturating_sub(r.evaluated_at_ms) > MAX_EVIDENCE_AGE_MS) => EvidenceStatus::Stale,
                Some(ev) if !registry_case_integrity || ev.status != "passed"
                    || ev.passed_cases != ev.total_cases || ev.total_cases == 0 => EvidenceStatus::Failed,
                Some(_) => EvidenceStatus::Passed,
            };
            // Evaluation evidence never implies endpoint privacy. A separately
            // authenticated, current endpoint policy is required at dispatch.
            let endpoint: Option<&EndpointPrivacyEvidence> = None;
            let privacy_status = PrivacyStatus::Unknown;
            let mut reasons = Vec::new();
            if evidence_status == EvidenceStatus::Failed {
                if reason_codes.iter().any(|code| code == "independent_review_failed_or_inventory_mismatch") {
                    reasons.push("Synthetic coding evaluation failed: the multi-file task did not pass independent host review or its source inventory did not match.".into());
                } else if reason_codes.iter().any(|code| code == "RUNTIME_FAILED") {
                    reasons.push("Synthetic coding evaluation failed: all three tasks ended with runtime failures.".into());
                } else { reasons.push("Synthetic coding evaluation did not pass all required tasks.".into()); }
            }
            if evidence_status == EvidenceStatus::Stale { reasons.push("Coding evaluation is stale or does not match the current Aider runtime.".into()); }
            if evidence_status == EvidenceStatus::NotEvaluated { reasons.push("No trusted coding evaluation is recorded for this model.".into()); }
            if privacy_status == PrivacyStatus::Unknown { reasons.push("Serving endpoint training, retention, and logging policy is unknown.".into()); }
            if privacy_status == PrivacyStatus::Incompatible { reasons.push("Serving endpoint policy does not guarantee no training, retention, and content logging.".into()); }
            if catalog_observed_at_ms > now_ms || now_ms.saturating_sub(catalog_observed_at_ms) > 24 * 60 * 60 * 1000 { reasons.push("Authenticated catalog evidence is stale.".into()); }
            if model.context_length.is_none() { reasons.push("Model context capacity is unknown.".into()); }
            reasons.push("No reviewed, privacy-verified serving endpoint is available for Economy dispatch.".into());
            let valid = false;
            EconomyModelStatus {
                provider_id: provider_id.into(),
                model_id: model.id.clone(),
                context_tokens: model.context_length,
                input_price_micros_per_1m: model_input,
                output_price_micros_per_1m: model_output,
                catalog_observed_at_ms,
                evaluation_status: evidence_status,
                evaluation_runtime_version: registry.as_ref().map(|r| r.runtime.version.clone()).filter(|_| evaluation.is_some()),
                current_runtime_version: Some(runtime_version.to_owned()),
                evaluation_at_ms: evaluation.map(|_| registry.as_ref().map_or(0, |r| r.evaluated_at_ms)),
                evaluation_passed,
                evaluation_cases,
                evaluation_reason_codes: reason_codes,
                privacy_status,
                privacy_source: endpoint.map(|value| value.source_url.clone()),
                privacy_observed_at_ms: endpoint.map(|value| value.observed_at_ms),
                selectable: valid,
                reasons,
            }
        })
        .collect()
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct EndpointPrivacyEvidence {
    /// Provider and serving endpoint are part of the attested identity.
    pub provider_id: String,
    pub endpoint_id: String,
    pub observed_at_ms: u64,
    pub source_url: String,
    pub no_training: bool,
    pub no_retention: bool,
    pub no_content_logging: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
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
        if privacy.provider_id != self.provider_id
            || self.privacy.provider_id != self.provider_id
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
        assert!(
            registry
                .models
                .iter()
                .all(|model| model.passed_cases < model.total_cases)
        );
        assert!(
            registry
                .cases
                .iter()
                .all(|case| case.runtime.observed_model_id.as_deref()
                    == Some(case.model_id.as_str()))
        );
        assert!(
            registry
                .cases
                .iter()
                .all(|case| case.runtime.request_count <= 3)
        );
        assert!(
            registry
                .cases
                .iter()
                .all(|case| case.runtime.latency_ms <= 120_000)
        );
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
            1_793_059_200_000,
            "aider",
            "0.86.2",
            1_793_059_200_000,
        );
        assert_eq!(statuses.len(), 3);
        assert!(
            statuses
                .iter()
                .all(|status| status.evaluation_status == EvidenceStatus::Failed)
        );
        assert!(statuses.iter().all(|status| !status.selectable && status.privacy_status == PrivacyStatus::Unknown));
        let laguna = statuses
            .iter()
            .find(|status| status.model_id == "poolside/laguna-s-2.1:free")
            .unwrap();
        assert_eq!((laguna.evaluation_passed, laguna.evaluation_cases), (2, 3));
        assert!(
            laguna
                .reasons
                .iter()
                .any(|reason| reason.contains("multi-file"))
        );
        for status in statuses
            .iter()
            .filter(|status| status.model_id.starts_with("nvidia/"))
        {
            assert_eq!((status.evaluation_passed, status.evaluation_cases), (0, 3));
            assert!(
                status
                    .reasons
                    .iter()
                    .any(|reason| reason.contains("runtime failures"))
            );
        }
        let stale = statuses_from_catalog(
            "openrouter",
            &models,
            1_793_059_200_000,
            "aider",
            "0.86.3",
            1_793_059_200_000,
        );
        assert!(
            stale
                .iter()
                .all(|status| status.evaluation_status == EvidenceStatus::Stale)
        );
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
            provider_id: "openrouter-main".into(),
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
