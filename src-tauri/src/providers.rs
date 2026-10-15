use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::mpsc::Receiver;
use std::time::Duration;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ProviderKind {
    Codex,
    Antigravity,
    Ollama,
    OpenRouter,
    Custom,
}

impl ProviderKind {
    pub fn label(self) -> &'static str {
        match self {
            Self::Codex => "Codex",
            Self::Antigravity => "Antigravity",
            Self::Ollama => "Ollama",
            Self::OpenRouter => "OpenRouter",
            Self::Custom => "Custom runtime",
        }
    }

    pub fn default_executable(self) -> &'static str {
        match self {
            Self::Codex => "codex",
            Self::Antigravity => "agy",
            Self::Ollama => "aider",
            Self::OpenRouter => "aider",
            Self::Custom => "",
        }
    }
}

pub const OPENROUTER_API_BASE: &str = "https://openrouter.ai/api/v1";
pub const OPENROUTER_MAX_MODELS: usize = 500;
pub const OPENROUTER_MAX_RESPONSE_BYTES: usize = 8 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OpenRouterModel {
    pub id: String,
    pub name: String,
    pub context_length: Option<u64>,
    pub prompt_price: Option<String>,
    pub completion_price: Option<String>,
    pub input_modalities: Option<Vec<String>>,
    pub output_modalities: Option<Vec<String>>,
    pub supported_parameters: Option<Vec<String>>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OpenRouterProbe {
    pub state: String,
    pub models: Vec<OpenRouterModel>,
    pub note: String,
}

pub fn parse_openrouter_models(value: &Value) -> Vec<OpenRouterModel> {
    value
        .get("data")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|item| {
            let id = item.get("id")?.as_str()?.trim();
            let name = item.get("name")?.as_str()?.trim();
            if id.is_empty()
                || id.len() > 160
                || name.is_empty()
                || name.len() > 160
                || id.chars().any(char::is_control)
                || name.chars().any(char::is_control)
            {
                return None;
            }
            let architecture = item.get("architecture");
            let pricing = item.get("pricing");
            Some(OpenRouterModel {
                id: id.to_owned(),
                name: name.to_owned(),
                context_length: item.get("context_length").and_then(Value::as_u64),
                prompt_price: pricing
                    .and_then(|v| v.get("prompt"))
                    .and_then(Value::as_str)
                    .map(str::to_owned),
                completion_price: pricing
                    .and_then(|v| v.get("completion"))
                    .and_then(Value::as_str)
                    .map(str::to_owned),
                input_modalities: architecture
                    .and_then(|v| v.get("input_modalities"))
                    .and_then(string_array),
                output_modalities: architecture
                    .and_then(|v| v.get("output_modalities"))
                    .and_then(string_array),
                supported_parameters: item.get("supported_parameters").and_then(string_array),
            })
        })
        .take(OPENROUTER_MAX_MODELS)
        .collect()
}

fn string_array(value: &Value) -> Option<Vec<String>> {
    Some(
        value
            .as_array()?
            .iter()
            .map(|item| item.as_str().map(str::to_owned))
            .collect::<Option<Vec<_>>>()?,
    )
}

pub fn openrouter_probe_with<F>(api_key: &str, request: F) -> OpenRouterProbe
where
    F: FnOnce(&str) -> Result<Value, u16>,
{
    if !valid_openrouter_key(api_key) {
        return OpenRouterProbe {
            state: "authenticationRequired".into(),
            models: Vec::new(),
            note: "Save a valid OpenRouter API key in Windows Credential Manager first.".into(),
        };
    }
    match request(api_key) {
        Ok(value) => {
            let models = parse_openrouter_models(&value);
            if models.is_empty() { return OpenRouterProbe { state: "unavailable".into(), models, note: "OpenRouter returned no valid model catalog entries.".into() }; }
            OpenRouterProbe { state: "ready".into(), note: format!("OpenRouter returned {} models. Missing metadata remains unknown.", models.len()), models }
        }
        Err(401 | 403) => OpenRouterProbe { state: "authenticationRequired".into(), models: Vec::new(), note: "OpenRouter rejected the saved API key. Check its validity and access.".into() },
        Err(429) => OpenRouterProbe { state: "rateLimited".into(), models: Vec::new(), note: "OpenRouter rate limited model discovery. Try again later.".into() },
        Err(_) => OpenRouterProbe { state: "unavailable".into(), models: Vec::new(), note: "OpenRouter model discovery failed. The response was omitted to protect credentials.".into() },
    }
}

pub fn valid_openrouter_key(value: &str) -> bool {
    let value = value.trim();
    !value.is_empty() && value.len() <= 4096 && !value.chars().any(char::is_control)
}

pub fn openrouter_credential_target(provider_id: &str) -> Result<String, String> {
    if provider_id.is_empty()
        || provider_id.len() > 80
        || !provider_id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_".contains(c))
    {
        return Err("The OpenRouter provider ID is invalid.".into());
    }
    Ok(format!("navis-fabrica-openrouter-{provider_id}"))
}

