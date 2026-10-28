//! Shared foreground provider dispatcher. The caller owns job history, workspace
//! isolation and approval policy; this module owns provider selection, safe
//! process setup, event normalization and cancellation.

use crate::budgets::BudgetStore;
use crate::paid_gateway::{PaidGateway, PaidGatewayConfig, PaidModelPolicy, PaidPolicyRefusal};
use crate::process::{self, OutputStream, ProcessControl, ProcessIdentity, ProcessOutput};
use crate::providers::{self, ProviderKind, ProviderProfile};
use crate::settings::Settings;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::ffi::OsString;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::Child;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

const MAX_PROMPT_BYTES: usize = 60_000;
const MAX_OUTPUT_BYTES: usize = 2 * 1024 * 1024;
const MAX_REVIEW_OUTPUT_BYTES: usize = 64 * 1024;
const MAX_TIMEOUT_SECONDS: u64 = 86_400;
const CODEX_STARTUP_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_AIDER_VERSION_PROBE_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_CODEX_MODEL_LIST_PAGES: usize = 16;
const MAX_CODEX_MODELS: usize = 500;
const PROVIDER_BILLING_ENV: &[&str] = &[
    "OPENAI_API_KEY",
    "OPENAI_API_BASE",
    "OPENAI_BASE_URL",
    "CODEX_API_KEY",
    "OPENROUTER_API_KEY",
    "ANTHROPIC_API_KEY",
    "ANTHROPIC_AUTH_TOKEN",
    "ANTHROPIC_BASE_URL",
    "GEMINI_API_KEY",
    "GOOGLE_API_KEY",
    "GOOGLE_APPLICATION_CREDENTIALS",
    "AZURE_OPENAI_API_KEY",
    "AZURE_OPENAI_ENDPOINT",
    "AWS_ACCESS_KEY_ID",
    "AWS_SECRET_ACCESS_KEY",
    "AWS_SESSION_TOKEN",
    "COHERE_API_KEY",
    "GROQ_API_KEY",
    "MISTRAL_API_KEY",
    "DEEPSEEK_API_KEY",
    "TOGETHER_API_KEY",
    "FIREWORKS_API_KEY",
    "XAI_API_KEY",
    "HUGGINGFACEHUB_API_TOKEN",
];

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RuntimeRequest {
    pub provider_id: Option<String>,
    pub model_id: Option<String>,
    pub cwd: PathBuf,
    pub prompt: String,
    pub timeout_seconds: u64,
    #[serde(default)]
    pub reasoning_effort: Option<String>,
    /// Restrict the adapter to a verified read-only execution mode.
    #[serde(default)]
    pub read_only: bool,
    pub test_command: Option<String>,
    /// Paid OpenRouter runs are rejected until the selected harness can enforce
    /// a per-run token/call ceiling. This flag is informational and never
    /// bypasses that safety gate.
    pub allow_paid: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum RuntimeState {
    Completed,
    Failed,
    AuthenticationRequired,
    RateLimited,
    UnsupportedModel,
    Cancelled,
    TimedOut,
    NeedsInput,
    UnsupportedRuntime,
    PaidBudgetUnenforceable,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum RuntimeEventKind {
    ProcessStarted,
    Output,
    State,
    ApprovalRequest,
    UserInputRequest,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RuntimeEvent {
    pub kind: RuntimeEventKind,
    pub stream: Option<String>,
    pub text: String,
    pub process: Option<ProcessIdentity>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RuntimeControl {
    Continue,
    Cancel,
    Respond(Value),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum BillingStatus {
    Unknown,
    LocalNoApiCharge,
    ProviderManagedUnknown,
    ProviderReported,
    ZeroCostPolicy,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RuntimeUsage {
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub total_tokens: Option<u64>,
    pub cost_usd: Option<f64>,
    pub source: Option<String>,
    pub confidence: Option<String>,
    pub billing: BillingStatus,
}

impl Default for RuntimeUsage {
    fn default() -> Self {
        Self {
            input_tokens: None,
            output_tokens: None,
            total_tokens: None,
            cost_usd: None,
            source: None,
            confidence: None,
            billing: BillingStatus::Unknown,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RuntimeOutcome {
    pub state: RuntimeState,
    pub provider_id: String,
    pub model_id: String,
    pub exit_code: Option<i32>,
    pub elapsed_ms: u64,
    pub message: String,
    pub usage: RuntimeUsage,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeError {
    pub code: &'static str,
    pub message: String,
}

impl RuntimeError {
    fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

pub trait RuntimeEventSink {
    fn on_event(&mut self, event: RuntimeEvent) -> RuntimeControl;

    /// Preserve default event delivery while allowing consumers to treat a
    /// completed app-server message as authoritative over its streamed deltas.
    fn on_final_output(&mut self, event: RuntimeEvent) -> RuntimeControl {
        self.on_event(event)
    }
}

/// Internal-only budget context for a paid run. It is never deserialized from
/// a frontend or CLI request; the supervisor supplies the already-reserved
/// durable parent envelope and the immutable reviewed model policy.
pub struct PaidDispatchContext<'a> {
    pub budgets: &'a BudgetStore,
    pub parent_reservation_id: &'a str,
    pub policy: &'a PaidModelPolicy,
    /// Immutable file allowlist admitted by the supervisor for this task.
    pub allowed_files: &'a [String],
    pub authenticated_canonical_slug: Option<&'a str>,
    pub economy_trial_task_id: Option<&'a str>,
    /// Consent is set only by the bounded synthetic trial runner.
    pub synthetic_trial_consent: bool,
    /// Opaque fixture proof created by the core's exact fixture verifier.
    pub synthetic_trial_fixture: Option<&'a crate::economy::VerifiedTrialFixture>,
    /// Opaque production Economy proof. Synthetic trial consent is a separate
    /// path and cannot populate this field.
    pub economy_admission: Option<&'a crate::economy::VerifiedEconomyAdmission>,
    pub cancel: Arc<AtomicBool>,
}

/// Execute the explicitly selected configured provider in `cwd`. This API
/// never chooses a fallback provider and never routes a paid request silently.
pub fn run(
    settings: &Settings,
    request: &RuntimeRequest,
    cancel: &AtomicBool,
    sink: &mut impl RuntimeEventSink,
) -> Result<RuntimeOutcome, RuntimeError> {
    run_internal(settings, request, cancel, sink, None, None, None)
}

#[derive(Debug, Clone)]
pub(crate) struct CodexModelObservation {
    pub id: String,
    pub context_tokens: Option<u64>,
    pub supports_tools: Option<bool>,
}

#[derive(Debug, Clone)]
pub(crate) struct CodexAccountModelProbe {
    /// `None` means the app-server did not provide a trustworthy account state.
    pub authenticated: Option<bool>,
    pub model_list_verified: bool,
    pub models: Vec<CodexModelObservation>,
}

/// Parses one Codex model/list page. It does not assert that the full inventory
/// is complete; the caller must follow each cursor until the terminal page.
/// Legacy `result.models` / bare result-array shapes remain supported.
fn parse_codex_model_page(
    response: &Value,
) -> Result<(Vec<CodexModelObservation>, Option<String>), &'static str> {
    if response.get("error").is_some() {
        return Err("model/list returned an RPC error");
    }
    let result = response.get("result").ok_or("missing result")?;
    let models = result
        .get("data")
        .or_else(|| result.get("models"))
        .and_then(Value::as_array)
        .or_else(|| result.as_array())
        .ok_or("missing model array")?;

    if models.is_empty() || models.len() > MAX_CODEX_MODELS {
        return Err("empty or over-limit model inventory");
    }
    let next_cursor = match result
        .as_object()
        .and_then(|object| object.get("nextCursor"))
    {
        None | Some(Value::Null) => None,
        Some(Value::String(cursor)) if !cursor.trim().is_empty() && cursor.len() <= 4096 => {
            Some(cursor.clone())
        }
        Some(_) => return Err("model/list continuation cursor is malformed"),
    };
    let mut parsed = Vec::with_capacity(models.len());
    for model in models {
        let id = model
            .get("id")
            .or_else(|| model.get("model"))
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|id| !id.is_empty() && id.len() <= 160 && !id.chars().any(char::is_control))
            .ok_or("model inventory contains an unidentifiable entry")?;
        if parsed
            .iter()
            .any(|known: &CodexModelObservation| known.id == id)
        {
            return Err("model inventory contains duplicate IDs");
        }
        parsed.push(CodexModelObservation {
            id: id.to_owned(),
            context_tokens: model
                .get("contextWindow")
                .or_else(|| model.pointer("/modelInfo/contextWindow"))
                .and_then(Value::as_u64),
            supports_tools: model.get("supportsTools").and_then(Value::as_bool),
        });
    }
    Ok((parsed, next_cursor))
}

/// Parses a one-page result only when it proves there is no continuation.
/// The live probe uses `parse_codex_model_page` and marks the aggregate complete
/// only after the final page returns a null or absent cursor.
fn parse_codex_model_inventory(
    response: &Value,
) -> Result<Vec<CodexModelObservation>, &'static str> {
    let (models, next_cursor) = parse_codex_model_page(response)?;
    if next_cursor.is_some() {
        return Err("model/list inventory has a continuation cursor");
    }
    Ok(models)
}

/// Model-free Codex app-server probe used only by explicit provider refresh.
/// It performs initialize, account/read, and model/list; it never starts a
/// thread or turn and never persists account identifiers or response text.
pub(crate) fn probe_codex_account_and_models(
    profile: &ProviderProfile,
    timeout: Duration,
) -> Result<CodexAccountModelProbe, String> {
    probe_codex_account_and_models_until(
        profile,
        Instant::now() + timeout.min(Duration::from_secs(10)),
    )
}

pub(crate) fn probe_codex_account_and_models_until(
    profile: &ProviderProfile,
    deadline: Instant,
) -> Result<CodexAccountModelProbe, String> {
    if profile.kind != ProviderKind::Codex {
        return Err("Codex observation probe received a different provider kind.".into());
    }
    if Instant::now() >= deadline {
        return Err("Codex metadata probe deadline expired before launch.".into());
    }
    let protocol_deadline = codex_probe_protocol_deadline(deadline)?;
    let executable = Path::new(profile.executable.trim());
    if !executable.is_absolute()
        || matches!(
            executable.extension().and_then(|value| value.to_str()),
            Some(extension)
                if extension.eq_ignore_ascii_case("cmd")
                    || extension.eq_ignore_ascii_case("bat")
        )
    {
        return Err("Codex observation requires an explicit absolute entrypoint supported by the configured process adapter.".into());
    }
    let temporary = tempfile::tempdir()
        .map_err(|_| "Could not create a bounded Codex probe workspace.".to_string())?;
    let args = codex_app_server_arguments();
    let (process, identity) = process::spawn_interactive(
        executable,
        &args,
        temporary.path(),
        &[],
        PROVIDER_BILLING_ENV,
    )
    .map_err(|_| "Could not start the configured Codex app-server entrypoint.".to_string())?;
    let mut child = OwnedProcess {
        child: process,
        identity,
        terminate_on_drop: true,
    };
    // Keep stdin open through tree cleanup. Dropping it when the protocol
    // exchange returns makes a streaming app-server exit on EOF before the
    // identity-checked process-tree termination can verify its target.
    let mut stdin = child.stdin.take().expect("piped stdin");
    let result = (|| -> Result<CodexAccountModelProbe, String> {
        let stdout = child.stdout.take().expect("piped stdout");
        let (sender, receiver) = mpsc::sync_channel::<Result<String, ()>>(16);
        thread::spawn(move || {
            let mut reader = BufReader::with_capacity(8 * 1024, stdout);
            loop {
                let mut bytes = Vec::new();
                match reader
                    .by_ref()
                    .take(1024 * 1024 + 1)
                    .read_until(b'\n', &mut bytes)
                {
                    Ok(0) => break,
                    Ok(_) if bytes.len() <= 1024 * 1024 => match String::from_utf8(bytes) {
                        Ok(line) => {
                            if sender.send(Ok(line)).is_err() {
                                break;
                            }
                        }
                        Err(_) => break,
                    },
                    _ => {
                        let _ = sender.send(Err(()));
                        break;
                    }
                }
            }
        });
        if let Some(stderr) = child.stderr.take() {
            thread::spawn(move || {
                let mut reader = BufReader::new(stderr);
                let mut buffer = [0u8; 4096];
                loop {
                    match std::io::Read::read(&mut reader, &mut buffer) {
                        Ok(0) | Err(_) => break,
                        _ => {}
                    }
                }
            });
        }
        send_codex(
            &mut stdin,
            &providers::app_server_initialize(1, env!("CARGO_PKG_VERSION")),
        )
        .map_err(|_| "Could not initialize the Codex app-server protocol.".to_string())?;
        let initialized = receive_codex_probe_response(&mut child, &receiver, protocol_deadline, 1)
            .map_err(|error| error.message)?;
        if initialized.get("error").is_some() {
            let state = codex_request_error_state(&initialized, RuntimeState::UnsupportedRuntime);
            return Err(codex_state_message(state).into());
        }
        send_codex(
            &mut stdin,
            &serde_json::json!({"method":"initialized","params":{}}),
        )
        .map_err(|_| "Could not complete Codex app-server initialization.".to_string())?;
        send_codex(
            &mut stdin,
            &serde_json::json!({"id":2,"method":"account/read","params":{}}),
        )
        .map_err(|_| "Could not request Codex account state.".to_string())?;
        let account = receive_codex_probe_response(&mut child, &receiver, protocol_deadline, 2)
            .map_err(|error| error.message)?;
        if account.get("error").is_some() {
            let state = codex_request_error_state(&account, RuntimeState::Failed);
            return Ok(CodexAccountModelProbe {
                authenticated: match state {
                    RuntimeState::AuthenticationRequired => Some(false),
                    _ => None,
                },
                model_list_verified: false,
                models: Vec::new(),
            });
        }
        let authenticated = codex_authenticated_from_account_read(&account);
        let mut request_id = 3_u64;
        let mut page_count = 0_usize;
        let mut cursor = None::<String>;
        let mut seen_cursors = std::collections::HashSet::new();
        let mut seen_model_ids = std::collections::HashSet::new();
        let mut all_models = Vec::new();
        loop {
            if Instant::now() >= protocol_deadline {
                return Ok(CodexAccountModelProbe {
                    authenticated,
                    model_list_verified: false,
                    models: Vec::new(),
                });
            }
            let params = match cursor.as_deref() {
                Some(value) => serde_json::json!({"includeHidden":false,"cursor":value}),
                None => serde_json::json!({"includeHidden":false}),
            };
            send_codex(
                &mut stdin,
                &serde_json::json!({"id":request_id,"method":"model/list","params":params}),
            )
            .map_err(|_| "Could not request Codex model inventory.".to_string())?;
            let response =
                receive_codex_probe_response(&mut child, &receiver, protocol_deadline, request_id)
                    .map_err(|error| error.message)?;
            if response.get("error").is_some() {
                let state = codex_request_error_state(&response, RuntimeState::Failed);
                return Ok(CodexAccountModelProbe {
                    authenticated: match state {
                        RuntimeState::AuthenticationRequired => Some(false),
                        _ => authenticated,
                    },
                    model_list_verified: false,
                    models: Vec::new(),
                });
            }
            let (page_models, next_cursor) = match parse_codex_model_page(&response) {
                Ok(page) => page,
                Err(_) => {
                    return Ok(CodexAccountModelProbe {
                        authenticated,
                        model_list_verified: false,
                        models: Vec::new(),
                    });
                }
            };
            if all_models.len().saturating_add(page_models.len()) > MAX_CODEX_MODELS
                || page_models
                    .iter()
                    .any(|model| !seen_model_ids.insert(model.id.clone()))
            {
                return Ok(CodexAccountModelProbe {
                    authenticated,
                    model_list_verified: false,
                    models: Vec::new(),
                });
            }
            all_models.extend(page_models);
            page_count += 1;
            let Some(next) = next_cursor else {
                return Ok(CodexAccountModelProbe {
                    authenticated,
                    model_list_verified: true,
                    models: all_models,
                });
            };
            if page_count >= MAX_CODEX_MODEL_LIST_PAGES || !seen_cursors.insert(next.clone()) {
                return Ok(CodexAccountModelProbe {
                    authenticated,
                    model_list_verified: false,
                    models: Vec::new(),
                });
            }
            cursor = Some(next);
            request_id = request_id
                .checked_add(1)
                .ok_or_else(|| "Codex model/list request ID exhausted.".to_string())?;
        }
    })();
    let cleanup = process::terminate_child_tree_until(&mut child.child, identity, deadline);
    child.terminate_on_drop = false;
    match cleanup {
        Ok(()) => result,
        Err(_) => Err(
            "Codex metadata probe cleanup could not be verified; dispatch must remain blocked."
                .into(),
        ),
    }
}

fn codex_probe_protocol_deadline(deadline: Instant) -> Result<Instant, String> {
    const CLEANUP_RESERVE: Duration = Duration::from_secs(2);
    let protocol_deadline = deadline.checked_sub(CLEANUP_RESERVE).ok_or_else(|| {
        "Codex metadata probe has insufficient time reserved for cleanup.".to_string()
    })?;
    if Instant::now() >= protocol_deadline {
        return Err("Codex metadata probe has insufficient time reserved for cleanup.".into());
    }
    Ok(protocol_deadline)
}

/// Execute a provider with Cargo output routed to a backend-owned workspace
/// directory. The directory is supplied by the supervisor, never by a
/// deserialized runtime request.
pub fn run_in_build_output(
    settings: &Settings,
    request: &RuntimeRequest,
    cancel: &AtomicBool,
    sink: &mut impl RuntimeEventSink,
    build_output: &Path,
) -> Result<RuntimeOutcome, RuntimeError> {
    run_internal(
        settings,
        request,
        cancel,
        sink,
        None,
        Some(build_output),
        None,
    )
}

pub fn run_budgeted(
    settings: &Settings,
    request: &RuntimeRequest,
    cancel: &AtomicBool,
    sink: &mut impl RuntimeEventSink,
    paid: &PaidDispatchContext<'_>,
) -> Result<RuntimeOutcome, RuntimeError> {
    run_internal(settings, request, cancel, sink, Some(paid), None, None)
}

pub fn run_budgeted_in_build_output(
    settings: &Settings,
    request: &RuntimeRequest,
    cancel: &AtomicBool,
    sink: &mut impl RuntimeEventSink,
    paid: &PaidDispatchContext<'_>,
    build_output: &Path,
) -> Result<RuntimeOutcome, RuntimeError> {
    run_internal(
        settings,
        request,
        cancel,
        sink,
        Some(paid),
        Some(build_output),
        None,
    )
}

fn codex_authenticated_from_account_read(response: &Value) -> Option<bool> {
    match response
        .pointer("/result/account/type")
        .and_then(Value::as_str)
    {
        Some("chatgpt") => Some(true),
        Some(_) => Some(false),
        None => None,
    }
}

fn run_internal(
    settings: &Settings,
    request: &RuntimeRequest,
    cancel: &AtomicBool,
    sink: &mut impl RuntimeEventSink,
    paid: Option<&PaidDispatchContext<'_>>,
    build_output: Option<&Path>,
    codex_output_schema: Option<&Value>,
) -> Result<RuntimeOutcome, RuntimeError> {
    let (profile, model_id) = resolve_profile_and_model(settings, request)?;
    if let Some(effort) = request.reasoning_effort.as_deref() {
        let model = profile
            .models
            .iter()
            .find(|model| model.id == model_id)
            .unwrap();
        if !model
            .reasoning_efforts
            .iter()
            .any(|supported| supported == effort)
        {
            return Err(RuntimeError::new(
                "UNSUPPORTED_REASONING_EFFORT",
                "The selected reasoning effort is not configured for this provider model.",
            ));
        }
    }
    let cwd = request.cwd.canonicalize().map_err(|_| {
        RuntimeError::new(
            "INVALID_WORKSPACE",
            "The selected project folder could not be resolved.",
        )
    })?;
    if !cwd.is_dir() {
        return Err(RuntimeError::new(
            "INVALID_WORKSPACE",
            "The selected project must be an existing folder.",
        ));
    }
    let prompt = request.prompt.trim();
    if prompt.is_empty() || request.prompt.len() > MAX_PROMPT_BYTES || request.prompt.contains('\0')
    {
        return Err(RuntimeError::new(
            "INVALID_PROMPT",
            "Enter a prompt of 1 to 60,000 characters.",
        ));
    }
    if request.timeout_seconds == 0 || request.timeout_seconds > MAX_TIMEOUT_SECONDS {
        return Err(RuntimeError::new(
            "INVALID_TIMEOUT",
            "The runtime timeout must be between 1 and 86,400 seconds.",
        ));
    }
    if request
        .test_command
        .as_ref()
        .is_some_and(|value| value.len() > 2_048 || value.chars().any(char::is_control))
    {
        return Err(RuntimeError::new(
            "INVALID_TEST_COMMAND",
            "The optional test command must be 2,048 characters or fewer without control characters.",
        ));
    }
    if profile.kind == ProviderKind::OpenRouter {
        if !request.allow_paid {
            return Ok(outcome(
                RuntimeState::PaidBudgetUnenforceable,
                profile,
                model_id,
                None,
                0,
                "Paid OpenRouter dispatch requires explicit authorization and a durable budget reservation.",
            ));
        }
        let Some(paid) = paid else {
            return Ok(outcome(
                RuntimeState::PaidBudgetUnenforceable,
                profile,
                model_id,
                None,
                0,
                "Paid OpenRouter dispatch requires the supervisor's durable budget context.",
            ));
        };
        return run_openrouter_paid(
            profile,
            &model_id,
            &cwd,
            prompt,
            request,
            cancel,
            sink,
            paid,
            build_output,
        );
    }
    if request.read_only && profile.kind != ProviderKind::Codex {
        return Err(RuntimeError::new(
            "READ_ONLY_RUNTIME_UNSUPPORTED",
            "This configured runtime does not expose a verified read-only workspace mode.",
        ));
    }
    if profile.kind == ProviderKind::Codex {
        if codex_output_schema.is_some() && !request.read_only {
            return Err(RuntimeError::new(
                "INVALID_OUTPUT_SCHEMA_SCOPE",
                "Structured output is restricted to read-only reviewer turns.",
            ));
        }
        return run_codex(
            profile,
            &model_id,
            &cwd,
            prompt,
            request.timeout_seconds,
            request.reasoning_effort.as_deref(),
            request.read_only,
            codex_output_schema,
            cancel,
            sink,
            build_output,
        );
    }
    let (arguments, mut environment, remove_environment) =
        launch_configuration(profile, &model_id, prompt, request)?;
    if let Some(build_output) = build_output {
        environment.push((
            OsString::from("CARGO_TARGET_DIR"),
            build_output.as_os_str().to_owned(),
        ));
    }
    let executable = Path::new(&profile.executable);
    let started = Instant::now();
    let mut diagnostic_tail = String::new();
    let secrets = sensitive_environment_values();
    let sink = std::cell::RefCell::new(sink);
    let result = process::run_streamed(
        executable,
        &arguments,
        &cwd,
        &environment,
        &remove_environment,
        Duration::from_secs(request.timeout_seconds),
        MAX_OUTPUT_BYTES,
        cancel,
        |identity| {
            let control = sink.borrow_mut().on_event(RuntimeEvent {
                kind: RuntimeEventKind::ProcessStarted,
                stream: None,
                text: "Provider process started.".into(),
                process: Some(identity),
            });
            if control == RuntimeControl::Cancel {
                cancel.store(true, Ordering::Release);
            }
            match control {
                RuntimeControl::Continue | RuntimeControl::Respond(_) => ProcessControl::Continue,
                RuntimeControl::Cancel => ProcessControl::Cancel,
            }
        },
        |output: ProcessOutput| {
            let raw = String::from_utf8_lossy(&output.bytes);
            let text = redact(&raw, &secrets);
            if output.stream == OutputStream::Stderr {
                diagnostic_tail.push_str(&text);
                if diagnostic_tail.len() > 16_384 {
                    let excess = diagnostic_tail.len() - 16_384;
                    let boundary = diagnostic_tail
                        .char_indices()
                        .find(|(index, _)| *index >= excess)
                        .map(|(index, _)| index)
                        .unwrap_or(0);
                    diagnostic_tail.drain(..boundary);
                }
            }
            let control = sink.borrow_mut().on_event(RuntimeEvent {
                kind: RuntimeEventKind::Output,
                stream: Some(
                    match output.stream {
                        OutputStream::Stdout => "stdout",
                        OutputStream::Stderr => "stderr",
                    }
                    .into(),
                ),
                text: if output.truncated {
                    format!("{text}\n[output chunk truncated]")
                } else {
                    text
                },
                process: None,
            });
            match control {
                RuntimeControl::Continue | RuntimeControl::Respond(_) => ProcessControl::Continue,
                RuntimeControl::Cancel => ProcessControl::Cancel,
            }
        },
    )
    .map_err(|_| {
        RuntimeError::new(
            "PROCESS_START_FAILED",
            "Could not start the configured provider executable.",
        )
    })?;
    let elapsed = started.elapsed().as_millis().min(u128::from(u64::MAX)) as u64;
    let code = result.status.and_then(|status| status.code());
    let state = if result.cancelled || cancel.load(Ordering::Acquire) {
        RuntimeState::Cancelled
    } else if result.timed_out {
        RuntimeState::TimedOut
    } else if result.output_incomplete {
        RuntimeState::Failed
    } else if code.is_some_and(|value| value == 0) {
        RuntimeState::Completed
    } else {
        classify_failure(&diagnostic_tail)
    };
    let message = match state {
        RuntimeState::Completed => "Provider process completed successfully.",
        RuntimeState::Cancelled => "Provider process was cancelled.",
        RuntimeState::TimedOut => "Provider process exceeded its configured deadline.",
        RuntimeState::Failed if result.output_incomplete => {
            "Provider process output ended without confirmed pipe completion."
        }
        RuntimeState::AuthenticationRequired => "Provider authentication is required or expired.",
        RuntimeState::RateLimited => "Provider rate limit was reached.",
        RuntimeState::UnsupportedModel => {
            "The selected model is not supported by the provider runtime."
        }
        _ => "Provider process failed.",
    };
    Ok(outcome(state, profile, model_id, code, elapsed, message))
}

fn run_openrouter_paid(
    profile: &ProviderProfile,
    model_id: &str,
    cwd: &Path,
    prompt: &str,
    request: &RuntimeRequest,
    cancel: &AtomicBool,
    sink: &mut impl RuntimeEventSink,
    paid: &PaidDispatchContext<'_>,
    build_output: Option<&Path>,
) -> Result<RuntimeOutcome, RuntimeError> {
    let runtime_started = Instant::now();
    let requested_timeout = Duration::from_secs(request.timeout_seconds);
    let remaining = || requested_timeout.saturating_sub(runtime_started.elapsed());
    let require_time = || {
        if cancel.load(Ordering::Acquire) {
            Err(RuntimeError::new(
                "CANCELLED",
                "The paid run was cancelled before provider dispatch.",
            ))
        } else if remaining().is_zero() {
            Err(RuntimeError::new(
                "RUNTIME_DEADLINE_EXCEEDED",
                "The paid run deadline expired before provider dispatch.",
            ))
        } else {
            Ok(remaining())
        }
    };
    if request.read_only {
        return Err(RuntimeError::new(
            "READ_ONLY_RUNTIME_UNSUPPORTED",
            "The paid Aider runtime does not provide a verified read-only workspace mode.",
        ));
    }
    let policy = paid.policy;
    if crate::economy::is_router_model_id(model_id)
        || (matches!(&policy.mode, crate::paid_gateway::GatewayMode::Paid)
            && crate::economy::is_free_variant_id(model_id))
    {
        return Err(RuntimeError::new(
            "ECONOMY_ADMISSION_REQUIRED",
            "Router identities and free model variants require a separately admitted Economy route.",
        ));
    }
    if policy.provider_id != profile.id
        || policy.model_id != model_id
        || policy.runtime_id != "aider"
        || policy.pricing.request_fee_micros != 0
        || (policy.max_cost_micros == 0
            && !matches!(
                &policy.mode,
                crate::paid_gateway::GatewayMode::SyntheticTrial { .. }
                    | crate::paid_gateway::GatewayMode::Economy { .. }
            ))
    {
        return Err(RuntimeError::new(
            "PAID_POLICY_MISMATCH",
            "The paid model policy does not match the selected provider, model, runtime, or zero-fee requirement.",
        ));
    }
    if matches!(
        &policy.mode,
        crate::paid_gateway::GatewayMode::Economy { .. }
    ) {
        let Some(admission) = paid.economy_admission else {
            return Err(RuntimeError::new(
                "ECONOMY_ADMISSION_REQUIRED",
                "A backend verified Economy admission is required before runtime setup.",
            ));
        };
        let current_time_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis()
            .min(u64::MAX as u128) as u64;
        if admission
            .validate_pinned_policy(&profile.id, policy, current_time_ms)
            .is_err()
            || admission.model_id() != model_id
            || request.timeout_seconds > admission.max_seconds()
        {
            return Err(RuntimeError::new(
                "ECONOMY_ADMISSION_INVALID",
                "The backend Economy proof or its task time ceiling no longer matches the selected route.",
            ));
        }
    } else if paid.economy_admission.is_some() {
        return Err(RuntimeError::new(
            "ECONOMY_ADMISSION_INVALID",
            "An Economy proof cannot be used by a Paid or synthetic trial policy.",
        ));
    }
    match &policy.mode {
        crate::paid_gateway::GatewayMode::SyntheticTrial { allow_training, .. } => {
            if !paid.synthetic_trial_consent
                || !*allow_training
                || paid.economy_trial_task_id.is_none()
                || paid.synthetic_trial_fixture.is_none_or(|fixture| {
                    !fixture.matches_trial_pair(
                        model_id,
                        paid.economy_trial_task_id.unwrap_or_default(),
                    ) || fixture.root != cwd
                })
            {
                return Err(RuntimeError::new(
                    "ECONOMY_TRIAL_CONSENT_REQUIRED",
                    "Synthetic trial mode requires an exact fixture task and explicit trial consent.",
                ));
            }
        }
        _ if paid.synthetic_trial_consent
            || paid.economy_trial_task_id.is_some()
            || paid.synthetic_trial_fixture.is_some() =>
        {
            return Err(RuntimeError::new(
                "ECONOMY_TRIAL_MODE_MISMATCH",
                "Synthetic trial context cannot be used with Paid or Economy mode.",
            ));
        }
        _ => {}
    }
    // Aider 0.86.2 automatically reads configuration and dotenv files from its
    // home, git root, and current directory. Reject those sources before even
    // making the read-only pricing request.
    check_aider_autoload_files(cwd)?;
    let source_files = resolve_paid_aider_source_files(cwd, paid.allowed_files)?;
    if let Some(task_id) = paid.economy_trial_task_id {
        let verified = crate::economy::verify_trial_fixture(task_id, cwd).map_err(|_| {
            RuntimeError::new(
                "ECONOMY_FIXTURE_CHANGED",
                "The synthetic trial workspace changed or contains unapproved content.",
            )
        })?;
        if paid.synthetic_trial_fixture.is_none_or(|proof| {
            proof.seed_inventory_fingerprint != verified.seed_inventory_fingerprint
                || proof.fixture_set_sha256 != verified.fixture_set_sha256
                || !proof.matches_trial_pair(model_id, task_id)
        }) {
            return Err(RuntimeError::new(
                "ECONOMY_FIXTURE_CHANGED",
                "Synthetic trial fixture proof is missing or no longer matches the approved seed.",
            ));
        }
        let expected = verified
            .task
            .seed_files
            .iter()
            .map(|file| file.path.as_str())
            .collect::<std::collections::BTreeSet<_>>();
        let observed = source_files
            .iter()
            .map(|file| {
                Path::new(file)
                    .strip_prefix(cwd)
                    .ok()
                    .map(|path| path.to_string_lossy().replace('\\', "/"))
            })
            .collect::<Option<std::collections::BTreeSet<_>>>();
        if observed.as_ref().is_none_or(|files| {
            files
                .iter()
                .map(String::as_str)
                .collect::<std::collections::BTreeSet<_>>()
                != expected
        }) {
            return Err(RuntimeError::new(
                "ECONOMY_FIXTURE_CHANGED",
                "The synthetic trial source allowlist does not match the approved seed.",
            ));
        }
    }
    let files = prepare_paid_aider_files(prompt, policy, &source_files)?;
    let aider_executable = resolve_aider_executable(&profile.executable)?;
    let python = resolve_aider_python(&aider_executable)?;
    let version_timeout = aider_version_probe_timeout(require_time()?);
    verify_aider_version(&aider_executable, &files, version_timeout, build_output)?;
    let guard_timeout = require_time()?.min(Duration::from_secs(10));
    verify_aider_guard(&python, &files, build_output, guard_timeout)?;
    let secret_ref = profile
        .secret_ref
        .as_deref()
        .filter(|reference| {
            providers::openrouter_credential_reference_matches(&profile.id, reference)
        })
        .ok_or_else(|| {
            RuntimeError::new(
                "AUTHENTICATION_REQUIRED",
                "A valid OpenRouter credential reference is required for the selected provider.",
            )
        })?;
    let api_key = crate::credentials::resolve(secret_ref)
        .filter(|key| providers::valid_openrouter_key(key))
        .ok_or_else(|| {
            RuntimeError::new(
                "AUTHENTICATION_REQUIRED",
                "The saved OpenRouter credential is unavailable or invalid.",
            )
        })?;

    // This read-only catalog request refreshes the pinned quote only after an
    // explicit run starts. The paid gateway still uses the reviewed ceiling.
    let catalog_timeout = require_time()?;
    let catalog = providers::openrouter_http_probe_with_timeout(&api_key, catalog_timeout);
    let _ = require_time()?;
    if catalog.state != "ready" {
        return Err(RuntimeError::new(
            "PAID_PRICING_UNAVAILABLE",
            "OpenRouter could not verify the selected model's current pricing before dispatch.",
        ));
    }
    let current = catalog
        .models
        .iter()
        .find(|model| model.id == policy.model_id)
        .ok_or_else(|| {
            RuntimeError::new(
                "PAID_MODEL_UNAVAILABLE",
                "The selected model is absent from the refreshed OpenRouter catalog.",
            )
        })?;
    let current_input = current
        .prompt_price
        .as_deref()
        .and_then(providers::openrouter_price_micros_per_million);
    let current_output = current
        .completion_price
        .as_deref()
        .and_then(providers::openrouter_price_micros_per_million);
    if matches!(&policy.mode, crate::paid_gateway::GatewayMode::Paid)
        && (current_input == Some(0) || current_output == Some(0))
    {
        return Err(RuntimeError::new(
            "ECONOMY_ADMISSION_REQUIRED",
            "Zero-priced models require a separately admitted Economy route.",
        ));
    }
    if current
        .context_length
        .is_none_or(|tokens| tokens < policy.model_context_window_tokens)
        || current_input.is_none_or(|price| price > policy.pricing.input_price_micros_per_1m)
        || current_output.is_none_or(|price| price > policy.pricing.output_price_micros_per_1m)
        || !current
            .input_modalities
            .as_ref()
            .is_some_and(|values| values.iter().any(|value| value == "text"))
        || !current
            .output_modalities
            .as_ref()
            .is_some_and(|values| values.iter().any(|value| value == "text"))
    {
        return Err(RuntimeError::new(
            "PAID_PRICING_CHANGED",
            "The refreshed model context or price exceeds the reviewed paid-run policy.",
        ));
    }
    if let Some(pin) = policy.endpoint_pin.as_ref() {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis()
            .min(u128::from(u64::MAX)) as u64;
        if !crate::economy::reviewed_paid_endpoint_policy_matches(
            model_id,
            &pin.canonical_model_id,
            &pin.endpoint_id,
            now,
        ) {
            return Err(RuntimeError::new(
                "PAID_ENDPOINT_POLICY_UNAVAILABLE",
                "The pinned endpoint has no fresh backend-reviewed privacy policy.",
            ));
        }
        let endpoint_timeout = require_time()?;
        if crate::providers::openrouter_pinned_paid_endpoint_probe_with_timeout(
            &api_key,
            crate::providers::PinnedPaidEndpointRequirements {
                model_id,
                canonical_model_id: &pin.canonical_model_id,
                endpoint_id: &pin.endpoint_id,
                input_price_ceiling_micros_per_1m: policy.pricing.input_price_micros_per_1m,
                output_price_ceiling_micros_per_1m: policy.pricing.output_price_micros_per_1m,
                input_tokens: policy.model_context_window_tokens,
                output_tokens: policy.max_output_tokens_per_call,
                requires_tools: pin.requires_tools,
            },
            endpoint_timeout,
        )
        .is_err()
        {
            return Err(RuntimeError::new(
                "PAID_ENDPOINT_CHANGED",
                "The authenticated pinned endpoint, price, capability, context, status, or ZDR evidence changed before runtime start.",
            ));
        }
    }
    let parent = paid.budgets.get(paid.parent_reservation_id).map_err(|_| {
        RuntimeError::new(
            "PAID_RESERVATION_INVALID",
            "The durable parent budget reservation could not be read.",
        )
    })?;
    let minimum_tokens = policy
        .model_context_window_tokens
        .checked_add(policy.max_output_tokens_per_call)
        .ok_or_else(|| {
            RuntimeError::new("PAID_POLICY_INVALID", "The paid token ceiling overflowed.")
        })?;
    if parent.state != crate::budgets::ReservationState::Reserved
        || parent.provider_id != policy.provider_id
        || parent.model_id != policy.model_id
        || parent.runtime_id != policy.runtime_id
        || parent.max_tokens < minimum_tokens
        || parent.max_cost_micros != policy.max_cost_micros
        || parent.pricing_source.as_deref() != Some(policy.pricing.source.as_str())
        || parent.pricing_verified_at_ms != Some(policy.pricing.observed_at_ms)
    {
        return Err(RuntimeError::new(
            "PAID_RESERVATION_MISMATCH",
            "The durable parent reservation does not match the reviewed paid-run policy.",
        ));
    }

    let aider_arguments = providers::aider_paid_gateway_arguments(
        model_id,
        &files.prompt_path,
        &files.metadata_path,
        &files.model_settings_path,
        &files.config_path,
        &files.chat_history_path,
        &files.input_history_path,
        &files.env_path,
        request.test_command.as_deref(),
        &source_files,
    );
    let (guard_executable, arguments) = guarded_aider_invocation(
        &python,
        Path::new(&files.guard_path),
        cwd,
        Path::new(&files.allowlist_path),
        &aider_arguments,
    )?;
    let mut gateway = PaidGateway::bind(
        PaidGatewayConfig {
            upstream_api_key: api_key.clone(),
            parent_reservation_id: paid.parent_reservation_id.into(),
            policy: policy.clone(),
            authenticated_canonical_slug: paid.authenticated_canonical_slug.map(str::to_owned),
            economy_admission: paid.economy_admission.cloned(),
            synthetic_trial_consent: paid.synthetic_trial_consent,
            synthetic_trial_fixture: paid.synthetic_trial_fixture.cloned(),
            request_timeout: require_time()?.min(Duration::from_secs(300)),
            dispatch_deadline: runtime_started + requested_timeout,
            cancel: paid.cancel.clone(),
        },
        paid.budgets.clone(),
    )
    .map_err(|_| {
        RuntimeError::new(
            "PAID_GATEWAY_START_FAILED",
            "The capped local OpenRouter gateway could not be started.",
        )
    })?;
    let child_token = gateway.child_token().to_owned();
    let remove_environment = ["*"];
    let environment = isolated_aider_environment_with_build_output(
        &files,
        &gateway.base_url(),
        &child_token,
        build_output,
    )
    .into_iter()
    .map(|(name, value)| (OsString::from(name), OsString::from(value)))
    .collect::<Vec<_>>();
    let mut diagnostic_tail = String::new();
    let mut secrets = sensitive_environment_values();
    secrets.push(api_key.clone());
    secrets.push(child_token);
    let sink = std::cell::RefCell::new(sink);
    let mut gateway_error = false;
    let mut user_cancelled = false;
    let process_cancel = AtomicBool::new(cancel.load(Ordering::Acquire));
    let monitor_done = AtomicBool::new(false);
    let policy_stop = gateway.policy_stop_signal();
    let process_timeout = require_time()?;
    let result = thread::scope(|scope| {
        scope.spawn(|| {
            while !monitor_done.load(Ordering::Acquire) {
                if cancel.load(Ordering::Acquire) || policy_stop.load(Ordering::Acquire) {
                    process_cancel.store(true, Ordering::Release);
                    return;
                }
                thread::sleep(Duration::from_millis(10));
            }
        });
        let result = process::run_streamed(
            &guard_executable,
            &arguments,
            cwd,
            &environment,
            &remove_environment,
            process_timeout,
            MAX_OUTPUT_BYTES,
            &process_cancel,
            |identity| {
                if let Some(task_id) = paid.economy_trial_task_id {
                    if crate::economy::verify_trial_fixture(task_id, cwd).is_err() {
                        gateway_error = true;
                        cancel.store(true, Ordering::Release);
                        process_cancel.store(true, Ordering::Release);
                        return ProcessControl::Cancel;
                    }
                }
                let control = sink.borrow_mut().on_event(RuntimeEvent {
                    kind: RuntimeEventKind::ProcessStarted,
                    stream: None,
                    text: if matches!(
                        &policy.mode,
                        crate::paid_gateway::GatewayMode::Economy { .. }
                            | crate::paid_gateway::GatewayMode::SyntheticTrial { .. }
                    ) {
                        "OpenRouter provider process started behind the capped local gateway."
                            .into()
                    } else {
                        "Paid provider process started behind the capped local gateway.".into()
                    },
                    process: Some(identity),
                });
                if matches!(
                    control,
                    RuntimeControl::Continue | RuntimeControl::Respond(_)
                ) {
                    if require_time().is_err() {
                        cancel.store(true, Ordering::Release);
                        process_cancel.store(true, Ordering::Release);
                        return ProcessControl::Cancel;
                    }
                    if gateway.authorize_parent_dispatch().is_err() {
                        gateway_error = true;
                        cancel.store(true, Ordering::Release);
                        process_cancel.store(true, Ordering::Release);
                        return ProcessControl::Cancel;
                    }
                    ProcessControl::Continue
                } else {
                    cancel.store(true, Ordering::Release);
                    process_cancel.store(true, Ordering::Release);
                    ProcessControl::Cancel
                }
            },
            |output| {
                let raw = String::from_utf8_lossy(&output.bytes);
                let text = redact(&raw, &secrets);
                if output.stream == OutputStream::Stderr {
                    diagnostic_tail.push_str(&text);
                    if diagnostic_tail.len() > 16_384 {
                        let excess = diagnostic_tail.len() - 16_384;
                        let boundary = diagnostic_tail
                            .char_indices()
                            .find(|(index, _)| *index >= excess)
                            .map(|(index, _)| index)
                            .unwrap_or(0);
                        diagnostic_tail.drain(..boundary);
                    }
                }
                let control = sink.borrow_mut().on_event(RuntimeEvent {
                    kind: RuntimeEventKind::Output,
                    stream: Some(
                        match output.stream {
                            OutputStream::Stdout => "stdout",
                            OutputStream::Stderr => "stderr",
                        }
                        .into(),
                    ),
                    text: if output.truncated {
                        format!("{text}\n[output chunk truncated]")
                    } else {
                        text
                    },
                    process: None,
                });
                if control == RuntimeControl::Cancel {
                    user_cancelled = true;
                    ProcessControl::Cancel
                } else if gateway.policy_refusal().is_some() {
                    // Fast path when a child emits output; the monitor also
                    // stops silent retry loops from the dedicated policy flag.
                    ProcessControl::Cancel
                } else {
                    ProcessControl::Continue
                }
            },
        );
        monitor_done.store(true, Ordering::Release);
        result
    })
    .map_err(|_| {
        RuntimeError::new(
            "PROCESS_START_FAILED",
            "Could not start the configured Aider executable.",
        )
    });
    gateway.stop();
    let result = result?;
    if gateway_error {
        return Err(RuntimeError::new(
            "PAID_DISPATCH_GATE_FAILED",
            "The durable parent reservation was not confirmed before the paid gateway opened.",
        ));
    }
    let elapsed = runtime_started
        .elapsed()
        .as_millis()
        .min(u128::from(u64::MAX)) as u64;
    let code = result.status.and_then(|status| status.code());
    let policy_refusal = gateway.policy_refusal();
    let state = classify_paid_runtime_state(
        result.cancelled,
        cancel.load(Ordering::Acquire) || user_cancelled,
        result.timed_out,
        result.output_incomplete,
        code,
        policy_refusal,
        &diagnostic_tail,
    );
    let zero_cost_policy = matches!(
        &policy.mode,
        crate::paid_gateway::GatewayMode::Economy { .. }
            | crate::paid_gateway::GatewayMode::SyntheticTrial { .. }
    );
    let message = if let Some(refusal) = policy_refusal {
        refusal.message()
    } else if zero_cost_policy {
        match state {
            RuntimeState::Completed => "OpenRouter provider process completed successfully.",
            RuntimeState::Cancelled => "OpenRouter provider process was cancelled.",
            RuntimeState::TimedOut => {
                "OpenRouter provider process exceeded its configured deadline."
            }
            RuntimeState::NeedsInput => {
                "OpenRouter provider run needs reconciliation before continuing."
            }
            RuntimeState::Failed if result.output_incomplete => {
                "OpenRouter provider output ended without confirmed pipe completion."
            }
            RuntimeState::AuthenticationRequired => {
                "OpenRouter authentication is required or expired."
            }
            RuntimeState::RateLimited => "OpenRouter rate limit was reached.",
            RuntimeState::UnsupportedModel => {
                "The selected OpenRouter model is not supported by Aider."
            }
            _ => "OpenRouter provider process failed.",
        }
    } else {
        match state {
            RuntimeState::Completed => "Paid provider process completed successfully.",
            RuntimeState::Cancelled => "Paid provider process was cancelled.",
            RuntimeState::TimedOut => "Paid provider process exceeded its configured deadline.",
            RuntimeState::NeedsInput => "Paid provider run needs reconciliation before continuing.",
            RuntimeState::Failed if result.output_incomplete => {
                "Paid provider output ended without confirmed pipe completion."
            }
            RuntimeState::AuthenticationRequired => {
                "OpenRouter authentication is required or expired."
            }
            RuntimeState::RateLimited => "OpenRouter rate limit was reached.",
            RuntimeState::UnsupportedModel => {
                "The selected OpenRouter model is not supported by Aider."
            }
            _ => "Paid provider process failed.",
        }
    };
    let mut result = outcome(state, profile, model_id.into(), code, elapsed, message);
    result.usage = match gateway.usage_totals() {
        Some((tokens, _cost_micros)) if zero_cost_policy => RuntimeUsage {
            input_tokens: None,
            output_tokens: None,
            total_tokens: Some(tokens),
            cost_usd: None,
            source: Some("openrouter-free-ledger-token-usage".into()),
            confidence: Some("provider-reported-token-usage".into()),
            billing: BillingStatus::ZeroCostPolicy,
        },
        Some((tokens, cost_micros)) => RuntimeUsage {
            input_tokens: None,
            output_tokens: None,
            total_tokens: Some(tokens),
            cost_usd: Some(cost_micros as f64 / 1_000_000.0),
            source: Some("openrouter-paid-call-ledger".into()),
            confidence: Some("provider-reported".into()),
            billing: BillingStatus::ProviderReported,
        },
        None => RuntimeUsage::default(),
    };
    Ok(result)
}

fn classify_paid_runtime_state(
    process_cancelled: bool,
    user_cancelled: bool,
    timed_out: bool,
    output_incomplete: bool,
    exit_code: Option<i32>,
    policy_refusal: Option<PaidPolicyRefusal>,
    diagnostic_tail: &str,
) -> RuntimeState {
    if user_cancelled {
        RuntimeState::Cancelled
    } else if policy_refusal.is_some() {
        RuntimeState::NeedsInput
    } else if process_cancelled {
        RuntimeState::Cancelled
    } else if timed_out {
        RuntimeState::TimedOut
    } else if output_incomplete {
        RuntimeState::Failed
    } else if exit_code == Some(0) {
        RuntimeState::Completed
    } else {
        classify_failure(diagnostic_tail)
    }
}

struct PaidAiderFiles {
    _directory: tempfile::TempDir,
    profile_path: String,
    appdata_path: String,
    local_appdata_path: String,
    prompt_path: String,
    metadata_path: String,
    model_settings_path: String,
    config_path: String,
    chat_history_path: String,
    input_history_path: String,
    env_path: String,
    guard_path: String,
    allowlist_path: String,
}

fn resolve_paid_aider_source_files(
    worktree: &Path,
    allowed_files: &[String],
) -> Result<Vec<String>, RuntimeError> {
    if allowed_files.is_empty() || allowed_files.len() > crate::jobs::MAX_TASK_FILES {
        return Err(RuntimeError::new(
            "PAID_ALLOWED_FILES_INVALID",
            "The paid task must have a bounded, non-empty admitted source-file list.",
        ));
    }
    let canonical_root = worktree.canonicalize().map_err(|_| {
        RuntimeError::new(
            "PAID_WORKTREE_INVALID",
            "The paid task worktree could not be resolved safely.",
        )
    })?;
    let mut resolved = Vec::with_capacity(allowed_files.len());
    for relative in allowed_files {
        let relative_path = Path::new(relative);
        if relative.trim().is_empty()
            || relative_path.is_absolute()
            || relative_path.components().any(|component| {
                matches!(
                    component,
                    std::path::Component::ParentDir | std::path::Component::Prefix(_)
                )
            })
        {
            return Err(RuntimeError::new(
                "PAID_ALLOWED_FILES_INVALID",
                "A paid task source path is not a safe relative path.",
            ));
        }
        let candidate = canonical_root.join(relative_path);
        let candidate_metadata = match std::fs::symlink_metadata(&candidate) {
            Ok(metadata) => Some(metadata),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(_) => {
                return Err(RuntimeError::new(
                    "PAID_ALLOWED_FILES_INVALID",
                    "An admitted source path could not be checked safely.",
                ));
            }
        };
        let admitted = if let Some(_metadata) = candidate_metadata {
            let canonical = candidate.canonicalize().map_err(|_| {
                RuntimeError::new(
                    "PAID_ALLOWED_FILES_INVALID",
                    "An admitted source path could not be resolved safely.",
                )
            })?;
            if !canonical.is_file() {
                return Err(RuntimeError::new(
                    "PAID_ALLOWED_FILES_INVALID",
                    "An admitted source path must refer to a file.",
                ));
            }
            canonical
        } else {
            let mut ancestor = candidate.as_path();
            loop {
                match std::fs::symlink_metadata(ancestor) {
                    Ok(_) => break,
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                        ancestor = ancestor.parent().ok_or_else(|| {
                            RuntimeError::new(
                                "PAID_ALLOWED_FILES_INVALID",
                                "A new source path has no existing worktree parent.",
                            )
                        })?;
                    }
                    Err(_) => {
                        return Err(RuntimeError::new(
                            "PAID_ALLOWED_FILES_INVALID",
                            "The parent of a new source path could not be checked safely.",
                        ));
                    }
                }
            }
            let canonical_parent = ancestor.canonicalize().map_err(|_| {
                RuntimeError::new(
                    "PAID_ALLOWED_FILES_INVALID",
                    "The parent of a new source path could not be resolved safely.",
                )
            })?;
            if !canonical_parent.starts_with(&canonical_root) {
                return Err(RuntimeError::new(
                    "PAID_ALLOWED_FILES_INVALID",
                    "An admitted source path resolves outside the task worktree.",
                ));
            }
            let remainder = candidate.strip_prefix(ancestor).map_err(|_| {
                RuntimeError::new(
                    "PAID_ALLOWED_FILES_INVALID",
                    "A new source path could not be normalized safely.",
                )
            })?;
            canonical_parent.join(remainder)
        };
        if !admitted.starts_with(&canonical_root) {
            return Err(RuntimeError::new(
                "PAID_ALLOWED_FILES_INVALID",
                "An admitted source path resolves outside the task worktree.",
            ));
        }
        resolved.push(admitted.to_string_lossy().into_owned());
    }
    Ok(resolved)
}

fn prepare_paid_aider_files(
    prompt: &str,
    policy: &PaidModelPolicy,
    source_files: &[String],
) -> Result<PaidAiderFiles, RuntimeError> {
    let directory = tempfile::tempdir().map_err(|_| {
        RuntimeError::new(
            "PAID_RUNTIME_SETUP_FAILED",
            "Temporary paid-run configuration files could not be created.",
        )
    })?;
    let profile = directory.path().join("profile");
    let appdata = profile.join("AppData").join("Roaming");
    let local_appdata = profile.join("AppData").join("Local");
    for path in [&profile, &appdata, &local_appdata] {
        std::fs::create_dir_all(path).map_err(|_| {
            RuntimeError::new(
                "PAID_RUNTIME_SETUP_FAILED",
                "Isolated Aider profile directories could not be created.",
            )
        })?;
    }
    let model = format!("openai/{}", policy.model_id);
    let metadata = serde_json::json!({(model): {
        "max_tokens": policy.model_context_window_tokens,
        "max_input_tokens": policy.model_context_window_tokens,
        "max_output_tokens": policy.max_output_tokens_per_call,
        "input_cost_per_token": policy.pricing.input_price_micros_per_1m as f64 / 1_000_000_000_000.0,
        "output_cost_per_token": policy.pricing.output_price_micros_per_1m as f64 / 1_000_000_000_000.0,
        "litellm_provider": "openai",
        "mode": "chat"
    }});
    let write = |name: &str, bytes: &[u8]| -> Result<String, RuntimeError> {
        let path = directory.path().join(name);
        std::fs::write(&path, bytes).map_err(|_| {
            RuntimeError::new(
                "PAID_RUNTIME_SETUP_FAILED",
                "Temporary paid-run configuration files could not be written.",
            )
        })?;
        Ok(path.to_string_lossy().into_owned())
    };
    Ok(PaidAiderFiles {
        profile_path: profile.to_string_lossy().into_owned(),
        appdata_path: appdata.to_string_lossy().into_owned(),
        local_appdata_path: local_appdata.to_string_lossy().into_owned(),
        prompt_path: write("prompt.md", prompt.as_bytes())?,
        metadata_path: write(
            "model-metadata.json",
            &serde_json::to_vec(&metadata).map_err(|_| {
                RuntimeError::new(
                    "PAID_RUNTIME_SETUP_FAILED",
                    "Pinned model metadata could not be encoded.",
                )
            })?,
        )?,
        model_settings_path: write("empty-model-settings.yml", b"{}\n")?,
        config_path: write("empty-config.yml", b"{}\n")?,
        chat_history_path: directory
            .path()
            .join("chat-history.md")
            .to_string_lossy()
            .into_owned(),
        input_history_path: directory
            .path()
            .join("input-history")
            .to_string_lossy()
            .into_owned(),
        env_path: write("empty.env", b"")?,
        guard_path: write(
            "paid_aider_guard.py",
            include_str!("paid_aider_guard.py").as_bytes(),
        )?,
        allowlist_path: write(
            "allowed-files.json",
            &serde_json::to_vec(source_files).map_err(|_| {
                RuntimeError::new(
                    "PAID_RUNTIME_SETUP_FAILED",
                    "The admitted source allowlist could not be encoded.",
                )
            })?,
        )?,
        _directory: directory,
    })
}

fn check_aider_autoload_files(cwd: &Path) -> Result<(), RuntimeError> {
    let home = std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(PathBuf::from);
    check_aider_autoload_files_from(cwd, home.as_deref())
}

fn check_aider_autoload_files_from(cwd: &Path, home: Option<&Path>) -> Result<(), RuntimeError> {
    const AUTOLOAD: &[&str] = &[
        ".env",
        ".aider.conf.yml",
        ".aider.model.settings.yml",
        ".aider.model.metadata.json",
    ];
    let mut roots = vec![cwd.to_path_buf()];
    let mut ancestor = cwd;
    while !ancestor.join(".git").exists() {
        let Some(parent) = ancestor.parent() else {
            break;
        };
        ancestor = parent;
    }
    if ancestor != cwd {
        roots.push(ancestor.to_path_buf());
    }
    if let Some(home) = home {
        roots.push(home.to_path_buf());
    }
    roots.sort();
    roots.dedup();
    for root in roots {
        for name in AUTOLOAD {
            match std::fs::symlink_metadata(root.join(name)) {
                Ok(_) => {
                    return Err(RuntimeError::new(
                        "AIDER_CONFIG_PRESENT",
                        "Paid Aider cannot run while an automatically loaded home or workspace configuration file is present.",
                    ));
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(_) => {
                    return Err(RuntimeError::new(
                        "AIDER_CONFIG_CHECK_FAILED",
                        "Paid Aider configuration sources could not be checked safely.",
                    ));
                }
            }
        }
    }
    Ok(())
}

fn isolated_aider_environment(
    files: &PaidAiderFiles,
    base_url: &str,
    child_token: &str,
) -> Vec<(String, String)> {
    let mut env = Vec::new();
    for name in ["PATH", "PATHEXT", "SYSTEMROOT", "WINDIR", "TEMP", "TMP"] {
        if let Some(value) = std::env::var_os(name) {
            env.push((name.into(), value.to_string_lossy().into_owned()));
        }
    }
    env.extend([
        ("USERPROFILE".into(), files.profile_path.clone()),
        ("HOME".into(), files.profile_path.clone()),
        (
            "HOMEDRIVE".into(),
            files.profile_path.chars().take(2).collect(),
        ),
        (
            "HOMEPATH".into(),
            files.profile_path.chars().skip(2).collect(),
        ),
        ("APPDATA".into(), files.appdata_path.clone()),
        ("LOCALAPPDATA".into(), files.local_appdata_path.clone()),
        ("XDG_CONFIG_HOME".into(), files.profile_path.clone()),
        ("PYTHONNOUSERSITE".into(), "1".into()),
        ("PYTHONIOENCODING".into(), "utf-8".into()),
        ("PYTHONUTF8".into(), "1".into()),
        ("LITELLM_LOCAL_MODEL_COST_MAP".into(), "True".into()),
        ("OPENAI_API_BASE".into(), base_url.into()),
        ("OPENAI_API_KEY".into(), child_token.into()),
        ("NO_PROXY".into(), "*".into()),
        ("no_proxy".into(), "*".into()),
    ]);
    env
}

fn isolated_aider_environment_with_build_output(
    files: &PaidAiderFiles,
    base_url: &str,
    child_token: &str,
    build_output: Option<&Path>,
) -> Vec<(String, String)> {
    let mut env = isolated_aider_environment(files, base_url, child_token);
    if let Some(build_output) = build_output {
        env.push((
            "CARGO_TARGET_DIR".into(),
            build_output.to_string_lossy().into_owned(),
        ));
    }
    env
}

fn resolve_aider_executable(executable: &str) -> Result<PathBuf, RuntimeError> {
    let configured = Path::new(executable);
    let direct = configured.is_absolute() || configured.components().count() > 1;
    let found = if direct {
        configured.canonicalize().ok()
    } else {
        let path = std::env::var_os("PATH").ok_or_else(|| {
            RuntimeError::new(
                "AIDER_PYTHON_UNAVAILABLE",
                "The configured Aider executable could not be resolved safely.",
            )
        })?;
        let name = configured.file_name().ok_or_else(|| {
            RuntimeError::new(
                "AIDER_PYTHON_UNAVAILABLE",
                "The configured Aider executable name is invalid.",
            )
        })?;
        let mut suffixes = vec![String::new()];
        #[cfg(windows)]
        if configured.extension().is_none() {
            suffixes.extend([".COM".into(), ".EXE".into(), ".BAT".into(), ".CMD".into()]);
        }
        std::env::split_paths(&path)
            .flat_map(|directory| {
                suffixes.iter().map(move |suffix| {
                    directory.join(format!("{}{}", name.to_string_lossy(), suffix))
                })
            })
            .find(|candidate| candidate.is_file())
            .and_then(|candidate| candidate.canonicalize().ok())
    };
    found.ok_or_else(|| {
        RuntimeError::new(
            "AIDER_PYTHON_UNAVAILABLE",
            "The configured Aider executable could not be resolved safely from its path or PATH.",
        )
    })
}

fn resolve_aider_python(aider: &Path) -> Result<PathBuf, RuntimeError> {
    let aider = aider.canonicalize().map_err(|_| {
        RuntimeError::new(
            "AIDER_PYTHON_UNAVAILABLE",
            "The configured Aider executable could not be resolved safely.",
        )
    })?;
    let script_dir = aider.parent().ok_or_else(|| {
        RuntimeError::new(
            "AIDER_PYTHON_UNAVAILABLE",
            "The configured Aider executable has no parent directory.",
        )
    })?;
    #[cfg(windows)]
    let names: &[&str] = &["python.exe"];
    #[cfg(not(windows))]
    let names: &[&str] = &["python3", "python"];
    // Preserve the venv-local executable spelling. On POSIX, venv Python is
    // commonly a symlink to the base interpreter; resolving it before exec
    // prevents Python from discovering the adjacent pyvenv.cfg.
    let python = names.iter().map(|name| script_dir.join(name)).find(|path| {
        path.is_file()
            && path.parent() == Some(script_dir)
            && path.canonicalize().is_ok_and(|target| target.is_file())
    });
    python.ok_or_else(|| {
        RuntimeError::new(
            "AIDER_PYTHON_UNAVAILABLE",
            "The configured Aider environment has no adjacent Python interpreter.",
        )
    })
}

pub(crate) fn guarded_aider_invocation(
    python: &Path,
    guard: &Path,
    worktree: &Path,
    allowlist: &Path,
    aider_args: &[String],
) -> Result<(PathBuf, Vec<String>), RuntimeError> {
    if !python.is_absolute()
        || !python.is_file()
        || !python.canonicalize().is_ok_and(|target| target.is_file())
    {
        return Err(RuntimeError::new(
            "AIDER_GUARD_SETUP_FAILED",
            "The Aider guard interpreter could not be resolved.",
        ));
    }
    let guard = guard.canonicalize().map_err(|_| {
        RuntimeError::new(
            "AIDER_GUARD_SETUP_FAILED",
            "The embedded Aider guard could not be resolved.",
        )
    })?;
    let worktree = worktree.canonicalize().map_err(|_| {
        RuntimeError::new(
            "AIDER_GUARD_SETUP_FAILED",
            "The admitted worktree could not be resolved.",
        )
    })?;
    let allowlist = allowlist.canonicalize().map_err(|_| {
        RuntimeError::new(
            "AIDER_GUARD_SETUP_FAILED",
            "The runner source allowlist could not be resolved.",
        )
    })?;
    if !python.is_file() || !guard.is_file() || !allowlist.is_file() || !worktree.is_dir() {
        return Err(RuntimeError::new(
            "AIDER_GUARD_SETUP_FAILED",
            "The guarded Aider invocation inputs are not regular files and a directory.",
        ));
    }
    let mut args = vec![
        "-I".to_owned(),
        guard.to_string_lossy().into_owned(),
        "--worktree".to_owned(),
        worktree.to_string_lossy().into_owned(),
        "--allowed-files-json".to_owned(),
        allowlist.to_string_lossy().into_owned(),
        "--".to_owned(),
    ];
    args.extend(aider_args.iter().cloned());
    Ok((python.to_path_buf(), args))
}

fn verify_aider_guard(
    python: &Path,
    files: &PaidAiderFiles,
    build_output: Option<&Path>,
    timeout: Duration,
) -> Result<(), RuntimeError> {
    if !python.is_absolute()
        || !python.is_file()
        || !python.canonicalize().is_ok_and(|target| target.is_file())
    {
        return Err(RuntimeError::new(
            "AIDER_GUARD_PROBE_FAILED",
            "The Aider guard interpreter could not be verified.",
        ));
    }
    let expected_prefix = python
        .parent()
        .and_then(Path::parent)
        .ok_or_else(|| {
            RuntimeError::new(
                "AIDER_GUARD_PROBE_FAILED",
                "The Aider Python environment prefix could not be verified.",
            )
        })?
        .canonicalize()
        .map_err(|_| {
            RuntimeError::new(
                "AIDER_GUARD_PROBE_FAILED",
                "The Aider Python environment prefix could not be verified.",
            )
        })?;
    let env = isolated_aider_environment_with_build_output(
        files,
        "http://127.0.0.1:1/v1",
        "fabrica-dummy",
        build_output,
    )
    .into_iter()
    .map(|(name, value)| (OsString::from(name), OsString::from(value)))
    .collect::<Vec<_>>();
    let output = process::capture_with_environment(
        &python,
        &["-I".into(), files.guard_path.clone(), "--probe".into()],
        Path::new(&files.profile_path),
        timeout.min(Duration::from_secs(10)),
        16 * 1024,
        Some(&env),
    )
    .map_err(|_| {
        RuntimeError::new(
            "AIDER_GUARD_PROBE_FAILED",
            "The Aider guard ownership probe could not be completed.",
        )
    })?;
    if output.timed_out
        || output.truncated
        || output.status.and_then(|status| status.code()) != Some(0)
    {
        return Err(RuntimeError::new(
            "AIDER_GUARD_PROBE_FAILED",
            "The Aider guard ownership probe was not successful.",
        ));
    }
    let facts: Value = serde_json::from_slice(&output.stdout).map_err(|_| {
        RuntimeError::new(
            "AIDER_GUARD_PROBE_FAILED",
            "The Aider guard returned invalid probe data.",
        )
    })?;
    let string = |key: &str| facts.get(key).and_then(Value::as_str);
    let prefix = string("prefix")
        .map(Path::new)
        .and_then(|p| p.canonicalize().ok());
    // Compare the lexical venv interpreter path: canonicalizing it would
    // collapse POSIX venv symlinks to the base interpreter and lose the venv.
    let interpreter = string("interpreter").map(Path::new);
    let package = string("packagePath")
        .map(Path::new)
        .and_then(|p| p.canonicalize().ok());
    let base_prefix = string("basePrefix")
        .map(Path::new)
        .and_then(|p| p.canonicalize().ok());
    if facts.get("ok").and_then(Value::as_bool) != Some(true)
        || string("distribution") != Some("aider-chat")
        || string("expectedVersion") != Some("0.86.2")
        || string("version") != Some("0.86.2")
        || facts.get("packageWithinPrefix").and_then(Value::as_bool) != Some(true)
        || facts
            .get("interpreterWithinPrefix")
            .and_then(Value::as_bool)
            != Some(true)
        || facts
            .get("distributionPathMatchesPackage")
            .and_then(Value::as_bool)
            != Some(true)
        || !interpreter_path_matches_probe(interpreter, python)
        || prefix.as_deref() != Some(expected_prefix.as_path())
        || package
            .as_ref()
            .is_none_or(|path| !path.starts_with(&expected_prefix))
        || base_prefix
            .as_ref()
            .is_none_or(|base| base == &expected_prefix)
    {
        return Err(RuntimeError::new(
            "AIDER_GUARD_PROBE_FAILED",
            "The installed Aider package is not owned by the adjacent venv interpreter.",
        ));
    }
    Ok(())
}

fn interpreter_path_matches_probe(reported: Option<&Path>, expected: &Path) -> bool {
    let Some(reported) = reported else {
        return false;
    };
    #[cfg(windows)]
    {
        fn windows_identity(path: &Path) -> Option<String> {
            let canonical = path.canonicalize().ok()?;
            let raw = canonical.to_string_lossy().replace('/', "\\");
            let without_namespace = if raw
                .get(..8)
                .is_some_and(|prefix| prefix.eq_ignore_ascii_case(r"\\?\UNC\"))
            {
                format!(r"\\{}", &raw[8..])
            } else if raw
                .get(..4)
                .is_some_and(|prefix| prefix.eq_ignore_ascii_case(r"\\?\"))
            {
                raw[4..].to_owned()
            } else {
                raw
            };
            Some(
                without_namespace
                    .trim_end_matches('\\')
                    .to_ascii_lowercase(),
            )
        }
        windows_identity(reported)
            .zip(windows_identity(expected))
            .is_some_and(|(a, b)| a == b)
    }
    #[cfg(not(windows))]
    {
        // Keep the venv's lexical interpreter path: resolving a POSIX venv
        // Python symlink can collapse it to the base interpreter.
        reported == expected
    }
}
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct AiderVersionProbe {
    exit_code: Option<i32>,
    timed_out: bool,
    output_matches: bool,
    output_truncated: bool,
    stdout_bytes: usize,
    stderr_bytes: usize,
    elapsed_ms: u128,
}

fn capture_aider_version_probe(
    executable: &Path,
    files: &PaidAiderFiles,
    timeout: Duration,
    build_output: Option<&Path>,
) -> Result<AiderVersionProbe, RuntimeError> {
    let safe_env = isolated_aider_environment_with_build_output(
        files,
        "http://127.0.0.1:1/v1",
        "fabrica-dummy",
        build_output,
    )
    .into_iter()
    .map(|(name, value)| (OsString::from(name), OsString::from(value)))
    .collect::<Vec<_>>();
    let started = Instant::now();
    let result = process::capture_with_environment(
        executable,
        &["--version".into()],
        Path::new(&files.profile_path),
        timeout,
        4096,
        Some(&safe_env),
    )
    .map_err(|_| {
        RuntimeError::new(
            "AIDER_VERSION_UNVERIFIED",
            "The paid Aider runtime version could not be verified.",
        )
    })?;
    Ok(AiderVersionProbe {
        exit_code: result.status.and_then(|status| status.code()),
        timed_out: result.timed_out,
        output_matches: aider_version_output_matches(&result.stdout),
        output_truncated: result.truncated,
        stdout_bytes: result.stdout.len(),
        stderr_bytes: result.stderr.len(),
        elapsed_ms: started.elapsed().as_millis(),
    })
}

fn aider_version_output_matches(output: &[u8]) -> bool {
    output == b"aider 0.86.2"
        || output == b"aider 0.86.2\n"
        || output == b"aider 0.86.2\r\n"
}

/// Isolated Windows startup can take several seconds even for `--version`.
/// Keep bounded headroom while never extending the parent run deadline.
fn aider_version_probe_timeout(remaining: Duration) -> Duration {
    remaining.min(MAX_AIDER_VERSION_PROBE_TIMEOUT)
}

fn validate_aider_version_probe(
    probe: &AiderVersionProbe,
) -> Result<(), RuntimeError> {
    if probe.timed_out {
        return Err(RuntimeError::new(
            "AIDER_VERSION_TIMEOUT",
            "The bounded Aider version probe timed out before confirming the required runtime.",
        ));
    }
    if probe.exit_code != Some(0) {
        return Err(RuntimeError::new(
            "AIDER_VERSION_EXIT_FAILED",
            "The Aider version probe exited unsuccessfully before confirming the required runtime.",
        ));
    }
    if probe.output_truncated || !probe.output_matches {
        return Err(RuntimeError::new(
            "AIDER_VERSION_UNSUPPORTED",
            "Paid OpenRouter dispatch requires exact Aider version output `aider 0.86.2`.",
        ));
    }
    Ok(())
}

fn verify_aider_version(
    executable: &Path,
    files: &PaidAiderFiles,
    timeout: Duration,
    build_output: Option<&Path>,
) -> Result<(), RuntimeError> {
    let probe = capture_aider_version_probe(executable, files, timeout, build_output)?;
    validate_aider_version_probe(&probe)
}

fn resolve_profile_and_model<'a>(
    settings: &'a Settings,
    request: &RuntimeRequest,
) -> Result<(&'a ProviderProfile, String), RuntimeError> {
    providers::validate_profiles(&settings.providers, &settings.provider_selection).map_err(
        |_| {
            RuntimeError::new(
                "INVALID_PROVIDER_SETTINGS",
                "Configured provider settings are invalid.",
            )
        },
    )?;
    let provider_id = request
        .provider_id
        .as_deref()
        .or(settings.provider_selection.provider_id.as_deref())
        .ok_or_else(|| {
            RuntimeError::new(
                "PROVIDER_NOT_SELECTED",
                "Select a configured provider before starting a run.",
            )
        })?;
    let profile = settings
        .providers
        .iter()
        .find(|profile| profile.id == provider_id)
        .ok_or_else(|| {
            RuntimeError::new(
                "PROVIDER_NOT_FOUND",
                "The selected provider profile is not configured.",
            )
        })?;
    let model_id = request
        .model_id
        .as_deref()
        .or(settings.provider_selection.model_id.as_deref())
        .ok_or_else(|| {
            RuntimeError::new(
                "MODEL_NOT_SELECTED",
                "Select a configured model before starting a run.",
            )
        })?;
    if !profile.models.iter().any(|model| model.id == model_id) {
        return Err(RuntimeError::new(
            "MODEL_NOT_FOUND",
            "The selected model is not configured for this provider.",
        ));
    }
    if request.provider_id.is_some()
        && request.model_id.is_none()
        && settings.provider_selection.provider_id.as_deref() != Some(provider_id)
    {
        return Err(RuntimeError::new(
            "MODEL_NOT_SELECTED",
            "A model must be explicitly selected when overriding the provider.",
        ));
    }
    Ok((profile, model_id.to_owned()))
}

fn launch_configuration(
    profile: &ProviderProfile,
    model_id: &str,
    prompt: &str,
    request: &RuntimeRequest,
) -> Result<
    (
        Vec<String>,
        Vec<(std::ffi::OsString, std::ffi::OsString)>,
        Vec<&'static str>,
    ),
    RuntimeError,
> {
    let empty_env_file = if cfg!(windows) { "NUL" } else { "/dev/null" };
    let args = match profile.kind {
        ProviderKind::Antigravity => {
            providers::antigravity_arguments(model_id, prompt, request.timeout_seconds)
        }
        ProviderKind::Ollama => {
            providers::validate_ollama_endpoint(
                profile
                    .endpoint
                    .as_deref()
                    .unwrap_or(providers::OLLAMA_DEFAULT_ENDPOINT),
            )
            .map_err(|_| {
                RuntimeError::new(
                    "INVALID_ENDPOINT",
                    "Ollama must use its configured numeric loopback endpoint.",
                )
            })?;
            providers::aider_ollama_arguments(
                model_id,
                prompt,
                request.test_command.as_deref(),
                empty_env_file,
            )
        }
        ProviderKind::Codex => {
            return Err(RuntimeError::new(
                "UNSUPPORTED_PROVIDER",
                "Codex uses its bidirectional app-server adapter.",
            ));
        }
        ProviderKind::OpenRouter => {
            return Err(RuntimeError::new(
                "PAID_BUDGET_UNENFORCEABLE",
                "Paid OpenRouter dispatch requires an enforceable whole-run call and token ceiling.",
            ));
        }
        ProviderKind::Custom => {
            return Err(RuntimeError::new(
                "UNSUPPORTED_PROVIDER",
                "No coding runtime is configured for this provider kind.",
            ));
        }
    };
    let mut remove = PROVIDER_BILLING_ENV.to_vec();
    if profile.kind == ProviderKind::Ollama {
        remove.extend([
            "OLLAMA_HOST",
            "OLLAMA_API_KEY",
            "HTTP_PROXY",
            "HTTPS_PROXY",
            "ALL_PROXY",
            "http_proxy",
            "https_proxy",
            "all_proxy",
        ]);
    }
    let environment = if profile.kind == ProviderKind::Ollama {
        vec![
            (
                "OLLAMA_API_BASE".into(),
                profile
                    .endpoint
                    .as_deref()
                    .unwrap_or(providers::OLLAMA_DEFAULT_ENDPOINT)
                    .into(),
            ),
            ("NO_PROXY".into(), "*".into()),
            ("no_proxy".into(), "*".into()),
        ]
    } else {
        Vec::new()
    };
    Ok((args, environment, remove))
}

fn run_codex(
    profile: &ProviderProfile,
    model_id: &str,
    cwd: &Path,
    prompt: &str,
    timeout_seconds: u64,
    reasoning_effort: Option<&str>,
    read_only: bool,
    output_schema: Option<&Value>,
    cancel: &AtomicBool,
    sink: &mut impl RuntimeEventSink,
    build_output: Option<&Path>,
) -> Result<RuntimeOutcome, RuntimeError> {
    run_codex_with_startup_timeout(
        profile,
        model_id,
        cwd,
        prompt,
        timeout_seconds,
        reasoning_effort,
        read_only,
        output_schema,
        cancel,
        sink,
        build_output,
        CODEX_STARTUP_TIMEOUT,
    )
}

#[allow(clippy::too_many_arguments)]
fn run_codex_with_startup_timeout(
    profile: &ProviderProfile,
    model_id: &str,
    cwd: &Path,
    prompt: &str,
    timeout_seconds: u64,
    reasoning_effort: Option<&str>,
    read_only: bool,
    output_schema: Option<&Value>,
    cancel: &AtomicBool,
    sink: &mut impl RuntimeEventSink,
    build_output: Option<&Path>,
    startup_timeout: Duration,
) -> Result<RuntimeOutcome, RuntimeError> {
    // Codex can also authenticate with a Platform API key stored in auth.json.
    // A $5 ChatGPT allowance cannot cap that billing path, so force the
    // documented ChatGPT-only login mode on every app-server invocation.
    let args = codex_app_server_arguments();
    let remove = PROVIDER_BILLING_ENV;
    let environment = build_output
        .map(|path| {
            vec![(
                OsString::from("CARGO_TARGET_DIR"),
                path.as_os_str().to_owned(),
            )]
        })
        .unwrap_or_default();
    let (child, identity) = process::spawn_interactive(
        Path::new(&profile.executable),
        &args,
        cwd,
        &environment,
        &remove,
    )
    .map_err(|_| {
        RuntimeError::new(
            "PROCESS_START_FAILED",
            "Could not start the configured Codex executable.",
        )
    })?;
    let mut child = OwnedProcess {
        child,
        identity,
        terminate_on_drop: true,
    };
    let started = Instant::now();
    let control = sink.on_event(RuntimeEvent {
        kind: RuntimeEventKind::ProcessStarted,
        stream: None,
        text: "Codex app-server started.".into(),
        process: Some(identity),
    });
    if control == RuntimeControl::Cancel {
        cancel.store(true, Ordering::Release);
    }
    if cancel.load(Ordering::Acquire) {
        process::terminate_child_tree(&mut child, identity);
        return Ok(outcome(
            RuntimeState::Cancelled,
            profile,
            model_id.into(),
            None,
            started.elapsed().as_millis() as u64,
            "Codex run was cancelled.",
        ));
    }

    let stdout = child.stdout.take().expect("piped stdout");
    let (sender, receiver) = mpsc::sync_channel::<Result<String, ()>>(32);
    thread::spawn(move || {
        let mut reader = BufReader::with_capacity(8 * 1024, stdout);
        loop {
            let mut bytes = Vec::new();
            match reader
                .by_ref()
                .take(1024 * 1024 + 1)
                .read_until(b'\n', &mut bytes)
            {
                Ok(0) => break,
                Ok(_) if bytes.len() <= 1024 * 1024 => {
                    let line = match String::from_utf8(bytes) {
                        Ok(line) => line,
                        Err(_) => {
                            let _ = sender.send(Err(()));
                            break;
                        }
                    };
                    if sender.send(Ok(line)).is_err() {
                        break;
                    }
                }
                Ok(_) => {
                    let _ = sender.send(Err(()));
                    break;
                }
                Err(_) => {
                    let _ = sender.send(Err(()));
                    break;
                }
            }
        }
    });
    // Drain stderr independently; diagnostics are not allowed to block protocol stdout.
    if let Some(stderr) = child.stderr.take() {
        thread::spawn(move || {
            let mut reader = BufReader::new(stderr);
            let mut buf = [0u8; 4096];
            loop {
                match std::io::Read::read(&mut reader, &mut buf) {
                    Ok(0) | Err(_) => break,
                    _ => {}
                }
            }
        });
    }
    let mut stdin = child.stdin.take().expect("piped stdin");
    let deadline = started + Duration::from_secs(timeout_seconds);
    let startup_deadline = (started + startup_timeout).min(deadline);
    let startup_timeout_code = if deadline <= startup_deadline {
        "CODEX_RUN_TIMED_OUT"
    } else {
        "CODEX_STARTUP_TIMEOUT"
    };
    let mut usage = RuntimeUsage::default();
    let mut next_id = 1u64;
    send_codex(
        &mut stdin,
        &providers::app_server_initialize(next_id, env!("CARGO_PKG_VERSION")),
    )?;
    let init_id = next_id;
    next_id += 1;
    let init = match receive_codex_response(
        &mut child,
        identity,
        &receiver,
        startup_deadline,
        startup_timeout_code,
        cancel,
        init_id,
    ) {
        Ok(response) => response,
        Err(error) if error.code == "CODEX_STARTUP_TIMEOUT" => {
            drop(stdin);
            return Ok(stop_codex_startup(
                child,
                identity,
                profile,
                model_id,
                started,
                RuntimeState::UnsupportedRuntime,
                "Codex app-server did not complete its startup handshake within 10 seconds.",
                sink,
            ));
        }
        Err(error) if error.code == "CODEX_RUN_TIMED_OUT" => {
            drop(stdin);
            return Ok(stop_codex_startup(
                child,
                identity,
                profile,
                model_id,
                started,
                RuntimeState::TimedOut,
                "Codex startup exceeded the task deadline.",
                sink,
            ));
        }
        Err(error) => return Err(error),
    };
    if init.get("error").is_some() {
        let state = codex_request_error_state(&init, RuntimeState::UnsupportedRuntime);
        let message = codex_state_message(state);
        drop(stdin);
        return Ok(stop_codex_startup(
            child, identity, profile, model_id, started, state, message, sink,
        ));
    }
    send_codex(
        &mut stdin,
        &serde_json::json!({"method":"initialized","params":{}}),
    )?;
    let thread_id_request = next_id;
    send_codex(
        &mut stdin,
        &codex_thread_start(
            thread_id_request,
            &cwd.to_string_lossy(),
            model_id,
            read_only,
        ),
    )?;
    let thread_response = match receive_codex_response(
        &mut child,
        identity,
        &receiver,
        startup_deadline,
        startup_timeout_code,
        cancel,
        thread_id_request,
    ) {
        Ok(response) => response,
        Err(error) if error.code == "CODEX_STARTUP_TIMEOUT" => {
            drop(stdin);
            return Ok(stop_codex_startup(
                child,
                identity,
                profile,
                model_id,
                started,
                RuntimeState::UnsupportedRuntime,
                "Codex app-server did not complete its startup handshake within 10 seconds.",
                sink,
            ));
        }
        Err(error) if error.code == "CODEX_RUN_TIMED_OUT" => {
            drop(stdin);
            return Ok(stop_codex_startup(
                child,
                identity,
                profile,
                model_id,
                started,
                RuntimeState::TimedOut,
                "Codex startup exceeded the task deadline.",
                sink,
            ));
        }
        Err(error) => return Err(error),
    };
    if thread_response.get("error").is_some() {
        let state = codex_request_error_state(&thread_response, RuntimeState::Failed);
        let message = codex_state_message(state);
        drop(stdin);
        return Ok(stop_codex_startup(
            child, identity, profile, model_id, started, state, message, sink,
        ));
    }
    let Some(thread_id) = thread_response
        .pointer("/result/thread/id")
        .and_then(Value::as_str)
        .map(str::to_owned)
    else {
        drop(stdin);
        return Ok(stop_codex_startup(
            child,
            identity,
            profile,
            model_id,
            started,
            RuntimeState::UnsupportedRuntime,
            "Codex app-server did not return a thread identifier.",
            sink,
        ));
    };
    next_id += 1;
    let turn_start_id = next_id;
    send_codex(
        &mut stdin,
        &providers::app_server_turn_start_with_output_schema(
            next_id,
            &thread_id,
            &cwd.to_string_lossy(),
            model_id,
            reasoning_effort,
            prompt,
            None,
            output_schema,
            read_only,
        ),
    )?;
    let mut final_state = None;
    while final_state.is_none() {
        if cancel.load(Ordering::Acquire) {
            process::terminate_child_tree(&mut child, identity);
            final_state = Some(RuntimeState::Cancelled);
            break;
        }
        let now = Instant::now();
        if now >= deadline {
            process::terminate_child_tree(&mut child, identity);
            final_state = Some(RuntimeState::TimedOut);
            break;
        }
        let line = match receiver.recv_timeout((deadline - now).min(Duration::from_millis(100))) {
            Ok(Ok(line)) => line,
            Ok(Err(())) => {
                process::terminate_child_tree(&mut child, identity);
                final_state = Some(RuntimeState::Failed);
                break;
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                if child.try_wait().ok().flatten().is_some() {
                    final_state = Some(RuntimeState::Failed);
                }
                continue;
            }
            Err(_) => {
                final_state = Some(RuntimeState::Failed);
                break;
            }
        };
        let message = match providers::parse_app_server_message(line.trim_end()) {
            Ok(value) => value,
            Err(_) => {
                final_state = Some(RuntimeState::Failed);
                continue;
            }
        };
        if message.get("method").and_then(Value::as_str) == Some("thread/tokenUsage/updated") {
            update_codex_token_usage(&mut usage, &message);
        }
        if message.get("id").and_then(Value::as_u64) == Some(turn_start_id)
            && message.get("error").is_some()
        {
            final_state = Some(codex_request_error_state(&message, RuntimeState::Failed));
            let text = codex_state_message(final_state.unwrap());
            let _ = sink.on_event(RuntimeEvent {
                kind: RuntimeEventKind::State,
                stream: None,
                text: text.into(),
                process: None,
            });
            continue;
        }
        let Some(event) = providers::normalize_app_server_message(&message) else {
            continue;
        };
        match event.kind.as_str() {
            "output" | "finalOutput" => {
                let output = RuntimeEvent {
                    kind: RuntimeEventKind::Output,
                    stream: Some("stdout".into()),
                    text: redact(&event.text, &sensitive_environment_values()),
                    process: None,
                };
                let control = if event.kind == "finalOutput" {
                    sink.on_final_output(output)
                } else {
                    sink.on_event(output)
                };
                if control == RuntimeControl::Cancel {
                    cancel.store(true, Ordering::Release);
                }
            }
            "approvalRequest" | "userInputRequest" => {
                let request_value: Value = serde_json::from_str(&event.text).unwrap_or(Value::Null);
                let id = request_value
                    .get("requestId")
                    .cloned()
                    .unwrap_or(Value::Null);
                let checkpoint_text = codex_input_request_checkpoint(
                    &request_value,
                    &thread_id,
                    &sensitive_environment_values(),
                );
                let kind = if event.kind == "approvalRequest" {
                    RuntimeEventKind::ApprovalRequest
                } else {
                    RuntimeEventKind::UserInputRequest
                };
                match sink.on_event(RuntimeEvent {
                    kind,
                    stream: None,
                    text: checkpoint_text,
                    process: None,
                }) {
                    RuntimeControl::Respond(response) => {
                        send_codex(&mut stdin, &providers::app_server_response(id, response))?
                    }
                    RuntimeControl::Cancel => {
                        cancel.store(true, Ordering::Release);
                        process::terminate_child_tree(&mut child, identity);
                        final_state = Some(RuntimeState::Cancelled);
                    }
                    RuntimeControl::Continue => {
                        process::terminate_child_tree(&mut child, identity);
                        final_state = Some(RuntimeState::NeedsInput);
                    }
                }
            }
            "providerError" => {
                let state = classify_failure(&event.text);
                let _ = sink.on_event(RuntimeEvent {
                    kind: RuntimeEventKind::State,
                    stream: None,
                    text: event.text,
                    process: None,
                });
                final_state = Some(state);
            }
            "state" => {
                let state = match event.terminal_state.as_deref() {
                    Some("completed") => RuntimeState::Completed,
                    Some("cancelled") => RuntimeState::Cancelled,
                    _ => RuntimeState::Failed,
                };
                let _ = sink.on_event(RuntimeEvent {
                    kind: RuntimeEventKind::State,
                    stream: None,
                    text: event.text,
                    process: None,
                });
                final_state = Some(state);
            }
            _ => {}
        }
    }
    if final_state == Some(RuntimeState::Completed) {
        usage.billing = BillingStatus::ProviderManagedUnknown;
    }
    drop(stdin);
    let grace_deadline = Instant::now() + Duration::from_millis(500);
    while Instant::now() < grace_deadline && child.try_wait().ok().flatten().is_none() {
        thread::sleep(Duration::from_millis(20));
    }
    if child.try_wait().ok().flatten().is_none() {
        process::terminate_child_tree(&mut child, identity);
    }
    let status = child.wait().ok();
    let elapsed = started.elapsed().as_millis().min(u128::from(u64::MAX)) as u64;
    let state = final_state.unwrap_or(RuntimeState::Failed);
    let message = codex_state_message(state);
    let _ = sink.on_event(RuntimeEvent {
        kind: RuntimeEventKind::State,
        stream: None,
        text: message.into(),
        process: None,
    });
    let mut result = outcome(
        state,
        profile,
        model_id.into(),
        status.and_then(|s| s.code()),
        elapsed,
        message,
    );
    result.usage = usage;
    Ok(result)
}

fn codex_input_request_checkpoint(request: &Value, thread_id: &str, secrets: &[String]) -> String {
    fn excerpt(value: &Value, secrets: &[String]) -> Option<String> {
        let raw = match value {
            Value::String(value) => value.clone(),
            Value::Null => return None,
            other => serde_json::to_string(other).ok()?,
        };
        let redacted = redact(&raw, secrets);
        let cleaned = redacted
            .chars()
            .map(|character| {
                if character.is_control() && !matches!(character, '\n' | '\r' | '\t') {
                    ' '
                } else {
                    character
                }
            })
            .take(700)
            .collect::<String>();
        (!cleaned.trim().is_empty()).then_some(cleaned)
    }

    let params = request.get("params").unwrap_or(&Value::Null);
    let request_id = request.get("requestId").and_then(|value| match value {
        Value::Number(_) => Some(value.clone()),
        Value::String(_) => excerpt(value, secrets).map(Value::String),
        _ => None,
    });
    let mut record = serde_json::json!({
        "state": "needs_input",
        "requestId": request_id,
        "method": request.get("method").and_then(Value::as_str).unwrap_or("unknown"),
        "threadId": thread_id
            .chars()
            .filter(|character| !character.is_control())
            .take(160)
            .collect::<String>(),
        "command": params.get("command").and_then(|value| excerpt(value, secrets)),
        "cwd": params.get("cwd").and_then(|value| excerpt(value, secrets)),
        "reason": params.get("reason").and_then(|value| excerpt(value, secrets)),
        "availableDecisions": params.get("availableDecisions").and_then(|value| excerpt(value, secrets)),
        "questions": params.get("questions").and_then(|value| excerpt(value, secrets)),
    });
    if let Ok(encoded) = serde_json::to_string(&record) {
        if encoded.len() <= 4 * 1024 {
            return encoded;
        }
    }
    for max_chars in [256, 128, 64, 32, 16, 8, 0] {
        for key in [
            "questions",
            "availableDecisions",
            "command",
            "cwd",
            "reason",
            "requestId",
            "threadId",
        ] {
            if let Some(Value::String(value)) = record.get_mut(key) {
                *value = value.chars().take(max_chars).collect();
            }
        }
        if let Ok(encoded) = serde_json::to_string(&record) {
            if encoded.len() <= 4 * 1024 {
                return encoded;
            }
        }
    }
    "{\"state\":\"needs_input\",\"method\":\"unknown\"}".into()
}

fn codex_state_message(state: RuntimeState) -> &'static str {
    match state {
        RuntimeState::Completed => "Codex turn completed.",
        RuntimeState::Failed => "Codex app-server run failed.",
        RuntimeState::AuthenticationRequired => "Codex authentication is required or expired.",
        RuntimeState::RateLimited => "Codex rate limit was reached.",
        RuntimeState::UnsupportedModel => "Codex model is unsupported.",
        RuntimeState::Cancelled => "Codex run was cancelled.",
        RuntimeState::TimedOut => "Codex exceeded its configured deadline.",
        RuntimeState::NeedsInput => "Codex requires approval or user input.",
        RuntimeState::UnsupportedRuntime => {
            "The configured Codex executable does not support the app-server protocol."
        }
        RuntimeState::PaidBudgetUnenforceable => {
            "Codex dispatch is unavailable under the current budget policy."
        }
    }
}

fn codex_request_error_state(response: &Value, fallback: RuntimeState) -> RuntimeState {
    providers::normalize_app_server_message(response)
        .filter(|event| event.kind == "providerError")
        .map(|event| classify_failure(&event.text))
        .filter(|state| *state != RuntimeState::Failed)
        .unwrap_or(fallback)
}

fn stop_codex_startup(
    mut child: OwnedProcess,
    identity: ProcessIdentity,
    profile: &ProviderProfile,
    model_id: &str,
    started: Instant,
    state: RuntimeState,
    message: &str,
    sink: &mut impl RuntimeEventSink,
) -> RuntimeOutcome {
    process::terminate_child_tree(&mut child.child, identity);
    let exit_code = child.wait().ok().and_then(|status| status.code());
    let _ = sink.on_event(RuntimeEvent {
        kind: RuntimeEventKind::State,
        stream: None,
        text: message.into(),
        process: None,
    });
    outcome(
        state,
        profile,
        model_id.into(),
        exit_code,
        started.elapsed().as_millis().min(u128::from(u64::MAX)) as u64,
        message,
    )
}

fn update_codex_token_usage(usage: &mut RuntimeUsage, message: &Value) {
    // `last` is per-request usage and can reset on every turn. Only `total` is
    // cumulative for the thread; treating `last` as job-total undercounts work
    // after multiple updates. Keep snapshots monotonic and leave absent fields
    // unknown rather than substituting zero.
    let total = message.pointer("/params/tokenUsage/total");
    let total_tokens = total
        .and_then(|value| value.get("totalTokens"))
        .and_then(Value::as_u64);
    let input_tokens = total
        .and_then(|value| value.get("inputTokens"))
        .and_then(Value::as_u64);
    let output_tokens = total
        .and_then(|value| value.get("outputTokens"))
        .and_then(Value::as_u64);
    if total_tokens.is_some() || input_tokens.is_some() || output_tokens.is_some() {
        usage.total_tokens = monotonic_usage(usage.total_tokens, total_tokens);
        usage.input_tokens = monotonic_usage(usage.input_tokens, input_tokens);
        usage.output_tokens = monotonic_usage(usage.output_tokens, output_tokens);
        usage.source = Some("codex-app-server-tokenUsage-total".into());
        usage.confidence = Some("provider-reported-cumulative".into());
        usage.billing = BillingStatus::ProviderManagedUnknown;
    }
}

fn monotonic_usage(previous: Option<u64>, observed: Option<u64>) -> Option<u64> {
    match (previous, observed) {
        (Some(previous), Some(observed)) => Some(previous.max(observed)),
        (Some(previous), None) => Some(previous),
        (None, observed) => observed,
    }
}

fn codex_app_server_arguments() -> Vec<String> {
    vec![
        "-c".into(),
        "forced_login_method=\"chatgpt\"".into(),
        "app-server".into(),
        "--stdio".into(),
    ]
}

#[cfg(test)]
mod codex_launch_tests {
    use super::{codex_app_server_arguments, codex_input_request_checkpoint};
    use serde_json::json;

    #[test]
    fn codex_app_server_forces_chatgpt_login_method() {
        assert_eq!(
            codex_app_server_arguments(),
            vec![
                "-c",
                "forced_login_method=\"chatgpt\"",
                "app-server",
                "--stdio"
            ]
        );
    }

    #[test]
    fn codex_input_request_checkpoint_is_bounded_redacted_and_thread_bound() {
        let request = json!({
            "requestId": 17,
            "method": "item/commandExecution/requestApproval",
            "params": {
                "command": format!("run --token secret-value {}", "x".repeat(3000)),
                "cwd": "C:/workspace",
                "reason": format!("Needs permission {}", "🙂".repeat(1800)),
                "questions": ["Should this be allowed?"]
            }
        });
        let checkpoint =
            codex_input_request_checkpoint(&request, "thread-local-123", &["secret-value".into()]);
        assert!(checkpoint.len() <= 4 * 1024);
        assert!(!checkpoint.contains("secret-value"));
        assert!(checkpoint.contains("thread-local-123"));
        assert!(checkpoint.contains("requestApproval"));
        assert!(checkpoint.contains("Needs permission"));
        let parsed: serde_json::Value = serde_json::from_str(&checkpoint).unwrap();
        assert_eq!(parsed["state"], "needs_input");
        assert_eq!(parsed["requestId"], 17);
        assert_eq!(parsed["threadId"], "thread-local-123");
    }
}

fn codex_thread_start(request_id: u64, cwd: &str, model_id: &str, read_only: bool) -> Value {
    if read_only {
        providers::app_server_read_only_thread_start(request_id, cwd, model_id)
    } else {
        providers::app_server_thread_start(request_id, cwd, model_id)
    }
}

struct OwnedProcess {
    child: Child,
    identity: ProcessIdentity,
    terminate_on_drop: bool,
}

impl std::ops::Deref for OwnedProcess {
    type Target = Child;
    fn deref(&self) -> &Self::Target {
        &self.child
    }
}

impl std::ops::DerefMut for OwnedProcess {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.child
    }
}

impl Drop for OwnedProcess {
    fn drop(&mut self) {
        if self.terminate_on_drop && self.child.try_wait().ok().flatten().is_none() {
            process::terminate_child_tree(&mut self.child, self.identity);
        }
    }
}

fn send_codex(stdin: &mut impl Write, message: &Value) -> Result<(), RuntimeError> {
    serde_json::to_writer(&mut *stdin, message).map_err(|_| {
        RuntimeError::new(
            "CODEX_PROTOCOL_ERROR",
            "Could not encode Codex protocol request.",
        )
    })?;
    stdin
        .write_all(b"\n")
        .and_then(|_| stdin.flush())
        .map_err(|_| {
            RuntimeError::new(
                "CODEX_PROTOCOL_ERROR",
                "Could not write Codex protocol request.",
            )
        })
}

fn receive_codex_response(
    child: &mut Child,
    identity: ProcessIdentity,
    receiver: &mpsc::Receiver<Result<String, ()>>,
    deadline: Instant,
    timeout_code: &'static str,
    cancel: &AtomicBool,
    request_id: u64,
) -> Result<Value, RuntimeError> {
    loop {
        if cancel.load(Ordering::Acquire) {
            process::terminate_child_tree(child, identity);
            return Err(RuntimeError::new(
                "CODEX_CANCELLED",
                "Codex startup was cancelled.",
            ));
        }
        if Instant::now() >= deadline {
            process::terminate_child_tree(child, identity);
            return Err(RuntimeError::new(
                timeout_code,
                "Codex startup handshake did not complete before its bounded deadline.",
            ));
        }
        match receiver.recv_timeout(Duration::from_millis(100)) {
            Ok(Ok(line)) => {
                let value = providers::parse_app_server_message(line.trim_end()).map_err(|_| {
                    RuntimeError::new(
                        "CODEX_PROTOCOL_ERROR",
                        "Codex emitted invalid protocol data.",
                    )
                })?;
                if value.get("id").and_then(Value::as_u64) == Some(request_id) {
                    return Ok(value);
                }
            }
            Ok(Err(())) | Err(mpsc::RecvTimeoutError::Disconnected) => {
                return Err(RuntimeError::new(
                    "CODEX_PROTOCOL_ERROR",
                    "Codex closed its protocol stream unexpectedly.",
                ));
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                if child.try_wait().ok().flatten().is_some() {
                    return Err(RuntimeError::new(
                        "CODEX_EXITED",
                        "Codex exited during protocol startup.",
                    ));
                }
            }
        }
    }
}

/// Probe variant leaves termination to the bounded outer cleanup stage, so a
/// handshake timeout cannot synchronously block beyond its dispatch deadline.
fn receive_codex_probe_response(
    child: &mut Child,
    receiver: &mpsc::Receiver<Result<String, ()>>,
    deadline: Instant,
    request_id: u64,
) -> Result<Value, RuntimeError> {
    loop {
        if Instant::now() >= deadline {
            return Err(RuntimeError::new(
                "CODEX_OBSERVATION_TIMEOUT",
                "Codex metadata handshake did not complete before its bounded deadline.",
            ));
        }
        match receiver.recv_timeout(Duration::from_millis(50)) {
            Ok(Ok(line)) => {
                let value = providers::parse_app_server_message(line.trim_end()).map_err(|_| {
                    RuntimeError::new(
                        "CODEX_PROTOCOL_ERROR",
                        "Codex emitted invalid metadata protocol data.",
                    )
                })?;
                if value.get("id").and_then(Value::as_u64) == Some(request_id) {
                    return Ok(value);
                }
            }
            Ok(Err(())) | Err(mpsc::RecvTimeoutError::Disconnected) => {
                return Err(RuntimeError::new(
                    "CODEX_PROTOCOL_ERROR",
                    "Codex closed its metadata protocol stream unexpectedly.",
                ));
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                if child.try_wait().ok().flatten().is_some() {
                    return Err(RuntimeError::new(
                        "CODEX_EXITED",
                        "Codex exited during metadata startup.",
                    ));
                }
            }
        }
    }
}

fn classify_failure(text: &str) -> RuntimeState {
    let text = text.to_ascii_lowercase();
    if [
        "unauthorized",
        "authentication",
        "sign in",
        "login required",
        "401",
        "invalid api key",
    ]
    .iter()
    .any(|needle| text.contains(needle))
    {
        RuntimeState::AuthenticationRequired
    } else if ["429", "rate limit", "too many requests"]
        .iter()
        .any(|needle| text.contains(needle))
    {
        RuntimeState::RateLimited
    } else if [
        "unsupported model",
        "model is unsupported",
        "model not found",
        "unknown model",
    ]
    .iter()
    .any(|needle| text.contains(needle))
    {
        RuntimeState::UnsupportedModel
    } else {
        RuntimeState::Failed
    }
}

fn redact(text: &str, secrets: &[String]) -> String {
    let value = secrets
        .iter()
        .filter(|secret| !secret.is_empty())
        .fold(text.to_owned(), |value, secret| {
            value.replace(secret, "[REDACTED]")
        });
    let mut output = String::with_capacity(value.len());
    let mut redact_next = false;
    for part in value.split_inclusive(char::is_whitespace) {
        let body = part.trim_end_matches(char::is_whitespace);
        let suffix = &part[body.len()..];
        let normalized = body.trim_matches(|character: char| !character.is_ascii_alphanumeric());
        let token_like = ["sk-", "ghp_", "gho_", "github_pat_"]
            .iter()
            .any(|prefix| normalized.starts_with(prefix));
        if redact_next || token_like {
            output.push_str("[REDACTED]");
        } else {
            output.push_str(body);
        }
        output.push_str(suffix);
        redact_next = normalized.eq_ignore_ascii_case("bearer");
    }
    output
}

/// Produce a single-line, redacted status summary within a UTF-8 byte budget.
/// This is a projection only; durable attempt JSON remains private to JobsStore.
pub(crate) fn safe_status_summary(text: &str, max_bytes: usize) -> Option<String> {
    const MARKER: &str = "… [truncated]";
    if max_bytes == 0 {
        return None;
    }
    let redacted = redact(text, &sensitive_environment_values());
    let normalized = redacted
        .chars()
        .map(|character| {
            if character.is_control() || character.is_whitespace() {
                ' '
            } else {
                character
            }
        })
        .collect::<String>();
    let normalized = normalized.split_whitespace().collect::<Vec<_>>().join(" ");
    let normalized = normalized.trim();
    if normalized.is_empty() {
        return None;
    }
    if normalized.len() <= max_bytes {
        return Some(normalized.to_owned());
    }
    if max_bytes <= MARKER.len() {
        let mut marker = String::new();
        for character in MARKER.chars() {
            if marker.len() + character.len_utf8() > max_bytes {
                break;
            }
            marker.push(character);
        }
        return Some(marker);
    }
    let content_budget = max_bytes - MARKER.len();
    let mut end = 0usize;
    for (index, character) in normalized.char_indices() {
        let next = index + character.len_utf8();
        if next > content_budget {
            break;
        }
        end = next;
    }
    let prefix = normalized[..end].trim_end();
    Some(format!("{prefix}{MARKER}"))
}

fn sensitive_environment_values() -> Vec<String> {
    std::env::vars_os()
        .filter_map(|(name, value)| {
            let name = name.to_string_lossy().to_ascii_lowercase();
            (name.contains("token")
                || name.contains("secret")
                || name.contains("password")
                || name.contains("credential")
                || name.contains("api_key")
                || name.contains("apikey")
                || name.ends_with("_auth"))
            .then(|| value.to_string_lossy().into_owned())
        })
        .filter(|value| !value.is_empty())
        .collect()
}

fn outcome(
    state: RuntimeState,
    profile: &ProviderProfile,
    model_id: String,
    exit_code: Option<i32>,
    elapsed_ms: u64,
    message: &str,
) -> RuntimeOutcome {
    let usage = if profile.kind == ProviderKind::Ollama {
        RuntimeUsage {
            cost_usd: Some(0.0),
            source: Some("local-runtime-policy".into()),
            confidence: Some("high".into()),
            billing: BillingStatus::LocalNoApiCharge,
            ..RuntimeUsage::default()
        }
    } else {
        RuntimeUsage::default()
    };
    RuntimeOutcome {
        state,
        provider_id: profile.id.clone(),
        model_id,
        exit_code,
        elapsed_ms,
        message: message.into(),
        usage,
    }
}

/// Read-only Codex app-server adapter for independent verification review.
/// Other adapters are rejected until their read-only behavior can be proven.
pub trait RuntimeObserver: Send + Sync {
    /// Return Cancel when persistence failed or dispatch must be stopped.
    fn on_event(&self, event: &RuntimeEvent) -> RuntimeControl;
    /// Called once the provider has stopped, before the adapter parses its review.
    fn on_outcome(&self, outcome: &RuntimeOutcome) -> Result<(), String>;
}

fn review_response_output_schema() -> Value {
    serde_json::json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["sourceFingerprint", "passed", "findings", "manualAcceptance", "details"],
        "properties": {
            "sourceFingerprint": { "type": "string", "maxLength": 128 },
            "passed": { "type": "boolean" },
            "findings": {
                "type": "array",
                "maxItems": 200,
                "items": { "type": "string", "maxLength": 2048 }
            },
            "manualAcceptance": {
                "type": "array",
                "maxItems": 100,
                "items": { "type": "string", "maxLength": 2048 }
            },
            "details": { "type": "string", "maxLength": 16384 }
        }
    })
}

fn run_review_with_output_schema(
    settings: &Settings,
    request: &RuntimeRequest,
    cancel: &AtomicBool,
    sink: &mut impl RuntimeEventSink,
    build_output: Option<&Path>,
) -> Result<RuntimeOutcome, RuntimeError> {
    let schema = review_response_output_schema();
    run_internal(
        settings,
        request,
        cancel,
        sink,
        None,
        build_output,
        Some(&schema),
    )
}

pub struct RuntimeReviewExecutor {
    settings: Settings,
    cwd: PathBuf,
    timeout_seconds: u64,
    observer: Option<Arc<dyn RuntimeObserver>>,
    deadline: Option<Instant>,
    build_output: Option<PathBuf>,
    reasoning_effort: Option<String>,
}

impl RuntimeReviewExecutor {
    pub fn new(settings: Settings, cwd: PathBuf, timeout_seconds: u64) -> Result<Self, String> {
        if timeout_seconds == 0 || timeout_seconds > 30 * 60 {
            return Err("Review timeout must be between 1 second and 30 minutes.".into());
        }
        Ok(Self {
            settings,
            cwd,
            timeout_seconds,
            observer: None,
            deadline: None,
            build_output: None,
            reasoning_effort: None,
        })
    }

    pub fn with_observer(mut self, observer: Arc<dyn RuntimeObserver>) -> Self {
        self.observer = Some(observer);
        self
    }

    pub fn with_deadline(mut self, deadline: Instant) -> Self {
        self.deadline = Some(deadline);
        self
    }

    pub fn with_build_output(mut self, build_output: PathBuf) -> Self {
        self.build_output = Some(build_output);
        self
    }

    pub fn with_reasoning_effort(mut self, reasoning_effort: Option<String>) -> Self {
        self.reasoning_effort = reasoning_effort;
        self
    }
}

fn review_dispatch_timeout(
    configured_seconds: u64,
    deadline: Option<Instant>,
) -> Result<u64, String> {
    let Some(deadline) = deadline else {
        return Ok(configured_seconds);
    };
    let remaining_seconds = deadline.saturating_duration_since(Instant::now()).as_secs();
    if remaining_seconds == 0 {
        return Err("Review batch deadline expired before provider dispatch.".into());
    }
    Ok(configured_seconds.min(remaining_seconds))
}

impl crate::verification::ReviewExecutor for RuntimeReviewExecutor {
    fn review(
        &self,
        request: &crate::verification::ReviewRequest,
    ) -> Result<crate::verification::ReviewResult, String> {
        if request.reviewer.task_id == request.worker_task_id {
            return Err("Independent review must use a separate task assignment.".into());
        }
        if self.reasoning_effort != request.reviewer.reasoning_effort {
            return Err(
                "Reviewer reasoning effort differs from the admitted review policy.".into(),
            );
        }
        let prompt = build_review_prompt(request)?;
        let reasoning_effort = self.reasoning_effort.clone();
        let mut runtime_request = RuntimeRequest {
            provider_id: Some(request.reviewer.provider_id.clone()),
            model_id: Some(request.reviewer.model_id.clone()),
            cwd: self.cwd.clone(),
            prompt,
            timeout_seconds: self.timeout_seconds,
            reasoning_effort,
            read_only: true,
            test_command: None,
            allow_paid: false,
        };
        // Recompute immediately before dispatch so queueing, reservation and
        // prompt construction time count against the supervisor's batch cap.
        runtime_request.timeout_seconds =
            review_dispatch_timeout(self.timeout_seconds, self.deadline)?;
        let cancel = AtomicBool::new(false);
        let mut sink = ReviewOutputSink {
            text: String::new(),
            final_output_seen: false,
            overflow: false,
            observer: self.observer.as_deref(),
        };
        let result = run_review_with_output_schema(
            &self.settings,
            &runtime_request,
            &cancel,
            &mut sink,
            self.build_output.as_deref(),
        )
        .map_err(|error| format!("review runtime failed ({}): {}", error.code, error.message))?;
        if let Some(observer) = &self.observer {
            observer.on_outcome(&result)?;
        }
        if sink.overflow {
            return Err("Reviewer output exceeded the 64 KiB response limit.".into());
        }
        if result.state != RuntimeState::Completed {
            let message = format!(
                "Reviewer runtime ended in {:?}: {}",
                result.state, result.message
            );
            return Err(
                if matches!(
                    result.state,
                    RuntimeState::NeedsInput
                        | RuntimeState::AuthenticationRequired
                        | RuntimeState::RateLimited
                ) {
                    format!("needs_input: {message}")
                } else {
                    message
                },
            );
        }
        let response = parse_review_response_with_source(
            &sink.text,
            &request.source.fingerprint,
            sink.final_output_seen,
        )?;
        Ok(crate::verification::ReviewResult {
            passed: response.passed,
            findings: response.findings,
            manual_acceptance: response.manual_acceptance,
            details: response.details,
        })
    }
}

struct ReviewOutputSink<'a> {
    text: String,
    final_output_seen: bool,
    overflow: bool,
    observer: Option<&'a dyn RuntimeObserver>,
}

impl RuntimeEventSink for ReviewOutputSink<'_> {
    fn on_event(&mut self, event: RuntimeEvent) -> RuntimeControl {
        if self
            .observer
            .is_some_and(|observer| observer.on_event(&event) == RuntimeControl::Cancel)
        {
            return RuntimeControl::Cancel;
        }
        if matches!(
            event.kind,
            RuntimeEventKind::ApprovalRequest | RuntimeEventKind::UserInputRequest
        ) {
            return RuntimeControl::Cancel;
        }
        if event.kind == RuntimeEventKind::Output
            && event
                .stream
                .as_deref()
                .is_none_or(|stream| stream == "stdout")
        {
            if self.final_output_seen {
                return RuntimeControl::Continue;
            }
            let remaining = MAX_REVIEW_OUTPUT_BYTES.saturating_sub(self.text.len());
            if event.text.len() > remaining {
                let boundary = event
                    .text
                    .char_indices()
                    .map(|(index, _)| index)
                    .take_while(|index| *index <= remaining)
                    .last()
                    .unwrap_or(0);
                self.text.push_str(&event.text[..boundary]);
                self.overflow = true;
                return RuntimeControl::Cancel;
            }
            self.text.push_str(&event.text);
        }
        RuntimeControl::Continue
    }

    fn on_final_output(&mut self, event: RuntimeEvent) -> RuntimeControl {
        if self
            .observer
            .is_some_and(|observer| observer.on_event(&event) == RuntimeControl::Cancel)
        {
            return RuntimeControl::Cancel;
        }
        if self.overflow {
            return RuntimeControl::Cancel;
        }
        if event.kind != RuntimeEventKind::Output
            || event
                .stream
                .as_deref()
                .is_some_and(|stream| stream != "stdout")
        {
            return RuntimeControl::Continue;
        }
        let text = event.text;
        self.text.clear();
        self.final_output_seen = true;
        if text.len() > MAX_REVIEW_OUTPUT_BYTES {
            let boundary = text
                .char_indices()
                .map(|(index, _)| index)
                .take_while(|index| *index <= MAX_REVIEW_OUTPUT_BYTES)
                .last()
                .unwrap_or(0);
            self.text.push_str(&text[..boundary]);
            self.overflow = true;
            return RuntimeControl::Cancel;
        }
        self.text.push_str(&text);
        RuntimeControl::Continue
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct BoundReviewResponse {
    source_fingerprint: String,
    passed: bool,
    findings: Vec<String>,
    manual_acceptance: Vec<String>,
    details: String,
}

fn build_review_prompt(request: &crate::verification::ReviewRequest) -> Result<String, String> {
    if request.acceptance_criteria.is_empty()
        || request.acceptance_criteria.len() > 200
        || request
            .acceptance_criteria
            .iter()
            .any(|item| item.len() > 2_048 || item.chars().any(char::is_control))
        || request.issue_fingerprint.len() > 128
        || request.source.head.len() > 128
        || request.source.fingerprint.len() > 128
        || request.check_receipts.is_empty()
        || request.check_receipts.len() > 100
        || request
            .check_receipts
            .iter()
            .any(|receipt| !receipt.passed || receipt.source != request.source)
    {
        return Err("Review evidence is missing, stale, or exceeds the adapter limits.".into());
    }
    let context = serde_json::json!({
        "jobId": request.job_id,
        "workerTaskId": request.worker_task_id,
        "reviewerTaskId": request.reviewer.task_id,
        "reviewerProviderId": request.reviewer.provider_id,
        "reviewerModelId": request.reviewer.model_id,
        "issueFingerprint": request.issue_fingerprint,
        "source": request.source,
        "acceptanceCriteria": request.acceptance_criteria,
        "checks": request.check_receipts.iter().map(|receipt| serde_json::json!({
            "checkId": receipt.check_id,
            "passed": receipt.passed,
            "exitCode": receipt.exit_code,
            "timedOut": receipt.timed_out,
            "truncated": receipt.truncated,
            "sourceFingerprint": receipt.source.fingerprint,
            "contract": receipt.contract.as_ref().map(|e| serde_json::json!({"status":e.status,"criteria":e.spec.criteria,"executedSpecimens":e.observations.len()})),
        })).collect::<Vec<_>>(),
    });
    let prompt = format!(
        "Review the exact checked workspace read-only as an independent reviewer. Do not edit files, run mutating commands, or claim unobserved behavior. Compare every supplied acceptance criterion and report only actionable findings. acceptanceCriteria contains only criteria assigned to this independent review. Passed contracts in checks are separately verified finite scopes: do not re-review their owned criteria or infer broader correctness from their results. Confirm current Git HEAD equals `source.head`, the checked commit identifier. `source.fingerprint` is a separate digest covering that HEAD, Git status, tracked diff, untracked paths, and untracked contents; it is not a commit hash, and a non-clean worktree can still be the exact checked snapshot. Review the complete changes, including staged, unstaged, and untracked content, against only the baseline explicitly named in the acceptance criteria or review context. Do not silently use `source.head` as the diff baseline if a different BASE is named. Never substitute origin/main or another moving ref. If the required baseline is absent or ambiguous, do not choose one: set passed false and explain the missing baseline as a blocking finding. Do not claim visual rendering, keyboard behavior, or other manual behavior without direct evidence; list any unverified manual criterion in manualAcceptance. `passed` means the independent source/code review found no review findings; it does not mean manual acceptance is complete. You may return passed=true with nonempty manualAcceptance when the source review is otherwise clear and manual criteria remain unverified. Such a result stays blocked for human acceptance and is never an overall completion. A false result always remains a failed review; verification does not use its manualAcceptance items to bypass findings or other review failure. Return exactly one JSON object with keys sourceFingerprint, passed, findings, manualAcceptance, details. sourceFingerprint must equal the supplied source fingerprint. A passed response must have no findings.\nREVIEW_CONTEXT_JSON:\n{}",
        serde_json::to_string(&context)
            .map_err(|_| "Could not encode review context.".to_string())?
    );
    if prompt.len() > MAX_PROMPT_BYTES {
        return Err("Review context exceeds the 60,000 byte prompt limit.".into());
    }
    Ok(prompt)
}

fn parse_review_response(
    output: &str,
    expected_source_fingerprint: &str,
) -> Result<BoundReviewResponse, String> {
    let trimmed = output.trim();
    let response: BoundReviewResponse = serde_json::from_str(trimmed).map_err(|error| {
        let category = match error.classify() {
            serde_json::error::Category::Io => "io",
            serde_json::error::Category::Syntax => "syntax",
            serde_json::error::Category::Data => "data",
            serde_json::error::Category::Eof => "eof",
        };
        format!(
            "Reviewer returned malformed or unsupported JSON [class={},line={},column={},bytes={}].",
            category,
            error.line(),
            error.column(),
            output.len()
        )
    })?;
    if response.source_fingerprint != expected_source_fingerprint {
        return Err("Reviewer response is bound to a different source fingerprint.".into());
    }
    if response.findings.len() > 200
        || response.manual_acceptance.len() > 100
        || response.details.len() > 16_384
        || response
            .findings
            .iter()
            .chain(&response.manual_acceptance)
            .any(|item| item.len() > 2_048 || item.chars().any(char::is_control))
        || (response.passed && !response.findings.is_empty())
    {
        return Err("Reviewer returned an invalid or oversized assessment.".into());
    }
    Ok(response)
}

fn review_output_source(final_output_seen: bool) -> &'static str {
    if final_output_seen {
        "completed_item"
    } else {
        "delta_fallback"
    }
}

fn parse_review_response_with_source(
    output: &str,
    expected_source_fingerprint: &str,
    final_output_seen: bool,
) -> Result<BoundReviewResponse, String> {
    parse_review_response(output, expected_source_fingerprint).map_err(|error| {
        format!(
            "{} [output_source={}].",
            error,
            review_output_source(final_output_seen)
        )
    })
}

#[cfg(test)]
mod review_adapter_tests {
    use super::*;
    use crate::verification::{
        CheckReceipt, ReviewExecutor, ReviewRequest, ReviewerIdentity, SourceSnapshot,
    };

    fn request() -> ReviewRequest {
        let source = SourceSnapshot {
            head: "abc123".into(),
            fingerprint: "source-fingerprint".into(),
        };
        ReviewRequest {
            job_id: 9,
            worker_task_id: "implementation-task".into(),
            issue_fingerprint: "issue-fingerprint".into(),
            source: source.clone(),
            reviewer: ReviewerIdentity {
                task_id: "independent-review".into(),
                provider_id: "codex-review".into(),
                model_id: "fixture".into(),
                reasoning_effort: None,
            },
            check_receipts: vec![CheckReceipt {
                check_id: "fixture-check".into(),
                source,
                exit_code: Some(0),
                timed_out: false,
                cancelled: false,
                output_incomplete: false,
                truncated: false,
                output_bytes: 4,
                passed: true,
                contract: None,
            }],
            acceptance_criteria: vec!["The requested behavior works.".into()],
        }
    }

    #[test]
    fn review_prompt_binds_issue_source_checks_and_criteria() {
        let prompt = build_review_prompt(&request()).unwrap();
        for expected in [
            "issue-fingerprint",
            "source-fingerprint",
            "abc123",
            "fixture-check",
            "The requested behavior works.",
            "read-only",
            "does not mean manual acceptance is complete",
            "stays blocked for human acceptance",
        ] {
            assert!(prompt.contains(expected));
        }
    }

    #[test]
    fn review_response_requires_exact_snapshot_binding_and_bounded_output() {
        let value = r#"{"sourceFingerprint":"source-fingerprint","passed":true,"findings":[],"manualAcceptance":[],"details":"Reviewed"}"#;
        assert!(parse_review_response(value, &request().source.fingerprint).is_ok());
        let mismatch = r#"{"sourceFingerprint":"other","passed":true,"findings":[],"manualAcceptance":[],"details":"Reviewed"}"#;
        let mismatch_error = parse_review_response(mismatch, &request().source.fingerprint)
            .err()
            .unwrap();
        assert!(mismatch_error.contains("different source fingerprint"));
        assert!(parse_review_response("{not-json}", &request().source.fingerprint).is_err());
        assert!(parse_review_response(
            "Reviewer note: {\"sourceFingerprint\":\"source-fingerprint\",\"passed\":true,\"findings\":[],\"manualAcceptance\":[],\"details\":\"Reviewed\"}",
            &request().source.fingerprint
        ).is_err());
        let unknown = r#"{"sourceFingerprint":"source-fingerprint","passed":true,"findings":[],"manualAcceptance":[],"details":"Reviewed","extra":true}"#;
        assert!(parse_review_response(unknown, &request().source.fingerprint).is_err());
        let malformed_with_sensitive_text = r#"{"sourceFingerprint":"source-fingerprint","passed":true,"findings":[],"manualAcceptance":[],"details":"DO_NOT_PERSIST"#;
        let final_diagnostic = parse_review_response_with_source(
            malformed_with_sensitive_text,
            &request().source.fingerprint,
            true,
        )
        .err()
        .unwrap();
        assert!(final_diagnostic.contains("class=eof"));
        assert!(
            final_diagnostic.contains(&format!("bytes={}", malformed_with_sensitive_text.len()))
        );
        assert!(final_diagnostic.contains("output_source=completed_item"));
        assert!(!final_diagnostic.contains("DO_NOT_PERSIST"));

        let unknown_with_sensitive_name = r#"{"sourceFingerprint":"source-fingerprint","passed":true,"findings":[],"manualAcceptance":[],"details":"Reviewed","api_key":"DO_NOT_PERSIST"}"#;
        let unknown_diagnostic = parse_review_response_with_source(
            unknown_with_sensitive_name,
            &request().source.fingerprint,
            false,
        )
        .err()
        .unwrap();
        assert!(unknown_diagnostic.contains("class=data"));
        assert!(unknown_diagnostic.contains("output_source=delta_fallback"));
        assert!(!unknown_diagnostic.contains("api_key"));
        assert!(!unknown_diagnostic.contains("DO_NOT_PERSIST"));

        let malformed_multibyte = r#"{"sourceFingerprint":"source-fingerprint","passed":true,"findings":[],"manualAcceptance":[],"details":"é"#;
        let multibyte_diagnostic =
            parse_review_response(malformed_multibyte, &request().source.fingerprint)
                .err()
                .unwrap();
        assert!(multibyte_diagnostic.contains(&format!("bytes={}", malformed_multibyte.len())));
        assert_ne!(
            malformed_multibyte.len(),
            malformed_multibyte.chars().count()
        );
        let mut sink = ReviewOutputSink {
            text: String::new(),
            final_output_seen: false,
            overflow: false,
            observer: None,
        };
        assert_eq!(
            sink.on_event(RuntimeEvent {
                kind: RuntimeEventKind::Output,
                stream: Some("stdout".into()),
                text: "x".repeat(MAX_REVIEW_OUTPUT_BYTES + 1),
                process: None
            }),
            RuntimeControl::Cancel
        );
        assert!(sink.overflow);

        let mut delta_only = ReviewOutputSink {
            text: String::new(),
            final_output_seen: false,
            overflow: false,
            observer: None,
        };
        assert_eq!(
            delta_only.on_event(RuntimeEvent {
                kind: RuntimeEventKind::Output,
                stream: Some("stdout".into()),
                text: value.into(),
                process: None,
            }),
            RuntimeControl::Continue
        );
        assert_eq!(delta_only.text.as_str(), value);

        let mut completed_messages = ReviewOutputSink {
            text: String::new(),
            final_output_seen: false,
            overflow: false,
            observer: None,
        };
        let completed = |text: &str| RuntimeEvent {
            kind: RuntimeEventKind::Output,
            stream: Some("stdout".into()),
            text: text.into(),
            process: None,
        };
        assert_eq!(
            completed_messages.on_final_output(completed("commentary")),
            RuntimeControl::Continue
        );
        assert_eq!(
            completed_messages.on_event(completed("split JSON delta")),
            RuntimeControl::Continue
        );
        assert_eq!(completed_messages.text, "commentary");
        assert_eq!(
            completed_messages.on_final_output(completed(value)),
            RuntimeControl::Continue
        );
        assert_eq!(completed_messages.text, value);
        assert_eq!(
            completed_messages.on_final_output(completed("last complete message")),
            RuntimeControl::Continue
        );
        assert_eq!(completed_messages.text, "last complete message");

        let mut oversized_final = ReviewOutputSink {
            text: String::new(),
            final_output_seen: false,
            overflow: false,
            observer: None,
        };
        assert_eq!(
            oversized_final.on_final_output(completed(&"x".repeat(MAX_REVIEW_OUTPUT_BYTES + 1))),
            RuntimeControl::Cancel
        );
        assert!(oversized_final.overflow);
        assert_eq!(
            oversized_final.on_final_output(completed(value)),
            RuntimeControl::Cancel
        );
        assert!(oversized_final.overflow);
    }

    #[test]
    fn review_response_allows_clear_review_with_manual_acceptance_pending() {
        let value = r#"{"sourceFingerprint":"source-fingerprint","passed":true,"findings":[],"manualAcceptance":["keyboard behavior requires direct evidence"],"details":"Source review is clear; manual acceptance remains pending."}"#;
        let response = parse_review_response(value, &request().source.fingerprint).unwrap();
        assert!(response.passed);
        assert!(response.findings.is_empty());
        assert_eq!(
            response.manual_acceptance,
            ["keyboard behavior requires direct evidence"]
        );

        let finding = r#"{"sourceFingerprint":"source-fingerprint","passed":true,"findings":["source defect"],"manualAcceptance":[],"details":"Review found a defect."}"#;
        assert!(parse_review_response(finding, &request().source.fingerprint).is_err());
        let insufficient_evidence = r#"{"sourceFingerprint":"source-fingerprint","passed":false,"findings":[],"manualAcceptance":[],"details":"The review could not establish the required baseline."}"#;
        let insufficient =
            parse_review_response(insufficient_evidence, &request().source.fingerprint).unwrap();
        assert!(!insufficient.passed);

        let failed_with_findings_and_manual = r#"{"sourceFingerprint":"source-fingerprint","passed":false,"findings":["source defect"],"manualAcceptance":["keyboard behavior requires direct evidence"],"details":"Review failed; manual behavior is also unverified."}"#;
        let failed = parse_review_response(
            failed_with_findings_and_manual,
            &request().source.fingerprint,
        )
        .unwrap();
        assert!(!failed.passed);
        assert_eq!(failed.findings, ["source defect"]);
        assert_eq!(
            failed.manual_acceptance,
            ["keyboard behavior requires direct evidence"]
        );
    }

    #[test]
    fn codex_review_thread_is_ephemeral_and_read_only() {
        let message = codex_thread_start(1, "C:/Projects/sample", "model", true);
        assert_eq!(message["params"]["sandbox"], "read-only");
        assert_eq!(message["params"]["ephemeral"], true);
        assert_eq!(message["params"]["approvalPolicy"], "never");
        assert_eq!(
            message["params"]["runtimeWorkspaceRoots"],
            serde_json::json!([])
        );
        assert_eq!(message["params"]["dynamicTools"], serde_json::json!([]));
        assert_eq!(
            message["params"]["config"]["apps"]["_default"]["enabled"],
            false
        );
        assert_eq!(
            message["params"]["config"]["tools"]["web_search"]["enabled"],
            false
        );
        assert!(message["params"]["config"]["mcp_servers"]
            .as_object()
            .unwrap()
            .is_empty());
        let turn = providers::app_server_turn_start_with_output_schema(
            2,
            "review-thread",
            "C:/temporary",
            "model",
            None,
            "draft",
            None,
            None,
            true,
        );
        assert_eq!(turn["params"]["approvalPolicy"], "never");
        assert_eq!(
            turn["params"]["runtimeWorkspaceRoots"],
            serde_json::json!([])
        );
        assert_eq!(
            turn["params"]["config"]["apps"]["_default"]["enabled"],
            false
        );
        assert_eq!(
            turn["params"]["config"]["tools"]["web_search"]["enabled"],
            false
        );
        assert!(turn["params"]["config"]["mcp_servers"]
            .as_object()
            .unwrap()
            .is_empty());
        let implementation = codex_thread_start(1, "C:/Projects/sample", "model", false);
        assert_eq!(implementation["params"]["sandbox"], "workspace-write");
    }

    #[test]
    fn reviewer_must_have_a_distinct_task_assignment() {
        let mut req = request();
        req.reviewer.task_id = req.worker_task_id.clone();
        let executor =
            RuntimeReviewExecutor::new(Settings::default(), PathBuf::from("."), 30).unwrap();
        assert!(executor.review(&req).unwrap_err().contains("separate task"));
    }

    #[cfg(windows)]
    #[test]
    fn fake_codex_review_adapter_uses_read_only_sandbox_and_snapshot_bound_json() {
        use crate::providers::{ModelProfile, ProviderKind, ProviderProfile};
        use std::fs;

        let folder = tempfile::tempdir().unwrap();
        let build_output = folder.path().join("isolated build output");
        fs::create_dir_all(&build_output).unwrap();
        fs::write(
            folder.path().join("expected-target.txt"),
            build_output.to_string_lossy().as_bytes(),
        )
        .unwrap();
        let script = folder.path().join("fake read-only reviewer.ps1");
        fs::write(&script, r#"$ErrorActionPreference = 'Stop'
if ($env:CARGO_TARGET_DIR -ne [IO.File]::ReadAllText((Join-Path (Get-Location) 'expected-target.txt'))) { throw 'review build output directory was not propagated' }
while ($line = [Console]::In.ReadLine()) {
  $message = $line | ConvertFrom-Json
  switch ($message.method) {
    'initialize' { [Console]::Out.WriteLine((ConvertTo-Json -Compress -InputObject @{id=$message.id;result=@{}})) }
    'thread/start' {
      if ($message.params.sandbox -ne 'read-only' -or -not $message.params.ephemeral) { throw 'review sandbox was not read-only and ephemeral' }
      [Console]::Out.WriteLine((ConvertTo-Json -Compress -InputObject @{id=$message.id;result=@{thread=@{id='review-thread'}}}))
    }
    'turn/start' {
      if ($message.params.effort -ne 'medium') { throw 'review reasoning effort was not forwarded' }
      $schema = $message.params.outputSchema
      if (-not $schema -or $schema.type -ne 'object' -or $schema.additionalProperties -ne $false) { throw 'strict reviewer output schema was not forwarded' }
      if ((@($schema.required) -join ',') -ne 'sourceFingerprint,passed,findings,manualAcceptance,details') { throw 'review output schema required keys changed' }
      if ($schema.properties.sourceFingerprint.type -ne 'string' -or $schema.properties.passed.type -ne 'boolean' -or $schema.properties.findings.type -ne 'array' -or $schema.properties.manualAcceptance.type -ne 'array' -or $schema.properties.details.type -ne 'string') { throw 'review output schema property types changed' }
      $review = '{"sourceFingerprint":"source-fingerprint","passed":true,"findings":[],"manualAcceptance":["keyboard behavior requires direct evidence"],"details":"Source review is clear; manual acceptance remains pending."}'
      [Console]::Out.WriteLine((ConvertTo-Json -Compress -InputObject @{method='item/completed';params=@{item=@{type='agentMessage';text='Reviewing the checked snapshot.'}}}))
      $split = [int]($review.Length / 2)
      [Console]::Out.WriteLine((ConvertTo-Json -Compress -InputObject @{method='item/agentMessage/delta';params=@{delta=$review.Substring(0, $split)}}))
      [Console]::Out.WriteLine((ConvertTo-Json -Compress -InputObject @{method='item/agentMessage/delta';params=@{delta=$review.Substring($split)}}))
      [Console]::Out.WriteLine((ConvertTo-Json -Compress -InputObject @{method='item/completed';params=@{item=@{type='agentMessage';text=$review}}}))
      [Console]::Out.WriteLine((ConvertTo-Json -Compress -InputObject @{method='turn/completed';params=@{turn=@{status='completed'}}}))
    }
  }
}"#).unwrap();
        let mut settings = Settings::default();
        settings.providers = vec![ProviderProfile {
            id: "codex-review".into(),
            kind: ProviderKind::Codex,
            name: "Synthetic reviewer".into(),
            executable: script.to_string_lossy().into_owned(),
            endpoint: None,
            secret_ref: None,
            models: vec![ModelProfile {
                id: "fixture".into(),
                name: "Fixture".into(),
                reasoning_efforts: vec!["medium".into()],
                context_window_tokens: None,
                supports_tools: Some(true),
                quality_rating: None,
            }],
        }];
        let executor = RuntimeReviewExecutor::new(settings, folder.path().to_path_buf(), 15)
            .unwrap()
            .with_build_output(build_output)
            .with_reasoning_effort(Some("medium".into()));
        let mut review = request();
        review.reviewer.reasoning_effort = Some("medium".into());
        let result = executor.review(&review).unwrap();
        assert!(result.passed);
        assert_eq!(
            result.details,
            "Source review is clear; manual acceptance remains pending."
        );
        assert_eq!(
            result.manual_acceptance,
            ["keyboard behavior requires direct evidence"]
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::{ModelProfile, ProviderSelection};

    #[test]
    fn codex_model_inventory_parser_rejects_empty_inventory_as_incomplete() {
        let response = serde_json::json!({"result":{"data":[]}});
        assert!(parse_codex_model_inventory(&response).is_err());
    }

    #[test]
    fn codex_model_inventory_parser_rejects_over_limit_inventory_as_incomplete() {
        let data = (0..501)
            .map(|index| serde_json::json!({"id":format!("model-{index}")}))
            .collect::<Vec<_>>();
        let response = serde_json::json!({"result":{"data":data}});
        assert!(parse_codex_model_inventory(&response).is_err());
    }

    #[test]
    fn codex_model_inventory_parser_rejects_malformed_and_mixed_entries() {
        for response in [
            serde_json::json!({"result":{"data":[{"name":"unidentified"}]}}),
            serde_json::json!({"result":{"data":[
                {"id":"valid-model"}, {"id":17}
            ]}}),
            serde_json::json!({"result":{"data":[{"id":"duplicate"},{"id":"duplicate"}]}}),
        ] {
            assert!(
                parse_codex_model_inventory(&response).is_err(),
                "{response}"
            );
        }
    }

    #[test]
    fn codex_model_inventory_parser_rejects_missing_collection_and_rpc_error() {
        for response in [
            serde_json::json!({"result":{"status":"ok"}}),
            serde_json::json!({"id":3,"error":{"code":-32603,"message":"temporary failure"}}),
        ] {
            assert!(
                parse_codex_model_inventory(&response).is_err(),
                "{response}"
            );
        }
    }

    #[test]
    fn codex_model_inventory_parser_allows_verified_inventory_to_prove_model_absence() {
        let response = serde_json::json!({"result":{"data":[
            {"id":"gpt-6.1-sol"}, {"id":"gpt-6-astra"}
        ]}});
        let models = parse_codex_model_inventory(&response).unwrap();
        assert!(!models.iter().any(|model| model.id == "gpt-6-luna"));
        assert_eq!(
            models
                .iter()
                .map(|model| model.id.as_str())
                .collect::<Vec<_>>(),
            vec!["gpt-6.1-sol", "gpt-6-astra"]
        );
    }

    #[test]
    fn codex_model_inventory_parser_rejects_missing_model_on_page_with_next_cursor() {
        let response = serde_json::json!({"result":{
            "data":[{"id":"gpt-6.1-sol"}], "nextCursor":"opaque-next-page"
        }});
        assert!(parse_codex_model_inventory(&response).is_err());
    }

    #[test]
    fn codex_model_inventory_parser_preserves_selected_model_capabilities() {
        let response = serde_json::json!({"result":{"models":[
            {"id":"gpt-6-luna", "contextWindow":65536, "supportsTools":true}
        ], "nextCursor":null}});
        let models = parse_codex_model_inventory(&response).unwrap();
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].id, "gpt-6-luna");
        assert_eq!(models[0].context_tokens, Some(65_536));
        assert_eq!(models[0].supports_tools, Some(true));
    }

    #[test]
    fn codex_model_page_parser_accepts_terminal_null_cursor_and_rejects_malformed_cursors() {
        let terminal = serde_json::json!({"result":{"data":[{"id":"model-a"}],"nextCursor":null}});
        let (models, cursor) = parse_codex_model_page(&terminal).unwrap();
        assert_eq!(models.len(), 1);
        assert!(cursor.is_none());

        for cursor in [
            serde_json::json!(17),
            serde_json::json!(""),
            serde_json::json!("x".repeat(4097)),
        ] {
            let response =
                serde_json::json!({"result":{"data":[{"id":"model-a"}],"nextCursor":cursor}});
            assert!(parse_codex_model_page(&response).is_err(), "{response}");
        }
    }

    #[test]
    fn codex_model_inventory_parser_preserves_legacy_bare_array_aliases() {
        let response = serde_json::json!([{
            "model":"legacy-model", "modelInfo":{"contextWindow":32768}, "supportsTools":false
        }]);
        // The wire response itself must still have the JSON-RPC `result` wrapper.
        let wrapped = serde_json::json!({"id":3,"result":response});
        let models = parse_codex_model_inventory(&wrapped).unwrap();
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].id, "legacy-model");
        assert_eq!(models[0].context_tokens, Some(32_768));
        assert_eq!(models[0].supports_tools, Some(false));
    }

    #[test]
    fn safe_status_summary_redacts_normalizes_and_truncates_utf8() {
        let input = format!(
            "Needs input\0\nBearer top-secret-token café🙂 {}",
            "🌙".repeat(1500)
        );
        let summary = safe_status_summary(&input, 2 * 1024).unwrap();
        assert!(summary.len() <= 2 * 1024);
        assert!(summary.ends_with("… [truncated]"));
        assert!(!summary.contains("top-secret-token"));
        assert!(!summary.chars().any(char::is_control));
        assert!(summary.contains("café🙂"));
        assert!(safe_status_summary("reason", 2).unwrap().len() <= 2);
    }
    use crate::settings::Settings;
    use tempfile::tempdir;

    #[test]
    fn codex_probe_reserves_cleanup_inside_the_original_deadline() {
        let deadline = Instant::now() + Duration::from_secs(5);
        let protocol_deadline = codex_probe_protocol_deadline(deadline).unwrap();
        assert!(protocol_deadline < deadline);
        assert!(deadline.duration_since(protocol_deadline) >= Duration::from_secs(2));

        let too_short = Instant::now() + Duration::from_millis(500);
        assert!(codex_probe_protocol_deadline(too_short).is_err());
    }

    #[test]
    fn codex_account_type_is_authoritative_over_requires_openai_auth() {
        assert_eq!(
            codex_authenticated_from_account_read(&serde_json::json!({
                "result": {"account": {"type": "chatgpt", "requiresOpenaiAuth": true}}
            })),
            Some(true)
        );
        assert_eq!(
            codex_authenticated_from_account_read(&serde_json::json!({
                "result": {"account": {"type": "apiKey", "requiresOpenaiAuth": false}}
            })),
            Some(false)
        );
        assert_eq!(
            codex_authenticated_from_account_read(&serde_json::json!({
                "result": {"account": null}
            })),
            None
        );
        assert_eq!(
            codex_authenticated_from_account_read(&serde_json::json!({"result": {}})),
            None
        );
    }

    #[test]
    fn review_dispatch_timeout_is_capped_by_deadline_and_refuses_expiry() {
        let bounded =
            review_dispatch_timeout(90, Some(Instant::now() + Duration::from_secs(7))).unwrap();
        assert!((1..=7).contains(&bounded));
        assert_eq!(review_dispatch_timeout(90, None).unwrap(), 90);
        assert!(
            review_dispatch_timeout(90, Some(Instant::now() - Duration::from_secs(1))).is_err()
        );
    }

    #[test]
    fn paid_aider_preflight_rejects_hostile_config_in_cwd_git_root_and_home() {
        let folder = tempdir().unwrap();
        let cwd = folder.path().join("repo").join("worktree");
        let git_root = folder.path().join("repo");
        let home = folder.path().join("home");
        std::fs::create_dir_all(&cwd).unwrap();
        std::fs::create_dir_all(&home).unwrap();
        std::fs::write(git_root.join(".git"), "synthetic git marker").unwrap();
        check_aider_autoload_files_from(&cwd, Some(&home)).unwrap();
        for root in [&cwd, &git_root, &home] {
            for name in [
                ".env",
                ".aider.conf.yml",
                ".aider.model.settings.yml",
                ".aider.model.metadata.json",
            ] {
                let path = root.join(name);
                std::fs::write(
                    &path,
                    "OPENAI_API_BASE=https://hostile.invalid\nOPENAI_API_KEY=sentinel\n",
                )
                .unwrap();
                let error = check_aider_autoload_files_from(&cwd, Some(&home)).unwrap_err();
                assert_eq!(error.code, "AIDER_CONFIG_PRESENT", "{}", path.display());
                std::fs::remove_file(path).unwrap();
            }
        }
    }

    #[test]
    fn paid_aider_preflight_uses_nested_worktree_git_root() {
        let folder = tempdir().unwrap();
        let primary = folder.path().join("primary");
        let worktree = primary.join(".worktrees").join("task");
        let home = folder.path().join("home");
        std::fs::create_dir_all(&worktree).unwrap();
        std::fs::create_dir_all(&home).unwrap();
        std::fs::write(primary.join(".git"), "primary git marker").unwrap();
        std::fs::write(worktree.join(".git"), "worktree git marker").unwrap();
        std::fs::write(primary.join(".env"), "OPENAI_API_KEY=unrelated").unwrap();
        check_aider_autoload_files_from(&worktree, Some(&home)).unwrap();
        std::fs::write(worktree.join(".env"), "OPENAI_API_KEY=hostile").unwrap();
        let error = check_aider_autoload_files_from(&worktree, Some(&home)).unwrap_err();
        assert_eq!(error.code, "AIDER_CONFIG_PRESENT");
    }

    #[test]
    fn paid_aider_source_paths_are_resolved_and_contained_by_the_worktree() {
        let folder = tempdir().unwrap();
        let worktree = folder.path().join("worktree");
        std::fs::create_dir_all(worktree.join("src")).unwrap();
        std::fs::write(worktree.join("src/lib.rs"), "pub fn fixture() {}\n").unwrap();
        let allowed = vec!["src/lib.rs".to_string(), "generated/nested.rs".to_string()];
        let resolved = resolve_paid_aider_source_files(&worktree, &allowed).unwrap();
        assert_eq!(resolved.len(), 2);
        assert_eq!(
            PathBuf::from(&resolved[0]),
            worktree.canonicalize().unwrap().join("src/lib.rs")
        );
        assert_eq!(
            PathBuf::from(&resolved[1]),
            worktree.canonicalize().unwrap().join("generated/nested.rs")
        );
        assert!(resolved
            .iter()
            .all(|path| Path::new(path).starts_with(worktree.canonicalize().unwrap())));
    }

    #[test]
    fn paid_aider_source_paths_reject_absolute_parent_and_empty_allowlists() {
        let folder = tempdir().unwrap();
        let worktree = folder.path().join("worktree");
        std::fs::create_dir_all(&worktree).unwrap();
        for allowed in [
            Vec::new(),
            vec!["../outside.rs".to_string()],
            vec![folder
                .path()
                .join("outside.rs")
                .to_string_lossy()
                .into_owned()],
        ] {
            let error = resolve_paid_aider_source_files(&worktree, &allowed).unwrap_err();
            assert_eq!(error.code, "PAID_ALLOWED_FILES_INVALID");
        }
    }

    #[test]
    fn interpreter_probe_identity_accepts_windows_namespace_alias_and_rejects_other_path() {
        let expected = std::env::current_exe().unwrap().canonicalize().unwrap();
        #[cfg(windows)]
        {
            let normal = expected
                .to_string_lossy()
                .trim_start_matches(r"\\?\")
                .to_owned();
            let namespaced = PathBuf::from(format!(r"\\?\{}", normal));
            assert!(interpreter_path_matches_probe(
                Some(Path::new(&normal)),
                &namespaced
            ));
            assert!(interpreter_path_matches_probe(
                Some(&namespaced),
                Path::new(&normal)
            ));
            let wrong = expected.parent().unwrap();
            assert!(!interpreter_path_matches_probe(Some(wrong), &expected));
        }
        #[cfg(not(windows))]
        {
            assert!(interpreter_path_matches_probe(Some(&expected), &expected));
            assert!(!interpreter_path_matches_probe(None, &expected));
            assert!(!interpreter_path_matches_probe(
                Some(Path::new("/different/python")),
                &expected
            ));
        }
    }

    #[test]
    fn aider_version_failures_distinguish_timeout_exit_and_mismatch() {
        let probe = |exit_code, timed_out, output_matches| AiderVersionProbe {
            exit_code,
            timed_out,
            output_matches,
            output_truncated: false,
            stdout_bytes: 14,
            stderr_bytes: 0,
            elapsed_ms: 1,
        };
        assert_eq!(
            validate_aider_version_probe(&probe(None, true, false))
                .unwrap_err()
                .code,
            "AIDER_VERSION_TIMEOUT"
        );
        assert_eq!(
            validate_aider_version_probe(&probe(Some(1), false, false))
                .unwrap_err()
                .code,
            "AIDER_VERSION_EXIT_FAILED"
        );
        assert_eq!(
            validate_aider_version_probe(&probe(Some(0), false, false))
                .unwrap_err()
                .code,
            "AIDER_VERSION_UNSUPPORTED"
        );
        assert!(validate_aider_version_probe(&probe(Some(0), false, true)).is_ok());
        assert!(aider_version_output_matches(b"aider 0.86.2"));
        assert!(aider_version_output_matches(b"aider 0.86.2\n"));
        assert!(aider_version_output_matches(b"aider 0.86.2\r\n"));
        assert!(!aider_version_output_matches(b" aider 0.86.2"));
        assert!(!aider_version_output_matches(b"aider 0.86.2 \n"));
        assert!(!aider_version_output_matches(b"aider 0.86.2\nwarning"));
    }

    #[test]
    fn aider_version_probe_timeout_has_headroom_but_never_exceeds_parent_deadline() {
        assert_eq!(
            aider_version_probe_timeout(Duration::from_secs(5)),
            Duration::from_secs(5)
        );
        assert_eq!(
            aider_version_probe_timeout(Duration::from_secs(120)),
            Duration::from_secs(30)
        );
        assert_eq!(
            aider_version_probe_timeout(Duration::from_secs(30)),
            Duration::from_secs(30)
        );
    }

    #[test]
    #[ignore = "metadata-only: requires FABRICA_AIDER_SMOKE_EXE; invokes only the isolated Aider --version probe"]
    fn actual_paid_aider_version_guard_probe_reports_bounded_metadata() {
        let executable = std::env::var_os("FABRICA_AIDER_SMOKE_EXE")
            .expect("root must provide the already-installed Aider executable");
        let executable = executable.to_string_lossy();
        let aider = resolve_aider_executable(&executable).unwrap();
        let policy = PaidModelPolicy {
            mode: crate::paid_gateway::GatewayMode::Paid,
            provider_id: "openrouter".into(),
            model_id: "vendor/model".into(),
            runtime_id: "aider".into(),
            model_context_window_tokens: 4096,
            max_output_tokens_per_call: 512,
            max_calls: 1,
            max_cost_micros: 500,
            pricing: crate::paid_gateway::PaidPricingCeiling {
                input_price_micros_per_1m: 1,
                output_price_micros_per_1m: 1,
                request_fee_micros: 0,
                source: "diagnostic-only".into(),
                observed_at_ms: 1,
            },
            endpoint_pin: None,
        };
        let files = prepare_paid_aider_files(
            "probe only",
            &policy,
            &["C:/synthetic/source.rs".into()],
        )
        .unwrap();
        let build_output = tempdir().unwrap();
        let timeout = aider_version_probe_timeout(Duration::from_secs(120));
        let probe = capture_aider_version_probe(
            &aider,
            &files,
            timeout,
            Some(build_output.path()),
        )
        .unwrap();
        eprintln!(
            "AIDER_VERSION_PROBE={}",
            serde_json::to_string(&probe).unwrap()
        );
        validate_aider_version_probe(&probe).unwrap();
    }

    #[test]
    #[ignore = "requires FABRICA_AIDER_SMOKE_EXE; launches only the local isolated Python --probe, never Aider or a provider"]
    fn actual_paid_aider_guard_probe_accepts_normal_and_namespaced_adjacent_python_paths() {
        let executable = std::env::var_os("FABRICA_AIDER_SMOKE_EXE")
            .expect("root must point this test at the already-installed Aider executable");
        let executable = executable.to_string_lossy();
        let aider = resolve_aider_executable(&executable).unwrap();
        let python = resolve_aider_python(&aider).unwrap();
        let policy = PaidModelPolicy {
            mode: crate::paid_gateway::GatewayMode::Paid,
            provider_id: "openrouter".into(),
            model_id: "vendor/model".into(),
            runtime_id: "aider".into(),
            model_context_window_tokens: 4096,
            max_output_tokens_per_call: 512,
            max_calls: 1,
            max_cost_micros: 500,
            pricing: crate::paid_gateway::PaidPricingCeiling {
                input_price_micros_per_1m: 1,
                output_price_micros_per_1m: 1,
                request_fee_micros: 0,
                source: "diagnostic-only".into(),
                observed_at_ms: 1,
            },
            endpoint_pin: None,
        };
        let files =
            prepare_paid_aider_files("probe only", &policy, &["C:/synthetic/source.rs".into()])
                .unwrap();
        let timeout = Duration::from_secs(10);
        verify_aider_guard(&python, &files, None, timeout)
            .expect("the production guard probe should accept its adjacent venv Python");
        #[cfg(windows)]
        {
            let lexical = python.to_string_lossy();
            assert!(lexical.starts_with(r"\\?\"));
            let normal = lexical.trim_start_matches(r"\\?\").to_owned();
            assert!(!normal.starts_with(r"\\?\"));
            verify_aider_guard(Path::new(&normal), &files, None, timeout)
                .expect("the same adjacent interpreter must pass with its ordinary Windows path");
        }
    }
    #[test]
    fn paid_aider_environment_contains_only_isolated_profile_and_dummy_gateway_secret() {
        let policy = PaidModelPolicy {
            mode: crate::paid_gateway::GatewayMode::Paid,
            provider_id: "openrouter".into(),
            model_id: "vendor/model".into(),
            runtime_id: "aider".into(),
            model_context_window_tokens: 4096,
            max_output_tokens_per_call: 512,
            max_calls: 1,
            max_cost_micros: 500,
            pricing: crate::paid_gateway::PaidPricingCeiling {
                input_price_micros_per_1m: 1,
                output_price_micros_per_1m: 1,
                request_fee_micros: 0,
                source: "test".into(),
                observed_at_ms: 1,
            },
            endpoint_pin: None,
        };
        let files = prepare_paid_aider_files("prompt", &policy, &["C:/worktree/src/lib.rs".into()])
            .unwrap();
        let env =
            isolated_aider_environment(&files, "http://127.0.0.1:4321/v1", "dummy-child-token");
        let values = env
            .iter()
            .map(|(key, value)| (key.as_str(), value.as_str()))
            .collect::<std::collections::HashMap<_, _>>();
        assert_eq!(
            values.get("OPENAI_API_BASE"),
            Some(&"http://127.0.0.1:4321/v1")
        );
        assert_eq!(values.get("OPENAI_API_KEY"), Some(&"dummy-child-token"));
        assert_eq!(
            values.get("USERPROFILE"),
            Some(&files.profile_path.as_str())
        );
        assert!(!values.contains_key("OPENROUTER_API_KEY"));
        assert!(!values.contains_key("HTTP_PROXY"));
        assert!(!values.contains_key("AIDER_CONFIG"));
        assert_eq!(values.get("LITELLM_LOCAL_MODEL_COST_MAP"), Some(&"True"));
        assert_eq!(values.get("PYTHONIOENCODING"), Some(&"utf-8"));
        assert_eq!(values.get("PYTHONUTF8"), Some(&"1"));
    }

    #[test]
    fn paid_policy_refusal_is_needs_input_even_if_child_exits_successfully() {
        assert_eq!(
            classify_paid_runtime_state(
                true,
                false,
                false,
                false,
                Some(0),
                Some(PaidPolicyRefusal::CallLimit),
                "",
            ),
            RuntimeState::NeedsInput
        );
        assert_eq!(
            classify_paid_runtime_state(
                true,
                true,
                false,
                false,
                Some(0),
                Some(PaidPolicyRefusal::CallLimit),
                "",
            ),
            RuntimeState::Cancelled
        );
    }

    #[derive(Default)]
    struct RecordingSink(Vec<RuntimeEvent>);

    impl RuntimeEventSink for RecordingSink {
        fn on_event(&mut self, event: RuntimeEvent) -> RuntimeControl {
            self.0.push(event);
            RuntimeControl::Continue
        }
    }

    #[cfg(windows)]
    #[test]
    fn codex_fake_app_server_runs_turn_and_keeps_unknown_billing_unknown() {
        use crate::providers::ProviderProfile;
        use std::fs;

        let folder = tempdir().unwrap();
        let script = folder.path().join("fake codex.ps1");
        fs::write(
            &script,
            r#"$ErrorActionPreference = 'Stop'
if ($args.Count -ne 4 -or $args[0] -ne '-c' -or $args[1] -ne 'forced_login_method="chatgpt"' -or $args[2] -ne 'app-server' -or $args[3] -ne '--stdio') { throw 'Codex launch arguments did not force ChatGPT authentication' }
if ($env:OPENAI_API_KEY -or $env:CODEX_API_KEY) { throw 'A Platform API key reached the Codex child' }
while ($line = [Console]::In.ReadLine()) {
  $message = $line | ConvertFrom-Json
  switch ($message.method) {
    'initialize' { [Console]::Out.WriteLine('{"id":1,"result":{}}') }
    'thread/start' { [Console]::Out.WriteLine('{"id":2,"result":{"thread":{"id":"fake-thread"}}}') }
    'turn/start' {
      if ($message.params.PSObject.Properties.Name -contains 'outputSchema') { throw 'ordinary implementation turn unexpectedly received reviewer output schema' }
      [Console]::Out.WriteLine('{"method":"thread/tokenUsage/updated","params":{"tokenUsage":{"last":{"totalTokens":17},"total":{"inputTokens":12,"outputTokens":5,"totalTokens":17}}}}')
      [Console]::Out.WriteLine('{"method":"thread/tokenUsage/updated","params":{"tokenUsage":{"last":{"totalTokens":9},"total":{"inputTokens":30,"outputTokens":11,"totalTokens":41}}}}')
      [Console]::Out.WriteLine('{"method":"thread/tokenUsage/updated","params":{"tokenUsage":{"last":{"totalTokens":9},"total":{"inputTokens":30,"outputTokens":11,"totalTokens":41}}}}')
      [Console]::Out.WriteLine('{"method":"item/agentMessage/delta","params":{"delta":"synthetic result"}}')
      [Console]::Out.WriteLine('{"method":"turn/completed","params":{"turn":{"status":"completed"}}}')
    }
  }
}"#,
        )
        .unwrap();

        let profile = ProviderProfile {
            id: "codex-fixture".into(),
            kind: ProviderKind::Codex,
            name: "Codex fixture".into(),
            executable: script.to_string_lossy().into_owned(),
            endpoint: None,
            secret_ref: None,
            models: vec![ModelProfile {
                id: "fixture-model".into(),
                name: "Fixture model".into(),
                reasoning_efforts: vec![],
                context_window_tokens: None,
                supports_tools: Some(true),
                quality_rating: None,
            }],
        };
        let mut settings = Settings::default();
        settings.providers = vec![profile];
        settings.provider_selection = ProviderSelection::default();
        let request = RuntimeRequest {
            provider_id: Some("codex-fixture".into()),
            model_id: Some("fixture-model".into()),
            cwd: folder.path().to_path_buf(),
            prompt: "Do the synthetic task".into(),
            timeout_seconds: 15,
            reasoning_effort: None,
            read_only: false,
            test_command: None,
            allow_paid: false,
        };
        let cancel = AtomicBool::new(false);
        let mut sink = RecordingSink::default();
        let result = run(&settings, &request, &cancel, &mut sink).unwrap();

        assert_eq!(result.state, RuntimeState::Completed);
        assert_eq!(result.usage.total_tokens, Some(41));
        assert_eq!(result.usage.input_tokens, Some(30));
        assert_eq!(result.usage.output_tokens, Some(11));
        assert_eq!(
            result.usage.confidence.as_deref(),
            Some("provider-reported-cumulative")
        );
        assert_eq!(result.usage.cost_usd, None);
        assert_eq!(result.usage.billing, BillingStatus::ProviderManagedUnknown);
        assert_eq!(
            sink.0.first().unwrap().kind,
            RuntimeEventKind::ProcessStarted
        );
        assert!(sink.0.iter().any(|event| event.text == "synthetic result"));
        assert!(sink.0.first().unwrap().process.is_some());

        struct CancellingSink;
        impl RuntimeEventSink for CancellingSink {
            fn on_event(&mut self, event: RuntimeEvent) -> RuntimeControl {
                if event.kind == RuntimeEventKind::Output {
                    RuntimeControl::Cancel
                } else {
                    RuntimeControl::Continue
                }
            }
        }
        let mut cancel_sink = CancellingSink;
        let cancelled = run(&settings, &request, &cancel, &mut cancel_sink).unwrap();
        assert_eq!(cancelled.state, RuntimeState::Cancelled);
    }

    #[cfg(windows)]
    fn codex_probe_fixture_profile(script: &std::path::Path) -> crate::providers::ProviderProfile {
        crate::providers::ProviderProfile {
            id: "codex-probe-fixture".into(),
            kind: ProviderKind::Codex,
            name: "Codex probe fixture".into(),
            executable: script.to_string_lossy().into_owned(),
            endpoint: None,
            secret_ref: None,
            models: vec![ModelProfile {
                id: "fixture-model".into(),
                name: "Fixture model".into(),
                reasoning_efforts: vec![],
                context_window_tokens: None,
                supports_tools: None,
                quality_rating: None,
            }],
        }
    }

    #[cfg(windows)]
    #[test]
    fn model_free_codex_probe_accepts_streaming_powershell_entrypoint_without_turns() {
        use std::fs;

        let folder = tempdir().unwrap();
        let script = folder.path().join("streaming codex probe.ps1");
        let methods = folder.path().join("protocol-methods.txt");
        let escaped_methods = methods.to_string_lossy().replace('\'', "''");
        let body = format!(
            r#"$ErrorActionPreference = 'Stop'
$methodsPath = '{escaped_methods}'
while ($line = [Console]::In.ReadLine()) {{
  $message = $line | ConvertFrom-Json
  if ($message.method) {{ [IO.File]::AppendAllText($methodsPath, ([string]$message.method + "`n")) }}
  switch ($message.method) {{
    'initialize' {{ [Console]::Out.WriteLine('{{"id":1,"result":{{}}}}') }}
    'account/read' {{ [Console]::Out.WriteLine('{{"id":2,"result":{{"account":{{"type":"chatgpt","requiresOpenaiAuth":true}}}}}}') }}
    'model/list' {{ [Console]::Out.WriteLine('{{"id":3,"result":{{"data":[{{"id":"fixture-model","contextWindow":32768}}]}}}}') }}
  }}
}}"#
        );
        fs::write(&script, body).unwrap();

        let started = Instant::now();
        let probe = probe_codex_account_and_models(
            &codex_probe_fixture_profile(&script),
            Duration::from_secs(10),
        )
        .unwrap();
        assert!(started.elapsed() < Duration::from_secs(15));
        assert_eq!(probe.authenticated, Some(true));
        assert!(probe.model_list_verified);
        assert_eq!(probe.models.len(), 1);
        assert_eq!(probe.models[0].id, "fixture-model");
        assert_eq!(probe.models[0].context_tokens, Some(32_768));

        let methods = fs::read_to_string(methods).unwrap();
        assert!(methods.contains("initialize"));
        assert!(methods.contains("account/read"));
        assert!(methods.contains("model/list"));
        assert!(!methods.contains("thread/start"));
        assert!(!methods.contains("turn/start"));
    }

    #[cfg(windows)]
    #[test]
    fn model_free_codex_probe_follows_next_cursor_until_selected_model_page() {
        use std::fs;

        let folder = tempdir().unwrap();
        let script = folder.path().join("paged codex probe.ps1");
        let requests = folder.path().join("model-list-requests.txt");
        let methods = folder.path().join("protocol-methods.txt");
        let body = r#"$ErrorActionPreference = 'Stop'
$requestsPath = '__REQUESTS__'
$methodsPath = '__METHODS__'
while ($line = [Console]::In.ReadLine()) {
  $message = $line | ConvertFrom-Json
  [IO.File]::AppendAllText($methodsPath, ([string]$message.method + "`n"))
  switch ($message.method) {
    'initialize' { [Console]::Out.WriteLine('{"id":1,"result":{}}') }
    'account/read' { [Console]::Out.WriteLine('{"id":2,"result":{"account":{"type":"chatgpt","requiresOpenaiAuth":true}}}') }
    'model/list' {
      $id = [int]$message.id
      $cursor = $message.params.cursor
      [IO.File]::AppendAllText($requestsPath, "$id|$cursor`n")
      if ($cursor -eq 'page-two') {
        $json = '{"id":' + $id + ',"result":{"data":[{"id":"gpt-6-luna","contextWindow":65536,"supportsTools":true}],"nextCursor":null}}'
      } else {
        $json = '{"id":' + $id + ',"result":{"data":[{"id":"gpt-6.1-sol"}],"nextCursor":"page-two"}}'
      }
      [Console]::Out.WriteLine($json)
    }
  }
}"#;
        let body = body
            .replace(
                "__REQUESTS__",
                &requests.to_string_lossy().replace(char::from(39), "''"),
            )
            .replace(
                "__METHODS__",
                &methods.to_string_lossy().replace(char::from(39), "''"),
            );
        fs::write(&script, body).unwrap();

        let mut profile = codex_probe_fixture_profile(&script);
        profile.models[0].id = "gpt-6-luna".into();
        let probe = probe_codex_account_and_models(&profile, Duration::from_secs(10)).unwrap();
        assert_eq!(probe.authenticated, Some(true));
        assert!(probe.model_list_verified);
        assert_eq!(probe.models.len(), 2);
        let selected = probe
            .models
            .iter()
            .find(|model| model.id == "gpt-6-luna")
            .unwrap();
        assert_eq!(selected.context_tokens, Some(65_536));
        assert_eq!(selected.supports_tools, Some(true));

        let requests = fs::read_to_string(requests).unwrap();
        let rows = requests.lines().collect::<Vec<_>>();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0], "3|");
        assert_eq!(rows[1], "4|page-two");
        let methods = fs::read_to_string(methods).unwrap();
        assert_eq!(
            methods.lines().collect::<Vec<_>>(),
            vec![
                "initialize",
                "initialized",
                "account/read",
                "model/list",
                "model/list"
            ]
        );
        assert!(!methods.contains("thread/start"));
        assert!(!methods.contains("turn/start"));
    }

    #[cfg(windows)]
    #[test]
    fn model_free_codex_probe_rejects_repeated_pagination_cursor() {
        use std::fs;

        let folder = tempdir().unwrap();
        let script = folder.path().join("repeated-cursor codex probe.ps1");
        let requests = folder.path().join("model-list-requests.txt");
        let methods = folder.path().join("protocol-methods.txt");
        let body = r#"$ErrorActionPreference = 'Stop'
$requestsPath = '__REQUESTS__'
$methodsPath = '__METHODS__'
while ($line = [Console]::In.ReadLine()) {
  $message = $line | ConvertFrom-Json
  [IO.File]::AppendAllText($methodsPath, ([string]$message.method + "`n"))
  switch ($message.method) {
    'initialize' { [Console]::Out.WriteLine('{"id":1,"result":{}}') }
    'account/read' { [Console]::Out.WriteLine('{"id":2,"result":{"account":{"type":"chatgpt","requiresOpenaiAuth":true}}}') }
    'model/list' {
      $id = [int]$message.id
      $cursor = $message.params.cursor
      [IO.File]::AppendAllText($requestsPath, "$id|$cursor`n")
      if ($cursor) {
        $json = '{"id":' + $id + ',"result":{"data":[{"id":"second-page-model"}],"nextCursor":"repeat-token"}}'
      } else {
        $json = '{"id":' + $id + ',"result":{"data":[{"id":"other-model"}],"nextCursor":"repeat-token"}}'
      }
      [Console]::Out.WriteLine($json)
    }
  }
}"#;
        let body = body
            .replace(
                "__REQUESTS__",
                &requests.to_string_lossy().replace(char::from(39), "''"),
            )
            .replace(
                "__METHODS__",
                &methods.to_string_lossy().replace(char::from(39), "''"),
            );
        fs::write(&script, body).unwrap();

        let probe = probe_codex_account_and_models(
            &codex_probe_fixture_profile(&script),
            Duration::from_secs(10),
        )
        .unwrap();
        assert_eq!(probe.authenticated, Some(true));
        assert!(!probe.model_list_verified);
        assert!(probe.models.is_empty());
        let requests = fs::read_to_string(requests).unwrap();
        assert_eq!(
            requests.lines().collect::<Vec<_>>(),
            vec!["3|", "4|repeat-token"],
            "must use distinct request IDs and stop before a third repeated-cursor request"
        );
        let methods = fs::read_to_string(methods).unwrap();
        assert_eq!(
            methods.lines().collect::<Vec<_>>(),
            vec![
                "initialize",
                "initialized",
                "account/read",
                "model/list",
                "model/list"
            ]
        );
        assert!(!methods.contains("thread/start"));
        assert!(!methods.contains("turn/start"));
    }

    #[cfg(windows)]
    #[test]
    fn model_free_codex_probe_times_out_on_buffered_powershell_entrypoint() {
        use std::fs;

        let folder = tempdir().unwrap();
        let script = folder.path().join("buffered codex probe.ps1");
        fs::write(
            &script,
            r#"[void][Console]::In.ReadToEnd()
[Console]::Out.WriteLine('{"id":1,"result":{}}')
"#,
        )
        .unwrap();
        let started = Instant::now();
        let result = probe_codex_account_and_models(
            &codex_probe_fixture_profile(&script),
            Duration::from_secs(3),
        );
        assert!(result.is_err());
        assert!(started.elapsed() < Duration::from_secs(6));
    }

    #[cfg(windows)]
    #[test]
    fn codex_buffered_stdio_wrapper_fails_fast_as_unsupported_runtime() {
        use crate::providers::ProviderProfile;
        use std::fs;

        let folder = tempdir().unwrap();
        let script = folder.path().join("buffered codex wrapper.ps1");
        fs::write(
            &script,
            r#"$ErrorActionPreference = 'Stop'
# This models a PowerShell shim that consumes all stdin before starting Node.
[void][Console]::In.ReadToEnd()
[Console]::Out.WriteLine('{"id":1,"result":{}}')
"#,
        )
        .unwrap();
        let profile = ProviderProfile {
            id: "codex-buffered-fixture".into(),
            kind: ProviderKind::Codex,
            name: "Codex buffered fixture".into(),
            executable: script.to_string_lossy().into_owned(),
            endpoint: None,
            secret_ref: None,
            models: vec![ModelProfile {
                id: "fixture-model".into(),
                name: "Fixture model".into(),
                reasoning_efforts: vec![],
                context_window_tokens: None,
                supports_tools: Some(true),
                quality_rating: None,
            }],
        };
        let cancel = AtomicBool::new(false);
        let mut sink = RecordingSink::default();
        let started = Instant::now();
        let result = run_codex_with_startup_timeout(
            &profile,
            "fixture-model",
            folder.path(),
            "Synthetic prompt",
            900,
            None,
            false,
            None,
            &cancel,
            &mut sink,
            None,
            Duration::from_millis(750),
        )
        .unwrap();

        assert_eq!(result.state, RuntimeState::UnsupportedRuntime);
        assert!(result.message.contains("startup handshake"));
        assert!(started.elapsed() < Duration::from_secs(5));
        assert!(sink.0.iter().any(|event| {
            event.kind == RuntimeEventKind::State && event.text.contains("startup handshake")
        }));
    }

    #[cfg(windows)]
    #[test]
    fn codex_startup_and_turn_json_rpc_errors_are_typed_and_bounded() {
        use crate::providers::ProviderProfile;
        use std::fs;

        for (failure_method, detail, expected) in [
            (
                "thread/start",
                "Unauthorized: sign in",
                RuntimeState::AuthenticationRequired,
            ),
            ("turn/start", "429 rate limit", RuntimeState::RateLimited),
            (
                "turn/start",
                "Model not found",
                RuntimeState::UnsupportedModel,
            ),
        ] {
            let folder = tempdir().unwrap();
            let script = folder.path().join("codex json rpc error.ps1");
            let error_response = serde_json::json!({
                "id": if failure_method == "thread/start" { 2 } else { 3 },
                "error": {"code": -32602, "message": detail}
            })
            .to_string();
            let thread_reply = if failure_method == "thread/start" {
                format!("[Console]::Out.WriteLine('{error_response}')")
            } else {
                "[Console]::Out.WriteLine('{\"id\":2,\"result\":{\"thread\":{\"id\":\"fixture-thread\"}}}')".into()
            };
            let turn_reply = if failure_method == "turn/start" {
                format!("[Console]::Out.WriteLine('{error_response}')")
            } else {
                String::new()
            };
            let body = format!(
                r#"$ErrorActionPreference = 'Stop'
while ($line = [Console]::In.ReadLine()) {{
  $message = $line | ConvertFrom-Json
  switch ($message.method) {{
    'initialize' {{ [Console]::Out.WriteLine('{{"id":1,"result":{{}}}}') }}
    'thread/start' {{ {thread_reply} }}
    'turn/start' {{ {turn_reply} }}
  }}
}}"#
            );
            fs::write(&script, body).unwrap();
            let profile = ProviderProfile {
                id: "codex-jsonrpc-fixture".into(),
                kind: ProviderKind::Codex,
                name: "Codex JSON-RPC fixture".into(),
                executable: script.to_string_lossy().into_owned(),
                endpoint: None,
                secret_ref: None,
                models: vec![ModelProfile {
                    id: "fixture-model".into(),
                    name: "Fixture model".into(),
                    reasoning_efforts: vec![],
                    context_window_tokens: None,
                    supports_tools: Some(true),
                    quality_rating: None,
                }],
            };
            let cancel = AtomicBool::new(false);
            let mut sink = RecordingSink::default();
            let started = Instant::now();
            let result = run_codex_with_startup_timeout(
                &profile,
                "fixture-model",
                folder.path(),
                "Synthetic prompt",
                900,
                None,
                false,
                None,
                &cancel,
                &mut sink,
                None,
                Duration::from_secs(2),
            )
            .unwrap();

            assert_eq!(result.state, expected, "{failure_method}: {detail}");
            assert!(started.elapsed() < Duration::from_secs(5));
            assert!(!result.message.contains(detail));
            assert!(sink.0.iter().any(|event| {
                event.kind == RuntimeEventKind::State && !event.text.contains(detail)
            }));
        }
    }

    #[cfg(windows)]
    #[test]
    fn codex_approval_is_forwarded_and_only_resumes_with_an_explicit_response() {
        use crate::providers::ProviderProfile;
        use std::fs;

        struct ApprovalSink(Vec<RuntimeEvent>);
        impl RuntimeEventSink for ApprovalSink {
            fn on_event(&mut self, event: RuntimeEvent) -> RuntimeControl {
                let approval = event.kind == RuntimeEventKind::ApprovalRequest;
                self.0.push(event);
                if approval {
                    RuntimeControl::Respond(serde_json::json!({"decision": "accept"}))
                } else {
                    RuntimeControl::Continue
                }
            }
        }

        let folder = tempdir().unwrap();
        let script = folder.path().join("fake approval server.ps1");
        fs::write(
            &script,
            r#"$ErrorActionPreference = 'Stop'
while ($line = [Console]::In.ReadLine()) {
  $message = $line | ConvertFrom-Json
  switch ($message.method) {
    'initialize' { [Console]::Out.WriteLine('{"id":1,"result":{}}') }
    'thread/start' { [Console]::Out.WriteLine('{"id":2,"result":{"thread":{"id":"approval-thread"}}}') }
    'turn/start' { [Console]::Out.WriteLine('{"id":44,"method":"item/commandExecution/requestApproval","params":{"command":"synthetic command"}}') }
    default {
      if ($message.id -eq 44 -and $message.result.decision -eq 'accept') {
        [Console]::Out.WriteLine('{"method":"turn/completed","params":{"turn":{"status":"completed"}}}')
      }
    }
  }
}"#,
        )
        .unwrap();

        let mut settings = Settings::default();
        settings.providers = vec![ProviderProfile {
            id: "codex-approval-fixture".into(),
            kind: ProviderKind::Codex,
            name: "Codex approval fixture".into(),
            executable: script.to_string_lossy().into_owned(),
            endpoint: None,
            secret_ref: None,
            models: vec![ModelProfile {
                id: "fixture-model".into(),
                name: "Fixture model".into(),
                reasoning_efforts: vec![],
                context_window_tokens: None,
                supports_tools: Some(true),
                quality_rating: None,
            }],
        }];
        let request = RuntimeRequest {
            provider_id: Some("codex-approval-fixture".into()),
            model_id: Some("fixture-model".into()),
            cwd: folder.path().to_path_buf(),
            prompt: "Require approval".into(),
            timeout_seconds: 15,
            reasoning_effort: None,
            read_only: false,
            test_command: None,
            allow_paid: false,
        };
        let cancel = AtomicBool::new(false);
        let mut sink = ApprovalSink(Vec::new());
        let result = run(&settings, &request, &cancel, &mut sink).unwrap();

        assert_eq!(result.state, RuntimeState::Completed);
        assert!(sink
            .0
            .iter()
            .any(|event| event.kind == RuntimeEventKind::ApprovalRequest));
    }

    #[test]
    fn unpaid_local_usage_is_explicitly_distinct_from_unknown_cloud_billing() {
        let local = RuntimeUsage {
            cost_usd: Some(0.0),
            source: Some("local-runtime-policy".into()),
            confidence: Some("high".into()),
            billing: BillingStatus::LocalNoApiCharge,
            ..RuntimeUsage::default()
        };
        let cloud = RuntimeUsage::default();
        assert_eq!(local.cost_usd, Some(0.0));
        assert_eq!(cloud.cost_usd, None);
        assert_eq!(cloud.billing, BillingStatus::Unknown);
    }

    #[test]
    fn normalized_provider_failures_become_actionable_states() {
        assert_eq!(
            classify_failure("Codex authentication is required or expired."),
            RuntimeState::AuthenticationRequired
        );
        assert_eq!(
            classify_failure("Codex rate limit was reached."),
            RuntimeState::RateLimited
        );
        assert_eq!(
            classify_failure("Codex model is unsupported."),
            RuntimeState::UnsupportedModel
        );
    }

    #[test]
    fn paid_openrouter_dispatch_is_rejected_before_process_start() {
        use crate::providers::ModelProfile;
        use crate::providers::ProviderProfile;

        let mut settings = Settings::default();
        settings.providers = vec![ProviderProfile {
            id: "openrouter-fixture".into(),
            kind: ProviderKind::OpenRouter,
            name: "OpenRouter fixture".into(),
            executable: "aider".into(),
            endpoint: None,
            secret_ref: Some("credential-store:openrouter-fixture".into()),
            models: vec![ModelProfile {
                id: "model-fixture".into(),
                name: "Fixture model".into(),
                reasoning_efforts: vec![],
                context_window_tokens: None,
                supports_tools: Some(true),
                quality_rating: None,
            }],
        }];
        let folder = tempdir().unwrap();
        let request = RuntimeRequest {
            provider_id: Some("openrouter-fixture".into()),
            model_id: Some("model-fixture".into()),
            cwd: folder.path().to_path_buf(),
            prompt: "Do not dispatch".into(),
            timeout_seconds: 5,
            reasoning_effort: None,
            read_only: false,
            test_command: None,
            allow_paid: true,
        };
        let cancel = AtomicBool::new(false);
        let mut sink = RecordingSink::default();
        let result = run(&settings, &request, &cancel, &mut sink).unwrap();
        assert_eq!(result.state, RuntimeState::PaidBudgetUnenforceable);
        assert!(sink.0.is_empty());
        assert_eq!(result.usage.cost_usd, None);
    }

    #[cfg(windows)]
    #[test]
    fn antigravity_and_local_ollama_adapters_run_only_the_selected_fixture() {
        use crate::providers::ProviderProfile;
        use std::fs;

        let folder = tempdir().unwrap();
        let executable = folder.path().join("fake coding runtime.ps1");
        fs::write(
            &executable,
            "[Console]::Out.WriteLine('synthetic agent output')\nexit 0\n",
        )
        .unwrap();

        for (kind, id, endpoint) in [
            (ProviderKind::Antigravity, "antigravity-fixture", None),
            (
                ProviderKind::Ollama,
                "ollama-fixture",
                Some("http://127.0.0.1:11434"),
            ),
        ] {
            let mut settings = Settings::default();
            settings.providers = vec![ProviderProfile {
                id: id.into(),
                kind,
                name: format!("{} fixture", kind.label()),
                executable: executable.to_string_lossy().into_owned(),
                endpoint: endpoint.map(str::to_owned),
                secret_ref: None,
                models: vec![ModelProfile {
                    id: "fixture-model".into(),
                    name: "Fixture model".into(),
                    reasoning_efforts: vec![],
                    context_window_tokens: None,
                    supports_tools: Some(true),
                    quality_rating: None,
                }],
            }];
            let request = RuntimeRequest {
                provider_id: Some(id.into()),
                model_id: Some("fixture-model".into()),
                cwd: folder.path().to_path_buf(),
                prompt: "Synthetic fixture task".into(),
                timeout_seconds: 10,
                reasoning_effort: None,
                read_only: false,
                test_command: None,
                allow_paid: false,
            };
            let cancel = AtomicBool::new(false);
            let mut sink = RecordingSink::default();
            let result = run(&settings, &request, &cancel, &mut sink).unwrap();
            assert_eq!(result.state, RuntimeState::Completed, "{}", kind.label());
            assert!(sink
                .0
                .iter()
                .any(|event| event.text.contains("synthetic agent output")));
            if kind == ProviderKind::Ollama {
                assert_eq!(result.usage.cost_usd, Some(0.0));
                assert_eq!(result.usage.billing, BillingStatus::LocalNoApiCharge);
            }
        }
    }
}
