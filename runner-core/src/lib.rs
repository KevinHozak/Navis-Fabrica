//! Headless shared core used by the Fabrica IPC bridge and `fabrica-runner` CLI.
//! This crate has no Tauri AppHandle or WebView dependency.

pub mod budgets;
pub mod credentials;
pub mod isolation;
pub mod issues;
pub mod jev;
pub mod jobs;
pub mod paid_gateway;
pub mod process;
pub mod provider_observations;
pub mod providers;
pub mod routing;
pub mod runtime;
pub mod settings;
pub mod supervisor;
pub mod supervisor_protocol;
pub mod usage;
pub mod verification;

use issues::{IssueError, IssueRecord, IssueStore};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::sync::Once;

pub const PROTOCOL_VERSION: u32 = 1;
const MAX_CONFIG_BYTES: u64 = 64 * 1024;
const MAX_BATCH_REQUEST_BYTES: u64 = 1024 * 1024;
const MAX_CORE_RUNTIME_EVENT_BYTES: usize = 2 * 1024 * 1024;
static TLS_PROVIDER: Once = Once::new();

pub fn initialize_crypto_provider() {
    TLS_PROVIDER.call_once(|| {
        let _ = rustls::crypto::ring::default_provider().install_default();
    });
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RunnerConfig {
    pub schema_version: u32,
    #[serde(default = "default_issue_scope")]
    pub issue_scope: String,
    #[serde(default = "default_issues_dir")]
    pub issues_dir: PathBuf,
}

fn default_issue_scope() -> String {
    "FAB".into()
}
fn default_issues_dir() -> PathBuf {
    PathBuf::from("issues")
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Operation {
    Capabilities,
    Doctor,
    IssuesList,
    IssuesPage,
    IssuesShow,
    IssuesCreate,
    IssuesUpdate,
    IssuesDelete,
    IssuesValidate,
    Plan,
    ProviderSettings,
    ProvidersDoctor,
    #[serde(rename = "provider-observations-refresh")]
    RefreshProviderObservations,
    RuntimeRun,
    BatchPreview,
    Submit,
    RunBatch,
    Status,
    Events,
    Pause,
    Resume,
    Stop,
    Cancel,
    Reconcile,
    Recover,
}

impl Operation {
    pub fn from_cli(value: &str) -> Option<Self> {
        Some(match value {
            "capabilities" => Self::Capabilities,
            "doctor" => Self::Doctor,
            "issues-list" => Self::IssuesList,
            "issues-page" => Self::IssuesPage,
            "issues-show" => Self::IssuesShow,
            "issues-create" => Self::IssuesCreate,
            "issues-update" => Self::IssuesUpdate,
            "issues-delete" => Self::IssuesDelete,
            "issues-validate" => Self::IssuesValidate,
            "plan" => Self::Plan,
            "provider-settings" => Self::ProviderSettings,
            "providers-doctor" => Self::ProvidersDoctor,
            "provider-observations-refresh" => Self::RefreshProviderObservations,
            "runtime-run" => Self::RuntimeRun,
            "batch-preview" => Self::BatchPreview,
            "submit" => Self::Submit,
            "run-batch" => Self::RunBatch,
            "status" => Self::Status,
            "events" => Self::Events,
            "pause" => Self::Pause,
            "resume" => Self::Resume,
            "stop" => Self::Stop,
            "cancel" => Self::Cancel,
            "reconcile" => Self::Reconcile,
            "recover" => Self::Recover,
            _ => return None,
        })
    }

    fn cli_name(self) -> &'static str {
        match self {
            Self::Capabilities => "capabilities",
            Self::Doctor => "doctor",
            Self::IssuesList => "issues-list",
            Self::IssuesPage => "issues-page",
            Self::IssuesShow => "issues-show",
            Self::IssuesCreate => "issues-create",
            Self::IssuesUpdate => "issues-update",
            Self::IssuesDelete => "issues-delete",
            Self::IssuesValidate => "issues-validate",
            Self::Plan => "plan",
            Self::ProviderSettings => "provider-settings",
            Self::ProvidersDoctor => "providers-doctor",
            Self::RefreshProviderObservations => "provider-observations-refresh",
            Self::RuntimeRun => "runtime-run",
            Self::BatchPreview => "batch-preview",
            Self::Submit => "submit",
            Self::RunBatch => "run-batch",
            Self::Status => "status",
            Self::Events => "events",
            Self::Pause => "pause",
            Self::Resume => "resume",
            Self::Stop => "stop",
            Self::Cancel => "cancel",
            Self::Reconcile => "reconcile",
            Self::Recover => "recover",
        }
    }

    fn supported(self) -> bool {
        matches!(
            self,
            Self::Capabilities
                | Self::Doctor
                | Self::IssuesList
                | Self::IssuesPage
                | Self::IssuesShow
                | Self::IssuesCreate
                | Self::IssuesUpdate
                | Self::IssuesDelete
                | Self::IssuesValidate
                | Self::Plan
                | Self::ProviderSettings
                | Self::ProvidersDoctor
                | Self::RefreshProviderObservations
                | Self::RuntimeRun
                | Self::BatchPreview
                | Self::Submit
                | Self::RunBatch
                | Self::Status
                | Self::Events
                | Self::Pause
                | Self::Resume
                | Self::Stop
                | Self::Cancel
                | Self::Reconcile
                | Self::Recover
        )
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CoreRequest {
    pub protocol_version: u32,
    pub operation: Operation,
    #[serde(default)]
    pub project_path: Option<PathBuf>,
    #[serde(default)]
    pub config_path: Option<PathBuf>,
    #[serde(default)]
    pub settings_path: Option<PathBuf>,
    #[serde(default)]
    pub allow_live_probe: bool,
    #[serde(default)]
    pub issue_id: Option<String>,
    #[serde(default)]
    pub offset: Option<usize>,
    #[serde(default)]
    pub limit: Option<usize>,
    #[serde(default)]
    pub expected_revision: Option<String>,
    #[serde(default)]
    pub record: Option<IssueRecord>,
    #[serde(default)]
    pub runtime_request: Option<runtime::RuntimeRequest>,
    #[serde(default)]
    pub data_root: Option<PathBuf>,
    #[serde(default)]
    pub batch_request: Option<supervisor::BatchRequest>,
    #[serde(default)]
    pub batch_request_path: Option<PathBuf>,
    #[serde(default)]
    pub batch_id: Option<String>,
    #[serde(default)]
    pub cursor: Option<u64>,
    #[serde(default)]
    pub worker_executable: Option<PathBuf>,
    #[serde(default)]
    pub preview_fingerprint: Option<String>,
    #[serde(default)]
    pub recovery: Option<supervisor::RecoveryRequest>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProtocolError {
    pub code: String,
    pub message: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CoreResponse {
    pub protocol_version: u32,
    pub ok: bool,
    pub exit_code: i32,
    pub result: Option<Value>,
    pub error: Option<ProtocolError>,
}

impl CoreResponse {
    fn success(result: Value) -> Self {
        Self {
            protocol_version: PROTOCOL_VERSION,
            ok: true,
            exit_code: 0,
            result: Some(result),
            error: None,
        }
    }
    fn failure(code: &str, message: impl Into<String>, exit_code: i32) -> Self {
        Self {
            protocol_version: PROTOCOL_VERSION,
            ok: false,
            exit_code,
            result: None,
            error: Some(ProtocolError {
                code: code.into(),
                message: message.into(),
            }),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Capability {
    pub name: String,
    pub supported: bool,
    pub description: String,
}

pub fn capabilities() -> Value {
    let supported = [
        "capabilities",
        "doctor",
        "issues-list",
        "issues-page",
        "issues-show",
        "issues-create",
        "issues-update",
        "issues-delete",
        "issues-validate",
        "plan",
        "provider-settings",
        "providers-doctor",
        "provider-observations-refresh",
        "runtime-run",
        "batch-preview",
        "submit",
        "status",
        "events",
        "pause",
        "resume",
        "stop",
        "cancel",
        "reconcile",
    ];
    json!({
        "protocolVersion": PROTOCOL_VERSION,
        "runnerVersion": env!("CARGO_PKG_VERSION"),
        "supportedCommands": supported,
        "unsupportedCommands": [],
        "providerCalls": true,
        "paidProviderCalls": "blockedWithoutEnforceableRunBudget",
        "persistentJobs": true
    })
}

struct RunnerService {
    project_root: PathBuf,
    config_path: PathBuf,
    issues_path: PathBuf,
    config: RunnerConfig,
}

impl RunnerService {
    fn load(project: &Path, config: &Path) -> Result<Self, CoreError> {
        let project_root = project.canonicalize().map_err(CoreError::io)?;
        if !project_root.is_dir() {
            return Err(CoreError::invalid("project path must be a directory"));
        }
        let config_path = config.canonicalize().map_err(CoreError::io)?;
        let metadata = fs::metadata(&config_path).map_err(CoreError::io)?;
        if metadata.len() > MAX_CONFIG_BYTES {
            return Err(CoreError::invalid("runner config exceeds 64 KiB"));
        }
        let bytes = fs::read(&config_path).map_err(CoreError::io)?;
        let config: RunnerConfig = serde_json::from_slice(&bytes)
            .map_err(|e| CoreError::invalid(format!("invalid runner config JSON: {e}")))?;
        if config.schema_version != 1 {
            return Err(CoreError::invalid(format!(
                "unsupported runner config schema version {}",
                config.schema_version
            )));
        }
        if config.issues_dir.as_os_str().is_empty()
            || config
                .issues_dir
                .components()
                .any(|c| !matches!(c, Component::Normal(_)))
        {
            return Err(CoreError::invalid(
                "issuesDir must be a relative path without dot or parent segments",
            ));
        }
        issues::validate_scope_name(&config.issue_scope).map_err(CoreError::invalid)?;
        let issues_path = project_root.join(&config.issues_dir);
        if issues_path.exists() {
            let resolved = issues_path.canonicalize().map_err(CoreError::io)?;
            if !resolved.starts_with(&project_root) || !resolved.is_dir() {
                return Err(CoreError::invalid(
                    "issuesDir must resolve to a directory inside the project",
                ));
            }
        }
        Ok(Self {
            project_root,
            config_path,
            issues_path,
            config,
        })
    }

    fn store(&self, write: bool) -> Result<IssueStore, CoreError> {
        let result = if write {
            IssueStore::open(&self.issues_path)
        } else {
            IssueStore::open_read_only(&self.issues_path)
        };
        result.map_err(CoreError::from_issue)
    }

    fn doctor(&self) -> Value {
        json!({
            "projectPath": self.project_root,
            "configPath": self.config_path,
            "issuesPath": self.issues_path,
            "issuesDirectoryExists": self.issues_path.is_dir(),
            "issueScope": self.config.issue_scope,
            "providerExecution": {"available":false,"reason":"provider adapters are not exposed by protocol v1"},
            "persistentJobs": {"available":false,"reason":"durable job submission is a later runner phase"}
        })
    }

    fn plan(&self, id: &str) -> Result<Value, CoreError> {
        let store = self.store(false)?;
        let issue = store.show(id).map_err(CoreError::from_issue)?;
        let all = store.list().map_err(CoreError::from_issue)?;
        let dependencies: Vec<_> = issue
            .record
            .dependencies()
            .into_iter()
            .map(
                |dep_id| match all.iter().find(|i| i.record.id() == dep_id) {
                    None => json!({"id":dep_id,"state":"missing","satisfied":false}),
                    Some(dep) => {
                        let done = matches!(dep.record.status(), "done" | "closed");
                        json!({"id":dep_id,"state":dep.record.status(),"satisfied":done})
                    }
                },
            )
            .collect();
        let blockers: Vec<_> = dependencies
            .iter()
            .filter(|d| d["satisfied"] != true)
            .cloned()
            .collect();
        let ready = issue.record.status() == "ready"
            && blockers.is_empty()
            && issue.unresolved_parents.is_empty();
        Ok(
            json!({"dryRun":true,"issue":issue,"dependencies":dependencies,"blockers":blockers,"readyToStart":ready,"executionSupported":false,"effects":[]}),
        )
    }
}

#[derive(Debug)]
struct CoreError {
    code: String,
    message: String,
    exit_code: i32,
}
impl CoreError {
    fn invalid(message: impl Into<String>) -> Self {
        Self {
            code: "INVALID_ARGUMENT".into(),
            message: message.into(),
            exit_code: 2,
        }
    }
    fn io(error: std::io::Error) -> Self {
        Self {
            code: "IO_ERROR".into(),
            message: error.to_string(),
            exit_code: 6,
        }
    }
    fn from_issue(error: IssueError) -> Self {
        let (code, exit_code) = match &error {
            IssueError::NotFound(_) => ("NOT_FOUND", 3),
            IssueError::Conflict(_) => ("CONFLICT", 4),
            IssueError::UnsupportedSchema(_) => ("SCHEMA_VERSION_UNSUPPORTED", 5),
            IssueError::Invalid(_) => ("VALIDATION_ERROR", 7),
            IssueError::Duplicate(_) => ("DUPLICATE_ID", 7),
            IssueError::Io(_) => ("IO_ERROR", 6),
            IssueError::Sql(_) => ("STORE_ERROR", 6),
        };
        Self {
            code: code.into(),
            message: error.to_string(),
            exit_code,
        }
    }
}

pub fn execute(request: CoreRequest) -> CoreResponse {
    initialize_crypto_provider();
    if request.protocol_version != PROTOCOL_VERSION {
        return CoreResponse::failure("PROTOCOL_VERSION_UNSUPPORTED", format!("protocol version {} is unsupported; this runner supports version {PROTOCOL_VERSION}",request.protocol_version), 5);
    }
    if matches!(request.operation, Operation::Capabilities) {
        return CoreResponse::success(capabilities());
    }
    if matches!(request.operation, Operation::RuntimeRun) {
        let (Some(settings_path), Some(runtime_request)) = (
            request.settings_path.as_deref(),
            request.runtime_request.as_ref(),
        ) else {
            return CoreResponse::failure(
                "INVALID_ARGUMENT",
                "settingsPath and runtimeRequest are required for runtime-run",
                2,
            );
        };
        let settings = match settings::load_settings(settings_path) {
            Ok(settings) => settings,
            Err(error) => return CoreResponse::failure("SETTINGS_INVALID", error, 2),
        };
        let Some(root) = Path::new(&settings.project_root).canonicalize().ok() else {
            return CoreResponse::failure(
                "INVALID_SETTINGS",
                "The configured project root could not be resolved",
                2,
            );
        };
        let Some(cwd) = runtime_request.cwd.canonicalize().ok() else {
            return CoreResponse::failure(
                "INVALID_WORKSPACE",
                "The selected project folder could not be resolved",
                2,
            );
        };
        if !cwd.is_dir() || !cwd.starts_with(&root) {
            return CoreResponse::failure(
                "INVALID_WORKSPACE",
                "The selected project folder must stay inside the configured project root",
                2,
            );
        }
        let cancel = std::sync::atomic::AtomicBool::new(false);
        let mut events = Vec::new();
        struct Sink<'a> {
            events: &'a mut Vec<runtime::RuntimeEvent>,
            bytes: usize,
        }
        impl runtime::RuntimeEventSink for Sink<'_> {
            fn on_event(&mut self, event: runtime::RuntimeEvent) -> runtime::RuntimeControl {
                let weight = event
                    .text
                    .len()
                    .saturating_add(event.stream.as_ref().map_or(0, String::len))
                    .saturating_add(256);
                if self.events.len() < 10_000
                    && self.bytes.saturating_add(weight) <= MAX_CORE_RUNTIME_EVENT_BYTES
                {
                    self.bytes += weight;
                    self.events.push(event);
                    runtime::RuntimeControl::Continue
                } else {
                    runtime::RuntimeControl::Cancel
                }
            }
        }
        let mut sink = Sink {
            events: &mut events,
            bytes: 0,
        };
        return match runtime::run(&settings, runtime_request, &cancel, &mut sink) {
            Ok(outcome) => CoreResponse::success(json!({"outcome":outcome,"events":events})),
            Err(error) => CoreResponse::failure(error.code, error.message, 6),
        };
    }
    if matches!(request.operation, Operation::RefreshProviderObservations) {
        let (Some(data_root), Some(settings_path)) = (
            request.data_root.as_deref(),
            request.settings_path.as_deref(),
        ) else {
            return CoreResponse::failure(
                "INVALID_ARGUMENT",
                "dataRoot and settingsPath are required for provider-observations-refresh",
                2,
            );
        };
        return match provider_observations::ProviderObservationStore::refresh(
            data_root,
            settings_path,
            request.allow_live_probe,
            usage::now_ms(),
        ) {
            Ok(snapshot) => CoreResponse::success(json!(snapshot)),
            Err(error) => CoreResponse::failure("PROVIDER_REFRESH_FAILED", error, 6),
        };
    }
    if let Some(supervisor_operation) = supervisor_operation(request.operation) {
        let Some(data_root) = request.data_root.as_deref() else {
            return CoreResponse::failure(
                "INVALID_ARGUMENT",
                "dataRoot is required for supervisor operations",
                2,
            );
        };
        let batch_request = match load_batch_request(&request) {
            Ok(value) => value,
            Err(error) => return CoreResponse::failure("INVALID_ARGUMENT", error, 2),
        };
        let result = supervisor_protocol::dispatch(
            data_root,
            supervisor_operation,
            batch_request,
            request.batch_id.as_deref(),
            request.cursor.unwrap_or(0),
            request.limit.unwrap_or(100).try_into().unwrap_or(101),
            request.worker_executable.as_deref(),
            request.preview_fingerprint.as_deref(),
            request.recovery.clone(),
        );
        return match result {
            Ok(result) => match serde_json::to_value(result) {
                Ok(value) => CoreResponse::success(value),
                Err(error) => CoreResponse::failure("SERIALIZATION_ERROR", error.to_string(), 6),
            },
            Err(error) => {
                let (code, exit) = match &error {
                    supervisor::SupervisorError::Invalid(_) => ("INVALID_ARGUMENT", 2),
                    supervisor::SupervisorError::Conflict(_) => ("CONFLICT", 4),
                    supervisor::SupervisorError::NotFound(_) => ("NOT_FOUND", 3),
                    supervisor::SupervisorError::WorkerLaunch(_) => ("WORKER_LAUNCH_FAILED", 6),
                    _ => ("SUPERVISOR_ERROR", 6),
                };
                CoreResponse::failure(code, error.to_string(), exit)
            }
        };
    }
    if !request.operation.supported() {
        return CoreResponse::failure(
            "CAPABILITY_UNAVAILABLE",
            format!(
                "command {} is not supported by protocol version {PROTOCOL_VERSION}",
                request.operation.cli_name()
            ),
            5,
        );
    }
    if matches!(
        request.operation,
        Operation::ProviderSettings | Operation::ProvidersDoctor
    ) {
        let Some(path) = request.settings_path.as_deref() else {
            return CoreResponse::failure(
                "INVALID_ARGUMENT",
                "settingsPath is required for provider commands",
                2,
            );
        };
        return match settings::load_settings(path) {
            Err(error) => CoreResponse::failure("SETTINGS_INVALID", error, 2),
            Ok(settings) => {
                let value = match request.operation {
                    Operation::ProviderSettings => {
                        json!({"schemaVersion":settings.schema_version,"providers":settings.providers,"providerSelection":settings.provider_selection})
                    }
                    Operation::ProvidersDoctor => {
                        json!({"schemaVersion":settings.schema_version,"providers":settings.providers.iter().map(|profile| providers::probe_installed(profile, request.allow_live_probe)).collect::<Vec<_>>()})
                    }
                    _ => unreachable!(),
                };
                CoreResponse::success(value)
            }
        };
    }
    let (Some(project), Some(config)) = (
        request.project_path.as_deref(),
        request.config_path.as_deref(),
    ) else {
        return CoreResponse::failure(
            "INVALID_ARGUMENT",
            "projectPath and configPath are required for this command",
            2,
        );
    };
    let result = (|| -> Result<Value, CoreError> {
        let service = RunnerService::load(project, config)?;
        match request.operation {
            Operation::Capabilities => Ok(capabilities()),
            Operation::Doctor => Ok(service.doctor()),
            Operation::IssuesList => Ok(serde_json::to_value(
                service
                    .store(false)?
                    .list()
                    .map_err(CoreError::from_issue)?,
            )
            .map_err(|e| CoreError::invalid(e.to_string()))?),
            Operation::IssuesPage => Ok(serde_json::to_value(
                service
                    .store(false)?
                    .page(request.offset.unwrap_or(0), request.limit.unwrap_or(100))
                    .map_err(CoreError::from_issue)?,
            )
            .map_err(|e| CoreError::invalid(e.to_string()))?),
            Operation::IssuesShow => {
                let id = request
                    .issue_id
                    .as_deref()
                    .ok_or_else(|| CoreError::invalid("issueId is required"))?;
                Ok(serde_json::to_value(
                    service
                        .store(false)?
                        .show(id)
                        .map_err(CoreError::from_issue)?,
                )
                .map_err(|e| CoreError::invalid(e.to_string()))?)
            }
            Operation::IssuesCreate => {
                let input = request
                    .record
                    .ok_or_else(|| CoreError::invalid("record is required"))?;
                let store = service.store(true)?;
                let record = store
                    .create_in_scope(input, &service.config.issue_scope)
                    .map_err(CoreError::from_issue)?;
                Ok(
                    serde_json::to_value(store.show(record.id()).map_err(CoreError::from_issue)?)
                        .map_err(|e| CoreError::invalid(e.to_string()))?,
                )
            }
            Operation::IssuesUpdate => {
                let input = request
                    .record
                    .ok_or_else(|| CoreError::invalid("record is required"))?;
                let revision = request
                    .expected_revision
                    .as_deref()
                    .ok_or_else(|| CoreError::invalid("expectedRevision is required"))?;
                let store = service.store(true)?;
                store
                    .update(input.clone(), revision)
                    .map_err(CoreError::from_issue)?;
                Ok(
                    serde_json::to_value(store.show(input.id()).map_err(CoreError::from_issue)?)
                        .map_err(|e| CoreError::invalid(e.to_string()))?,
                )
            }
            Operation::IssuesDelete => {
                let id = request
                    .issue_id
                    .as_deref()
                    .ok_or_else(|| CoreError::invalid("issueId is required"))?;
                let revision = request
                    .expected_revision
                    .as_deref()
                    .ok_or_else(|| CoreError::invalid("expectedRevision is required"))?;
                service
                    .store(true)?
                    .delete(id, revision)
                    .map_err(CoreError::from_issue)?;
                Ok(json!({"deleted":true,"issueId":id}))
            }
            Operation::IssuesValidate => {
                let issues = service
                    .store(false)?
                    .list()
                    .map_err(CoreError::from_issue)?;
                let unresolved: Vec<_> = issues
                    .iter()
                    .flat_map(|i| {
                        i.unresolved_dependencies
                            .iter()
                            .map(move |id| json!({"issueId":i.record.id(),"dependencyId":id}))
                    })
                    .collect();
                let unresolved_parents: Vec<_> = issues
                    .iter()
                    .flat_map(|i| {
                        i.unresolved_parents
                            .iter()
                            .map(move |id| json!({"issueId":i.record.id(),"parentId":id}))
                    })
                    .collect();
                let valid = unresolved.is_empty() && unresolved_parents.is_empty();
                Ok(
                    json!({"valid":valid,"issueCount":issues.len(),"unresolvedDependencies":unresolved,"unresolvedParents":unresolved_parents}),
                )
            }
            Operation::Plan => {
                let id = request
                    .issue_id
                    .as_deref()
                    .ok_or_else(|| CoreError::invalid("issueId is required"))?;
                service.plan(id)
            }
            Operation::ProviderSettings
            | Operation::ProvidersDoctor
            | Operation::RefreshProviderObservations => unreachable!(),
            _ => unreachable!("unsupported operations returned above"),
        }
    })();
    match result {
        Ok(value) => CoreResponse::success(value),
        Err(error) => CoreResponse::failure(&error.code, error.message, error.exit_code),
    }
}

fn supervisor_operation(operation: Operation) -> Option<supervisor_protocol::SupervisorOperation> {
    use supervisor_protocol::SupervisorOperation as S;
    Some(match operation {
        Operation::BatchPreview => S::BatchPreview,
        Operation::Submit => S::Submit,
        Operation::RunBatch => S::RunBatch,
        Operation::Status => S::Status,
        Operation::Events => S::Events,
        Operation::Pause => S::Pause,
        Operation::Resume => S::Resume,
        Operation::Stop => S::Stop,
        Operation::Cancel => S::Cancel,
        Operation::Reconcile => S::Reconcile,
        Operation::Recover => S::Recover,
        _ => return None,
    })
}

fn load_batch_request(request: &CoreRequest) -> Result<Option<supervisor::BatchRequest>, String> {
    if let Some(batch_request) = &request.batch_request {
        return Ok(Some(batch_request.clone()));
    }
    let Some(path) = request.batch_request_path.as_deref() else {
        return Ok(None);
    };
    let metadata =
        fs::metadata(path).map_err(|_| "batchRequestPath could not be inspected".to_owned())?;
    if metadata.len() > MAX_BATCH_REQUEST_BYTES {
        return Err("batch request exceeds the 1 MiB request-file limit".into());
    }
    let bytes = fs::read(path).map_err(|_| "batchRequestPath could not be read".to_owned())?;
    if bytes.len() as u64 > MAX_BATCH_REQUEST_BYTES {
        return Err("batch request exceeds the 1 MiB request-file limit".into());
    }
    serde_json::from_slice(&bytes)
        .map(Some)
        .map_err(|error| format!("batch request JSON is invalid: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    fn request(
        operation: Operation,
        project: Option<PathBuf>,
        config: Option<PathBuf>,
    ) -> CoreRequest {
        CoreRequest {
            protocol_version: PROTOCOL_VERSION,
            operation,
            project_path: project,
            config_path: config,
            settings_path: None,
            allow_live_probe: false,
            issue_id: None,
            offset: None,
            limit: None,
            expected_revision: None,
            record: None,
            runtime_request: None,
            data_root: None,
            batch_request: None,
            batch_request_path: None,
            batch_id: None,
            cursor: None,
            worker_executable: None,
            preview_fingerprint: None,
            recovery: None,
        }
    }
    fn setup() -> (tempfile::TempDir, PathBuf, PathBuf) {
        let d = tempfile::tempdir().unwrap();
        let project = d.path().join("Project with spaces");
        fs::create_dir_all(&project).unwrap();
        let config = project.join("runner.json");
        fs::write(
            &config,
            r#"{"schemaVersion":1,"issueScope":"FAB","issuesDir":"issues"}"#,
        )
        .unwrap();
        (d, project, config)
    }
    #[test]
    fn protocol_version_and_capabilities_are_explicit() {
        let r = execute(request(Operation::Capabilities, None, None));
        assert!(r.ok);
        assert_eq!(r.result.unwrap()["protocolVersion"], 1);
        let mut bad = request(Operation::Doctor, None, None);
        bad.protocol_version = 2;
        assert_eq!(
            execute(bad).error.unwrap().code,
            "PROTOCOL_VERSION_UNSUPPORTED"
        );
    }
    #[test]
    fn provider_observations_refresh_uses_the_public_wire_name() {
        let operation: Operation =
            serde_json::from_str("\"provider-observations-refresh\"").unwrap();
        assert!(matches!(operation, Operation::RefreshProviderObservations));
    }
    #[test]
    fn doctor_and_issue_crud_use_explicit_paths_with_spaces() {
        let (_d, project, config) = setup();
        let doctor = execute(request(
            Operation::Doctor,
            Some(project.clone()),
            Some(config.clone()),
        ));
        assert!(doctor.ok);
        let created = execute(CoreRequest {
            operation: Operation::IssuesCreate,
            project_path: Some(project.clone()),
            config_path: Some(config.clone()),
            record: Some(sample_record()),
            ..request(Operation::IssuesCreate, None, None)
        });
        assert!(created.ok, "{:?}", created.error);
        let list = execute(request(
            Operation::IssuesList,
            Some(project.clone()),
            Some(config.clone()),
        ));
        assert_eq!(list.result.unwrap().as_array().unwrap().len(), 1);
        let page = execute(CoreRequest {
            operation: Operation::IssuesPage,
            offset: Some(0),
            limit: Some(100),
            ..request(
                Operation::IssuesPage,
                Some(project.clone()),
                Some(config.clone()),
            )
        });
        assert!(page.ok);
        let page = page.result.unwrap();
        assert_eq!(page["total"], 1);
        assert_eq!(page["items"][0]["id"], "FAB-S001");
        assert!(page["items"][0].get("body").is_none());
    }
    #[test]
    fn supported_submission_without_required_arguments_fails_closed() {
        let r = execute(request(Operation::Submit, None, None));
        assert!(!r.ok);
        assert_eq!(r.exit_code, 2);
        assert_eq!(r.error.unwrap().code, "INVALID_ARGUMENT");
    }
    fn sample_record() -> IssueRecord {
        let mut frontmatter = serde_json::Map::new();
        for (k, v) in [
            ("title", json!("Smoke issue")),
            ("type", json!("task")),
            ("status", json!("ready")),
            ("parent", json!([])),
            ("depends_on", json!([])),
        ] {
            frontmatter.insert(k.into(), v);
        }
        IssueRecord {
            frontmatter,
            body: "# Smoke issue\n".into(),
        }
    }
}