pub fn aider_openrouter_arguments(
    model_id: &str,
    prompt_file: &str,
    test_command: Option<&str>,
    empty_env_file: &str,
) -> Vec<String> {
    let mut args = vec![
        "--model".into(),
        format!("openrouter/{model_id}"),
        "--message".into(),
        prompt_file.into(),
        "--yes".into(),
        "--no-auto-commits".into(),
        "--no-gitignore".into(),
        "--no-add-gitignore-files".into(),
        "--no-analytics".into(),
        "--no-check-update".into(),
        "--no-restore-chat-history".into(),
        "--no-suggest-shell-commands".into(),
        "--no-stream".into(),
        "--no-pretty".into(),
        "--env-file".into(),
        empty_env_file.into(),
    ];
    if let Some(command) = test_command.filter(|value| !value.trim().is_empty()) {
        args.extend(["--test-cmd".into(), command.into(), "--auto-test".into()]);
    }
    args
}

pub fn openrouter_http_probe(api_key: &str) -> OpenRouterProbe {
    openrouter_probe_with(api_key, |key| {
        let client = reqwest::blocking::Client::builder()
            .connect_timeout(Duration::from_secs(3))
            .timeout(Duration::from_secs(15))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|_| 0u16)?;
        let response = client
            .get(format!("{OPENROUTER_API_BASE}/models").as_str())
            .bearer_auth(key)
            .send()
            .map_err(|_| 0u16)?;
        if !response.status().is_success() {
            return Err(response.status().as_u16());
        }
        if response
            .content_length()
            .is_some_and(|length| length > OPENROUTER_MAX_RESPONSE_BYTES as u64)
        {
            return Err(0);
        }
        let mut bytes = Vec::new();
        response
            .take(OPENROUTER_MAX_RESPONSE_BYTES as u64 + 1)
            .read_to_end(&mut bytes)
            .map_err(|_| 0u16)?;
        if bytes.len() > OPENROUTER_MAX_RESPONSE_BYTES {
            return Err(0);
        }
        serde_json::from_slice(&bytes).map_err(|_| 0u16)
    })
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
    #[serde(default)]
    pub endpoint: Option<String>,
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
    pub skill_support: bool,
    pub run_start: bool,
    pub run_cancel: bool,
    pub event_stream: bool,
    pub approval_requests: bool,
    pub interactive_input: bool,
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
#[serde(rename_all = "camelCase")]
pub struct AntigravityProbe {
    pub version: Option<String>,
    pub state: String,
    pub models: Vec<String>,
    pub note: String,
}

pub const OLLAMA_DEFAULT_ENDPOINT: &str = "http://127.0.0.1:11434";
pub const OLLAMA_MIN_CONTEXT_TOKENS: u64 = 8_192;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OllamaModel {
    pub id: String,
    pub size_bytes: u64,
    pub context_tokens: Option<u64>,
    pub tools_supported: bool,
    pub eligible: bool,
    pub note: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OllamaProbe {
    pub state: String,
    pub endpoint: String,
    pub models: Vec<OllamaModel>,
    pub available_memory_bytes: Option<u64>,
    pub note: String,
}

pub fn validate_ollama_endpoint(endpoint: &str) -> Result<reqwest::Url, String> {
    let url = reqwest::Url::parse(endpoint.trim())
        .map_err(|_| "Ollama must use a valid local HTTP endpoint.".to_string())?;
    let loopback = url
        .host_str()
        .and_then(|host| {
            host.strip_prefix('[')
                .and_then(|value| value.strip_suffix(']'))
                .unwrap_or(host)
                .parse::<std::net::IpAddr>()
                .ok()
        })
        .is_some_and(|address| address.is_loopback());
    if url.scheme() != "http"
        || !loopback
        || url.port().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || (url.path() != "" && url.path() != "/")
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err("Ollama profiles are local-only: use HTTP with a numeric loopback address and port, without credentials or a URL path.".into());
    }
    Ok(url)
}

pub fn ollama_models_from_responses(tags: &Value, details: &[(String, Value)]) -> Vec<OllamaModel> {
    let detail_by_id = details
        .iter()
        .map(|(id, value)| (id.as_str(), value))
        .collect::<std::collections::HashMap<_, _>>();
    tags.get("models")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|item| {
            let id = item
                .get("name")
                .or_else(|| item.get("model"))
                .and_then(Value::as_str)?;
            if id.trim().is_empty() || id.len() > 160 || id.chars().any(char::is_control) {
                return None;
            }
            let detail = detail_by_id.get(id).copied();
            let info = detail.and_then(|value| value.get("model_info"));
            let context_tokens = info.and_then(|value| {
                value.as_object()?.iter().find_map(|(key, value)| {
                    key.ends_with(".context_length")
                        .then(|| value.as_u64())
                        .flatten()
                })
            });
            let tools_supported = detail
                .and_then(|value| value.get("capabilities"))
                .and_then(Value::as_array)
                .is_some_and(|caps| caps.iter().any(|cap| cap.as_str() == Some("tools")));
            let eligible = tools_supported
                && context_tokens.is_some_and(|value| value >= OLLAMA_MIN_CONTEXT_TOKENS);
            let note = if !tools_supported {
                "Ollama did not report tool-calling support for this model.".into()
            } else if context_tokens.is_none() {
                "Model context length is unknown; this model cannot be selected for agent work."
                    .into()
            } else if context_tokens.is_some_and(|value| value < OLLAMA_MIN_CONTEXT_TOKENS) {
                "Model context is below the 8,192-token minimum for agent work.".into()
            } else {
                "Tool calling and the minimum agent context were reported by Ollama.".into()
            };
            Some(OllamaModel {
                id: id.into(),
                size_bytes: item.get("size").and_then(Value::as_u64).unwrap_or(0),
                context_tokens,
                tools_supported,
                eligible,
                note,
            })
        })
        .take(50)
        .collect()
}

pub fn ollama_probe(endpoint: &str) -> OllamaProbe {
    let normalized = match validate_ollama_endpoint(endpoint) {
        Ok(url) => url,
        Err(message) => {
            return OllamaProbe {
                state: "unavailable".into(),
                endpoint: endpoint.into(),
                models: Vec::new(),
                available_memory_bytes: available_memory_bytes(),
                note: message,
            }
        }
    };
    let endpoint = normalized.as_str().trim_end_matches('/').to_owned();
    let client = match reqwest::blocking::Client::builder()
        .connect_timeout(Duration::from_millis(500))
        .timeout(Duration::from_secs(3))
        .redirect(reqwest::redirect::Policy::none())
        .build()
    {
        Ok(client) => client,
        Err(_) => {
            return OllamaProbe {
                state: "unavailable".into(),
                endpoint,
                models: Vec::new(),
                available_memory_bytes: available_memory_bytes(),
                note: "Could not initialize the local Ollama health check.".into(),
            }
        }
    };
    let tags: Value = match client.get(format!("{endpoint}/api/tags")).send().and_then(|response| response.error_for_status()).and_then(|response| response.json()) {
        Ok(value) => value,
        Err(_) => return OllamaProbe { state: "unavailable".into(), endpoint, models: Vec::new(), available_memory_bytes: available_memory_bytes(), note: "Ollama is not reachable at the configured local endpoint, or its model-list request failed.".into() },
    };
    let model_ids = tags
        .get("models")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .take(50)
        .filter_map(|item| {
            item.get("name")
                .or_else(|| item.get("model"))
                .and_then(Value::as_str)
                .map(str::to_owned)
        })
        .collect::<Vec<_>>();
    let details = std::thread::scope(|scope| {
        let workers = model_ids.chunks(8).map(|models| {
            let models = models.to_vec();
            let client = client.clone();
            let endpoint = endpoint.clone();
            scope.spawn(move || {
                models
                    .into_iter()
                    .filter_map(|id| {
                        let value = client
                            .post(format!("{endpoint}/api/show"))
                            .json(&json!({ "model": id, "verbose": false }))
                            .send()
                            .and_then(|response| response.error_for_status())
                            .and_then(|response| response.json::<Value>());
                        value.ok().map(|value| (id, value))
                    })
                    .collect::<Vec<_>>()
            })
        });
        workers
            .map(|worker| worker.join().unwrap_or_default())
            .flatten()
            .collect::<Vec<_>>()
    });
    let parsed = ollama_models_from_responses(&tags, &details);
    let eligible = parsed.iter().filter(|model| model.eligible).count();
    OllamaProbe {
        state: "ready".into(), endpoint, models: parsed,
        available_memory_bytes: available_memory_bytes(),
        note: format!("Ollama is reachable. Up to 50 local models were inspected; {eligible} report tool support and at least {OLLAMA_MIN_CONTEXT_TOKENS} context tokens. Unknown capabilities remain ineligible."),
    }
}

#[cfg(windows)]
fn available_memory_bytes() -> Option<u64> {
    use windows_sys::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
    let mut status = MEMORYSTATUSEX {
        dwLength: std::mem::size_of::<MEMORYSTATUSEX>() as u32,
        ..unsafe { std::mem::zeroed() }
    };
    (unsafe { GlobalMemoryStatusEx(&mut status) } != 0).then_some(status.ullAvailPhys)
}

#[cfg(not(windows))]
fn available_memory_bytes() -> Option<u64> {
    None
}

pub fn ollama_timeout_seconds(
    model_size_bytes: u64,
    available_memory_bytes: Option<u64>,
) -> Option<u64> {
    let available = available_memory_bytes?;
    if model_size_bytes == 0 || available == 0 {
        return None;
    }
    let ratio = available as f64 / model_size_bytes as f64;
    Some((1_200.0 / ratio.sqrt()).round().clamp(300.0, 3_600.0) as u64)
}

pub fn aider_ollama_arguments(
    model_id: &str,
    prompt_file: &str,
    test_command: Option<&str>,
    empty_env_file: &str,
) -> Vec<String> {
    let mut args = vec![
        "--model".into(),
        format!("ollama_chat/{model_id}"),
        "--message".into(),
        prompt_file.into(),
        "--yes".into(),
        "--no-auto-commits".into(),
        "--no-gitignore".into(),
        "--no-add-gitignore-files".into(),
        "--no-analytics".into(),
        "--no-check-update".into(),
        "--no-restore-chat-history".into(),
        "--no-suggest-shell-commands".into(),
        "--no-stream".into(),
        "--no-pretty".into(),
        "--env-file".into(),
        empty_env_file.into(),
    ];
    if let Some(test_command) = test_command.filter(|value| !value.trim().is_empty()) {
        args.extend([
            "--test-cmd".into(),
            test_command.into(),
            "--auto-test".into(),
        ]);
    }
    args
}

pub fn probe_antigravity(executable: &Path) -> AntigravityProbe {
    let temp = match tempfile::tempdir() {
        Ok(temp) => temp,
        Err(_) => {
            return AntigravityProbe {
                version: None,
                state: "unavailable".into(),
                models: Vec::new(),
                note: "Could not create a temporary directory for the provider probe.".into(),
            }
        }
    };
    let version = Command::new(executable)
        .current_dir(temp.path())
        .arg("--version")
        .output()
        .ok()
        .and_then(|output| {
            output
                .status
                .success()
                .then(|| String::from_utf8_lossy(&output.stdout).trim().to_owned())
        })
        .filter(|value| !value.is_empty());
    if version.is_none() {
        return AntigravityProbe {
            version: None,
            state: "unavailable".into(),
            models: Vec::new(),
            note: "The Antigravity CLI version probe could not complete.".into(),
        };
    }
    let models = Command::new(executable)
        .current_dir(temp.path())
        .arg("models")
        .output();
    match models {
        Ok(output) if output.status.success() => {
            let models = String::from_utf8_lossy(&output.stdout)
                .lines()
                .filter_map(|line| line.split_whitespace().next())
                .filter(|name| name.starts_with("gemini-"))
                .map(str::to_owned)
                .take(200)
                .collect::<Vec<_>>();
            AntigravityProbe {
                version,
                state: "ready".into(),
                note: if models.is_empty() {
                    "The CLI is available, but it returned no model identifiers.".into()
                } else {
                    "Authenticated CLI; model IDs were read from agy models.".into()
                },
                models,
            }
        }
        Ok(output) => {
            let diagnostic = format!(
                "{} {}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            )
            .to_ascii_lowercase();
            let authentication_required = [
                "sign in",
                "not logged",
                "authentication required",
                "auth required",
            ]
            .iter()
            .any(|marker| diagnostic.contains(marker));
            AntigravityProbe {
                version,
                state: if authentication_required {
                    "authenticationRequired"
                } else {
                    "unavailable"
                }
                .into(),
                models: Vec::new(),
                note: if authentication_required {
                    "The CLI is installed, but model discovery requires an Antigravity sign-in. No credentials were read into Navis-Fabrica.".into()
                } else {
                    "The CLI is installed, but model discovery is unavailable. Diagnostics were omitted to avoid exposing credential data.".into()
                },
            }
        }
        Err(_) => AntigravityProbe {
            version,
            state: "unavailable".into(),
            models: Vec::new(),
            note: "The Antigravity model-list command could not start.".into(),
        },
    }
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodexEvent {
    pub kind: String,
    pub text: String,
    pub terminal_state: Option<String>,
}

pub fn antigravity_arguments(model_id: &str, prompt: &str, timeout_seconds: u64) -> Vec<String> {
    let arguments = vec![
        "--sandbox".into(),
        "--print-timeout".into(),
        format!("{timeout_seconds}s"),
        "--model".into(),
        model_id.into(),
        "--print".into(),
        prompt.into(),
    ];
    arguments
}

pub fn app_server_initialize(request_id: u64, app_version: &str) -> Value {
    json!({
        "id": request_id,
        "method": "initialize",
        "params": {
            "clientInfo": { "name": "navis_fabrica", "title": "Navis-Fabrica", "version": app_version },
            "capabilities": { "experimentalApi": true, "requestAttestation": false }
        }
    })
}

pub fn app_server_thread_start(request_id: u64, cwd: &str, model_id: &str) -> Value {
    json!({
        "id": request_id,
        "method": "thread/start",
        "params": {
            "cwd": cwd,
            "runtimeWorkspaceRoots": [cwd],
            "model": model_id,
            "allowProviderModelFallback": false,
            "approvalPolicy": "on-request",
            "sandbox": "workspace-write",
            "ephemeral": false
        }
    })
}

pub fn app_server_turn_start(
    request_id: u64,
    thread_id: &str,
    cwd: &str,
    model_id: &str,
    effort: Option<&str>,
    prompt: &str,
    skill: Option<(&str, &str)>,
) -> Value {
    let mut input = Vec::new();
    if let Some((name, path)) = skill {
        input.push(json!({ "type": "skill", "name": name, "path": path }));
    }
    input.push(json!({ "type": "text", "text": prompt, "text_elements": [] }));
    let mut params = json!({
        "threadId": thread_id,
        "cwd": cwd,
        "runtimeWorkspaceRoots": [cwd],
        "model": model_id,
        "approvalPolicy": "on-request",
        "input": input
    });
    if let Some(effort) = effort {
        params["effort"] = json!(effort);
    }
    json!({ "id": request_id, "method": "turn/start", "params": params })
}

pub fn app_server_response(request_id: Value, result: Value) -> Value {
    json!({ "id": request_id, "result": result })
}

pub fn parse_app_server_message(line: &str) -> Result<Value, String> {
    let value: Value = serde_json::from_str(line)
        .map_err(|_| "Codex app-server emitted malformed JSON.".to_string())?;
    if !value.is_object()
        || value
            .get("jsonrpc")
            .is_some_and(|version| version.as_str() != Some("2.0"))
    {
        return Err("Codex app-server emitted an invalid protocol message.".into());
    }
    if value
        .get("method")
        .is_some_and(|method| !method.is_string())
    {
        return Err("Codex app-server emitted an invalid protocol method.".into());
    }
    Ok(value)
}

pub fn normalize_app_server_message(message: &Value) -> Option<CodexEvent> {
    let method = message.get("method")?.as_str()?;
    let params = message.get("params").unwrap_or(&Value::Null);
    match method {
        "item/agentMessage/delta" => Some(CodexEvent {
            kind: "output".into(),
            text: params.get("delta")?.as_str()?.into(),
            terminal_state: None,
        }),
        "item/completed" => {
            let item = params.get("item")?;
            if item.get("type")?.as_str()? != "agentMessage" {
                return None;
            }
            Some(CodexEvent {
                kind: "finalOutput".into(),
                text: item.get("text")?.as_str()?.into(),
                terminal_state: None,
            })
        }
        "turn/completed" => {
            let status = params.get("turn")?.get("status")?.as_str()?;
            let state = match status {
                "completed" => "completed",
                "interrupted" => "cancelled",
                _ => "failed",
            };
            let message = params
                .get("turn")
                .and_then(|turn| turn.get("error"))
                .and_then(|error| error.get("message"))
                .and_then(Value::as_str)
                .unwrap_or("");
            Some(CodexEvent {
                kind: if state == "failed" {
                    "providerError"
                } else {
                    "state"
                }
                .into(),
                text: if state == "failed" {
                    if is_authentication_error(message) {
                        "Codex authentication is required or expired.".into()
                    } else {
                        "Codex reported that the turn failed.".into()
                    }
                } else {
                    state.into()
                },
                terminal_state: Some(state.into()),
            })
        }
        "error" => {
            let message = params.get("message").and_then(Value::as_str).unwrap_or("");
            Some(CodexEvent {
                kind: "providerError".into(),
                text: if is_authentication_error(message) {
                    "Codex authentication is required or expired.".into()
                } else {
                    "Codex app-server reported an error.".into()
                },
                terminal_state: Some("failed".into()),
            })
        }
        "item/commandExecution/requestApproval"
        | "item/fileChange/requestApproval"
        | "item/permissions/requestApproval" => Some(CodexEvent {
            kind: "approvalRequest".into(),
            text: serde_json::to_string(&json!({
                "requestId": message.get("id"),
                "method": method,
                "params": {
                    "kind": params.get("kind"),
                    "reason": params.get("reason"),
                    "command": params.get("command"),
                    "cwd": params.get("cwd"),
                    "permissions": params.get("permissions"),
                    "availableDecisions": params.get("availableDecisions")
                }
            }))
            .ok()?,
            terminal_state: None,
        }),
        "item/tool/requestUserInput" => Some(CodexEvent {
            kind: "userInputRequest".into(),
            text: serde_json::to_string(&json!({
                "requestId": message.get("id"),
                "method": method,
                "questions": params.get("questions")
            }))
            .ok()?,
            terminal_state: None,
        }),
        _ => None,
    }
}

fn is_authentication_error(message: &str) -> bool {
    let message = message.to_ascii_lowercase();
    [
        "unauthorized",
        "authentication",
        "sign in",
        "login required",
        "401",
        "invalid api key",
    ]
    .iter()
    .any(|needle| message.contains(needle))
}

pub fn normalize_codex_stderr(message: &str) -> Option<CodexEvent> {
    is_authentication_error(message).then(|| CodexEvent {
        kind: "providerError".into(),
        text: "Codex authentication is required or expired.".into(),
        terminal_state: Some("failed".into()),
    })
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
    let capabilities = match profile.kind {
        ProviderKind::Codex => AdapterCapabilities {
            run_start: true,
            run_cancel: true,
            event_stream: true,
            skill_support: true,
            approval_requests: true,
            interactive_input: true,
            ..AdapterCapabilities::local_preflight()
        },
        ProviderKind::Antigravity => AdapterCapabilities {
            health: true,
            model_listing: true,
            run_start: true,
            run_cancel: true,
            event_stream: true,
            skill_support: true,
            ..AdapterCapabilities::local_preflight()
        },
        ProviderKind::Ollama => AdapterCapabilities {
            health: true,
            model_listing: true,
            run_start: true,
            run_cancel: true,
            event_stream: true,
            checkpoints: true,
            ..AdapterCapabilities::local_preflight()
        },
        ProviderKind::OpenRouter => AdapterCapabilities {
            health: true,
            model_listing: true,
            run_start: true,
            run_cancel: true,
            event_stream: true,
            checkpoints: true,
            ..AdapterCapabilities::local_preflight()
        },
        _ => AdapterCapabilities::local_preflight(),
    };
    ProviderAvailability {
        provider_id: profile.id.clone(),
        executable_found: executable_path.is_some(),
        executable_path: executable_path.map(|path| path.to_string_lossy().into_owned()),
        authentication_reference: if profile
            .secret_ref
            .as_deref()
            .is_some_and(|value| !value.trim().is_empty())
        {
            AuthenticationReferenceStatus::ReferenceConfigured
        } else {
            AuthenticationReferenceStatus::Missing
        },
        capabilities,
        note: if profile.kind == ProviderKind::Codex {
            "Codex app-server runs are supported with streamed output, on-request approvals, and user input. Authentication is checked when a run starts; this check never launches the provider.".into()
        } else if profile.kind == ProviderKind::Antigravity {
            "Antigravity uses the installed agy --print CLI with process output and cancellation. This version does not expose structured progress or reasoning-effort flags; model listing requires sign-in. Headless approvals, interactive input, and resumable checkpoints are unavailable.".into()
        } else if profile.kind == ProviderKind::Ollama {
            "Ollama discovery is restricted to a numeric loopback endpoint. Aider provides model-driven edits and configured test execution; model tool-call support and context are checked before runs. Portable handoff is explicit and does not resume completed side effects.".into()
        } else if profile.kind == ProviderKind::OpenRouter {
            "OpenRouter provides a model catalog and API access. Aider is the separately configured coding harness; shell and file-edit behavior comes from that harness. Authentication is stored in Windows Credential Manager.".into()
        } else {
            "Executable presence is checked without launching it. Authentication validity and provider operations are not implemented by this adapter.".into()
        },
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
        if profile.kind == ProviderKind::Ollama {
            validate_ollama_endpoint(
                profile
                    .endpoint
                    .as_deref()
                    .unwrap_or(OLLAMA_DEFAULT_ENDPOINT),
            )?;
            if profile.secret_ref.is_some() {
                return Err(
                    "Local-only Ollama profiles cannot store or use remote provider credentials."
                        .into(),
                );
            }
        } else if profile.endpoint.is_some() {
            return Err("Only Ollama profiles may configure an endpoint.".into());
        }
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
            endpoint: None,
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
        assert!(status.capabilities.run_start);
        assert!(status.capabilities.run_cancel);
        assert!(status.capabilities.event_stream);
        assert!(status.capabilities.approval_requests);
        assert!(status.capabilities.interactive_input);
        assert!(!status.capabilities.checkpoints);
        assert!(status.note.contains("never launches"));
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

    #[test]
    fn antigravity_command_uses_documented_flags_and_keeps_prompt_as_one_argument() {
        let args = antigravity_arguments("gemini-test", "echo `danger`", 90);
        assert_eq!(
            args,
            vec![
                "--sandbox",
                "--print-timeout",
                "90s",
                "--model",
                "gemini-test",
                "--print",
                "echo `danger`"
            ]
        );
    }

    #[test]
    fn antigravity_capabilities_do_not_claim_interaction_support() {
        let mut profile = profile();
        profile.kind = ProviderKind::Antigravity;
        let status = inspect(&profile, None);
        assert!(status.capabilities.health);
        assert!(status.capabilities.model_listing);
        assert!(status.capabilities.skill_support);
        assert!(
            status.capabilities.run_start
                && status.capabilities.run_cancel
                && status.capabilities.event_stream
        );
        assert!(
            !status.capabilities.approval_requests
                && !status.capabilities.interactive_input
                && !status.capabilities.checkpoints
        );
    }

    #[test]
    fn ollama_profiles_accept_only_credential_free_numeric_loopback_endpoints() {
        assert!(validate_ollama_endpoint("http://127.0.0.1:11434").is_ok());
        assert!(validate_ollama_endpoint("http://[::1]:11434").is_ok());
        for endpoint in [
            "https://127.0.0.1:11434",
            "http://localhost:11434",
            "http://192.168.1.5:11434",
            "http://user:pass@127.0.0.1:11434",
            "http://127.0.0.1:11434/api",
        ] {
            assert!(
                validate_ollama_endpoint(endpoint).is_err(),
                "accepted {endpoint}"
            );
        }
        let mut local = profile();
        local.kind = ProviderKind::Ollama;
        local.secret_ref = None;
        local.endpoint = Some("http://127.0.0.1:11434".into());
        assert!(validate_profiles(&[local.clone()], &ProviderSelection::default()).is_ok());
        local.secret_ref = Some("credential-store:cloud-key".into());
        assert!(validate_profiles(&[local], &ProviderSelection::default()).is_err());
    }

    #[test]
    fn ollama_model_discovery_requires_reported_tool_support_and_context() {
        let tags = json!({"models": [
            {"name": "agent-ready", "size": 8_000_000_000u64},
            {"name": "no-tools", "size": 2_000_000_000u64},
            {"name": "unknown-context", "size": 2_000_000_000u64}
        ]});
        let details = vec![
            (
                "agent-ready".into(),
                json!({"capabilities": ["completion", "tools"], "model_info": {"model.context_length": 32768}}),
            ),
            (
                "no-tools".into(),
                json!({"capabilities": ["completion"], "model_info": {"model.context_length": 32768}}),
            ),
            (
                "unknown-context".into(),
                json!({"capabilities": ["completion", "tools"], "model_info": {}}),
            ),
        ];
        let models = ollama_models_from_responses(&tags, &details);
        assert_eq!(models.len(), 3);
        assert!(models[0].eligible);
        assert!(!models[1].eligible && !models[1].tools_supported);
        assert!(!models[2].eligible && models[2].context_tokens.is_none());
    }

    #[test]
    fn ollama_timeout_scales_with_available_memory_and_model_size() {
        assert_eq!(
            ollama_timeout_seconds(8_000_000_000, Some(8_000_000_000)),
            Some(1200)
        );
        assert!(ollama_timeout_seconds(8_000_000_000, Some(2_000_000_000)).unwrap() > 1200);
        assert_eq!(ollama_timeout_seconds(0, Some(8_000_000_000)), None);
        assert_eq!(ollama_timeout_seconds(8_000_000_000, None), None);
    }

    #[test]
    fn aider_invocation_pins_local_chat_model_and_disables_remote_side_effects() {
        let args = aider_ollama_arguments("qwen3", "Change the module", Some("cargo test"), "NUL");
        assert!(args
            .windows(2)
            .any(|pair| pair == ["--model", "ollama_chat/qwen3"]));
        assert!(args
            .windows(2)
            .any(|pair| pair == ["--message", "Change the module"]));
        assert!(!args.contains(&"--message-file".into()));
        assert!(args
            .windows(2)
            .any(|pair| pair == ["--test-cmd", "cargo test"]));
        assert!(args.contains(&"--no-auto-commits".into()));
        assert!(args.contains(&"--no-analytics".into()));
        assert!(args.windows(2).any(|pair| pair == ["--env-file", "NUL"]));
    }

    #[test]
    fn app_server_requests_pin_project_model_effort_and_skill() {
        let start = app_server_thread_start(2, r"C:\Projects\sample", "model-a");
        assert_eq!(start["method"], "thread/start");
        assert!(start.get("jsonrpc").is_none());
        assert_eq!(start["params"]["cwd"], r"C:\Projects\sample");
        assert_eq!(
            start["params"]["runtimeWorkspaceRoots"][0],
            r"C:\Projects\sample"
        );
        assert_eq!(start["params"]["model"], "model-a");
        assert_eq!(start["params"]["approvalPolicy"], "on-request");
        let turn = app_server_turn_start(
            3,
            "thread-id",
            r"C:\Projects\sample",
            "model-a",
            Some("high"),
            "Make this change",
            Some(("dev-work", r"C:\Users\me\.codex\skills\dev-work\SKILL.md")),
        );
        assert!(turn.get("jsonrpc").is_none());
        assert_eq!(turn["params"]["effort"], "high");
        assert_eq!(turn["params"]["input"][0]["type"], "skill");
        assert_eq!(turn["params"]["input"][1]["text"], "Make this change");
    }

    #[test]
    fn app_server_stream_normalizes_deltas_final_output_failures_and_requests() {
        let delta = serde_json::json!({
            "method": "item/agentMessage/delta",
            "params": { "delta": "Hello" }
        });
        let event = normalize_app_server_message(&delta).unwrap();
        assert_eq!(event.kind, "output");
        assert_eq!(event.text, "Hello");

        let final_message = serde_json::json!({
            "method": "item/completed",
            "params": { "item": { "type": "agentMessage", "text": "Final answer" } }
        });
        assert_eq!(
            normalize_app_server_message(&final_message).unwrap().kind,
            "finalOutput"
        );

        let auth_error = serde_json::json!({
            "method": "turn/completed",
            "params": { "turn": { "status": "failed", "error": { "message": "401 unauthorized" } } }
        });
        let event = normalize_app_server_message(&auth_error).unwrap();
        assert_eq!(event.terminal_state.as_deref(), Some("failed"));
        assert_eq!(event.text, "Codex authentication is required or expired.");

        let request = serde_json::json!({
            "id": 42,
            "method": "item/commandExecution/requestApproval",
            "params": { "command": "git status", "availableDecisions": ["accept", "decline"] }
        });
        let event = normalize_app_server_message(&request).unwrap();
        assert_eq!(event.kind, "approvalRequest");
        let display: serde_json::Value = serde_json::from_str(&event.text).unwrap();
        assert_eq!(display["requestId"], 42);
        assert_eq!(display["params"]["availableDecisions"][0], "accept");

        let permission_request = serde_json::json!({
            "id": 43,
            "method": "item/permissions/requestApproval",
            "params": { "reason": "Allow a workspace root", "permissions": { "fileSystem": { "write": ["/workspace"] } } }
        });
        let event = normalize_app_server_message(&permission_request).unwrap();
        let display: serde_json::Value = serde_json::from_str(&event.text).unwrap();
        assert_eq!(display["method"], "item/permissions/requestApproval");
        assert_eq!(
            display["params"]["permissions"]["fileSystem"]["write"][0],
            "/workspace"
        );
    }

    #[test]
    fn app_server_parser_accepts_codex_wire_messages_without_jsonrpc() {
        assert!(parse_app_server_message("not-json").is_err());
        assert!(parse_app_server_message("[]").is_err());
        assert!(
            parse_app_server_message(r#"{"jsonrpc":"1.0","method":"turn/completed"}"#).is_err()
        );
        assert!(
            parse_app_server_message(r#"{"id":1,"result":{"userAgent":"codex_cli/0.162.1"}}"#)
                .is_ok()
        );
        assert!(parse_app_server_message(
            r#"{"method":"turn/completed","params":{"turn":{"status":"completed"}}}"#
        )
        .is_ok());
        assert!(parse_app_server_message(
            r#"{"jsonrpc":"2.0","method":"turn/completed","params":{}}"#
        )
        .is_ok());
    }

    #[test]
    fn app_server_messages_follow_codex_wire_shape() {
        let initialize = app_server_initialize(1, "0.14.0");
        let thread = app_server_thread_start(2, "/workspace", "model-a");
        let response = app_server_response(serde_json::json!(3), json!({}));
        assert!(initialize.get("jsonrpc").is_none());
        assert!(thread.get("jsonrpc").is_none());
        assert!(response.get("jsonrpc").is_none());
        assert_eq!(response["id"], 3);
    }

    #[test]
    fn normalizes_authentication_failures_from_runtime_stderr() {
        let event = normalize_codex_stderr("Unauthorized: please sign in").unwrap();
        assert_eq!(event.kind, "providerError");
        assert_eq!(event.terminal_state.as_deref(), Some("failed"));
        assert_eq!(event.text, "Codex authentication is required or expired.");
        assert!(normalize_codex_stderr("temporary filesystem warning").is_none());
    }

    #[test]
    fn openrouter_catalog_preserves_reported_metadata_and_unknowns() {
        let value = json!({"data": [
            {"id":"vendor/model-a", "name":"Model A", "context_length":32768,
             "pricing":{"prompt":"0.000001", "completion":"0.000002"},
             "architecture":{"input_modalities":["text"], "output_modalities":["text"]},
             "supported_parameters":["tools"]},
            {"id":"vendor/model-b", "name":"Model B", "pricing":{}, "architecture":{}},
            {"id":"bad", "name":""}
        ]});
        let models = parse_openrouter_models(&value);
        assert_eq!(models.len(), 2);
        assert_eq!(models[0].context_length, Some(32768));
        assert_eq!(models[0].prompt_price.as_deref(), Some("0.000001"));
        assert_eq!(
            models[0].supported_parameters.as_deref(),
            Some(["tools".to_string()].as_slice())
        );
        assert_eq!(models[1].context_length, None);
        assert_eq!(models[1].prompt_price, None);
        assert_eq!(models[1].input_modalities, None);
    }

    #[test]
    fn openrouter_probe_maps_auth_rate_limit_and_malformed_catalog_without_leaking() {
        let rejected = openrouter_probe_with("sk-or-valid", |_| Err(401));
        assert_eq!(rejected.state, "authenticationRequired");
        let limited = openrouter_probe_with("sk-or-valid", |_| Err(429));
        assert_eq!(limited.state, "rateLimited");
        let malformed =
            openrouter_probe_with("sk-or-valid", |_| Ok(json!({"error":"sk-or-secret"})));
        assert_eq!(malformed.state, "unavailable");
        assert!(!malformed.note.contains("sk-or-secret"));
        assert_eq!(
            openrouter_probe_with("", |_| panic!("must not request")).state,
            "authenticationRequired"
        );
    }

    #[test]
    fn openrouter_credentials_are_validated_and_aider_receives_no_key_in_arguments() {
        assert_eq!(
            openrouter_credential_target("openRouter-a1b2").unwrap(),
            "navis-fabrica-openrouter-openRouter-a1b2"
        );
        assert!(openrouter_credential_target("../key").is_err());
        assert!(valid_openrouter_key("sk-or-v1-value"));
        assert!(!valid_openrouter_key("bad\nkey"));
        let args =
            aider_openrouter_arguments("vendor/model", "change files", Some("npm test"), "NUL");
        assert!(args
            .windows(2)
            .any(|pair| pair == ["--model", "openrouter/vendor/model"]));
        assert!(args.windows(2).any(|pair| pair == ["--env-file", "NUL"]));
        assert!(!args.iter().any(|arg| arg.contains("sk-or-")));
    }
}
