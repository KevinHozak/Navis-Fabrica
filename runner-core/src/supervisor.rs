//! Durable batch supervision for explicitly started local runner jobs.
//!
//! Batch controls and policies live in `batches.sqlite3`; job/attempt leases,
//! checkpoints and process references remain owned by `JobsStore`. Budget
//! reservations and verification evidence remain owned by their respective stores.

use crate::budgets::{BudgetError, BudgetPolicy, BudgetStore, ReservationRequest};
use crate::isolation::{PrepareRequest, RepoIsolation, VerifiedPredecessor};
use crate::jobs::{
    AttemptOutcome, ClaimedJob, DependencyResolution, Job, JobPlan, JobState, JobsError, JobsStore,
    PlannedTask, RecoveryDecision, SubmitJob, TaskKind,
};
use crate::providers::{ProviderKind, ProviderProfile};
use crate::runtime::{
    self, BillingStatus, RuntimeControl, RuntimeEvent, RuntimeEventKind, RuntimeEventSink,
    RuntimeObserver, RuntimeRequest, RuntimeState, RuntimeUsage,
};
use crate::settings::{self, Settings};
use crate::verification::{
    source_snapshot, validate_check, CheckExecutor, CheckOutput, CheckSpec, ProcessCheckExecutor,
    RepairProgress, RepairState, ReviewerIdentity, VerificationEnvelope, VerificationOutcome,
    VerificationService,
};
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const BATCH_SCHEMA: i64 = 1;
const MAX_BATCH_JOBS: usize = 128;
const MAX_WORKERS: u32 = 8;
const MAX_WINDOW_SECONDS: u64 = 7 * 24 * 60 * 60;
const MAX_LEASE_MS: i64 = 60_000;
const MAX_IDLE_MS: u64 = 60 * 60 * 1000;

#[derive(Debug)]
pub enum SupervisorError {
    Invalid(String),
    Conflict(String),
    NotFound(String),
    Store(String),
    Jobs(JobsError),
    Budget(BudgetError),
    Io(String),
    WorkerLaunch(String),
    Runtime(String),
    Verification(String),
}
impl std::fmt::Display for SupervisorError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Invalid(s) => write!(f, "invalid batch: {s}"),
            Self::Conflict(s) => write!(f, "batch conflict: {s}"),
            Self::NotFound(s) => write!(f, "batch not found: {s}"),
            Self::Store(s) => write!(f, "batch store error: {s}"),
            Self::Jobs(e) => write!(f, "{e}"),
            Self::Budget(e) => write!(f, "{e}"),
            Self::Io(s) => write!(f, "batch I/O error: {s}"),
            Self::WorkerLaunch(s) => write!(f, "worker launch failed after batch admission: {s}"),
            Self::Runtime(s) => write!(f, "worker runtime error: {s}"),
            Self::Verification(s) => write!(f, "verification error: {s}"),
        }
    }
}
impl std::error::Error for SupervisorError {}

// Verification::Display prefixes the inner message with `verification error: `.
// Classify this control-flow marker from its typed inner value, not Display text.
fn verification_error_requires_input(error: &SupervisorError) -> bool {
    matches!(error, SupervisorError::Verification(message) if message.starts_with("needs_input:"))
}

impl From<JobsError> for SupervisorError {
    fn from(v: JobsError) -> Self {
        Self::Jobs(v)
    }
}
impl From<BudgetError> for SupervisorError {
    fn from(v: BudgetError) -> Self {
        Self::Budget(v)
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum BatchState {
    Starting,
    Running,
    Paused,
    Stopping,
    Cancelling,
    Waiting,
    Completed,
    Failed,
    WindowExpired,
    Cancelled,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum WorkerLaunchMode {
    Detached,
    Hosted,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PermittedEffect {
    ProviderExecution,
    WorkspaceWrites,
    LocalChecks,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct BatchPolicy {
    pub window_seconds: u64,
    pub max_concurrency: u32,
    pub max_auth_retries: u8,
    pub max_network_retries: u8,
    pub budget: BudgetPolicy,
    pub permitted_effects: Vec<PermittedEffect>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct VerificationPolicy {
    pub issue_id: String,
    pub checks: Vec<CheckSpec>,
    pub reviewer: ReviewerIdentity,
    pub acceptance_criteria: Vec<String>,
}

const MAX_REVIEW_ACCEPTANCE_CRITERIA: usize = 200;
const MAX_REVIEW_ACCEPTANCE_ITEM_BYTES: usize = 2_048;

fn merge_review_acceptance_criteria(
    policy_criteria: &[String],
    reviewer_criteria: &[String],
) -> Result<Vec<String>, SupervisorError> {
    let mut merged = Vec::new();
    let mut seen = HashSet::new();
    for (source, criteria) in [
        ("verification policy", policy_criteria),
        ("reviewer task", reviewer_criteria),
    ] {
        for criterion in criteria {
            if criterion.trim().is_empty()
                || criterion.len() > MAX_REVIEW_ACCEPTANCE_ITEM_BYTES
                || criterion.chars().any(char::is_control)
            {
                return Err(SupervisorError::Verification(format!(
                    "needs_input: {source} acceptance criterion is blank, exceeds 2,048 bytes, or contains control characters"
                )));
            }
            if seen.insert(criterion.as_str()) {
                if merged.len() == MAX_REVIEW_ACCEPTANCE_CRITERIA {
                    return Err(SupervisorError::Verification(
                        "needs_input: combined reviewer acceptance criteria exceed 200 unique items".into(),
                    ));
                }
                merged.push(criterion.clone());
            }
        }
    }
    if merged.is_empty() {
        return Err(SupervisorError::Verification(
            "needs_input: combined reviewer acceptance criteria are empty".into(),
        ));
    }
    Ok(merged)
}

fn build_verification_envelope(
    policy: &VerificationPolicy,
    reviewer_task: &PlannedTask,
    issue_fingerprint: String,
    worker_task_id: String,
    code_snapshot: crate::verification::SourceSnapshot,
    remaining_ms: i64,
) -> Result<VerificationEnvelope, SupervisorError> {
    let acceptance_criteria =
        merge_review_acceptance_criteria(&policy.acceptance_criteria, &reviewer_task.acceptance)?;
    let remaining_ms = remaining_ms.max(0) as u64;
    Ok(VerificationEnvelope {
        issue_fingerprint,
        worker_task_id,
        code_snapshot,
        checks: policy
            .checks
            .iter()
            .cloned()
            .map(|mut check| {
                check.timeout_ms = check.timeout_ms.min(remaining_ms);
                check
            })
            .collect(),
        reviewer: policy.reviewer.clone(),
        acceptance_criteria,
    })
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct BatchRequest {
    pub batch_id: String,
    pub project_path: PathBuf,
    pub managed_worktree_root: PathBuf,
    pub build_root: PathBuf,
    pub settings_path: PathBuf,
    #[serde(default)]
    pub settings_fingerprint: Option<String>,
    pub verified_base_commit: String,
    pub jobs: Vec<SubmitJob>,
    pub verification: Vec<VerificationPolicy>,
    #[serde(default)]
    pub issue_sources: BTreeMap<String, PathBuf>,
    /// Immutable paid-call ceilings keyed by `{issue_id}/{task_id}`. Ordinary
    /// and previously saved requests remain compatible with an empty map.
    #[serde(default)]
    pub paid_policies: BTreeMap<String, crate::paid_gateway::PaidModelPolicy>,
    /// Route decisions resolved from fresh observations during preview. Submit
    /// recomputes and compares these before persisting or dispatching work.
    #[serde(default)]
    pub resolved_routes: BTreeMap<String, crate::routing::CandidateRoute>,
    /// Fresh observation evidence bound by preview and rechecked before work.
    #[serde(default)]
    pub provider_observation_fingerprint: Option<String>,
    /// Explicit provider kinds that may receive a model-free metadata refresh at
    /// each dispatch boundary. Older requests deserialize as Disabled.
    #[serde(default)]
    pub dispatch_refresh_consent: crate::provider_observations::ModelFreeRefreshConsent,
    /// Immutable semantic routing commitments derived during preview.
    #[serde(default)]
    pub route_commitments: BTreeMap<String, RouteSemanticCommitment>,
    pub policy: BatchPolicy,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RouteSemanticCommitment {
    pub provider_id: String,
    pub model_id: String,
    pub runtime_identity_fingerprint: String,
    pub local_only: bool,
    pub billing: crate::routing::BillingPolicy,
    pub requires_tools: bool,
    pub minimum_context_tokens: Option<u64>,
    pub authorized_candidates: Vec<crate::routing::CandidateRoute>,
    pub paid_quote: Option<CommittedPaidQuote>,
    /// Exact backend-minted Economy endpoint pin. These fields are only
    /// commitments; they cannot create or restore a VerifiedEconomyAdmission.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub economy_endpoint_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub economy_evidence_digest: Option<String>,
}

impl RouteSemanticCommitment {
    pub(crate) fn validate_economy_pin(
        &self,
        facts: &crate::routing::CandidateFacts,
    ) -> Result<(), String> {
        if let Some(endpoint_id) = self.economy_endpoint_id.as_deref() {
            let admission = facts.economy_admission.as_ref().ok_or_else(|| {
                "fresh Economy admission could not be rehydrated before dispatch".to_owned()
            })?;
            if admission.endpoint_id() != endpoint_id
                || self.economy_evidence_digest.as_deref() != Some(admission.evidence_digest())
                || admission.provider_profile_id() != self.provider_id
                || admission.model_id() != self.model_id
            {
                return Err("fresh Economy endpoint or evidence pin changed since preview".into());
            }
        } else if self.economy_evidence_digest.is_some() {
            return Err("Economy evidence digest has no committed endpoint pin".into());
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CommittedPaidQuote {
    pub input_price_micros_per_1m: u64,
    pub output_price_micros_per_1m: u64,
    pub request_fee_micros: u64,
    pub source: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_catalog_pricing: Option<crate::paid_gateway::PaidModelCatalogPricing>,
    pub max_calls: u32,
    pub max_cost_micros: u64,
    pub context_tokens: u64,
    pub max_output_tokens_per_call: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub endpoint_pin: Option<crate::paid_gateway::PaidEndpointPin>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct PlannedIssue {
    pub issue_id: String,
    pub task_count: usize,
    pub dependencies: Vec<String>,
    pub provider_models: Vec<(String, String)>,
    pub eligibility: PreviewEligibility,
    pub routing_reasons: Vec<String>,
    pub tasks: Vec<PlannedTaskRouting>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PreviewEligibility {
    Eligible,
    Blocked,
    WaitingForDependency,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct PlannedTaskRouting {
    pub task_id: String,
    pub provider_id: Option<String>,
    pub model_id: Option<String>,
    pub assignment_source: String,
    pub eligible: bool,
    pub reasons: Vec<String>,
    #[serde(default)]
    pub route_provenance: Option<Value>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct DryRunPlan {
    pub batch_id: String,
    pub issues: Vec<PlannedIssue>,
    pub policy: BatchPolicy,
    pub worker_count: u32,
    pub warnings: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BatchPreview {
    pub request: BatchRequest,
    pub plan: DryRunPlan,
    pub provider_observation_fingerprint: String,
    pub fingerprint: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct BatchSnapshot {
    pub batch_id: String,
    pub state: BatchState,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
    pub deadline_ms: i64,
    pub job_ids: Vec<i64>,
    pub control_note: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct MorningSummary {
    pub batch_id: String,
    pub completed: Vec<String>,
    pub blocked: Vec<String>,
    pub failed: Vec<String>,
    pub cancelled: Vec<String>,
    pub unstarted: Vec<String>,
    pub needs_input: Vec<String>,
    pub reconciliation_required: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BatchEvent {
    pub sequence: u64,
    pub timestamp_ms: i64,
    pub kind: String,
    pub payload: Value,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BatchEventPage {
    pub events: Vec<BatchEvent>,
    pub next_cursor: u64,
    pub has_more: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct JobStatusView {
    pub id: i64,
    pub issue_id: String,
    pub state: JobState,
    pub completion_gate: String,
    pub attempt_count: u32,
    pub max_attempts: u8,
    pub updated_at_ms: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_attempt_summary: Option<String>,
    #[serde(default)]
    pub checkpoints_truncated: bool,
    pub checkpoints: Vec<CheckpointStatusView>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CheckpointStatusView {
    pub key: String,
    pub value: Value,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BudgetStatusView {
    pub reservations: u64,
    pub reserved: u64,
    pub dispatched: u64,
    pub unresolved: u64,
    pub tokens_used: u64,
    pub cost_used_micros: u64,
    pub seconds_used: u64,
    pub max_tokens: u64,
    pub max_cost_micros: u64,
    pub max_seconds: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BatchStatusView {
    pub snapshot: BatchSnapshot,
    pub summary: MorningSummary,
    pub jobs: Vec<JobStatusView>,
    pub budget: BudgetStatusView,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReconcileReport {
    pub batch_id: String,
    pub items: Vec<ReconcileStatusItem>,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RecoveryAction {
    SafeToRetry,
    KeepNeedsInput,
    MarkFailed,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RecoveryRequest {
    pub job_id: i64,
    pub decision: RecoveryAction,
    pub evidence: String,
    #[serde(default)]
    pub process_confirmed_stopped: bool,
    #[serde(default)]
    pub effects_reconciled: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RecoveryResult {
    pub batch_id: String,
    pub job: Job,
    pub batch_membership_verified: bool,
    pub process_confirmed_stopped: bool,
    pub effects_reconciled: bool,
    pub decision: RecoveryAction,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReconcileStatusItem {
    pub job_id: i64,
    pub attempt_number: u32,
    pub worker_id: String,
    pub process_ref: Option<String>,
    pub process_alive: Option<bool>,
    pub lease_until_ms: i64,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct WorkerSlot {
    pub worker_id: String,
    pub slot: u32,
    pub pid: Option<u32>,
    pub creation_time_100ns: Option<u64>,
    pub state: String,
    pub updated_at_ms: i64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkerExit {
    Completed,
    Paused,
    WaitingForInput,
    Stopping,
    Cancelled,
    Failed,
    WindowExpired,
}

#[derive(Clone, Debug)]
pub struct DataPaths {
    pub root: PathBuf,
    pub jobs: PathBuf,
    pub budgets: PathBuf,
    pub effects: PathBuf,
    pub batches: PathBuf,
}
impl DataPaths {
    pub fn new(root: impl Into<PathBuf>) -> Result<Self, SupervisorError> {
        let root = root.into();
        fs::create_dir_all(&root).map_err(|e| SupervisorError::Io(e.to_string()))?;
        let root = fs::canonicalize(root).map_err(|e| SupervisorError::Io(e.to_string()))?;
        let paths = Self {
            jobs: root.join("jobs.sqlite3"),
            budgets: root.join("budgets.sqlite3"),
            effects: root.join("effects.sqlite3"),
            batches: root.join("batches.sqlite3"),
            root,
        };
        let files = [&paths.jobs, &paths.budgets, &paths.effects, &paths.batches];
        for i in 0..files.len() {
            for j in i + 1..files.len() {
                if same_file_path(files[i], files[j]) {
                    return Err(SupervisorError::Invalid("runner stores must use separate SQLite files because they own independent schema versions".into()));
                }
            }
        }
        Ok(paths)
    }
}

#[derive(Clone)]
pub struct Supervisor {
    paths: DataPaths,
}

impl Supervisor {
    pub fn open(data_root: impl Into<PathBuf>) -> Result<Self, SupervisorError> {
        let paths = DataPaths::new(data_root)?;
        let store = BatchStore::open(&paths.batches)?;
        drop(store);
        Ok(Self { paths })
    }

    pub fn paths(&self) -> &DataPaths {
        &self.paths
    }

    pub(crate) fn admitted_request(
        &self,
        batch_id: &str,
    ) -> Result<Option<BatchRequest>, SupervisorError> {
        let store = BatchStore::open(&self.paths.batches)?;
        match store.request(batch_id) {
            Ok(request) => Ok(Some(request)),
            Err(SupervisorError::NotFound(_)) => Ok(None),
            Err(error) => Err(error),
        }
    }

    /// Pure admission preview: validates policy and provider/model availability but
    /// never writes a job, starts a process, performs a provider probe, or schedules work.
    pub fn dry_run(&self, request: &BatchRequest) -> Result<DryRunPlan, SupervisorError> {
        Self::dry_run_request_with_observations(request, &self.paths.root)
    }

    /// Compatibility preview with no observation source. Configured membership
    /// alone is not evidence, so all provider-backed tasks remain blocked.
    pub fn dry_run_request(request: &BatchRequest) -> Result<DryRunPlan, SupervisorError> {
        validate_request_inner(request, false)?;
        let settings = settings::load_settings(&request.settings_path)
            .map_err(|error| SupervisorError::Invalid(format!("settings_load: {error}")))?;
        Self::dry_run_request_with_facts(request, &settings, &[])
    }

    /// Read-only preview from the saved backend observations. It never probes,
    /// creates a data directory, or starts a provider.
    pub fn dry_run_request_with_observations(
        request: &BatchRequest,
        data_root: &Path,
    ) -> Result<DryRunPlan, SupervisorError> {
        validate_request_inner(request, false)?;
        let settings = settings::load_settings(&request.settings_path)
            .map_err(|error| SupervisorError::Invalid(format!("settings_load: {error}")))?;
        let (facts, fingerprint, observation_problem) =
            load_provider_observations(&settings, data_root, now_ms().max(0) as u128)?;
        if request
            .provider_observation_fingerprint
            .as_deref()
            .is_some_and(|old| old != fingerprint)
        {
            return Err(SupervisorError::Conflict(
                "provider observations changed since preview; refresh and review the route plan"
                    .into(),
            ));
        }
        let mut plan = Self::dry_run_request_with_facts(request, &settings, &facts)?;
        if let Some(problem) = observation_problem {
            plan.warnings.push(format!(
                "Provider routing observations are unavailable: {problem}"
            ));
        }
        Ok(plan)
    }

    fn dry_run_request_with_facts(
        request: &BatchRequest,
        settings: &Settings,
        candidate_facts: &[crate::routing::CandidateFacts],
    ) -> Result<DryRunPlan, SupervisorError> {
        validate_request_inner(request, false)?;
        validate_reviewer_reasoning_efforts(settings, request)?;
        let mut issues = Vec::new();
        let mut warnings = Vec::new();
        for job in &request.jobs {
            let mut provider_models = Vec::new();
            let mut planned_tasks = Vec::new();
            let mut routing_reasons = Vec::new();
            for task in &job.plan.tasks {
                let assignment_source = if task.provider_id.is_some() || task.model_id.is_some() {
                    "explicit"
                } else if task
                    .route_requirements
                    .as_ref()
                    .is_some_and(|requirements| !requirements.allowed_candidates.is_empty())
                {
                    "authorized_candidates"
                } else {
                    "settings_default"
                };
                let routed = route_task(request, settings, job, task, candidate_facts, None)?;
                let (provider_id, model_id, eligible, provenance, reasons) = (
                    routed.provider_id,
                    routed.model_id,
                    routed.eligible,
                    routed.provenance,
                    routed.reasons,
                );
                if let (Some(provider), Some(model)) = (&provider_id, &model_id) {
                    provider_models.push((provider.clone(), model.clone()));
                }
                if !eligible {
                    routing_reasons.push(format!("{}: {}", task.task_id, reasons.join("; ")));
                }
                planned_tasks.push(PlannedTaskRouting {
                    task_id: task.task_id.clone(),
                    provider_id,
                    model_id,
                    assignment_source: if eligible {
                        assignment_source.to_owned()
                    } else {
                        "unavailable".to_owned()
                    },
                    eligible,
                    reasons,
                    route_provenance: provenance,
                });
            }
            let waiting_for_dependency = job.dependencies.iter().any(|d| {
                matches!(
                    d.resolution,
                    DependencyResolution::Unresolved | DependencyResolution::Job { .. }
                )
            });
            if waiting_for_dependency {
                warnings.push(format!("{} has an explicitly unresolved prerequisite and will remain blocked until evidence is supplied.", job.issue_id));
            }
            let eligibility = if !routing_reasons.is_empty() {
                PreviewEligibility::Blocked
            } else if waiting_for_dependency {
                PreviewEligibility::WaitingForDependency
            } else {
                PreviewEligibility::Eligible
            };
            issues.push(PlannedIssue {
                issue_id: job.issue_id.clone(),
                task_count: job.plan.tasks.len(),
                dependencies: job
                    .dependencies
                    .iter()
                    .map(|d| d.issue_id.clone())
                    .collect(),
                provider_models,
                eligibility,
                routing_reasons,
                tasks: planned_tasks,
            });
        }
        for key in request.paid_policies.keys() {
            let policy = &request.paid_policies[key];
            if !matches!(policy.mode, crate::paid_gateway::GatewayMode::Paid) {
                return Err(SupervisorError::Invalid(format!(
                    "paid policy {key} cannot supply a serialized Economy or synthetic-trial admission"
                )));
            }
            let candidate_is_authorized = request.jobs.iter().any(|job| {
                job.plan.tasks.iter().any(|task| {
                    if paid_policy_key(&job.issue_id, &task.task_id) != *key {
                        return false;
                    }
                    let allowed = task
                        .route_requirements
                        .as_ref()
                        .map(|requirements| requirements.allowed_candidates.clone())
                        .filter(|routes| !routes.is_empty())
                        .unwrap_or_else(|| {
                            selected_profile(settings, task)
                                .ok()
                                .and_then(|profile| {
                                    selected_model(task, settings, profile)
                                        .ok()
                                        .map(|model_id| {
                                            vec![crate::routing::CandidateRoute {
                                                provider_id: profile.id.clone(),
                                                model_id,
                                            }]
                                        })
                                })
                                .unwrap_or_default()
                        });
                    allowed.iter().any(|route| {
                        route.provider_id == policy.provider_id
                            && route.model_id == policy.model_id
                            && settings.providers.iter().any(|profile| {
                                profile.id == route.provider_id
                                    && profile.kind == ProviderKind::OpenRouter
                            })
                    })
                })
            });
            if !candidate_is_authorized {
                return Err(SupervisorError::Invalid(format!(
                    "paid policy {key} does not match an explicitly authorized OpenRouter candidate in this batch"
                )));
            }
        }
        Ok(DryRunPlan {
            batch_id: request.batch_id.clone(),
            issues,
            worker_count: request.policy.max_concurrency,
            policy: request.policy.clone(),
            warnings,
        })
    }

    /// Persist the exact requested batch and all admission policy before submitting jobs.
    /// Calling this twice with the same batch ID and identical request is idempotent.
    pub fn start(
        &self,
        request: &BatchRequest,
        now_ms: i64,
    ) -> Result<BatchSnapshot, SupervisorError> {
        let mut normalized = normalize_request(request)?;
        let current_settings = read_settings_fingerprint(&normalized.settings_path)?;
        if normalized
            .settings_fingerprint
            .as_ref()
            .is_some_and(|value| value != &current_settings)
        {
            return Err(SupervisorError::Conflict(
                "settings changed since preview; review a fresh plan".into(),
            ));
        }
        normalized.settings_fingerprint = Some(current_settings);
        validate_request(&normalized)?;
        let store = BatchStore::open(&self.paths.batches)?;
        match store.request(&normalized.batch_id) {
            Ok(admitted) => {
                if admitted != normalized {
                    return Err(SupervisorError::Conflict(
                        "batch ID was already used for a different immutable request".into(),
                    ));
                }
                let settings = settings::load_settings(&normalized.settings_path)
                    .map_err(|error| SupervisorError::Invalid(format!("settings_load: {error}")))?;
                let existing_snapshot = store.get(&normalized.batch_id)?;
                if all_selected_routes_refresh_consented(&normalized, &settings) {
                    let (facts, fingerprint, problem) = load_provider_observations(
                        &settings,
                        &self.paths.root,
                        existing_snapshot.created_at_ms.max(0) as u128,
                    )?;
                    if problem.is_some()
                        || normalized.provider_observation_fingerprint.as_deref()
                            != Some(fingerprint.as_str())
                    {
                        return Err(SupervisorError::Conflict(
                            "original provider evidence is unavailable; cannot validate idempotent admission".into(),
                        ));
                    }
                    let preview = Self::dry_run_request_with_facts(&normalized, &settings, &facts)?;
                    if preview
                        .issues
                        .iter()
                        .any(|issue| issue.eligibility == PreviewEligibility::Blocked)
                        || routes_from_plan(&preview) != normalized.resolved_routes
                        || derive_route_commitments(&normalized, &facts)?
                            != normalized.route_commitments
                    {
                        return Err(SupervisorError::Conflict(
                            "durable route semantics no longer match the original admission evidence".into(),
                        ));
                    }
                } else {
                    let preview = self.dry_run(&normalized)?;
                    if preview
                        .issues
                        .iter()
                        .any(|issue| issue.eligibility == PreviewEligibility::Blocked)
                    {
                        return Err(SupervisorError::Invalid(
                            "batch preview contains blocked task routes".into(),
                        ));
                    }
                }
                // Idempotent attachment must not re-run admission against an
                // observation cache that can age while the batch is active.
                // The exact persisted request and settings fingerprint remain
                // authoritative; selected routes refresh at dispatch when consented.
                let snapshot = existing_snapshot;
                if now_ms >= snapshot.deadline_ms {
                    return Err(SupervisorError::Conflict(
                        "original batch deadline has expired; idempotent start will not extend it"
                            .into(),
                    ));
                }
                return Ok(snapshot);
            }
            Err(SupervisorError::NotFound(_)) => {}
            Err(error) => return Err(error),
        }
        let preview = self.dry_run(&normalized)?;
        if preview
            .issues
            .iter()
            .any(|issue| issue.eligibility == PreviewEligibility::Blocked)
        {
            return Err(SupervisorError::Invalid(
                "batch preview contains blocked task routes".into(),
            ));
        }
        let request_json = serde_json::to_string(&normalized)
            .map_err(|e| SupervisorError::Invalid(e.to_string()))?;
        store.begin(&normalized, &request_json, now_ms)?;
        let jobs = JobsStore::open(&self.paths.jobs)?;
        let submitted = jobs.submit_batch(&normalized.jobs, now_ms)?;
        let budgets = BudgetStore::open(&self.paths.budgets)?;
        for job in &submitted {
            let input = normalized
                .jobs
                .iter()
                .find(|v| v.issue_id == job.issue_id)
                .ok_or_else(|| {
                    SupervisorError::Store("submitted job has no immutable request snapshot".into())
                })?;
            let per_job = job_budget(&input.plan);
            budgets.configure(
                &normalized.batch_id,
                &job.id.to_string(),
                &normalized.policy.budget,
                &per_job,
            )?;
        }
        store.link_jobs(&normalized.batch_id, &submitted, now_ms)?;
        Ok(store.get(&normalized.batch_id)?)
    }

    /// Persist and explicitly launch the requested number of independent workers.
    /// The CLI calls this only for a user-selected start action; dry-run never launches.
    pub fn start_and_launch(
        &self,
        request: &BatchRequest,
        worker_executable: &Path,
        now_ms: i64,
    ) -> Result<BatchSnapshot, SupervisorError> {
        let normalized = normalize_request(request)?;
        if self.admitted_request(&normalized.batch_id)?.is_some() {
            return Err(SupervisorError::Conflict(
                "existing batches require foreground reattachment with their saved preview fingerprint".into(),
            ));
        }
        let batch = self.start(&normalized, now_ms)?;
        self.launch_workers_after_admission(
            &normalized,
            worker_executable,
            now_ms,
            WorkerLaunchMode::Detached,
        )?;
        Ok(BatchStore::open(&self.paths.batches)?.get(&batch.batch_id)?)
    }

    /// Explicitly launch workers hosted by the current process. The caller must
    /// remain alive and retain its Windows job until workers reach control or
    /// terminal state; this never falls back to detached launch.
    pub fn start_and_launch_hosted(
        &self,
        request: &BatchRequest,
        worker_executable: &Path,
        now_ms: i64,
    ) -> Result<BatchSnapshot, SupervisorError> {
        let normalized = normalize_request(request)?;
        if self.admitted_request(&normalized.batch_id)?.is_some() {
            return Err(SupervisorError::Conflict(
                "existing batches require foreground reattachment with their saved preview fingerprint".into(),
            ));
        }
        let batch = self.start(&normalized, now_ms)?;
        self.launch_workers_after_admission(
            &normalized,
            worker_executable,
            now_ms,
            WorkerLaunchMode::Hosted,
        )?;
        Ok(BatchStore::open(&self.paths.batches)?.get(&batch.batch_id)?)
    }

    /// Start an explicitly selected batch with workers hosted by this process.
    /// The caller remains alive until durable terminal/control state is reached;
    /// hosted workers are never silently replaced with detached workers.
    pub fn run_batch_foreground(
        &self,
        request: &BatchRequest,
        worker_executable: &Path,
        now: i64,
    ) -> Result<BatchSnapshot, SupervisorError> {
        let normalized = normalize_request(request)?;
        let batch = self.start(&normalized, now)?;
        let snapshot = BatchStore::open(&self.paths.batches)?.get(&batch.batch_id)?;
        match snapshot.state {
            BatchState::Completed
            | BatchState::Failed
            | BatchState::WindowExpired
            | BatchState::Cancelled => {
                return Err(SupervisorError::Conflict(
                    "run-batch cannot restart a terminal batch".into(),
                ));
            }
            BatchState::Paused | BatchState::Waiting => {
                self.ensure_waiting_batch_resumable(&snapshot)?;
                self.resume_and_launch_hosted(&batch.batch_id, worker_executable, now)?;
            }
            BatchState::Starting | BatchState::Running => {
                self.launch_workers_after_admission(
                    &normalized,
                    worker_executable,
                    now,
                    WorkerLaunchMode::Hosted,
                )?;
            }
            BatchState::Stopping | BatchState::Cancelling => {}
        }
        self.wait_for_hosted_batch(&batch.batch_id)
    }

    /// Reattach a foreground host to an already-admitted immutable batch.
    /// Admission preview is intentionally not repeated here: an explicitly
    /// consented model-free dispatch refresh is the freshness gate for each
    /// selected route, while the durable request, settings, route commitments,
    /// and original deadline remain fixed.
    pub fn run_admitted_batch_foreground(
        &self,
        request: &BatchRequest,
        worker_executable: &Path,
        prior_preview_fingerprint: &str,
        now: i64,
    ) -> Result<BatchSnapshot, SupervisorError> {
        let normalized = normalize_request(request)?;
        validate_request(&normalized)?;
        if prior_preview_fingerprint.len() != 40
            || !prior_preview_fingerprint
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit())
        {
            return Err(SupervisorError::Invalid(
                "reattaching an admitted batch requires its saved preview fingerprint".into(),
            ));
        }
        let store = BatchStore::open(&self.paths.batches)?;
        let admitted = store.request(&normalized.batch_id)?;
        if admitted != normalized {
            return Err(SupervisorError::Conflict(
                "run-batch request differs from the durable admitted request".into(),
            ));
        }
        let current_settings = read_settings_fingerprint(&normalized.settings_path)?;
        if normalized.settings_fingerprint.as_deref() != Some(current_settings.as_str()) {
            return Err(SupervisorError::Conflict(
                "provider settings changed since admission; review a new batch".into(),
            ));
        }
        let settings = settings::load_settings(&normalized.settings_path)
            .map_err(|error| SupervisorError::Invalid(format!("settings_load: {error}")))?;
        let batch = store.get(&normalized.batch_id)?;
        let effective_now = now.max(now_ms());
        let freshness_time = if all_selected_routes_refresh_consented(&normalized, &settings) {
            // Reconstruct the exact admission preview at the durable batch's
            // creation time. This preserves request/plan/worker identity while
            // allowing stale evidence to be revalidated by the explicitly
            // consented selected-route refresh immediately before dispatch.
            batch.created_at_ms.max(0) as u128
        } else {
            effective_now.max(0) as u128
        };
        let original_preview = preview_batch_request_at(
            &self.paths.root,
            &normalized,
            worker_executable,
            freshness_time,
        )?;
        if original_preview.fingerprint != prior_preview_fingerprint {
            return Err(SupervisorError::Conflict(
                "admitted request, route plan, provider evidence, or worker identity differs from the saved preview".into(),
            ));
        }
        if effective_now >= batch.deadline_ms {
            return Err(SupervisorError::Conflict(
                "original batch deadline has expired; reattachment will not extend it".into(),
            ));
        }
        match batch.state {
            BatchState::Completed
            | BatchState::Failed
            | BatchState::WindowExpired
            | BatchState::Cancelled => {
                return Err(SupervisorError::Conflict(
                    "run-batch cannot restart a terminal admitted batch".into(),
                ));
            }
            BatchState::Paused | BatchState::Waiting => {
                self.ensure_waiting_batch_resumable(&batch)?;
                self.resume_and_launch_hosted(&batch.batch_id, worker_executable, effective_now)?;
            }
            BatchState::Starting | BatchState::Running => {
                self.launch_workers_after_admission(
                    &normalized,
                    worker_executable,
                    effective_now,
                    WorkerLaunchMode::Hosted,
                )?;
            }
            BatchState::Stopping | BatchState::Cancelling => {}
        }
        self.wait_for_hosted_batch(&batch.batch_id)
    }

    fn ensure_waiting_batch_resumable(
        &self,
        snapshot: &BatchSnapshot,
    ) -> Result<(), SupervisorError> {
        if snapshot.state != BatchState::Waiting {
            return Ok(());
        }
        let jobs = JobsStore::open(&self.paths.jobs)?;
        for id in &snapshot.job_ids {
            let job = jobs.get(*id)?;
            if matches!(
                job.state,
                JobState::NeedsInput
                    | JobState::Blocked
                    | JobState::ReconciliationRequired
                    | JobState::Failed
                    | JobState::Stale
                    | JobState::Cancelled
            ) {
                return Err(SupervisorError::Conflict(
                    "waiting batch contains a job requiring explicit recovery or input; resume will not restart it".into(),
                ));
            }
        }
        Ok(())
    }

    fn wait_for_hosted_batch(&self, batch_id: &str) -> Result<BatchSnapshot, SupervisorError> {
        self.wait_for_hosted_batch_with_liveness(batch_id, |this, store, snapshot| {
            this.hosted_worker_is_live(store, snapshot)
        })
    }

    fn wait_for_hosted_batch_with_liveness<F>(
        &self,
        batch_id: &str,
        mut is_live: F,
    ) -> Result<BatchSnapshot, SupervisorError>
    where
        F: FnMut(&Self, &BatchStore, &BatchSnapshot) -> Result<bool, SupervisorError>,
    {
        loop {
            let store = BatchStore::open(&self.paths.batches)?;
            let snapshot = store.get(batch_id)?;
            match snapshot.state {
                BatchState::Completed
                | BatchState::Failed
                | BatchState::WindowExpired
                | BatchState::Cancelled
                | BatchState::Waiting => return Ok(snapshot),
                BatchState::Paused => {
                    if now_ms() >= snapshot.deadline_ms {
                        return Err(SupervisorError::Conflict(
                            "foreground host reached the absolute batch deadline; durable jobs and reservations remain held for reconciliation".into(),
                        ));
                    }
                    if !is_live(self, &store, &snapshot)? {
                        if let Some(fresh) = self.refresh_after_hosted_worker_exit(&snapshot)? {
                            return Ok(fresh);
                        }
                    }
                }
                BatchState::Starting
                | BatchState::Running
                | BatchState::Stopping
                | BatchState::Cancelling => {
                    if now_ms() >= snapshot.deadline_ms {
                        return Err(SupervisorError::Conflict(
                            "foreground host reached the absolute batch deadline; durable jobs and reservations remain held for reconciliation".into(),
                        ));
                    }
                    if !is_live(self, &store, &snapshot)? {
                        if let Some(fresh) = self.refresh_after_hosted_worker_exit(&snapshot)? {
                            return Ok(fresh);
                        }
                        return Err(SupervisorError::Conflict("all hosted workers exited before the batch reached terminal or control state; persisted status requires reconciliation".into()));
                    }
                }
            }
            thread::sleep(Duration::from_millis(200));
        }
    }

    // Liveness is observed separately from the durable batch row. A worker can
    // commit terminal/control state and exit between those reads, so always
    // refresh the batch before deciding that a dead process left active work.
    fn refresh_after_hosted_worker_exit(
        &self,
        previously_observed: &BatchSnapshot,
    ) -> Result<Option<BatchSnapshot>, SupervisorError> {
        let fresh = BatchStore::open(&self.paths.batches)?.get(&previously_observed.batch_id)?;
        Ok((!matches!(
            fresh.state,
            BatchState::Starting
                | BatchState::Running
                | BatchState::Stopping
                | BatchState::Cancelling
        ))
        .then_some(fresh))
    }

    fn hosted_worker_is_live(
        &self,
        store: &BatchStore,
        snapshot: &BatchSnapshot,
    ) -> Result<bool, SupervisorError> {
        let mut live = false;
        for slot in store.worker_slots(&snapshot.batch_id)? {
            match slot.state.as_str() {
                "running" => {
                    let (Some(pid), Some(creation_time_100ns)) =
                        (slot.pid, slot.creation_time_100ns)
                    else {
                        return Err(SupervisorError::Conflict(
                            "hosted worker process identity is incomplete; durable status requires reconciliation".into(),
                        ));
                    };
                    let identity = crate::process::ProcessIdentity {
                        pid,
                        creation_time_100ns: Some(creation_time_100ns),
                    };
                    match crate::process::process_identity_matches(identity) {
                        Ok(true) => live = true,
                        Ok(false) => store.worker_runtime_failed(
                            &slot.worker_id,
                            "hosted worker process exited; active job and reservation state retained for reconciliation",
                            now_ms(),
                        )?,
                        Err(error) => {
                            return Err(SupervisorError::Conflict(format!(
                                "cannot verify hosted worker liveness: {error}"
                            )))
                        }
                    }
                }
                "launching" => {
                    if now_ms().saturating_sub(slot.updated_at_ms) > 45_000 {
                        store.worker_failed(
                            &slot.worker_id,
                            "hosted worker launch handshake expired; explicit reconciliation required",
                            now_ms(),
                        )?;
                    } else {
                        live = true;
                    }
                }
                _ => {}
            }
        }
        Ok(live)
    }

    /// Resume a paused/stopped batch and replenish only missing worker capacity.
    pub fn resume_and_launch(
        &self,
        batch_id: &str,
        worker_executable: &Path,
        now: i64,
    ) -> Result<BatchSnapshot, SupervisorError> {
        self.resume_and_launch_mode(batch_id, worker_executable, now, WorkerLaunchMode::Detached)
    }

    /// Explicitly resume/reconnect workers under the current foreground host.
    /// This path never attempts detached breakaway and preserves the admitted
    /// request, budgets, attempt counts, and absolute deadline.
    pub fn resume_and_launch_hosted(
        &self,
        batch_id: &str,
        worker_executable: &Path,
        now: i64,
    ) -> Result<BatchSnapshot, SupervisorError> {
        self.resume_and_launch_mode(batch_id, worker_executable, now, WorkerLaunchMode::Hosted)
    }

    fn resume_and_launch_mode(
        &self,
        batch_id: &str,
        worker_executable: &Path,
        now: i64,
        launch_mode: WorkerLaunchMode,
    ) -> Result<BatchSnapshot, SupervisorError> {
        let store = BatchStore::open(&self.paths.batches)?;
        let snapshot = store.get(batch_id)?;
        let request = store.request(batch_id)?;
        validate_settings_fingerprint(&request).map_err(|e| {
            SupervisorError::Conflict(format!(
                "cannot resume because admitted settings changed: {e}"
            ))
        })?;
        if now >= snapshot.deadline_ms {
            return Err(SupervisorError::Conflict(
                "cannot reattach workers after the absolute batch deadline".into(),
            ));
        }
        for job in &request.jobs {
            if request.issue_sources.contains_key(&job.issue_id) {
                validate_job_source_binding(&request, &job.issue_id, &job.source_fingerprint)
                    .map_err(|e| {
                        SupervisorError::Conflict(format!(
                            "cannot resume because issue source {} changed: {e}",
                            job.issue_id
                        ))
                    })?;
            }
        }
        if matches!(
            snapshot.state,
            BatchState::Completed
                | BatchState::Failed
                | BatchState::WindowExpired
                | BatchState::Cancelled
        ) {
            return Err(SupervisorError::Conflict(
                "cannot resume a terminal batch".into(),
            ));
        }
        self.ensure_waiting_batch_resumable(&snapshot)?;
        if snapshot.state != BatchState::Running && snapshot.state != BatchState::Starting {
            self.resume(batch_id, "explicitly resumed", now)?;
        }
        self.launch_workers_after_admission(&request, worker_executable, now, launch_mode)?;
        BatchStore::open(&self.paths.batches)?.get(batch_id)
    }

    /// Apply one explicit recovery decision only after proving membership and
    /// checking all inspectable process identities owned by the interrupted job.
    pub fn recover_job(
        &self,
        batch_id: &str,
        recovery: RecoveryRequest,
        now: i64,
    ) -> Result<RecoveryResult, SupervisorError> {
        let batch_store = BatchStore::open(&self.paths.batches)?;
        let snapshot = batch_store.get(batch_id)?;
        let request = batch_store.request(batch_id)?;
        validate_settings_fingerprint(&request).map_err(|e| {
            SupervisorError::Conflict(format!(
                "recovery refused because admitted settings changed: {e}"
            ))
        })?;
        for entry in &request.jobs {
            if request.issue_sources.contains_key(&entry.issue_id) {
                validate_job_source_binding(&request, &entry.issue_id, &entry.source_fingerprint)
                    .map_err(|e| {
                    SupervisorError::Conflict(format!(
                        "recovery refused because issue source {} changed: {e}",
                        entry.issue_id
                    ))
                })?;
            }
        }
        let job = JobsStore::open(&self.paths.jobs)?.get(recovery.job_id)?;
        if !snapshot.job_ids.contains(&job.id)
            || !request
                .jobs
                .iter()
                .any(|entry| entry.issue_id == job.issue_id)
        {
            return Err(SupervisorError::Conflict(
                "job does not belong to this persisted batch".into(),
            ));
        }
        let jobs = JobsStore::open(&self.paths.jobs)?;
        let attempt = jobs.recovery_attempt_info(job.id)?;
        let worker_slots = batch_store.worker_slots(batch_id)?;
        if let Some(slot) = worker_slots
            .iter()
            .find(|slot| slot.worker_id == attempt.worker_id && slot.state == "running")
        {
            let (Some(pid), Some(creation)) = (slot.pid, slot.creation_time_100ns) else {
                return Err(SupervisorError::Conflict(
                    "the owning worker has no inspectable process identity; recovery is blocked"
                        .into(),
                ));
            };
            if crate::process::process_identity_matches(crate::process::ProcessIdentity {
                pid,
                creation_time_100ns: Some(creation),
            })
            .unwrap_or(true)
            {
                return Err(SupervisorError::Conflict("the worker that owns this attempt is still live (or liveness is unknown); recovery is blocked".into()));
            }
        }
        let mut identities = Vec::new();
        if let Some(raw) = attempt.process_ref.as_deref() {
            identities.push(
                serde_json::from_str::<crate::process::ProcessIdentity>(raw).map_err(|_| {
                    SupervisorError::Conflict(
                        "stored attempt process identity is malformed; recovery is blocked".into(),
                    )
                })?,
            );
        }
        if let Some(value) = jobs.get_checkpoint(job.id, "runtime_process_identity")? {
            identities.push(
                serde_json::from_value::<crate::process::ProcessIdentity>(value).map_err(|_| {
                    SupervisorError::Conflict(
                        "stored runtime process identity is malformed; recovery is blocked".into(),
                    )
                })?,
            );
        }
        let dispatch_intents = request
            .jobs
            .iter()
            .filter(|entry| entry.issue_id == job.issue_id)
            .flat_map(|entry| entry.plan.tasks.iter())
            .filter(|task| matches!(task.kind, TaskKind::Implementation | TaskKind::Worker))
            .map(|task| task.task_id.as_str());
        for task_id in dispatch_intents {
            let dispatch_key = format!("dispatch_{}", safe_checkpoint_key(task_id));
            if jobs
                .get_checkpoint(job.id, &dispatch_key)?
                .is_some_and(|value| {
                    value.get("state").and_then(Value::as_str) == Some("intent_persisted")
                })
            {
                let process_key = format!("runtime_process_{}", safe_checkpoint_key(task_id));
                let process = jobs.get_checkpoint(job.id, &process_key)?.ok_or_else(|| {
                    SupervisorError::Conflict("a runtime dispatch intent has no task-specific process checkpoint; recovery is blocked".into())
                })?;
                if process.get("state").and_then(Value::as_str) != Some("started") {
                    return Err(SupervisorError::Conflict("a runtime dispatch may have started without a persisted process identity; recovery is blocked".into()));
                }
                identities.push(serde_json::from_value::<crate::process::ProcessIdentity>(process)
                    .map_err(|_| SupervisorError::Conflict("task-specific runtime process identity is malformed; recovery is blocked".into()))?);
            }
        }
        if let Some(value) = jobs.get_checkpoint(job.id, "verification_review_process")? {
            if value.get("state").and_then(Value::as_str) == Some("started") {
                let (Some(pid), creation) = (
                    value.get("pid").and_then(Value::as_u64),
                    value.get("creationTime100ns").and_then(Value::as_u64),
                ) else {
                    return Err(SupervisorError::Conflict("reviewer start is recorded without an inspectable owned process identity; recovery is blocked".into()));
                };
                identities.push(crate::process::ProcessIdentity {
                    pid: pid as u32,
                    creation_time_100ns: creation,
                });
            }
        }
        for (index, policy) in request
            .verification
            .iter()
            .filter(|p| p.issue_id == job.issue_id)
            .flat_map(|p| p.checks.iter().enumerate())
        {
            let check_key = format!("verification_check_{index}");
            if jobs
                .get_checkpoint(job.id, &check_key)?
                .is_some_and(|v| v.get("state").and_then(Value::as_str) == Some("started"))
            {
                let process_key = format!("verification_check_process_{index}");
                let process = jobs.get_checkpoint(job.id, &process_key)?.ok_or_else(|| SupervisorError::Conflict(format!("check {} was started without a journaled process identity; recovery is blocked", policy.id)))?;
                if process.get("state").and_then(Value::as_str) != Some("started") {
                    return Err(SupervisorError::Conflict(format!(
                        "check {} has no started process identity checkpoint; recovery is blocked",
                        policy.id
                    )));
                }
                let identity = checkpoint_process_identity(&process).ok_or_else(|| {
                    SupervisorError::Conflict(format!(
                        "check {} process identity is incomplete; recovery is blocked",
                        policy.id
                    ))
                })?;
                identities.push(identity);
            }
        }
        if identities
            .iter()
            .any(|identity| crate::process::process_identity_matches(*identity).unwrap_or(true))
        {
            return Err(SupervisorError::Conflict("an owned runtime or reviewer process is still live (or its liveness is unknown); recovery is blocked".into()));
        }
        if recovery.decision == RecoveryAction::SafeToRetry
            && (!recovery.process_confirmed_stopped || !recovery.effects_reconciled)
        {
            return Err(SupervisorError::Invalid("safe_to_retry requires explicit processConfirmedStopped and effectsReconciled confirmations".into()));
        }
        let decision = match recovery.decision {
            RecoveryAction::SafeToRetry => RecoveryDecision::SafeToRetry {
                evidence: recovery.evidence,
                process_confirmed_stopped: recovery.process_confirmed_stopped,
                effects_reconciled: recovery.effects_reconciled,
            },
            RecoveryAction::KeepNeedsInput => RecoveryDecision::KeepNeedsInput {
                evidence: recovery.evidence,
            },
            RecoveryAction::MarkFailed => RecoveryDecision::MarkFailed {
                evidence: recovery.evidence,
            },
        };
        if recovery.decision == RecoveryAction::SafeToRetry {
            preserve_recovered_reservation_envelopes(
                &BudgetStore::open(&self.paths.budgets)?,
                &jobs,
                &request,
                &job,
                &attempt,
                now.max(0) as u64,
            )?;
        }
        let updated = jobs.resolve_recovery(job.id, decision, now)?;
        Ok(RecoveryResult {
            batch_id: batch_id.into(),
            job: updated,
            batch_membership_verified: true,
            process_confirmed_stopped: recovery.process_confirmed_stopped,
            effects_reconciled: recovery.effects_reconciled,
            decision: recovery.decision,
        })
    }

    fn launch_workers_after_admission(
        &self,
        request: &BatchRequest,
        worker_executable: &Path,
        now: i64,
        launch_mode: WorkerLaunchMode,
    ) -> Result<(), SupervisorError> {
        self.launch_workers_with_mode(request, worker_executable, now, launch_mode)
            .map_err(|error| SupervisorError::WorkerLaunch(error.to_string()))
    }

    fn launch_workers_with_mode(
        &self,
        request: &BatchRequest,
        worker_executable: &Path,
        now: i64,
        launch_mode: WorkerLaunchMode,
    ) -> Result<(), SupervisorError> {
        if !worker_executable.is_absolute() || !worker_executable.is_file() {
            return Err(SupervisorError::Invalid(
                "worker executable must be an existing absolute file".into(),
            ));
        }
        let store = BatchStore::open(&self.paths.batches)?;
        let mut slots = store.worker_slots(&request.batch_id)?;
        let mut live = 0u32;
        for slot in &slots {
            if slot.state == "running" {
                let identity = slot.pid.map(|pid| crate::process::ProcessIdentity {
                    pid,
                    creation_time_100ns: slot.creation_time_100ns,
                });
                if let Some(identity) = identity {
                    match crate::process::process_identity_matches(identity) {
                        Ok(true) => live += 1,
                        Ok(false) => store.worker_failed(
                            &slot.worker_id,
                            "registered worker process is no longer alive",
                            now,
                        )?,
                        Err(error) => {
                            return Err(SupervisorError::Conflict(format!(
                                "cannot safely reattach while worker liveness is unknown: {error}"
                            )));
                        }
                    }
                } else {
                    store.worker_failed(
                        &slot.worker_id,
                        "registered worker has no persisted process identity",
                        now,
                    )?;
                }
            } else if slot.state == "launching" {
                // The worker has a 20-second one-shot handshake. Reclaim only
                // launch intents older than that plus a startup margin.
                if now.saturating_sub(slot.updated_at_ms) > 45_000 {
                    store.worker_failed(
                        &slot.worker_id,
                        "worker launch handshake expired; slot reclaimed during explicit reattach",
                        now,
                    )?;
                } else {
                    live += 1;
                }
            }
        }
        let mut next_slot = slots
            .iter()
            .map(|s| s.slot)
            .max()
            .map_or(0, |n| n.saturating_add(1));
        let capacity = request.policy.max_concurrency.saturating_sub(live);
        for _ in 0..capacity {
            let slot = next_slot;
            next_slot = next_slot.saturating_add(1);
            let worker_id = format!("{}-{slot}-{now}", request.batch_id);
            if !store.reserve_worker(&request.batch_id, &worker_id, slot, now)? {
                continue;
            }
            let args = vec![
                "--data-root".into(),
                self.paths.root.to_string_lossy().into_owned(),
                "--batch-id".into(),
                request.batch_id.clone(),
                "--worker-id".into(),
                worker_id.clone(),
            ];
            let spawned = match launch_mode {
                WorkerLaunchMode::Detached => crate::process::spawn_detached_worker(
                    worker_executable,
                    &args,
                    &request.project_path,
                ),
                WorkerLaunchMode::Hosted => crate::process::spawn_hosted_worker(
                    worker_executable,
                    &args,
                    &request.project_path,
                ),
            };
            match spawned {
                Ok(identity) => store.worker_started(
                    &worker_id,
                    identity.pid,
                    identity.creation_time_100ns,
                    now,
                )?,
                Err(error) => {
                    store.worker_failed(&worker_id, &error.to_string(), now)?;
                    return Err(SupervisorError::Io(format!(
                        "could not launch worker slot {slot}: {error}"
                    )));
                }
            }
        }
        slots = store.worker_slots(&request.batch_id)?;
        if slots
            .iter()
            .all(|slot| slot.state != "running" && slot.state != "launching")
        {
            return Err(SupervisorError::Conflict(
                "batch has no registered live worker".into(),
            ));
        }
        Ok(())
    }

    pub fn pause(
        &self,
        batch_id: &str,
        note: &str,
        now: i64,
    ) -> Result<BatchSnapshot, SupervisorError> {
        self.control(batch_id, "paused", note, now)
    }
    pub fn resume(
        &self,
        batch_id: &str,
        note: &str,
        now: i64,
    ) -> Result<BatchSnapshot, SupervisorError> {
        self.control(batch_id, "running", note, now)
    }
    pub fn stop(
        &self,
        batch_id: &str,
        note: &str,
        now: i64,
    ) -> Result<BatchSnapshot, SupervisorError> {
        self.control(batch_id, "stopping", note, now)
    }
    pub fn cancel(
        &self,
        batch_id: &str,
        note: &str,
        now: i64,
    ) -> Result<BatchSnapshot, SupervisorError> {
        let store = BatchStore::open(&self.paths.batches)?;
        let current = store.get(batch_id)?;
        if current.state == BatchState::Cancelled {
            return Ok(current);
        }
        if current.state != BatchState::Cancelling {
            store.control(batch_id, "cancelling", note, now)?;
        }
        let request = store.request(batch_id)?;
        let snapshot = store.get(batch_id)?;
        let jobs = JobsStore::open(&self.paths.jobs)?;
        for id in &snapshot.job_ids {
            let mut job = jobs.get(*id)?;
            if job.state == JobState::NeedsInput
                && !owned_processes_stopped_for_cancel(&jobs, &job, &request)?
            {
                // A NeedsInput outcome can be recorded while a provider or
                // verifier process is still alive, or before its identity is
                // durably known. Keep the job visible until that is resolved.
                continue;
            }
            if matches!(
                job.state,
                JobState::Queued
                    | JobState::Blocked
                    | JobState::NeedsInput
                    | JobState::Leased
                    | JobState::Running
            ) {
                match jobs.request_cancel(*id, now) {
                    Ok(updated) => job = updated,
                    // A worker may finish between the read and request. Re-read
                    // to handle an awaiting-verification transition safely.
                    Err(JobsError::Conflict(_)) => job = jobs.get(*id)?,
                    Err(error) => return Err(error.into()),
                }
            }
            if matches!(job.state, JobState::Leased | JobState::Running) {
                if let Ok(Some(value)) = jobs.get_checkpoint(*id, "runtime_process_identity") {
                    if let Ok(identity) =
                        serde_json::from_value::<crate::process::ProcessIdentity>(value)
                    {
                        let _ = crate::process::terminate_owned_process(identity);
                    }
                }
            } else if job.state == JobState::AwaitingVerification {
                if let Some(policy) = request
                    .verification
                    .iter()
                    .find(|p| p.issue_id == job.issue_id)
                {
                    for index in 0..policy.checks.len().min(128) {
                        if let Ok(Some(value)) =
                            jobs.get_checkpoint(*id, &format!("verification_check_process_{index}"))
                        {
                            if let Some(identity) = checkpoint_process_identity(&value) {
                                let _ = crate::process::terminate_owned_process(identity);
                            }
                        }
                    }
                }
                if let Ok(Some(value)) = jobs.get_checkpoint(*id, "verification_review_process") {
                    if let Some(identity) = checkpoint_process_identity(&value) {
                        let _ = crate::process::terminate_owned_process(identity);
                    }
                }
            }
        }
        let states = snapshot_jobs(&jobs, &snapshot.job_ids)?;
        let all_safe_terminal = states.iter().all(|state| is_safe_cancel_terminal(*state));
        if all_safe_terminal && !store.has_active_verifications(batch_id, now)? {
            // A cancelling worker or idle foreground host may race this call.
            // Ambiguous or not-yet-verified jobs remain visible for reconciliation.
            store.set_cancelled_if_idle(batch_id, now)?;
        }
        store.get(batch_id)
    }
    fn control(
        &self,
        batch_id: &str,
        state: &str,
        note: &str,
        now: i64,
    ) -> Result<BatchSnapshot, SupervisorError> {
        BatchStore::open(&self.paths.batches)?.control(batch_id, state, note, now)
    }

    pub fn status(&self, batch_id: &str) -> Result<BatchSnapshot, SupervisorError> {
        BatchStore::open(&self.paths.batches)?.get(batch_id)
    }

    pub fn status_view(&self, batch_id: &str) -> Result<BatchStatusView, SupervisorError> {
        let snapshot = self.status(batch_id)?;
        let summary = self.morning_summary(batch_id)?;
        let jobs = JobsStore::open(&self.paths.jobs)?;
        let request = BatchStore::open(&self.paths.batches)?.request(batch_id)?;
        let mut linked = Vec::with_capacity(snapshot.job_ids.len().min(MAX_BATCH_JOBS));
        let mut checkpoint_bytes = 0usize;
        for id in snapshot.job_ids.iter().take(MAX_BATCH_JOBS) {
            let j = jobs.get(*id)?;
            let mut checkpoints = Vec::new();
            let mut checkpoints_truncated = false;
            for key in status_checkpoint_keys(&request, &j) {
                let is_runtime_output = key.starts_with("runtime_output_");
                if let Some(value) = jobs.get_checkpoint(j.id, &key)? {
                    if checkpoints.len() >= 20 || checkpoint_bytes >= 64 * 1024 {
                        checkpoints_truncated = true;
                        break;
                    }
                    let Ok(encoded) = serde_json::to_vec(&value) else {
                        checkpoints_truncated = true;
                        continue;
                    };
                    let status_value = if encoded.len() > 4096 {
                        if is_runtime_output {
                            match bounded_runtime_output_status_value(&value, 4096) {
                                Some(projected) => projected,
                                None => {
                                    checkpoints_truncated = true;
                                    continue;
                                }
                            }
                        } else {
                            checkpoints_truncated = true;
                            continue;
                        }
                    } else {
                        value
                    };
                    let Ok(status_bytes) = serde_json::to_vec(&status_value) else {
                        checkpoints_truncated = true;
                        continue;
                    };
                    if status_bytes.len() > 4096 {
                        checkpoints_truncated = true;
                        continue;
                    }
                    if checkpoint_bytes.saturating_add(status_bytes.len()) > 64 * 1024 {
                        checkpoints_truncated = true;
                        break;
                    }
                    checkpoint_bytes += status_bytes.len();
                    checkpoints.push(CheckpointStatusView {
                        key,
                        value: status_value,
                    });
                }
            }
            linked.push(JobStatusView {
                id: j.id,
                issue_id: j.issue_id,
                state: j.state,
                completion_gate: j.completion_gate,
                attempt_count: j.attempt_count,
                max_attempts: j.max_attempts,
                updated_at_ms: j.updated_at_ms,
                last_attempt_summary: if matches!(
                    j.state,
                    JobState::NeedsInput | JobState::ReconciliationRequired
                ) {
                    jobs.latest_attempt_summary(j.id)?
                        .and_then(|summary| runtime::safe_status_summary(&summary, 2 * 1024))
                } else {
                    None
                },
                checkpoints_truncated,
                checkpoints,
            });
        }
        let budget = read_budget_status(&self.paths.budgets, batch_id)?;
        Ok(BatchStatusView {
            snapshot,
            summary,
            jobs: linked,
            budget,
        })
    }

    pub fn events(
        &self,
        batch_id: &str,
        cursor: u64,
        limit: u32,
    ) -> Result<BatchEventPage, SupervisorError> {
        if limit == 0 || limit > 100 {
            return Err(SupervisorError::Invalid(
                "event limit must be between 1 and 100".into(),
            ));
        }
        let events = BatchStore::open(&self.paths.batches)?.events(batch_id, cursor, limit + 1)?;
        let has_more = events.len() > limit as usize;
        let events = events.into_iter().take(limit as usize).collect::<Vec<_>>();
        let next_cursor = events.last().map_or(cursor, |event| event.sequence);
        Ok(BatchEventPage {
            events,
            next_cursor,
            has_more,
        })
    }

    pub fn reconcile_batch(&self, batch_id: &str) -> Result<ReconcileReport, SupervisorError> {
        let snapshot = self.status(batch_id)?;
        let jobs = JobsStore::open(&self.paths.jobs)?;
        let items = jobs.reconcile_expired_ids(&snapshot.job_ids, now_ms())?;
        let items = items
            .into_iter()
            .map(|item| {
                let process_alive = item
                    .process_ref
                    .as_deref()
                    .and_then(|value| {
                        serde_json::from_str::<crate::process::ProcessIdentity>(value).ok()
                    })
                    .and_then(|identity| crate::process::process_identity_matches(identity).ok());
                ReconcileStatusItem {
                    job_id: item.job_id,
                    attempt_number: item.attempt_number,
                    worker_id: item.worker_id,
                    process_ref: item.process_ref,
                    process_alive,
                    lease_until_ms: item.lease_until_ms,
                }
            })
            .collect();
        Ok(ReconcileReport {
            batch_id: batch_id.into(),
            items,
        })
    }

    pub fn morning_summary(&self, batch_id: &str) -> Result<MorningSummary, SupervisorError> {
        let batch = BatchStore::open(&self.paths.batches)?.get(batch_id)?;
        let jobs = JobsStore::open(&self.paths.jobs)?;
        let mut summary = MorningSummary {
            batch_id: batch_id.into(),
            completed: vec![],
            blocked: vec![],
            failed: vec![],
            cancelled: vec![],
            unstarted: vec![],
            needs_input: vec![],
            reconciliation_required: vec![],
        };
        for id in batch.job_ids {
            let job = jobs.get(id)?;
            match job.state {
                JobState::Verified => summary.completed.push(job.issue_id),
                JobState::Blocked => summary.blocked.push(job.issue_id),
                JobState::Failed | JobState::Stale => summary.failed.push(job.issue_id),
                JobState::Cancelled => summary.cancelled.push(job.issue_id),
                JobState::NeedsInput => summary.needs_input.push(job.issue_id),
                JobState::ReconciliationRequired => {
                    summary.reconciliation_required.push(job.issue_id)
                }
                JobState::Leased | JobState::Running
                    if job.lease_until_ms.is_some_and(|until| until <= now_ms()) =>
                {
                    summary.reconciliation_required.push(job.issue_id)
                }
                _ => summary.unstarted.push(job.issue_id),
            }
        }
        Ok(summary)
    }

    /// Run the durable worker loop for one pre-registered detached worker process.
    /// The current process identity must match the identity persisted by the launcher.
    pub fn run_worker(
        &self,
        batch_id: &str,
        worker_id: &str,
        identity: crate::process::ProcessIdentity,
    ) -> Result<WorkerExit, SupervisorError> {
        let batches = BatchStore::open(&self.paths.batches)?;
        // The child can execute immediately after spawn, before its parent commits the
        // returned process identity. Wait briefly for that durable registration.
        let handshake_deadline = std::time::Instant::now() + Duration::from_secs(20);
        let slot = loop {
            let slot = batches.worker_slot(batch_id, worker_id)?.ok_or_else(|| {
                SupervisorError::Conflict("worker has no durable launch registration".into())
            })?;
            if slot.state != "launching" || std::time::Instant::now() >= handshake_deadline {
                break slot;
            }
            thread::sleep(Duration::from_millis(100));
        };
        if slot.state != "running"
            || slot.pid != Some(identity.pid)
            || slot.creation_time_100ns != identity.creation_time_100ns
            || (cfg!(windows) && identity.creation_time_100ns.is_none())
        {
            return Err(SupervisorError::Conflict(
                "worker process identity does not match its registered slot".into(),
            ));
        }
        let request = batches.request(batch_id)?;
        if !request.issue_sources.is_empty() && request.settings_fingerprint.is_none() {
            return Err(SupervisorError::Conflict(
                "worker request has no persisted settings fingerprint; preview and resubmit".into(),
            ));
        }
        let snapshot = batches.get(batch_id)?;
        let jobs = JobsStore::open(&self.paths.jobs)?;
        refresh_batch_sources(&request, &jobs, &snapshot.job_ids)?;
        let _expired = jobs.reconcile_expired_ids(&snapshot.job_ids, now_ms())?;
        let mut waiting_for_input = false;
        let mut last_reconcile = std::time::Instant::now();
        loop {
            let control = batches.control_state(batch_id)?;
            match control.as_str() {
                "paused" => {
                    batches.finish_worker(batch_id, worker_id, now_ms())?;
                    return Ok(WorkerExit::Paused);
                }
                "stopping" | "cancelling" => {
                    if control == "cancelling" {
                        for id in &snapshot.job_ids {
                            let job = jobs.get(*id)?;
                            if matches!(job.state, JobState::Queued | JobState::Blocked)
                                || (job.state == JobState::NeedsInput
                                    && owned_processes_stopped_for_cancel(&jobs, &job, &request)?)
                            {
                                match jobs.request_cancel(*id, now_ms()) {
                                    Ok(_) | Err(JobsError::Conflict(_)) => {}
                                    Err(error) => return Err(error.into()),
                                }
                            }
                        }
                    }
                    let states = snapshot_jobs(&jobs, &snapshot.job_ids)?;
                    let no_active_job = states
                        .iter()
                        .all(|state| !matches!(state, JobState::Leased | JobState::Running));
                    let no_active_verification =
                        !batches.has_active_verifications(batch_id, now_ms())?;
                    if control == "stopping" && no_active_job && no_active_verification {
                        batches.set_terminal(batch_id, "waiting", now_ms())?;
                    } else if control == "cancelling"
                        && no_active_job
                        && no_active_verification
                        && states.iter().all(|state| is_safe_cancel_terminal(*state))
                    {
                        batches.set_cancelled_if_idle(batch_id, now_ms())?;
                    }
                    batches.finish_worker(batch_id, worker_id, now_ms())?;
                    return Ok(if control == "cancelling" {
                        WorkerExit::Cancelled
                    } else {
                        WorkerExit::Stopping
                    });
                }
                "run" | "running" | "starting" => {}
                "completed" => {
                    batches.finish_worker(batch_id, worker_id, now_ms())?;
                    return Ok(WorkerExit::Completed);
                }
                "waiting" => {
                    batches.finish_worker(batch_id, worker_id, now_ms())?;
                    return Ok(WorkerExit::WaitingForInput);
                }
                "failed" => {
                    batches.finish_worker(batch_id, worker_id, now_ms())?;
                    return Ok(WorkerExit::Failed);
                }
                "cancelled" => {
                    batches.finish_worker(batch_id, worker_id, now_ms())?;
                    return Ok(WorkerExit::Cancelled);
                }
                other => {
                    return Err(SupervisorError::Store(format!(
                        "unknown persisted batch control {other:?}"
                    )));
                }
            }
            if now_ms() >= snapshot.deadline_ms {
                batches.control(
                    batch_id,
                    "stopping",
                    "batch wall-clock window expired",
                    now_ms(),
                )?;
                batches.set_terminal(batch_id, "window_expired", now_ms())?;
                batches.finish_worker(batch_id, worker_id, now_ms())?;
                return Ok(WorkerExit::WindowExpired);
            }
            if last_reconcile.elapsed() >= Duration::from_secs(5) {
                let _ = jobs.reconcile_expired_ids(&snapshot.job_ids, now_ms())?;
                refresh_batch_sources(&request, &jobs, &snapshot.job_ids)?;
                last_reconcile = std::time::Instant::now();
            }
            let claim = jobs.claim_ids(worker_id, &snapshot.job_ids, now_ms(), MAX_LEASE_MS)?;
            let Some(claim) = claim else {
                waiting_for_input |= self.verify_ready_jobs(batch_id, worker_id, &request)?;
                let states = snapshot_jobs(&jobs, &snapshot.job_ids)?;
                if states.iter().all(|state| is_terminal_or_waiting(*state)) {
                    let final_state = if states.iter().any(|state| {
                        matches!(
                            state,
                            JobState::NeedsInput
                                | JobState::Blocked
                                | JobState::ReconciliationRequired
                        )
                    }) {
                        "waiting"
                    } else if states.iter().any(|state| {
                        *state == JobState::Failed
                            || *state == JobState::Stale
                            || *state == JobState::ReconciliationRequired
                    }) {
                        "failed"
                    } else {
                        "completed"
                    };
                    let final_state = if now_ms() >= snapshot.deadline_ms {
                        "window_expired"
                    } else {
                        final_state
                    };
                    batches.set_terminal(batch_id, final_state, now_ms())?;
                    batches.finish_worker(batch_id, worker_id, now_ms())?;
                    return Ok(
                        if states.iter().all(|state| *state == JobState::Cancelled) {
                            WorkerExit::Cancelled
                        } else if waiting_for_input || final_state == "waiting" {
                            WorkerExit::WaitingForInput
                        } else if final_state == "failed" {
                            WorkerExit::Failed
                        } else if final_state == "window_expired" {
                            WorkerExit::WindowExpired
                        } else {
                            WorkerExit::Completed
                        },
                    );
                }
                thread::sleep(Duration::from_millis(500));
                continue;
            };
            let result = self.execute_claim(&request, worker_id, claim);
            if let Err(error) = result {
                return Err(error);
            }
        }
    }

    fn execute_claim(
        &self,
        request: &BatchRequest,
        worker_id: &str,
        claim: ClaimedJob,
    ) -> Result<(), SupervisorError> {
        let jobs = JobsStore::open(&self.paths.jobs)?;
        let now = now_ms();
        if request.issue_sources.contains_key(&claim.job.issue_id) {
            if let Err(error) = validate_job_source_binding(
                request,
                &claim.job.issue_id,
                &claim.job.source_fingerprint,
            ) {
                let actual = authoritative_issue_revision(request, &claim.job.issue_id)
                    .unwrap_or_else(|_| format!("{}-missing", claim.job.source_fingerprint));
                let actual = if actual == claim.job.source_fingerprint {
                    format!("{}-snapshot-mismatch", actual)
                } else {
                    actual
                };
                if jobs.mark_stale_before_dispatch(
                    claim.job.id,
                    worker_id,
                    claim.fencing_token,
                    &actual,
                    now,
                )? {
                    return Ok(());
                }
                return Err(SupervisorError::Conflict(format!(
                    "issue source validation failed without a fingerprint change: {error}"
                )));
            }
        }
        jobs.start_attempt(claim.job.id, worker_id, claim.fencing_token, now)?;
        if let Err(error) = validate_settings_fingerprint(request) {
            return self.finish_attempt(
                &claim,
                AttemptOutcome::NeedsInput {
                    summary: format!("provider settings changed after batch admission: {error}"),
                },
            );
        }
        let cancel = Arc::new(AtomicBool::new(false));
        let batch_deadline_ms = BatchStore::open(&self.paths.batches)?
            .get(&request.batch_id)?
            .deadline_ms;
        let heartbeat = LeaseHeartbeat::start(
            self.paths.clone(),
            request.batch_id.clone(),
            claim.job.id,
            worker_id.to_owned(),
            claim.fencing_token,
            batch_deadline_ms,
            Arc::clone(&cancel),
        );
        let settings =
            settings::load_settings(&request.settings_path).map_err(SupervisorError::Invalid)?;
        if let Err(error) = validate_settings_fingerprint(request) {
            return self.finish_attempt(
                &claim,
                AttemptOutcome::NeedsInput {
                    summary: format!("provider settings changed while preparing the run: {error}"),
                },
            );
        }
        let isolation = RepoIsolation::new(
            &request.project_path,
            &request.managed_worktree_root,
            &self.paths.root,
        )
        .map_err(|e| SupervisorError::Runtime(format!("workspace_isolation: {e}")))?;
        let inspection = isolation
            .inspect()
            .map_err(|e| SupervisorError::Runtime(format!("workspace_inspection: {e}")))?;
        if !inspection
            .verified_head
            .eq_ignore_ascii_case(&request.verified_base_commit)
        {
            return self.finish_attempt(
                &claim,
                AttemptOutcome::NeedsInput {
                    summary: "repository HEAD changed after the batch was reviewed; select a new verified base before resuming".into(),
                },
            );
        }
        let batch_snapshot = BatchStore::open(&self.paths.batches)?.get(&request.batch_id)?;
        let predecessors = match verified_predecessors(
            &jobs,
            request,
            &self.paths.root,
            &batch_snapshot.job_ids,
            &claim.job,
        ) {
            Ok(value) => value,
            Err(SupervisorError::Conflict(message)) => {
                return self
                    .finish_attempt(&claim, AttemptOutcome::NeedsInput { summary: message });
            }
            Err(error) => return Err(error),
        };
        let workspace_remaining_ms = batch_deadline_ms.saturating_sub(now_ms());
        if workspace_remaining_ms < 1_000 {
            return self.finish_attempt(
                &claim,
                AttemptOutcome::NeedsInput {
                    summary: "absolute batch deadline expired before workspace preparation".into(),
                },
            );
        }
        let workspace = match isolation.prepare_workspace_with_deadline(
            PrepareRequest {
                issue_id: claim.job.issue_id.clone(),
                issue_snapshot_fingerprint: claim.job.source_fingerprint.clone(),
                verified_base_commit: inspection.verified_head,
                predecessor_commits: predecessors,
                build_root: request.build_root.clone(),
            },
            std::time::Instant::now() + Duration::from_millis(workspace_remaining_ms as u64),
            &cancel,
        ) {
            Ok(workspace) => workspace,
            Err(_error) if cancel.load(Ordering::SeqCst) => {
                return self.finish_attempt(
                    &claim,
                    AttemptOutcome::NeedsInput {
                        summary: "batch was cancelled while preparing the workspace; reconcile effects before retry".into(),
                    },
                )
            }
            Err(error) => {
                let error_text = error.to_string().chars().take(512).collect::<String>();
                let guidance = "inspect the existing workspace ownership record and source state before retrying";
                jobs.record_checkpoint(
                    claim.job.id,
                    worker_id,
                    claim.fencing_token,
                    "workspace_preparation_failure",
                    &json!({
                        "statusKey": "workspace_preparation_failure",
                        "stage": "prepare_workspace",
                        "error": error_text,
                        "guidance": guidance,
                    }),
                    now_ms(),
                )?;
                return self.finish_attempt(
                    &claim,
                    AttemptOutcome::NeedsInput {
                        summary: format!(
                            "workspace preparation failed: {error_text}; {guidance}"
                        ),
                    },
                );
            }
        };
        let implementation_tasks = claim
            .plan
            .tasks
            .iter()
            .filter(|task| matches!(task.kind, TaskKind::Implementation | TaskKind::Worker))
            .cloned()
            .collect::<Vec<_>>();
        if implementation_tasks.is_empty() {
            return self.finish_attempt(
                &claim,
                AttemptOutcome::NeedsInput {
                    summary: "job has no executable implementation task".into(),
                },
            );
        }
        let mut outputs = Vec::new();
        for task in &implementation_tasks {
            let remaining_ms = batch_deadline_ms.saturating_sub(now_ms());
            if remaining_ms < 1000 {
                return self.finish_attempt(&claim, AttemptOutcome::NeedsInput {
                    summary: "absolute batch deadline expired before the next implementation dispatch".into(),
                });
            }
            if let Err(error) = validate_settings_fingerprint(request) {
                return self.finish_attempt(
                    &claim,
                    AttemptOutcome::NeedsInput {
                        summary: format!("provider settings changed before dispatch: {error}"),
                    },
                );
            }
            if request.issue_sources.contains_key(&claim.job.issue_id) {
                match validate_job_source_binding(request,&claim.job.issue_id,&claim.job.source_fingerprint) {
                    Ok(())=>{},
                    Err(error)=>return self.finish_attempt(&claim,AttemptOutcome::NeedsInput{summary:format!("authoritative issue source changed before dispatch; reconcile current effects before retry: {error}")}),
                }
            }
            if jobs.cancellation_requested(
                claim.job.id,
                worker_id,
                claim.fencing_token,
                now_ms(),
            )? {
                return self.finish_attempt(&claim, AttemptOutcome::NeedsInput {
                    summary: "job cancellation was requested; process ownership and workspace effects require reconciliation".into(),
                });
            }
            let Some(submit_job) = request
                .jobs
                .iter()
                .find(|job| job.issue_id == claim.job.issue_id)
            else {
                return self.finish_attempt(
                    &claim,
                    AttemptOutcome::NeedsInput {
                        summary: "reviewed routing task is missing from the admitted request"
                            .into(),
                    },
                );
            };
            let route_key = paid_policy_key(&claim.job.issue_id, &task.task_id);
            let Some(committed_route) = request.resolved_routes.get(&route_key) else {
                return self.finish_attempt(&claim, AttemptOutcome::NeedsInput {
                    summary: "provider route lacks a reviewed preview binding; preview again before dispatch".into(),
                });
            };
            let Some(commitment) = request.route_commitments.get(&route_key) else {
                return self.finish_attempt(&claim, AttemptOutcome::NeedsInput {
                    summary: "provider route lacks an immutable semantic commitment; preview again before dispatch".into(),
                });
            };
            if committed_route.provider_id != commitment.provider_id
                || committed_route.model_id != commitment.model_id
            {
                return self.finish_attempt(
                    &claim,
                    AttemptOutcome::NeedsInput {
                        summary: "provider route differs from its immutable semantic commitment"
                            .into(),
                    },
                );
            }
            let selected_profile = settings
                .providers
                .iter()
                .find(|profile| profile.id == committed_route.provider_id);
            let Some(selected_profile) = selected_profile else {
                return self.finish_attempt(
                    &claim,
                    AttemptOutcome::NeedsInput {
                        summary: "selected provider profile was removed since preview".into(),
                    },
                );
            };
            let kind_consented = match &request.dispatch_refresh_consent {
                crate::provider_observations::ModelFreeRefreshConsent::Disabled => false,
                crate::provider_observations::ModelFreeRefreshConsent::Providers { kinds } => {
                    kinds.contains(&selected_profile.kind)
                }
            };
            let dispatch_deadline =
                Instant::now() + Duration::from_millis(remaining_ms.max(0) as u64);
            let (dispatch_facts, refresh_audit) = if kind_consented {
                match crate::provider_observations::fresh_candidate_facts(
                    &settings,
                    &committed_route.provider_id,
                    &committed_route.model_id,
                    dispatch_deadline,
                    &request.dispatch_refresh_consent,
                ) {
                    Ok(mut fresh) => {
                        if commitment.economy_endpoint_id.is_some() {
                            let requirements = task.route_requirements.clone().unwrap_or_default();
                            let remaining =
                                dispatch_deadline.saturating_duration_since(Instant::now());
                            let admission = crate::economy::refresh_profile_admission(
                                selected_profile,
                                &committed_route.model_id,
                                requirements
                                    .max_input_tokens
                                    .or(requirements.context_tokens),
                                requirements.max_output_tokens,
                                requirements.require_tools.unwrap_or(true),
                                task.limits.max_tokens.unwrap_or(0),
                                task.limits.max_seconds.unwrap_or(0),
                                remaining.min(Duration::from_secs(30)),
                                now_ms().max(0) as u64,
                            );
                            match admission {
                                Ok(admission) => fresh.facts.economy_admission = Some(admission),
                                Err(reason) => {
                                    return self.finish_attempt(&claim, AttemptOutcome::NeedsInput {
                                        summary: format!("fresh Economy admission failed before reservation: {reason}"),
                                    });
                                }
                            }
                        }
                        if cancel.load(Ordering::SeqCst) || now_ms() >= batch_deadline_ms {
                            let reason = "batch cancellation or absolute deadline arrived during provider refresh";
                            let _ = jobs.record_checkpoint(claim.job.id, worker_id, claim.fencing_token,
                                "dispatch_route_refresh", &json!({"state":"needs_input","reason":reason,"snapshotFingerprint":fresh.snapshot_fingerprint,"settingsFingerprint":fresh.settings_fingerprint,"runtimeIdentityFingerprint":fresh.runtime_identity_fingerprint,"observedAtMs":fresh.observed_at_ms}), now_ms());
                            return self.finish_attempt(
                                &claim,
                                AttemptOutcome::NeedsInput {
                                    summary: format!("{reason}; no provider call was started"),
                                },
                            );
                        }
                        if let Err(reason) = check_fresh_route_commitment(
                            request, &settings, submit_job, task, &fresh,
                        ) {
                            let _ = jobs.record_checkpoint(claim.job.id, worker_id, claim.fencing_token,
                                "dispatch_route_refresh", &json!({"state":"needs_input","reason":reason,"snapshotFingerprint":fresh.snapshot_fingerprint,"settingsFingerprint":fresh.settings_fingerprint,"runtimeIdentityFingerprint":fresh.runtime_identity_fingerprint,"observedAtMs":fresh.observed_at_ms}), now_ms());
                            return self.finish_attempt(&claim, AttemptOutcome::NeedsInput {
                                summary: format!("dispatch-time provider evidence no longer matches the reviewed route; no reservation or provider call was started: {reason}"),
                            });
                        }
                        let audit = json!({"state":"matched","snapshotFingerprint":fresh.snapshot_fingerprint,"settingsFingerprint":fresh.settings_fingerprint,"runtimeIdentityFingerprint":fresh.runtime_identity_fingerprint,"observedAtMs":fresh.observed_at_ms});
                        (vec![fresh.facts], Some(audit))
                    }
                    Err(reason) => {
                        let audit = json!({"state":"needs_input","reason":reason.to_string()});
                        let _ = jobs.record_checkpoint(
                            claim.job.id,
                            worker_id,
                            claim.fencing_token,
                            "dispatch_route_refresh",
                            &audit,
                            now_ms(),
                        );
                        return self.finish_attempt(&claim, AttemptOutcome::NeedsInput {
                            summary: format!("dispatch-time model-free provider refresh could not verify the reviewed route; no reservation or provider call was started: {reason}"),
                        });
                    }
                }
            } else {
                let Some(reviewed_observation_fingerprint) =
                    request.provider_observation_fingerprint.as_deref()
                else {
                    return self.finish_attempt(&claim, AttemptOutcome::NeedsInput {
                        summary: "provider routing evidence was not bound to an explicit preview; refresh and review routes before retry".into(),
                    });
                };
                let (current_facts, current_observation_fingerprint, observation_problem) =
                    load_provider_observations(
                        &settings,
                        &self.paths.root,
                        now_ms().max(0) as u128,
                    )?;
                if observation_problem.is_some()
                    || reviewed_observation_fingerprint != current_observation_fingerprint
                {
                    return self.finish_attempt(&claim, AttemptOutcome::NeedsInput {
                        summary: format!("provider routing evidence changed or became unavailable before dispatch; refresh and review routes before retry: {}", observation_problem.unwrap_or_else(|| "observation snapshot fingerprint changed".into())),
                    });
                }
                (current_facts, None)
            };
            let refreshed_pricing_time = refresh_audit.as_ref().and_then(|_| {
                dispatch_facts
                    .iter()
                    .find(|fact| {
                        fact.provider_id == committed_route.provider_id
                            && fact.model_id == committed_route.model_id
                    })
                    .and_then(|fact| fact.pricing_verified_at_ms)
                    .and_then(|value| u64::try_from(value).ok())
            });
            let refreshed_endpoint_time = refresh_audit.as_ref().and_then(|_| {
                dispatch_facts
                    .iter()
                    .find(|fact| {
                        fact.provider_id == committed_route.provider_id
                            && fact.model_id == committed_route.model_id
                    })
                    .and_then(|fact| fact.paid_endpoint_quote.as_ref())
                    .and_then(|quote| u64::try_from(quote.observed_at_ms).ok())
            });
            let routed = route_task(
                request,
                &settings,
                submit_job,
                task,
                &dispatch_facts,
                refreshed_pricing_time,
            )?;
            if !routed.eligible {
                return self.finish_attempt(
                    &claim,
                    AttemptOutcome::NeedsInput {
                        summary: format!(
                            "provider route is no longer eligible before dispatch: {}",
                            routed.reasons.join("; ")
                        ),
                    },
                );
            }
            let (Some(provider_id), Some(model_id)) = (routed.provider_id, routed.model_id) else {
                return self.finish_attempt(
                    &claim,
                    AttemptOutcome::NeedsInput {
                        summary: "reviewed provider route is incomplete before dispatch".into(),
                    },
                );
            };
            let profile = settings
                .providers
                .iter()
                .find(|profile| profile.id == provider_id)
                .ok_or_else(|| {
                    SupervisorError::Invalid(format!(
                        "selected provider {provider_id} is missing from current settings"
                    ))
                })?;
            let economy_admission = routed.economy_admission.clone();
            let reservation_id = reservation_id(
                &request.batch_id,
                claim.job.id,
                &task.task_id,
                claim.attempt_number,
            );
            let budgets = BudgetStore::open(&self.paths.budgets)?;
            let committed_paid_policy = request
                .paid_policies
                .get(&paid_policy_key(&claim.job.issue_id, &task.task_id))
                .filter(|policy| policy.provider_id == profile.id && policy.model_id == model_id);
            let economy_policy = economy_admission
                .as_ref()
                .map(|admission| admission.gateway_policy());
            if profile.kind == ProviderKind::OpenRouter
                && committed_paid_policy.is_none()
                && economy_policy.is_none()
            {
                return self.finish_attempt(&claim, AttemptOutcome::NeedsInput {
                    summary: "OpenRouter task has no current backend-admitted Paid or Economy policy.".into(),
                });
            }
            if committed_paid_policy.is_some() && economy_policy.is_some() {
                return self.finish_attempt(
                    &claim,
                    AttemptOutcome::NeedsInput {
                        summary: "Task cannot carry both Paid and Economy dispatch policies."
                            .into(),
                    },
                );
            }
            let execution_paid_policy = if let Some(policy) = committed_paid_policy
                .filter(|policy| matches!(policy.mode, crate::paid_gateway::GatewayMode::Paid))
            {
                let refreshed_at = if refresh_audit.is_some() {
                    refreshed_pricing_time
                } else {
                    None
                };
                Some(
                    execution_paid_policy(policy, refreshed_at, refreshed_endpoint_time).map_err(|reason| {
                        SupervisorError::Verification(format!("needs_input: {reason}"))
                    })?,
                )
            } else if let Some(policy) = economy_policy {
                Some(policy)
            } else {
                None
            };
            let paid_policy = execution_paid_policy.as_ref();
            let max_tokens = if economy_admission.is_some() {
                economy_admission
                    .as_ref()
                    .map(|admission| admission.max_tokens())
                    .ok_or_else(|| {
                        SupervisorError::Invalid("Economy token reservation is missing".into())
                    })?
            } else {
                match paid_policy {
                    Some(policy) => paid_call_token_ceiling(policy).ok_or_else(|| {
                        SupervisorError::Invalid("paid token reservation overflowed".into())
                    })?,
                    None => task.limits.max_tokens.ok_or_else(|| {
                        SupervisorError::Invalid("task has no token budget".into())
                    })?,
                }
            };
            let max_cost = if economy_admission.is_some() {
                0
            } else {
                paid_policy.map_or(task.limits.max_cost_micros.unwrap_or(0), |policy| {
                    policy.max_cost_micros
                })
            };
            let max_seconds = task
                .limits
                .max_seconds
                .ok_or_else(|| SupervisorError::Invalid("task has no time budget".into()))?;
            let max_seconds = economy_admission.as_ref().map_or(max_seconds, |admission| {
                max_seconds.min(admission.max_seconds())
            });
            let is_paid = paid_policy.is_some_and(|policy| {
                matches!(policy.mode, crate::paid_gateway::GatewayMode::Paid)
            });
            let reservation = ReservationRequest {
                reservation_id: reservation_id.clone(),
                batch_id: request.batch_id.clone(),
                job_id: claim.job.id.to_string(),
                task_id: task.task_id.clone(),
                provider_id: profile.id.clone(),
                model_id: model_id.clone(),
                runtime_id: paid_policy.map_or_else(
                    || profile.kind.telemetry_id().to_owned(),
                    |policy| policy.runtime_id.clone(),
                ),
                max_tokens,
                max_cost_micros: Some(max_cost),
                max_seconds: max_seconds
                    .min((batch_deadline_ms.saturating_sub(now_ms()) / 1000).max(1) as u64),
                is_paid,
                pricing_source: paid_policy.map(|policy| policy.pricing.source.clone()),
                pricing_verified_at_ms: paid_policy.map(|policy| policy.pricing.observed_at_ms),
            };
            if let Some(audit) = refresh_audit {
                jobs.record_checkpoint(
                    claim.job.id,
                    worker_id,
                    claim.fencing_token,
                    "dispatch_route_refresh",
                    &audit,
                    now_ms(),
                )?;
            }
            let reserve_deadline_ms = batch_deadline_ms;
            let reserve_cancel = Arc::clone(&cancel);
            let reserve_batch_id = request.batch_id.clone();
            let reserve_paths = self.paths.clone();
            let reserve_result =
                reserve_with_capacity_wait(&budgets, &reservation, reserve_deadline_ms, || {
                    if reserve_cancel.load(Ordering::SeqCst) {
                        return Err(SupervisorError::Verification(
                            "needs_input: cancellation or lease loss before provider dispatch"
                                .into(),
                        ));
                    }
                    let batches = BatchStore::open(&reserve_paths.batches)?;
                    if matches!(
                        batches.control_state(&reserve_batch_id)?.as_str(),
                        "cancelling" | "cancelled"
                    ) {
                        return Err(SupervisorError::Verification(
                            "needs_input: batch cancellation requested before provider dispatch"
                                .into(),
                        ));
                    }
                    Ok(())
                });
            match reserve_result {
                Ok(_) => {}
                Err(SupervisorError::Verification(reason))
                    if reason.starts_with("needs_input:") =>
                {
                    return self
                        .finish_attempt(&claim, AttemptOutcome::NeedsInput { summary: reason });
                }
                Err(error) => return Err(error),
            }
            jobs.record_checkpoint(claim.job.id, worker_id, claim.fencing_token,
                &format!("dispatch_{}", safe_checkpoint_key(&task.task_id)),
                &json!({"state":"intent_persisted","reservationId":reservation_id,"providerId":profile.id,"modelId":model_id,"taskId":task.task_id}), now_ms())?;
            jobs.record_checkpoint(
                claim.job.id,
                worker_id,
                claim.fencing_token,
                &format!("runtime_process_{}", safe_checkpoint_key(&task.task_id)),
                &json!({"state":"pending"}),
                now_ms(),
            )?;
            let issue_snapshot = match authoritative_issue_snapshot(&request.jobs, &claim.job) {
                Ok(snapshot) => snapshot,
                Err(_) => {
                    budgets.release_before_dispatch(&reservation_id)?;
                    return self.finish_attempt(
                        &claim,
                        AttemptOutcome::NeedsInput {
                            summary: "The admitted issue snapshot no longer matches this job; refresh and review the batch.".into(),
                        },
                    );
                }
            };
            let prompt = implementation_prompt(
                &claim.job.issue_id,
                &claim.job.source_fingerprint,
                issue_snapshot,
                &task,
                &claim.plan,
            );
            let runtime_remaining_ms = batch_deadline_ms.saturating_sub(now_ms());
            if runtime_remaining_ms < 1000 {
                budgets.release_before_dispatch(&reservation_id)?;
                return self.finish_attempt(
                    &claim,
                    AttemptOutcome::NeedsInput {
                        summary: "absolute batch deadline expired before provider process start"
                            .into(),
                    },
                );
            }
            let timeout = max_seconds.min((runtime_remaining_ms / 1000).max(1) as u64);
            let mut sink = JobRuntimeSink {
                jobs: &jobs,
                budgets: &budgets,
                job_id: claim.job.id,
                worker_id,
                fencing_token: claim.fencing_token,
                reservation_id: &reservation_id,
                task_id: &task.task_id,
                issue_fingerprint: &claim.job.source_fingerprint,
                dispatched: false,
                output: String::new(),
                failed: None,
            };
            let reasoning_effort = implementation_reasoning_effort(&settings, profile, &model_id);
            let runtime_request = RuntimeRequest {
                provider_id: Some(profile.id.clone()),
                model_id: Some(model_id),
                cwd: workspace.record.worktree_path.clone(),
                prompt,
                timeout_seconds: timeout,
                reasoning_effort,
                read_only: false,
                test_command: None,
                allow_paid: paid_policy.is_some(),
            };
            let outcome = if let Some(policy) = paid_policy {
                runtime::run_budgeted_in_build_output(
                    &settings,
                    &runtime_request,
                    &cancel,
                    &mut sink,
                    &runtime::PaidDispatchContext {
                        budgets: &budgets,
                        parent_reservation_id: &reservation_id,
                        policy,
                        allowed_files: &task.files,
                        authenticated_canonical_slug: economy_admission
                            .as_ref()
                            .map(|admission| admission.canonical_slug()),
                        economy_trial_task_id: None,
                        synthetic_trial_consent: false,
                        synthetic_trial_fixture: None,
                        economy_admission: economy_admission.as_ref(),
                        cancel: Arc::clone(&cancel),
                    },
                    &workspace.record.build_output_path,
                )
            } else {
                runtime::run_in_build_output(
                    &settings,
                    &runtime_request,
                    &cancel,
                    &mut sink,
                    &workspace.record.build_output_path,
                )
            };
            let elapsed = match &outcome {
                Ok(o) => o.elapsed_ms / 1000,
                Err(_) => 0,
            };
            let usage = outcome.as_ref().ok().map(|o| o.usage.clone());
            if is_paid {
                settle_paid_parent_usage(&budgets, &reservation_id, sink.dispatched, elapsed)?;
            } else {
                settle_runtime_usage(
                    &budgets,
                    &reservation_id,
                    sink.dispatched,
                    usage.as_ref(),
                    economy_admission.as_ref(),
                    elapsed,
                )?;
            }
            if let Some(failure) = sink.failed.take() {
                return self
                    .finish_attempt(&claim, AttemptOutcome::NeedsInput { summary: failure });
            }
            let outcome = match outcome {
                Ok(outcome) => outcome,
                Err(error) => {
                    return self.finish_attempt(
                        &claim,
                        AttemptOutcome::Failed {
                            summary: error.message,
                        },
                    );
                }
            };
            outputs.push(sink.output);
            match outcome.state {
                RuntimeState::Completed => {}
                RuntimeState::NeedsInput | RuntimeState::AuthenticationRequired | RuntimeState::RateLimited => {
                    return self.finish_attempt(&claim, AttemptOutcome::NeedsInput { summary: outcome.message });
                }
                RuntimeState::Cancelled => return self.finish_attempt(&claim, AttemptOutcome::NeedsInput { summary: "provider process was cancelled; reconcile the exact owned process and workspace before retry".into() }),
                state => return self.finish_attempt(&claim, AttemptOutcome::Failed { summary: format!("runtime ended in {state:?}: {}", outcome.message) }),
            }
        }
        let allowed = implementation_tasks
            .iter()
            .flat_map(|task| task.files.iter().cloned())
            .collect::<Vec<_>>();
        if !self.capture_post_provider_changes(
            &claim,
            worker_id,
            &isolation,
            &workspace.record.worktree_path,
            &allowed,
            &outputs.join("\n"),
            batch_deadline_ms,
            &cancel,
        )? {
            drop(heartbeat);
            return Ok(());
        }
        drop(heartbeat);
        self.finish_attempt(
            &claim,
            AttemptOutcome::ProposedComplete {
                summary: "implementation tasks completed; independent verification is required"
                    .into(),
            },
        )
    }

    /// Capture exact provider changes after checking their declared task scope.
    /// False means a durable NeedsInput was recorded and verification must not start.
    fn capture_post_provider_changes(
        &self,
        claim: &ClaimedJob,
        worker_id: &str,
        isolation: &RepoIsolation,
        worktree_path: &Path,
        allowed: &[String],
        summary: &str,
        batch_deadline_ms: i64,
        cancelled: &AtomicBool,
    ) -> Result<bool, SupervisorError> {
        let capture_started = Instant::now();
        let remaining_ms = batch_deadline_ms.saturating_sub(now_ms()).max(0) as u64;
        let metadata_lock_deadline = capture_started + Duration::from_millis(remaining_ms);
        let snapshot = match source_snapshot(worktree_path) {
            Ok(snapshot) => snapshot,
            Err(error) => {
                self.finish_post_provider_capture_failure(
                    claim,
                    worker_id,
                    "source_snapshot",
                    &error.to_string(),
                )?;
                return Ok(false);
            }
        };
        let observed = match isolation.observed_changed_paths_until(
            &claim.job.issue_id,
            metadata_lock_deadline,
            cancelled,
        ) {
            Ok(paths) => paths,
            Err(error) => {
                self.finish_post_provider_capture_failure(
                    claim,
                    worker_id,
                    "observed_changed_paths",
                    &error.to_string(),
                )?;
                return Ok(false);
            }
        };
        let after_wait = match source_snapshot(worktree_path) {
            Ok(snapshot) => snapshot,
            Err(error) => {
                self.finish_post_provider_capture_failure(
                    claim,
                    worker_id,
                    "source_snapshot_after_metadata_wait",
                    &error.to_string(),
                )?;
                return Ok(false);
            }
        };
        if after_wait.fingerprint != snapshot.fingerprint {
            self.finish_post_provider_capture_failure(
                claim,
                worker_id,
                "source_changed_during_metadata_wait",
                "workspace content changed while waiting for repository metadata; inspect and reconcile before retrying",
            )?;
            return Ok(false);
        }
        if let Err(error) = validate_observed_change_scope(&observed, allowed) {
            self.finish_post_provider_capture_failure(
                claim,
                worker_id,
                "allowed_scope_validation",
                &error,
            )?;
            return Ok(false);
        }
        if let Err(error) = isolation.record_owned_changes_until(
            &claim.job.issue_id,
            &after_wait.fingerprint,
            &observed,
            metadata_lock_deadline,
            cancelled,
        ) {
            self.finish_post_provider_capture_failure(
                claim,
                worker_id,
                "record_owned_changes",
                &error.to_string(),
            )?;
            return Ok(false);
        }
        let after_record = match source_snapshot(worktree_path) {
            Ok(snapshot) => snapshot,
            Err(error) => {
                self.finish_post_provider_capture_failure(
                    claim,
                    worker_id,
                    "source_snapshot_after_record",
                    &error.to_string(),
                )?;
                return Ok(false);
            }
        };
        if after_record.fingerprint != after_wait.fingerprint {
            self.finish_post_provider_capture_failure(
                claim,
                worker_id,
                "source_changed_during_recording",
                "workspace content changed while recording ownership; inspect and reconcile before retrying",
            )?;
            return Ok(false);
        }
        JobsStore::open(&self.paths.jobs)?.record_checkpoint(
            claim.job.id,
            worker_id,
            claim.fencing_token,
            "implementation_proposed",
            &json!({"source":after_record,"summary":summary}),
            now_ms(),
        )?;
        Ok(true)
    }
    fn finish_post_provider_capture_failure(
        &self,
        claim: &ClaimedJob,
        worker_id: &str,
        stage: &str,
        error: &str,
    ) -> Result<(), SupervisorError> {
        let error = error.chars().take(512).collect::<String>();
        let guidance = "inspect and reconcile the owned workspace before retrying";
        let jobs = JobsStore::open(&self.paths.jobs)?;
        jobs.record_checkpoint(
            claim.job.id,
            worker_id,
            claim.fencing_token,
            "post_provider_capture_failure",
            &json!({
                "statusKey": "post_provider_capture_failure",
                "state": "needs_input",
                "stage": stage,
                "error": error,
                "guidance": guidance,
            }),
            now_ms(),
        )?;
        self.finish_attempt(
            claim,
            AttemptOutcome::NeedsInput {
                summary: format!(
                    "post-provider workspace capture failed at {stage}: {error}; {guidance}"
                ),
            },
        )
    }

    fn finish_attempt(
        &self,
        claim: &ClaimedJob,
        outcome: AttemptOutcome,
    ) -> Result<(), SupervisorError> {
        let jobs = JobsStore::open(&self.paths.jobs)?;
        let proposed = matches!(outcome, AttemptOutcome::ProposedComplete { .. });
        let job = jobs.record_attempt_outcome(
            claim.job.id,
            &claim.worker_id,
            claim.fencing_token,
            outcome,
            now_ms(),
        )?;
        if !proposed
            && matches!(
                job.state,
                JobState::NeedsInput | JobState::ReconciliationRequired
            )
        {
            let round = repair_round(&jobs, claim.job.id);
            if round > 0 {
                let name = format!("verification_repair_round_{round}");
                if let Some(value) = jobs.get_checkpoint(claim.job.id, &name)? {
                    let prior: RepairProgress = serde_json::from_value(value)
                        .map_err(|e| SupervisorError::Store(e.to_string()))?;
                    if prior.state == RepairState::Started {
                        let progress=RepairProgress{round,state:RepairState::Interrupted,source_fingerprint:prior.source_fingerprint,result_source_fingerprint:None,summary:"repair attempt stopped; owned effects require reconciliation before retry".into()};
                        VerificationService::<ProcessCheckExecutor,runtime::RuntimeReviewExecutor>::save_repair_progress(&jobs,claim.job.id,&claim.job.source_fingerprint,&progress,now_ms()).map_err(|e|SupervisorError::Verification(e.to_string()))?;
                    }
                }
            }
        }
        if proposed && job.state == JobState::AwaitingVerification {
            let source = jobs
                .get_checkpoint(claim.job.id, "implementation_proposed")?
                .and_then(|value| {
                    value
                        .pointer("/source/fingerprint")
                        .and_then(Value::as_str)
                        .map(str::to_owned)
                });
            if let Some(result_source) = source {
                for round in 1..=2 {
                    let name = format!("verification_repair_round_{round}");
                    let Some(value) = jobs.get_checkpoint(claim.job.id, &name)? else {
                        continue;
                    };
                    let progress: RepairProgress = serde_json::from_value(value)
                        .map_err(|e| SupervisorError::Store(e.to_string()))?;
                    if progress.state == RepairState::Started {
                        VerificationService::<ProcessCheckExecutor,runtime::RuntimeReviewExecutor>::save_repair_progress(
                            &jobs,claim.job.id,&claim.job.source_fingerprint,&RepairProgress{round,state:RepairState::Completed,source_fingerprint:progress.source_fingerprint,result_source_fingerprint:Some(result_source),summary:"bounded repair implementation completed".into()},now_ms()
                        ).map_err(|e|SupervisorError::Verification(e.to_string()))?;
                        break;
                    }
                }
            }
        }
        Ok(())
    }

    fn verify_ready_jobs(
        &self,
        batch_id: &str,
        worker_id: &str,
        request: &BatchRequest,
    ) -> Result<bool, SupervisorError> {
        let batches = BatchStore::open(&self.paths.batches)?;
        let snapshot = batches.get(batch_id)?;
        let deadline_ms = snapshot.deadline_ms;
        let jobs = JobsStore::open(&self.paths.jobs)?;
        let settings =
            settings::load_settings(&request.settings_path).map_err(SupervisorError::Invalid)?;
        let mut needs_input = false;
        for id in snapshot.job_ids {
            let job = jobs.get(id)?;
            if job.state != JobState::AwaitingVerification {
                continue;
            }
            let policy = request
                .verification
                .iter()
                .find(|entry| entry.issue_id == job.issue_id)
                .ok_or_else(|| SupervisorError::NotFound(job.issue_id.clone()))?;
            if deadline_ms.saturating_sub(now_ms()) < 1000 {
                needs_input = true;
                continue;
            }
            if !batches.acquire_verification(batch_id, id, worker_id, now_ms(), MAX_LEASE_MS)? {
                continue;
            }
            let lease = VerificationLeaseHeartbeat::start(
                self.paths.clone(),
                batch_id.to_owned(),
                id,
                worker_id.to_owned(),
                deadline_ms,
            );
            let lease_lost = Arc::clone(&lease.lost);
            let interrupted =
                verification_has_interrupted_checkpoint(&jobs, &job, &request.verification)?;
            let mut result = if interrupted {
                persist_verification_needs_input(
                    &jobs,
                    &self.paths.root,
                    request,
                    &job,
                    policy,
                    "verification was interrupted after a durable check/review start; inspect the recorded process and rerun explicitly",
                    now_ms(),
                )?;
                needs_input = true;
                Err(SupervisorError::Verification(
                    "needs_input: verification was interrupted after a durable check/review start"
                        .into(),
                ))
            } else {
                self.verify_one(
                    request,
                    worker_id,
                    &settings,
                    &jobs,
                    &job,
                    batch_id,
                    deadline_ms,
                    lease_lost,
                )
            };
            if lease.lost.load(Ordering::SeqCst) {
                result = Err(SupervisorError::Verification(
                    "needs_input: verification lease ownership was lost".into(),
                ));
                needs_input = true;
            }
            drop(lease);
            batches.release_verification(batch_id, id, worker_id)?;
            match result {
                Ok(VerificationOutcome::Verified { .. }) => {}
                Ok(VerificationOutcome::NeedsManualAcceptance { .. }) => needs_input = true,
                Ok(VerificationOutcome::NeedsInput { reason }) => {
                    persist_verification_needs_input(
                        &jobs,
                        &self.paths.root,
                        request,
                        &job,
                        policy,
                        &reason,
                        now_ms(),
                    )?;
                    needs_input = true;
                }
                Err(error) => {
                    if error
                        .to_string()
                        .contains("verification lease ownership was lost")
                        || error
                            .to_string()
                            .contains("verification lease is no longer owned")
                    {
                        needs_input = true;
                        continue;
                    }
                    if error.to_string().contains("StaleIssueSource")
                        || error
                            .to_string()
                            .contains("authoritative issue Markdown changed during verification")
                    {
                        let current = authoritative_issue_revision(request, &job.issue_id)
                            .unwrap_or_else(|_| format!("{}-missing", job.source_fingerprint));
                        let current = if current == job.source_fingerprint {
                            format!("{}-changed", current)
                        } else {
                            current
                        };
                        let _ = jobs.check_source_fingerprint(id, &current, now_ms())?;
                        continue;
                    }
                    let issue = request
                        .jobs
                        .iter()
                        .find(|entry| entry.issue_id == job.issue_id)
                        .ok_or_else(|| SupervisorError::NotFound(job.issue_id.clone()))?;
                    let review_input =
                        jobs.get_checkpoint(id, "verification_review")?
                            .is_some_and(|v| {
                                v.get("state").and_then(Value::as_str) == Some("needs_input")
                            });
                    if verification_error_requires_input(&error)
                        || error
                            .to_string()
                            .contains("verification gate ownership check failed")
                        || error.to_string().contains("GateGuardFailed")
                        || review_input
                    {
                        persist_verification_needs_input(
                            &jobs,
                            &self.paths.root,
                            request,
                            &job,
                            policy,
                            &error.to_string(),
                            now_ms(),
                        )?;
                        needs_input = true;
                        continue;
                    }
                    let isolation = RepoIsolation::new(
                        &request.project_path,
                        &request.managed_worktree_root,
                        &self.paths.root,
                    )
                    .map_err(|e| SupervisorError::Runtime(e.to_string()))?;
                    let record = isolation.recover_workspace(&job.issue_id).ok();
                    let code = record
                        .as_ref()
                        .and_then(|r| source_snapshot(&r.worktree_path).ok());
                    let Some(code) = code else {
                        needs_input = true;
                        continue;
                    };
                    let round = repair_round(&jobs, id);
                    if deadline_ms.saturating_sub(now_ms()) < 1000 {
                        needs_input = true;
                        continue;
                    }
                    if round < 2 && jobs.get(id)?.attempt_count < jobs.get(id)?.max_attempts.into()
                    {
                        let next = round + 1;
                        let progress = RepairProgress {
                            round: next as u8,
                            state: RepairState::Started,
                            source_fingerprint: code.fingerprint.clone(),
                            result_source_fingerprint: None,
                            summary: format!(
                                "verification failed; bounded repair round {next} queued: {error}"
                            ),
                        };
                        VerificationService::<ProcessCheckExecutor, runtime::RuntimeReviewExecutor>::save_repair_progress(&jobs,id,&job.source_fingerprint,&progress,now_ms())
                            .map_err(|e| SupervisorError::Verification(e.to_string()))?;
                        jobs.requeue_for_verification_repair(
                            id,
                            &job.source_fingerprint,
                            next as u8,
                            &progress.summary,
                            now_ms(),
                        )?;
                    } else {
                        if round > 0 {
                            let name = format!("verification_repair_round_{round}");
                            if let Some(value) = jobs.get_checkpoint(id, &name)? {
                                let prior: RepairProgress = serde_json::from_value(value)
                                    .map_err(|e| SupervisorError::Store(e.to_string()))?;
                                let exhausted = RepairProgress {
                                    round: round as u8,
                                    state: RepairState::Exhausted,
                                    source_fingerprint: prior.source_fingerprint,
                                    result_source_fingerprint: Some(code.fingerprint.clone()),
                                    summary: format!(
                                        "bounded verification repair budget exhausted: {error}"
                                    ),
                                };
                                VerificationService::<
                                    ProcessCheckExecutor,
                                    runtime::RuntimeReviewExecutor,
                                >::save_repair_progress(
                                    &jobs,
                                    id,
                                    &job.source_fingerprint,
                                    &exhausted,
                                    now_ms(),
                                )
                                .map_err(|e| SupervisorError::Verification(e.to_string()))?;
                            }
                        }
                        jobs.fail_verification(
                            id,
                            &issue.source_fingerprint,
                            &code.head,
                            &code.fingerprint,
                            &format!("verification failed after bounded repairs: {error}"),
                            now_ms(),
                        )?;
                        let _ = policy;
                    }
                }
            }
        }
        Ok(needs_input)
    }

    fn verify_one(
        &self,
        request: &BatchRequest,
        worker_id: &str,
        settings: &Settings,
        jobs: &JobsStore,
        job: &Job,
        batch_id: &str,
        deadline_ms: i64,
        lease_lost: Arc<AtomicBool>,
    ) -> Result<VerificationOutcome, SupervisorError> {
        validate_settings_fingerprint(request).map_err(|e| {
            SupervisorError::Verification(format!(
                "needs_input: provider settings changed after batch admission: {e}"
            ))
        })?;
        let isolation = RepoIsolation::new(
            &request.project_path,
            &request.managed_worktree_root,
            &self.paths.root,
        )
        .map_err(|e| SupervisorError::Runtime(e.to_string()))?;
        let recovery_remaining_ms = deadline_ms.saturating_sub(now_ms());
        if recovery_remaining_ms < 1 {
            return Err(SupervisorError::Verification(
                "needs_input: batch deadline expired before workspace recovery".into(),
            ));
        }
        let recovery_deadline =
            Instant::now() + Duration::from_millis(recovery_remaining_ms as u64);
        let record = isolation
            .recover_workspace_with_control(&job.issue_id, recovery_deadline, &lease_lost)
            .map_err(|error| {
                if RepoIsolation::is_metadata_lock_wait_stop(&error) {
                    SupervisorError::Verification(format!(
                        "needs_input: workspace ownership recovery could not acquire the repository metadata lock: {error}"
                    ))
                } else {
                    SupervisorError::Runtime(error.to_string())
                }
            })?;
        let policy = request
            .verification
            .iter()
            .find(|v| v.issue_id == job.issue_id)
            .ok_or_else(|| SupervisorError::NotFound(job.issue_id.clone()))?;
        let implementation = request
            .jobs
            .iter()
            .find(|v| v.issue_id == job.issue_id)
            .and_then(|v| {
                v.plan
                    .tasks
                    .iter()
                    .find(|t| matches!(t.kind, TaskKind::Implementation | TaskKind::Worker))
            })
            .ok_or_else(|| SupervisorError::Invalid("implementation identity missing".into()))?;
        let source = source_snapshot(&record.worktree_path)
            .map_err(|e| SupervisorError::Verification(e.to_string()))?;
        let remaining_ms = deadline_ms.saturating_sub(now_ms());
        if remaining_ms < 1000 {
            return Err(SupervisorError::Verification(
                "needs_input: batch deadline expired before verification dispatch".into(),
            ));
        }
        let reviewer_task = request
            .jobs
            .iter()
            .find(|entry| entry.issue_id == job.issue_id)
            .and_then(|entry| {
                entry
                    .plan
                    .tasks
                    .iter()
                    .find(|task| task.task_id == policy.reviewer.task_id)
            })
            .ok_or_else(|| {
                SupervisorError::Verification(
                    "needs_input: reviewed reviewer task is missing".into(),
                )
            })?;
        let envelope = build_verification_envelope(
            policy,
            reviewer_task,
            job.source_fingerprint.clone(),
            implementation.task_id.clone(),
            source,
            remaining_ms,
        )?;
        // Contract-only verification needs neither live reviewer metadata nor a model reservation.
        let reviewer: Box<dyn crate::verification::ReviewExecutor> =
            if !crate::verification::requires_independent_review(&envelope)
                .map_err(|error| SupervisorError::Verification(format!("needs_input: {error}")))?
            {
                Box::new(ContractOnlyReviewer)
            } else {
                let submit_job = request
                    .jobs
                    .iter()
                    .find(|entry| entry.issue_id == job.issue_id)
                    .ok_or_else(|| {
                        SupervisorError::Verification(
                            "needs_input: reviewed issue route is missing".into(),
                        )
                    })?;
                let route_key = paid_policy_key(&job.issue_id, &reviewer_task.task_id);
                let committed_route = request.resolved_routes.get(&route_key).ok_or_else(|| {
                    SupervisorError::Verification(
                        "needs_input: reviewer route was not bound to preview".into(),
                    )
                })?;
                let commitment = request.route_commitments.get(&route_key).ok_or_else(|| {
                    SupervisorError::Verification(
                        "needs_input: reviewer route lacks semantic commitment".into(),
                    )
                })?;
                if committed_route.provider_id != commitment.provider_id
                    || committed_route.model_id != commitment.model_id
                {
                    return Err(SupervisorError::Verification(
                        "needs_input: reviewer route differs from semantic commitment".into(),
                    ));
                }
                let reviewer_profile = settings
                    .providers
                    .iter()
                    .find(|profile| profile.id == committed_route.provider_id)
                    .ok_or_else(|| {
                        SupervisorError::Verification(
                            "needs_input: reviewer provider profile is missing".into(),
                        )
                    })?;
                let kind_consented = match &request.dispatch_refresh_consent {
                    crate::provider_observations::ModelFreeRefreshConsent::Disabled => false,
                    crate::provider_observations::ModelFreeRefreshConsent::Providers { kinds } => {
                        kinds.contains(&reviewer_profile.kind)
                    }
                };
                let current_facts = if kind_consented {
                    let remaining = deadline_ms.saturating_sub(now_ms());
                    if remaining < 1000 || lease_lost.load(Ordering::SeqCst) {
                        let reason = "batch deadline or verification lease ended before reviewer metadata refresh";
                        let _ = jobs.record_verification_checkpoint(
                            job.id,
                            &job.source_fingerprint,
                            "verification_route_refresh",
                            &json!({"state":"needs_input","reason":reason}),
                            now_ms(),
                        );
                        return Err(SupervisorError::Verification("needs_input: batch deadline or verification lease ended before reviewer metadata refresh".into()));
                    }
                    let fresh = match crate::provider_observations::fresh_candidate_facts(
                        settings,
                        &committed_route.provider_id,
                        &committed_route.model_id,
                        Instant::now() + Duration::from_millis(remaining as u64),
                        &request.dispatch_refresh_consent,
                    ) {
                        Ok(fresh) => fresh,
                        Err(error) => {
                            let reason = format!(
                                "reviewer metadata refresh failed before dispatch: {error}"
                            );
                            let _ = jobs.record_verification_checkpoint(
                                job.id,
                                &job.source_fingerprint,
                                "verification_route_refresh",
                                &json!({"state":"needs_input","reason":reason}),
                                now_ms(),
                            );
                            return Err(SupervisorError::Verification(format!(
                                "needs_input: {reason}"
                            )));
                        }
                    };
                    if lease_lost.load(Ordering::SeqCst) || now_ms() >= deadline_ms {
                        let reason = "batch deadline or verification lease ended during reviewer metadata refresh";
                        let _ = jobs.record_verification_checkpoint(
                            job.id,
                            &job.source_fingerprint,
                            "verification_route_refresh",
                            &json!({"state":"needs_input","reason":reason}),
                            now_ms(),
                        );
                        return Err(SupervisorError::Verification("needs_input: batch deadline or verification lease ended during reviewer metadata refresh".into()));
                    }
                    if let Err(error) = check_fresh_route_commitment(
                        request,
                        settings,
                        submit_job,
                        reviewer_task,
                        &fresh,
                    ) {
                        let reason = format!(
                            "reviewer metadata no longer matches the reviewed commitment: {error}"
                        );
                        let _ = jobs.record_verification_checkpoint(job.id, &job.source_fingerprint,
                    "verification_route_refresh", &json!({"state":"needs_input","reason":reason,"snapshotFingerprint":fresh.snapshot_fingerprint,"settingsFingerprint":fresh.settings_fingerprint,"runtimeIdentityFingerprint":fresh.runtime_identity_fingerprint,"observedAtMs":fresh.observed_at_ms}), now_ms());
                        return Err(SupervisorError::Verification(format!(
                            "needs_input: {reason}"
                        )));
                    }
                    jobs.record_verification_checkpoint(job.id, &job.source_fingerprint,
                "verification_route_refresh", &json!({"state":"matched","snapshotFingerprint":fresh.snapshot_fingerprint,"settingsFingerprint":fresh.settings_fingerprint,"runtimeIdentityFingerprint":fresh.runtime_identity_fingerprint,"observedAtMs":fresh.observed_at_ms}), now_ms())
                .map_err(|error| SupervisorError::Verification(error.to_string()))?;
                    vec![fresh.facts]
                } else {
                    let reviewed_observation_fingerprint = request
                        .provider_observation_fingerprint
                        .as_deref()
                        .ok_or_else(|| {
                            SupervisorError::Verification(
                                "needs_input: provider routing evidence was not bound to preview"
                                    .into(),
                            )
                        })?;
                    let (facts, current_observation_fingerprint, observation_problem) =
                        load_provider_observations(
                            settings,
                            &self.paths.root,
                            now_ms().max(0) as u128,
                        )
                        .map_err(|error| {
                            SupervisorError::Verification(format!("needs_input: {error}"))
                        })?;
                    if observation_problem.is_some()
                        || reviewed_observation_fingerprint != current_observation_fingerprint
                    {
                        return Err(SupervisorError::Verification(format!(
                            "needs_input: provider routing evidence changed or became unavailable before reviewer dispatch: {}",
                            observation_problem.unwrap_or_else(|| {
                                "observation snapshot fingerprint changed".into()
                            })
                        )));
                    }
                    facts
                };
                let reviewer_pricing_time = if kind_consented {
                    current_facts
                        .iter()
                        .find(|fact| {
                            fact.provider_id == committed_route.provider_id
                                && fact.model_id == committed_route.model_id
                        })
                        .and_then(|fact| fact.pricing_verified_at_ms)
                        .and_then(|value| u64::try_from(value).ok())
                } else {
                    None
                };
                let reviewer_route = route_task(
                    request,
                    settings,
                    submit_job,
                    reviewer_task,
                    &current_facts,
                    reviewer_pricing_time,
                )
                .map_err(|error| SupervisorError::Verification(format!("needs_input: {error}")))?;
                if !reviewer_route.eligible
                    || reviewer_route.provider_id.as_deref()
                        != Some(policy.reviewer.provider_id.as_str())
                    || reviewer_route.model_id.as_deref() != Some(policy.reviewer.model_id.as_str())
                    || request.resolved_routes.get(&route_key).is_none()
                {
                    return Err(SupervisorError::Verification(format!(
                        "needs_input: reviewer route no longer matches the reviewed preview: {}",
                        reviewer_route.reasons.join("; ")
                    )));
                }
                let reviewer_limits = review_limits(request, &job.issue_id)?;
                let reviewer_budget_seconds =
                    reviewer_limits.2.min((remaining_ms / 1000).max(1) as u64);
                let reservation_id = format!(
                    "{}-{}-review-r{}-a{}",
                    safe_checkpoint_key(&request.batch_id),
                    job.id,
                    repair_round(jobs, job.id),
                    job.attempt_count
                );
                let budgets = BudgetStore::open(&self.paths.budgets)?;
                let reviewer_reservation = ReservationRequest {
                    reservation_id: reservation_id.clone(),
                    batch_id: request.batch_id.clone(),
                    job_id: job.id.to_string(),
                    task_id: safe_checkpoint_key(&policy.reviewer.task_id),
                    provider_id: policy.reviewer.provider_id.clone(),
                    model_id: policy.reviewer.model_id.clone(),
                    runtime_id: settings
                        .providers
                        .iter()
                        .find(|p| p.id == policy.reviewer.provider_id)
                        .map(|p| p.kind.telemetry_id().to_owned())
                        .unwrap_or_else(|| "unknown".into()),
                    max_tokens: reviewer_limits.0,
                    max_cost_micros: Some(reviewer_limits.1),
                    max_seconds: reviewer_budget_seconds,
                    is_paid: false,
                    pricing_source: None,
                    pricing_verified_at_ms: None,
                };
                let batches = BatchStore::open(&self.paths.batches)?;
                reserve_with_capacity_wait(&budgets, &reviewer_reservation, deadline_ms, || {
                    if now_ms() >= deadline_ms {
                        return Err(SupervisorError::Verification(
                    "needs_input: absolute batch deadline expired while waiting for reviewer capacity"
                        .into(),
                ));
                    }
                    if lease_lost.load(Ordering::SeqCst) {
                        return Err(SupervisorError::Verification(
                    "needs_input: verification lease ownership was lost while waiting for reviewer capacity"
                        .into(),
                ));
                    }
                    if matches!(
                        batches.control_state(batch_id)?.as_str(),
                        "cancelling" | "cancelled"
                    ) {
                        return Err(SupervisorError::Verification(
                            "needs_input: batch cancellation requested before reviewer dispatch"
                                .into(),
                        ));
                    }
                    if !batches.verification_lease_owned(batch_id, job.id, worker_id, now_ms())? {
                        return Err(SupervisorError::Verification(
                    "needs_input: verification lease is no longer owned while waiting for reviewer capacity"
                        .into(),
                ));
                    }
                    Ok(())
                })?;
                let observer = Arc::new(ReviewRuntimeSink {
                    jobs: JobsStore::open(&self.paths.jobs)?,
                    budgets,
                    job_id: job.id,
                    attempt_number: job.attempt_count,
                    issue_fingerprint: job.source_fingerprint.clone(),
                    reservation_id,
                    dispatched: Mutex::new((false, false)),
                });
                let reviewer_remaining_ms = deadline_ms.saturating_sub(now_ms());
                if reviewer_remaining_ms < 1000 {
                    return Err(SupervisorError::Verification(
                        "needs_input: batch deadline expired before reviewer dispatch".into(),
                    ));
                }
                let timeout = reviewer_limits
                    .2
                    .min((reviewer_remaining_ms / 1000).max(1) as u64);
                let reviewer = runtime::RuntimeReviewExecutor::new(
                    settings.clone(),
                    record.worktree_path.clone(),
                    timeout,
                )
                .map_err(SupervisorError::Runtime)?
                .with_build_output(record.build_output_path.clone())
                .with_reasoning_effort(policy.reviewer.reasoning_effort.clone())
                .with_deadline(
                    std::time::Instant::now() + Duration::from_millis(reviewer_remaining_ms as u64),
                )
                .with_observer(observer);
                Box::new(reviewer)
            };
        let batches = BatchStore::open(&self.paths.batches)?;
        let cancel = Arc::new(AtomicBool::new(false));
        let cancel_watch = VerificationCancelWatcher::start(
            self.paths.clone(),
            batch_id.to_owned(),
            job.id,
            worker_id.to_owned(),
            deadline_ms,
            Arc::clone(&lease_lost),
            Arc::clone(&cancel),
        );
        let service = VerificationService {
            checks: DeadlineCheckExecutor {
                deadline_ms,
                build_output: record.build_output_path.clone(),
            },
            reviewer,
        };
        let guard = || -> Result<(), String> {
            if now_ms() >= deadline_ms {
                return Err(
                    "needs_input: absolute batch deadline expired before verification completion"
                        .into(),
                );
            }
            if matches!(
                batches.control_state(batch_id).as_deref(),
                Ok("cancelling" | "cancelled")
            ) {
                return Err(
                    "needs_input: batch cancellation was requested before verification completion"
                        .into(),
                );
            }
            if lease_lost.load(Ordering::SeqCst) {
                return Err("verification lease heartbeat was lost".into());
            }
            if !batches
                .verification_lease_owned(batch_id, job.id, worker_id, now_ms())
                .map_err(|e| e.to_string())?
            {
                return Err("verification lease is no longer owned by this worker".into());
            }
            Ok(())
        };
        let mut on_check_spawn = |index: usize, identity: crate::process::ProcessIdentity| {
            if cfg!(windows) && identity.creation_time_100ns.is_none() {
                return Err("check process identity lacks Windows creation time".into());
            }
            jobs.record_verification_checkpoint(
                job.id,
                &job.source_fingerprint,
                &format!("verification_check_process_{index}"),
                &json!({"state":"started","pid":identity.pid,"creationTime100ns":identity.creation_time_100ns}),
                now_ms(),
            ).map_err(|e| e.to_string())
        };
        let result = if let Some(issue_path) = request.issue_sources.get(&job.issue_id) {
            service
                .verify_job_with_issue_source_and_gate_guard_managed(
                    &record.worktree_path,
                    jobs,
                    job.id,
                    &envelope,
                    Some(issue_path),
                    guard,
                    &cancel,
                    &mut on_check_spawn,
                    now_ms(),
                )
                .map_err(|e| SupervisorError::Verification(e.to_string()))
        } else {
            service
                .verify_job_with_issue_source_and_gate_guard_managed(
                    &record.worktree_path,
                    jobs,
                    job.id,
                    &envelope,
                    None,
                    guard,
                    &cancel,
                    &mut on_check_spawn,
                    now_ms(),
                )
                .map_err(|e| SupervisorError::Verification(e.to_string()))
        };
        drop(on_check_spawn);
        drop(cancel_watch);
        result
    }
}

/// Defensive sentinel: a scope-planning error must not dispatch or silently pass review.
struct ContractOnlyReviewer;
impl crate::verification::ReviewExecutor for ContractOnlyReviewer {
    fn review(
        &self,
        _: &crate::verification::ReviewRequest,
    ) -> Result<crate::verification::ReviewResult, String> {
        Err("independent review unexpectedly required for contract-only verification".into())
    }
}

/// Entry point for the detached `fabrica-worker` executable.
pub fn worker_main(
    data_root: &Path,
    batch_id: &str,
    worker_id: &str,
) -> Result<WorkerExit, SupervisorError> {
    let identity = crate::process::current_process_identity()
        .map_err(|e| SupervisorError::Io(format!("cannot identify worker process: {e}")))?;
    let supervisor = Supervisor::open(data_root)?;
    match supervisor.run_worker(batch_id, worker_id, identity) {
        Ok(exit) => Ok(exit),
        Err(error) => {
            // Keep the job attempt leased/running for explicit reconciliation, but make the
            // detached worker's failure visible in durable batch status and events.
            record_worker_runtime_failure(&supervisor, worker_id, &error, now_ms());
            Err(error)
        }
    }
}

fn worker_failure_stage(error: &SupervisorError) -> &'static str {
    match error {
        SupervisorError::Invalid(message) if message.starts_with("settings_load:") => {
            "settings_load"
        }
        SupervisorError::Invalid(_) => "input_validation",
        SupervisorError::Conflict(_) => "state_conflict",
        SupervisorError::NotFound(_) => "missing_state",
        SupervisorError::Store(_) => "storage",
        SupervisorError::Jobs(_) => "job_store",
        SupervisorError::Budget(_) => "budget_store",
        SupervisorError::Io(_) => "local_io",
        SupervisorError::WorkerLaunch(_) => "worker_launch",
        SupervisorError::Runtime(message) if message.starts_with("workspace_isolation:") => {
            "workspace_isolation"
        }
        SupervisorError::Runtime(message) if message.starts_with("workspace_inspection:") => {
            "workspace_inspection"
        }
        SupervisorError::Runtime(message) if message.starts_with("workspace_preparation:") => {
            "workspace_preparation"
        }
        SupervisorError::Runtime(_) => "runtime",
        SupervisorError::Verification(_) => "verification",
    }
}

fn record_worker_runtime_failure(
    supervisor: &Supervisor,
    worker_id: &str,
    error: &SupervisorError,
    now: i64,
) {
    if let Ok(store) = BatchStore::open(&supervisor.paths.batches) {
        let summary = format!(
            "detached worker stopped during {}; active attempts and reservations remain unreconciled",
            worker_failure_stage(error)
        );
        let _ = store.worker_runtime_failed(worker_id, &summary, now);
    }
}

fn snapshot_jobs(jobs: &JobsStore, ids: &[i64]) -> Result<Vec<JobState>, SupervisorError> {
    ids.iter()
        .map(|id| jobs.get(*id).map(|job| job.state).map_err(Into::into))
        .collect()
}

fn is_terminal_or_waiting(state: JobState) -> bool {
    matches!(
        state,
        JobState::Verified
            | JobState::Blocked
            | JobState::NeedsInput
            | JobState::Failed
            | JobState::Cancelled
            | JobState::ReconciliationRequired
            | JobState::Stale
    )
}

fn is_safe_cancel_terminal(state: JobState) -> bool {
    matches!(
        state,
        JobState::Cancelled | JobState::Verified | JobState::Failed | JobState::Stale
    )
}

/// Return true only when every possibly dispatched child for a NeedsInput job
/// is either durably absent or confirmed stopped. Missing/malformed identity
/// after a dispatch intent is indeterminate and therefore blocks terminal cancel.
fn owned_processes_stopped_for_cancel(
    jobs: &JobsStore,
    job: &Job,
    request: &BatchRequest,
) -> Result<bool, SupervisorError> {
    let parse_exact_identity = |value: &Value| {
        checkpoint_process_identity(value)
            .filter(|identity| identity.pid != 0 && identity.creation_time_100ns.is_some())
    };
    let attempt = match jobs.recovery_attempt_info(job.id) {
        Ok(attempt) => attempt,
        Err(JobsError::Conflict(_)) => return Ok(false),
        Err(error) => return Err(error.into()),
    };
    let mut identities = Vec::new();
    if let Some(raw) = attempt.process_ref.as_deref() {
        let Ok(identity) = serde_json::from_str::<crate::process::ProcessIdentity>(raw) else {
            return Ok(false);
        };
        if identity.pid == 0 || identity.creation_time_100ns.is_none() {
            return Ok(false);
        }
        identities.push(identity);
    }
    if let Some(value) = jobs.get_checkpoint(job.id, "runtime_process_identity")? {
        let Some(identity) = parse_exact_identity(&value) else {
            return Ok(false);
        };
        identities.push(identity);
    }

    for task in request
        .jobs
        .iter()
        .filter(|entry| entry.issue_id == job.issue_id)
        .flat_map(|entry| entry.plan.tasks.iter())
        .filter(|task| matches!(task.kind, TaskKind::Implementation | TaskKind::Worker))
    {
        let dispatch_key = format!("dispatch_{}", safe_checkpoint_key(&task.task_id));
        let dispatch = jobs.get_checkpoint(job.id, &dispatch_key)?;
        let process_key = format!("runtime_process_{}", safe_checkpoint_key(&task.task_id));
        let process = jobs.get_checkpoint(job.id, &process_key)?;
        if dispatch.is_some() || process.is_some() {
            let Some(dispatch) = dispatch else {
                return Ok(false);
            };
            if dispatch.get("state").and_then(Value::as_str) != Some("intent_persisted") {
                return Ok(false);
            }
            let Some(process) = process else {
                return Ok(false);
            };
            if process.get("state").and_then(Value::as_str) != Some("started") {
                return Ok(false);
            }
            let Some(identity) = parse_exact_identity(&process) else {
                return Ok(false);
            };
            identities.push(identity);
        }
    }

    if let Some(policy) = request
        .verification
        .iter()
        .find(|p| p.issue_id == job.issue_id)
    {
        for (index, _) in policy.checks.iter().enumerate() {
            let check_key = format!("verification_check_{index}");
            let check = jobs.get_checkpoint(job.id, &check_key)?;
            let process_key = format!("verification_check_process_{index}");
            let process = jobs.get_checkpoint(job.id, &process_key)?;
            if check.is_some() || process.is_some() {
                let Some(checkpoint) = check else {
                    return Ok(false);
                };
                if !matches!(
                    checkpoint.get("state").and_then(Value::as_str),
                    Some("started" | "completed" | "needs_input")
                ) {
                    return Ok(false);
                }
                let Some(process) = process else {
                    return Ok(false);
                };
                let Some(identity) = parse_exact_identity(&process) else {
                    return Ok(false);
                };
                identities.push(identity);
            }
        }
        let review = jobs.get_checkpoint(job.id, "verification_review")?;
        let review_process = jobs.get_checkpoint(job.id, "verification_review_process")?;
        if review
            .as_ref()
            .is_some_and(|value| value.get("state").and_then(Value::as_str) == Some("started"))
            && review_process.is_none()
        {
            return Ok(false);
        }
        if let Some(value) = review_process {
            if value.get("state").and_then(Value::as_str) != Some("started") {
                return Ok(false);
            }
            let Some(identity) = parse_exact_identity(&value) else {
                return Ok(false);
            };
            identities.push(identity);
        }
    }

    for identity in identities {
        match crate::process::process_identity_matches(identity) {
            Ok(false) => {}
            Ok(true) => {
                // The identity was parsed from this job's durable journal and
                // matched the exact live process. Ask the owned-process helper
                // to stop it, then require positive confirmation before the
                // job can become terminal.
                let _ = crate::process::terminate_owned_process(identity);
                let deadline = Instant::now() + Duration::from_secs(2);
                loop {
                    match crate::process::process_identity_matches(identity) {
                        Ok(false) => break,
                        Ok(true) if Instant::now() < deadline => {
                            thread::sleep(Duration::from_millis(50));
                        }
                        Ok(true) | Err(_) => return Ok(false),
                    }
                }
            }
            Err(_) => return Ok(false),
        }
    }
    Ok(true)
}

fn has_job_prerequisite(request: &BatchRequest, issue_id: &str) -> bool {
    request
        .jobs
        .iter()
        .find(|job| job.issue_id == issue_id)
        .is_some_and(|job| {
            job.dependencies
                .iter()
                .any(|dep| matches!(dep.resolution, DependencyResolution::Job { .. }))
        })
}

fn verified_predecessors(
    jobs: &JobsStore,
    request: &BatchRequest,
    data_root: &Path,
    batch_job_ids: &[i64],
    job: &Job,
) -> Result<Vec<VerifiedPredecessor>, SupervisorError> {
    let isolation = RepoIsolation::new(
        &request.project_path,
        &request.managed_worktree_root,
        data_root,
    )
    .map_err(|e| SupervisorError::Runtime(e.to_string()))?;
    let selected = request
        .jobs
        .iter()
        .find(|entry| entry.issue_id == job.issue_id)
        .ok_or_else(|| SupervisorError::NotFound(job.issue_id.clone()))?;
    let mut result = Vec::new();
    for dependency in &selected.dependencies {
        let DependencyResolution::Job {
            prerequisite_issue_id,
            required_fingerprint,
        } = &dependency.resolution
        else {
            continue;
        };
        let predecessor = request
            .jobs
            .iter()
            .find(|candidate| &candidate.issue_id == prerequisite_issue_id)
            .ok_or_else(|| SupervisorError::NotFound(prerequisite_issue_id.clone()))?;
        if predecessor.source_fingerprint != *required_fingerprint {
            return Err(SupervisorError::Conflict(format!(
                "prerequisite {prerequisite_issue_id} source fingerprint changed"
            )));
        }
        let predecessor_job = batch_job_ids
            .iter()
            .map(|id| jobs.get(*id))
            .collect::<Result<Vec<_>, _>>()?
            .into_iter()
            .find(|candidate| {
                candidate.issue_id == *prerequisite_issue_id
                    && candidate.source_fingerprint == *required_fingerprint
            })
            .ok_or_else(|| {
                SupervisorError::Conflict(format!(
                    "prerequisite {prerequisite_issue_id} is not present in the shared job store"
                ))
            })?;
        if predecessor_job.state != JobState::Verified {
            continue;
        }
        let record = isolation.recover_workspace(prerequisite_issue_id)
            .map_err(|e| SupervisorError::Runtime(format!("verified prerequisite {prerequisite_issue_id} has no recoverable isolation record: {e}")))?;
        let Some(commit) = record.owned_commits.last() else {
            return Err(SupervisorError::Conflict(format!(
                "prerequisite {prerequisite_issue_id} is verified but has no recorded local commit to integrate; explicit reconciliation/input is required"
            )));
        };
        let evidence = jobs.get_checkpoint(predecessor_job.id, "verification_complete")?
            .unwrap_or_else(|| json!({"completionGate":predecessor_job.completion_gate,"issueFingerprint":predecessor_job.source_fingerprint}));
        result.push(VerifiedPredecessor {
            issue_id: prerequisite_issue_id.clone(),
            commit: commit.clone(),
            source_fingerprint: predecessor_job.source_fingerprint,
            verification_evidence: evidence.to_string(),
        });
    }
    Ok(result)
}

fn implementation_prompt(
    issue_id: &str,
    issue_fingerprint: &str,
    issue_snapshot: &str,
    task: &PlannedTask,
    plan: &JobPlan,
) -> String {
    let files = task
        .files
        .iter()
        .map(|f| format!("- {f}"))
        .collect::<Vec<_>>()
        .join("\n");
    let acceptance = task
        .acceptance
        .iter()
        .map(|f| format!("- {f}"))
        .collect::<Vec<_>>()
        .join("\n");
    format!(
        "Implement the selected local issue in this isolated worktree. Do not commit, push, publish, merge, or deploy.\nIssue: {issue_id}\nIssue snapshot fingerprint: {issue_fingerprint}\nAuthoritative issue requirements (stored snapshot):\n{issue_snapshot}\nTask: {}\nPermitted files:\n{}\nAcceptance criteria:\n{}\nPlan task count: {}\nWork only within the permitted files, run no undeclared external actions, and report a concise completion summary. If blocked by approval, credentials, or required human input, stop and state the specific requirement.",
        task.task_id,
        files,
        acceptance,
        plan.tasks.len()
    )
}

fn authoritative_issue_snapshot<'a>(
    submitted_jobs: &'a [SubmitJob],
    job: &Job,
) -> Result<&'a str, SupervisorError> {
    let mut matches = submitted_jobs
        .iter()
        .filter(|submitted| submitted.issue_id == job.issue_id);
    let submitted = matches
        .next()
        .ok_or_else(|| SupervisorError::NotFound(job.issue_id.clone()))?;
    if matches.next().is_some() || submitted.source_fingerprint != job.source_fingerprint {
        return Err(SupervisorError::Conflict(
            "admitted issue snapshot does not match the claimed job".into(),
        ));
    }
    Ok(&submitted.issue_snapshot)
}

fn safe_checkpoint_key(value: &str) -> String {
    let key = value
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .take(80)
        .collect::<String>();
    if key.is_empty() {
        "task".into()
    } else {
        key
    }
}

fn checkpoint_process_identity(value: &Value) -> Option<crate::process::ProcessIdentity> {
    let pid = value.get("pid")?.as_u64()?;
    let creation = value.get("creationTime100ns").and_then(Value::as_u64);
    if pid == 0 || pid > u32::MAX as u64 || (cfg!(windows) && creation.is_none()) {
        return None;
    }
    Some(crate::process::ProcessIdentity {
        pid: pid as u32,
        creation_time_100ns: creation,
    })
}

fn bounded_runtime_output_status_value(value: &Value, max_bytes: usize) -> Option<Value> {
    let tail = value.get("tail")?.as_str()?;
    let original_bytes = tail.len();
    let stream = value.get("stream").cloned().unwrap_or(Value::Null);
    let boundaries = tail
        .char_indices()
        .map(|(index, _)| index)
        .chain(std::iter::once(tail.len()))
        .collect::<Vec<_>>();
    let mut low = 0usize;
    let mut high = boundaries.len().saturating_sub(1);
    while low < high {
        let middle = low + (high - low) / 2;
        let projected = json!({
            "tail": &tail[boundaries[middle]..],
            "stream": stream.clone(),
            "truncated": true,
            "originalBytes": original_bytes,
        });
        if serde_json::to_vec(&projected).ok()?.len() <= max_bytes {
            high = middle;
        } else {
            low = middle + 1;
        }
    }
    let projected = json!({
        "tail": &tail[boundaries[low]..],
        "stream": stream.clone(),
        "truncated": true,
        "originalBytes": original_bytes,
    });
    (serde_json::to_vec(&projected).ok()?.len() <= max_bytes).then_some(projected)
}

fn status_checkpoint_keys(request: &BatchRequest, job: &Job) -> Vec<String> {
    let mut keys = vec![
        "implementation_proposed".into(),
        "verification_complete".into(),
        "verification_review".into(),
        "verification_review_process".into(),
        "runtime_process_identity".into(),
        "runtime_effect_intent".into(),
        "runtime_usage".into(),
        "workspace_preparation_failure".into(),
        "post_provider_capture_failure".into(),
        "verification_repair_round_1".into(),
        "verification_repair_round_2".into(),
    ];
    keys.push(format!(
        "runtime_output_{}",
        safe_checkpoint_key(&job.source_fingerprint)
    ));
    if let Some(submission) = request
        .jobs
        .iter()
        .find(|entry| entry.issue_id == job.issue_id)
    {
        for task in &submission.plan.tasks {
            if matches!(task.kind, TaskKind::Implementation | TaskKind::Worker) {
                keys.push(format!("dispatch_{}", safe_checkpoint_key(&task.task_id)));
                keys.push(format!(
                    "runtime_process_{}",
                    safe_checkpoint_key(&task.task_id)
                ));
            }
        }
    }
    if let Some(policy) = request
        .verification
        .iter()
        .find(|entry| entry.issue_id == job.issue_id)
    {
        for index in 0..policy.checks.len().min(64) {
            keys.push(format!("verification_check_{index}"));
            keys.push(format!("verification_check_process_{index}"));
        }
    }
    keys
}

fn trim_utf8_prefix(value: &mut String, requested: usize) {
    let mut boundary = requested.min(value.len());
    while !value.is_char_boundary(boundary) {
        boundary += 1;
    }
    value.drain(..boundary);
}

fn reservation_id(batch: &str, job_id: i64, task_id: &str, attempt: u32) -> String {
    format!(
        "{}-{}-{}-{}",
        safe_checkpoint_key(batch)
            .chars()
            .take(40)
            .collect::<String>(),
        job_id,
        safe_checkpoint_key(task_id)
            .chars()
            .take(40)
            .collect::<String>(),
        attempt
    )
}

/// Once an explicit SafeToRetry decision has proved the old process is stopped
/// and its effects were reconciled, release only the active concurrency held
/// by that exact attempt. Missing usage stays unresolved and continues to
/// consume the original aggregate token, cost, and time ceilings.
fn preserve_recovered_reservation_envelopes(
    budgets: &BudgetStore,
    jobs: &JobsStore,
    request: &BatchRequest,
    job: &Job,
    attempt: &crate::jobs::RecoveryAttemptInfo,
    now: u64,
) -> Result<(), SupervisorError> {
    let mut reservations = Vec::new();
    for entry in request
        .jobs
        .iter()
        .filter(|entry| entry.issue_id == job.issue_id)
    {
        for task in entry
            .plan
            .tasks
            .iter()
            .filter(|task| matches!(task.kind, TaskKind::Implementation | TaskKind::Worker))
        {
            let key = format!("dispatch_{}", safe_checkpoint_key(&task.task_id));
            let expected = reservation_id(
                &request.batch_id,
                job.id,
                &task.task_id,
                attempt.attempt_number,
            );
            let checkpoint = jobs.get_checkpoint(job.id, &key)?;
            let has_dispatch_evidence = if let Some(checkpoint) = checkpoint {
                if checkpoint.get("state").and_then(Value::as_str) != Some("intent_persisted") {
                    return Err(SupervisorError::Conflict(
                        "recovery dispatch checkpoint has an unknown state".into(),
                    ));
                }
                match checkpoint.get("reservationId").and_then(Value::as_str) {
                    Some(id) if id == expected => true,
                    Some(id)
                        if (1..attempt.attempt_number).any(|prior| {
                            reservation_id(&request.batch_id, job.id, &task.task_id, prior) == id
                        }) =>
                    {
                        false
                    }
                    _ => {
                        return Err(SupervisorError::Conflict(
                            "recovery dispatch checkpoint does not match the current or a prior attempt of the admitted task".into(),
                        ));
                    }
                }
            } else {
                false
            };
            let current_reservation = budgets.get_optional(&expected)?;
            if current_reservation.is_none() && has_dispatch_evidence {
                return Err(SupervisorError::Conflict(
                    "current dispatch checkpoint has no matching budget reservation; recovery is blocked".into(),
                ));
            }
            if current_reservation.is_some() {
                reservations.push((expected, task.task_id.clone(), has_dispatch_evidence));
            }
        }
    }

    if let Some(policy) = request
        .verification
        .iter()
        .find(|entry| entry.issue_id == job.issue_id)
    {
        let id = format!(
            "{}-{}-review-r{}-a{}",
            safe_checkpoint_key(&request.batch_id),
            job.id,
            repair_round(jobs, job.id),
            attempt.attempt_number
        );
        let reviewer_checkpoint = jobs.get_checkpoint(job.id, "verification_review_process")?;
        let has_dispatch_evidence = reviewer_checkpoint.as_ref().is_some_and(|value| {
            value.get("state").and_then(Value::as_str) == Some("started")
                && value.get("reservationId").and_then(Value::as_str) == Some(id.as_str())
                && value.get("attemptNumber").and_then(Value::as_u64)
                    == Some(u64::from(attempt.attempt_number))
        });
        let current_reservation = budgets.get_optional(&id)?;
        if current_reservation.is_none() && has_dispatch_evidence {
            return Err(SupervisorError::Conflict(
                "current reviewer checkpoint has no matching budget reservation; recovery is blocked".into(),
            ));
        }
        if current_reservation.is_some() {
            reservations.push((
                id,
                safe_checkpoint_key(&policy.reviewer.task_id),
                has_dispatch_evidence,
            ));
        }
    }

    let mut verified = Vec::with_capacity(reservations.len());
    for (id, task_id, has_dispatch_evidence) in reservations {
        let reservation = budgets.get(&id)?;
        if reservation.batch_id != request.batch_id
            || reservation.job_id != job.id.to_string()
            || reservation.task_id != task_id
        {
            return Err(SupervisorError::Conflict(
                "recovery reservation ownership does not match the persisted batch, job, and task"
                    .into(),
            ));
        }
        verified.push((reservation, has_dispatch_evidence));
    }
    for (reservation, has_dispatch_evidence) in verified {
        let id = reservation.reservation_id;
        match reservation.state {
            crate::budgets::ReservationState::Dispatched => {
                if !has_dispatch_evidence {
                    return Err(SupervisorError::Conflict(
                        "a dispatched reservation has no persisted process/dispatch evidence; recovery is blocked".into(),
                    ));
                }
                budgets.settle(&id, None, None, None, "explicit-recovery-unknown", now)?;
            }
            crate::budgets::ReservationState::Reserved => {
                budgets.release_before_dispatch(&id)?;
            }
            crate::budgets::ReservationState::Unresolved
            | crate::budgets::ReservationState::Settled
            | crate::budgets::ReservationState::Released => {}
        }
    }
    Ok(())
}

fn validate_observed_change_scope(observed: &[String], allowed: &[String]) -> Result<(), String> {
    fn normalize(path: &str) -> Result<String, String> {
        let relative = Path::new(path);
        if path.trim().is_empty()
            || relative.is_absolute()
            || relative.components().any(|component| {
                matches!(
                    component,
                    std::path::Component::ParentDir | std::path::Component::Prefix(_)
                )
            })
        {
            return Err("changed path is not safe and relative".into());
        }
        Ok(path.replace('\\', "/").to_ascii_lowercase())
    }
    let allowed = allowed
        .iter()
        .map(|path| normalize(path))
        .collect::<Result<HashSet<_>, _>>()?;
    for path in observed {
        let normalized = normalize(path)?;
        if !allowed.contains(&normalized) {
            let sample = path.chars().take(192).collect::<String>();
            return Err(format!(
                "observed changed path is outside declared task scope: {sample}"
            ));
        }
    }
    Ok(())
}

fn repair_round(jobs: &JobsStore, id: i64) -> u8 {
    (1..=2)
        .rev()
        .find(|round| {
            jobs.get_checkpoint(id, &format!("verification_repair_round_{round}"))
                .ok()
                .flatten()
                .is_some()
        })
        .unwrap_or(0) as u8
}

fn interrupt_repair_if_started(
    jobs: &JobsStore,
    id: i64,
    issue_fingerprint: &str,
) -> Result<(), SupervisorError> {
    let round = repair_round(jobs, id);
    if round == 0 {
        return Ok(());
    }
    let name = format!("verification_repair_round_{round}");
    if let Some(value) = jobs.get_checkpoint(id, &name)? {
        let prior: RepairProgress =
            serde_json::from_value(value).map_err(|e| SupervisorError::Store(e.to_string()))?;
        if prior.state == RepairState::Started {
            let progress = RepairProgress {
                round,
                state: RepairState::Interrupted,
                source_fingerprint: prior.source_fingerprint,
                result_source_fingerprint: None,
                summary: "review requires human input; this repair round is interrupted".into(),
            };
            VerificationService::<ProcessCheckExecutor,runtime::RuntimeReviewExecutor>::save_repair_progress(jobs,id,issue_fingerprint,&progress,now_ms()).map_err(|e|SupervisorError::Verification(e.to_string()))?;
        }
    }
    Ok(())
}

fn verification_has_interrupted_checkpoint(
    jobs: &JobsStore,
    job: &Job,
    policies: &[VerificationPolicy],
) -> Result<bool, SupervisorError> {
    let policy = policies
        .iter()
        .find(|p| p.issue_id == job.issue_id)
        .ok_or_else(|| SupervisorError::NotFound(job.issue_id.clone()))?;
    for index in 0..policy.checks.len() {
        if jobs
            .get_checkpoint(job.id, &format!("verification_check_{index}"))?
            .is_some_and(|v| v.get("state").and_then(Value::as_str) == Some("started"))
        {
            return Ok(true);
        }
    }
    Ok(jobs
        .get_checkpoint(job.id, "verification_review")?
        .is_some_and(|v| v.get("state").and_then(Value::as_str) == Some("started")))
}

fn persist_verification_needs_input(
    jobs: &JobsStore,
    data_root: &Path,
    request: &BatchRequest,
    job: &Job,
    policy: &VerificationPolicy,
    reason: &str,
    at_ms: i64,
) -> Result<(), SupervisorError> {
    let isolation = RepoIsolation::new(
        &request.project_path,
        &request.managed_worktree_root,
        data_root,
    )
    .map_err(|e| SupervisorError::Runtime(e.to_string()))?;
    let current = isolation
        .recover_workspace(&job.issue_id)
        .ok()
        .and_then(|record| source_snapshot(&record.worktree_path).ok());
    let recorded = || -> Result<Option<(String, String)>, SupervisorError> {
        let review = jobs.get_checkpoint(job.id, "verification_review")?;
        if let Some(value) = review {
            if let Some((head, fingerprint)) = checkpoint_code_identity(&value) {
                return Ok(Some((head.to_owned(), fingerprint.to_owned())));
            }
        }
        for index in 0..policy.checks.len() {
            if let Some(value) =
                jobs.get_checkpoint(job.id, &format!("verification_check_{index}"))?
            {
                if let Some((head, fingerprint)) = checkpoint_code_identity(&value) {
                    return Ok(Some((head.to_owned(), fingerprint.to_owned())));
                }
            }
        }
        // Workspace recovery can be unavailable precisely because the shared
        // repository metadata lock is contended. The implementation proposal
        // already contains a durable source identity and avoids reacquiring it.
        if let Some(value) = jobs.get_checkpoint(job.id, "implementation_proposed")? {
            // Production stores the source snapshot under `source` in this
            // proposal checkpoint; this is the exact durable identity written
            // before ProposedComplete closes the worker lease.
            if let Some(source) = value.get("source") {
                let head = source.get("head").and_then(Value::as_str);
                let fingerprint = source.get("fingerprint").and_then(Value::as_str);
                if let (Some(head), Some(fingerprint)) = (head, fingerprint) {
                    if !head.trim().is_empty() && !fingerprint.trim().is_empty() {
                        return Ok(Some((head.to_owned(), fingerprint.to_owned())));
                    }
                }
            }
        }
        Ok(None)
    };
    let (code_head, code_fingerprint, source_is_current) = if let Some(source) = current {
        (source.head, source.fingerprint, true)
    } else if let Some((head, fingerprint)) = recorded()? {
        (head, fingerprint, false)
    } else {
        return Err(SupervisorError::Verification(
            "needs_input: unable to persist manual acceptance because no current or checkpointed source identity is available".into(),
        ));
    };
    let reason = if source_is_current {
        reason.to_owned()
    } else {
        format!(
            "{reason}; workspace source identity is unavailable, inspect the current workspace before accepting"
        )
    };
    jobs.require_manual_acceptance(
        job.id,
        &job.source_fingerprint,
        &code_head,
        &code_fingerprint,
        &[reason],
        at_ms,
    )?;
    interrupt_repair_if_started(jobs, job.id, &job.source_fingerprint)?;
    Ok(())
}

fn checkpoint_code_identity(value: &Value) -> Option<(&str, &str)> {
    let head = value
        .get("codeHead")
        .and_then(Value::as_str)
        .or_else(|| value.get("receipt")?.get("source")?.get("head")?.as_str())?;
    let fingerprint = value
        .get("codeFingerprint")
        .and_then(Value::as_str)
        .or_else(|| {
            value
                .get("receipt")?
                .get("source")?
                .get("fingerprint")?
                .as_str()
        })?;
    (!head.trim().is_empty() && !fingerprint.trim().is_empty()).then_some((head, fingerprint))
}

struct DeadlineCheckExecutor {
    deadline_ms: i64,
    build_output: PathBuf,
}
impl CheckExecutor for DeadlineCheckExecutor {
    fn execute(&self, repository: &Path, check: &CheckSpec) -> Result<CheckOutput, String> {
        let remaining_ms = self.deadline_ms.saturating_sub(now_ms());
        if remaining_ms <= 0 {
            return Err("absolute batch deadline expired before check dispatch".into());
        }
        let mut bounded = check.clone();
        bounded.timeout_ms = bounded.timeout_ms.min(remaining_ms as u64);
        ProcessCheckExecutor.execute_in_build_output(repository, &bounded, &self.build_output)
    }

    fn execute_managed(
        &self,
        repository: &Path,
        check: &CheckSpec,
        cancel: &AtomicBool,
        on_spawn: &mut dyn FnMut(crate::process::ProcessIdentity) -> Result<(), String>,
    ) -> Result<CheckOutput, String> {
        let remaining_ms = self.deadline_ms.saturating_sub(now_ms());
        if remaining_ms <= 0 {
            return Err("absolute batch deadline expired before check dispatch".into());
        }
        let mut bounded = check.clone();
        bounded.timeout_ms = bounded.timeout_ms.min(remaining_ms as u64);
        ProcessCheckExecutor.execute_managed_in_build_output(
            repository,
            &bounded,
            &self.build_output,
            cancel,
            |identity| on_spawn(identity),
        )
    }
}

struct VerificationLeaseHeartbeat {
    stop: Arc<AtomicBool>,
    lost: Arc<AtomicBool>,
    thread: Option<thread::JoinHandle<()>>,
}

struct VerificationCancelWatcher {
    stop: Arc<AtomicBool>,
    thread: Option<thread::JoinHandle<()>>,
}
impl VerificationCancelWatcher {
    fn start(
        paths: DataPaths,
        batch: String,
        job: i64,
        worker: String,
        deadline_ms: i64,
        lease_lost: Arc<AtomicBool>,
        cancel: Arc<AtomicBool>,
    ) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = Arc::clone(&stop);
        let thread = thread::spawn(move || {
            while !thread_stop.load(Ordering::SeqCst) {
                if now_ms() >= deadline_ms || lease_lost.load(Ordering::SeqCst) {
                    cancel.store(true, Ordering::Release);
                    break;
                }
                match BatchStore::open(&paths.batches).and_then(|store| {
                    let state = store.control_state(&batch)?;
                    if matches!(state.as_str(), "cancelling" | "cancelled") {
                        return Ok(false);
                    }
                    store.verification_lease_owned(&batch, job, &worker, now_ms())
                }) {
                    Ok(true) => {}
                    Ok(false) | Err(_) => {
                        cancel.store(true, Ordering::Release);
                        break;
                    }
                }
                thread::sleep(Duration::from_millis(40));
            }
        });
        Self {
            stop,
            thread: Some(thread),
        }
    }
}
impl Drop for VerificationCancelWatcher {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl VerificationLeaseHeartbeat {
    fn start(paths: DataPaths, batch: String, job: i64, worker: String, deadline_ms: i64) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let lost = Arc::new(AtomicBool::new(false));
        let s = Arc::clone(&stop);
        let l = Arc::clone(&lost);
        let thread = thread::spawn(move || {
            while !s.load(Ordering::SeqCst) {
                thread::sleep(Duration::from_secs(5));
                if s.load(Ordering::SeqCst) {
                    break;
                }
                let remaining = deadline_ms.saturating_sub(now_ms());
                if remaining <= 0 {
                    l.store(true, Ordering::SeqCst);
                    break;
                }
                let result = BatchStore::open(&paths.batches).and_then(|store| {
                    store.renew_verification(
                        &batch,
                        job,
                        &worker,
                        now_ms(),
                        MAX_LEASE_MS.min(remaining),
                    )
                });
                if result.is_err() {
                    l.store(true, Ordering::SeqCst);
                    break;
                }
            }
        });
        Self {
            stop,
            lost,
            thread: Some(thread),
        }
    }
}
impl Drop for VerificationLeaseHeartbeat {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

struct LeaseHeartbeat {
    stop: Arc<AtomicBool>,
    thread: Option<thread::JoinHandle<()>>,
}
impl LeaseHeartbeat {
    fn start(
        paths: DataPaths,
        batch: String,
        job: i64,
        worker: String,
        fence: u64,
        deadline_ms: i64,
        cancel: Arc<AtomicBool>,
    ) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let stop_thread = Arc::clone(&stop);
        let thread = thread::spawn(move || {
            while !stop_thread.load(Ordering::SeqCst) {
                thread::sleep(Duration::from_secs(5));
                if stop_thread.load(Ordering::SeqCst) {
                    break;
                }
                let remaining = deadline_ms.saturating_sub(now_ms());
                if remaining <= 0 {
                    cancel.store(true, Ordering::SeqCst);
                    break;
                }
                let result = (|| -> Result<(), SupervisorError> {
                    let jobs = JobsStore::open(&paths.jobs)?;
                    jobs.heartbeat(job, &worker, fence, now_ms(), MAX_LEASE_MS.min(remaining))?;
                    if jobs.cancellation_requested(job, &worker, fence, now_ms())? {
                        cancel.store(true, Ordering::SeqCst);
                    }
                    let control = BatchStore::open(&paths.batches)?.control_state(&batch)?;
                    if control == "cancelling" {
                        cancel.store(true, Ordering::SeqCst);
                    }
                    Ok(())
                })();
                if result.is_err() {
                    cancel.store(true, Ordering::SeqCst);
                    break;
                }
            }
        });
        Self {
            stop,
            thread: Some(thread),
        }
    }
}
impl Drop for LeaseHeartbeat {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(handle) = self.thread.take() {
            let _ = handle.join();
        }
    }
}

fn settle_runtime_usage(
    budgets: &BudgetStore,
    reservation_id: &str,
    dispatched: bool,
    usage: Option<&RuntimeUsage>,
    economy_admission: Option<&crate::economy::VerifiedEconomyAdmission>,
    elapsed_seconds: u64,
) -> Result<(), SupervisorError> {
    if !dispatched {
        budgets.release_before_dispatch(reservation_id)?;
        return Ok(());
    }
    let tokens = usage.and_then(|u| u.total_tokens);
    let cost = usage
        .and_then(|u| {
            if u.billing == BillingStatus::LocalNoApiCharge {
                Some(0.0)
            } else if u.billing == BillingStatus::ZeroCostPolicy {
                let reservation = budgets.get(reservation_id).ok()?;
                let now = now_ms().max(0) as u64;
                if reservation.state != crate::budgets::ReservationState::Dispatched
                    || reservation.max_cost_micros != 0
                    || reservation.runtime_id != "aider"
                    || reservation.pricing_source.as_deref()
                        != Some("authenticated-openrouter-model-and-zdr-endpoint-metadata")
                    || economy_admission.is_none_or(|admission| {
                        admission.provider_profile_id() != reservation.provider_id
                            || admission.model_id() != reservation.model_id
                            || admission.gateway_policy().runtime_id != reservation.runtime_id
                    })
                    || reservation.pricing_verified_at_ms.is_none_or(|observed| {
                        observed > now || now.saturating_sub(observed) > 24 * 60 * 60 * 1000
                    })
                    || u.source.as_deref() != Some("openrouter-free-ledger-token-usage")
                {
                    return None;
                }
                let expected_selected_source =
                    format!("openrouter-response-usage|model={}", reservation.model_id);
                let expected_canonical_source = format!(
                    "openrouter-response-usage|model={}",
                    economy_admission?.canonical_slug()
                );
                let calls = budgets.free_calls_for_parent(reservation_id).ok()?;
                let mut total_tokens = 0_u64;
                if calls.is_empty() || calls.len() > 1_000 {
                    return None;
                }
                for call in calls {
                    if call.state != crate::budgets::PaidCallState::Settled
                        || !matches!(
                            call.settlement_source.as_deref(),
                            Some(source)
                                if source == expected_selected_source
                                    || source == expected_canonical_source
                        )
                    {
                        return None;
                    }
                    total_tokens = total_tokens.checked_add(call.actual_tokens?)?;
                }
                (Some(total_tokens) == u.total_tokens).then_some(0.0)
            } else {
                u.cost_usd
            }
        })
        .filter(|v| v.is_finite() && *v >= 0.0)
        .map(|v| (v * 1_000_000.0).round().min(u64::MAX as f64) as u64);
    let source = usage
        .and_then(|u| u.source.as_deref())
        .unwrap_or("runtime-usage-unavailable");
    let _ = budgets.settle(
        reservation_id,
        tokens,
        cost,
        Some(elapsed_seconds),
        source,
        now_ms().max(0) as u64,
    )?;
    Ok(())
}

/// Wait for temporary provider concurrency pressure without creating a new job
/// attempt or any provider effect. Other budget failures are permanent for this
/// admitted envelope and must be surfaced for human review.
fn reserve_with_capacity_wait<F>(
    budgets: &BudgetStore,
    reservation: &ReservationRequest,
    deadline_ms: i64,
    mut guard: F,
) -> Result<crate::budgets::Reservation, SupervisorError>
where
    F: FnMut() -> Result<(), SupervisorError>,
{
    loop {
        guard()?;
        if now_ms() >= deadline_ms {
            return Err(SupervisorError::Verification(
                "needs_input: absolute batch deadline expired while waiting for provider capacity"
                    .into(),
            ));
        }
        match budgets.reserve(reservation, now_ms().max(0) as u64) {
            Ok(value) => return Ok(value),
            Err(BudgetError::Exhausted(reason)) if reason.contains("concurrency cap is full") => {
                let remaining = deadline_ms.saturating_sub(now_ms()).max(0) as u64;
                if remaining == 0 {
                    return Err(SupervisorError::Verification(
                        "needs_input: absolute batch deadline expired while waiting for provider capacity"
                            .into(),
                    ));
                }
                thread::sleep(Duration::from_millis(100.min(remaining)));
            }
            Err(BudgetError::Exhausted(reason)) => {
                return Err(SupervisorError::Verification(format!(
                    "needs_input: provider budget cannot admit this task before dispatch: {reason}"
                )));
            }
            Err(error) => return Err(SupervisorError::Budget(error)),
        }
    }
}

fn settle_paid_parent_usage(
    budgets: &BudgetStore,
    reservation_id: &str,
    dispatched: bool,
    elapsed_seconds: u64,
) -> Result<(), SupervisorError> {
    let calls = budgets.paid_calls_for_parent(reservation_id)?;
    if !dispatched && calls.is_empty() {
        budgets.release_before_dispatch(reservation_id)?;
        return Ok(());
    }
    let settled = !calls.is_empty()
        && calls
            .iter()
            .all(|call| call.state == crate::budgets::PaidCallState::Settled);
    let totals = if settled {
        calls.iter().try_fold((0u64, 0u64), |(tokens, cost), call| {
            Some((
                tokens.checked_add(call.actual_tokens?)?,
                cost.checked_add(call.actual_cost_micros?)?,
            ))
        })
    } else {
        None
    };
    let (tokens, cost, source) = match totals {
        Some((tokens, cost)) => (Some(tokens), Some(cost), "paid-child-ledger"),
        None => (None, None, "paid-child-ledger-unresolved"),
    };
    budgets.settle(
        reservation_id,
        tokens,
        cost,
        Some(elapsed_seconds),
        source,
        now_ms().max(0) as u64,
    )?;
    Ok(())
}

struct JobRuntimeSink<'a> {
    jobs: &'a JobsStore,
    budgets: &'a BudgetStore,
    job_id: i64,
    worker_id: &'a str,
    fencing_token: u64,
    reservation_id: &'a str,
    task_id: &'a str,
    issue_fingerprint: &'a str,
    dispatched: bool,
    output: String,
    failed: Option<String>,
}
impl RuntimeEventSink for JobRuntimeSink<'_> {
    fn on_event(&mut self, event: RuntimeEvent) -> RuntimeControl {
        match event.kind {
            RuntimeEventKind::ProcessStarted => {
                let Some(identity) = event.process else {
                    self.failed = Some("runtime did not provide an owned process identity".into());
                    return RuntimeControl::Cancel;
                };
                if cfg!(windows) && identity.creation_time_100ns.is_none() {
                    self.failed =
                        Some("runtime process identity has no Windows creation time".into());
                    return RuntimeControl::Cancel;
                }
                let encoded = match serde_json::to_string(&identity) {
                    Ok(value) => value,
                    Err(error) => {
                        self.failed = Some(error.to_string());
                        return RuntimeControl::Cancel;
                    }
                };
                if let Err(error) = self.jobs.record_process_reference(
                    self.job_id,
                    self.worker_id,
                    self.fencing_token,
                    &encoded,
                    now_ms(),
                ) {
                    self.failed = Some(error.to_string());
                    return RuntimeControl::Cancel;
                }
                if let Err(error) = self.jobs.record_checkpoint(
                    self.job_id,
                    self.worker_id,
                    self.fencing_token,
                    "runtime_process_identity",
                    &json!({"pid":identity.pid,"creationTime100ns":identity.creation_time_100ns}),
                    now_ms(),
                ) {
                    self.failed = Some(error.to_string());
                    return RuntimeControl::Cancel;
                }
                if let Err(error) = self.jobs.record_checkpoint(
                    self.job_id,
                    self.worker_id,
                    self.fencing_token,
                    &format!("runtime_process_{}", safe_checkpoint_key(self.task_id)),
                    &json!({"state":"started","pid":identity.pid,"creationTime100ns":identity.creation_time_100ns}),
                    now_ms(),
                ) {
                    self.failed = Some(error.to_string());
                    return RuntimeControl::Cancel;
                }
                match self
                    .budgets
                    .mark_dispatched(self.reservation_id, now_ms().max(0) as u64)
                {
                    Ok(_) => {
                        self.dispatched = true;
                        RuntimeControl::Continue
                    }
                    Err(error) => {
                        self.failed = Some(error.to_string());
                        RuntimeControl::Cancel
                    }
                }
            }
            RuntimeEventKind::Output => {
                self.output.push_str(&event.text);
                if self.output.len() > 32 * 1024 {
                    let keep = self.output.len() - 32 * 1024;
                    trim_utf8_prefix(&mut self.output, keep);
                }
                let key = format!(
                    "runtime_output_{}",
                    safe_checkpoint_key(self.issue_fingerprint)
                );
                if let Err(error) = self.jobs.record_checkpoint(
                    self.job_id,
                    self.worker_id,
                    self.fencing_token,
                    &key,
                    &json!({"tail":self.output,"stream":event.stream}),
                    now_ms(),
                ) {
                    self.failed = Some(error.to_string());
                    return RuntimeControl::Cancel;
                }
                RuntimeControl::Continue
            }
            RuntimeEventKind::ApprovalRequest | RuntimeEventKind::UserInputRequest => {
                let checkpoint = runtime_input_request_checkpoint(&event.text);
                let encoded = match serde_json::to_string(&checkpoint) {
                    Ok(value) if value.len() <= 4 * 1024 => value,
                    _ => r#"{"state":"needs_input","statusKey":"runtime_input_request","method":"unknown","detailsTruncated":true}"#.into(),
                };
                if let Err(error) = self.jobs.record_checkpoint(
                    self.job_id,
                    self.worker_id,
                    self.fencing_token,
                    "runtime_input_request",
                    &serde_json::from_str::<Value>(&encoded).unwrap_or_else(|_| json!({"state":"needs_input","statusKey":"runtime_input_request","method":"unknown","detailsTruncated":true})),
                    now_ms(),
                ) {
                    self.failed = Some(format!("provider requested user input, but its durable checkpoint could not be saved: {error}"));
                    return RuntimeControl::Cancel;
                }
                self.failed = Some("provider requested approval or user input; unattended execution stopped without approving the request".into());
                RuntimeControl::Cancel
            }
            RuntimeEventKind::State => RuntimeControl::Continue,
        }
    }
}

fn runtime_input_request_checkpoint(text: &str) -> Value {
    fn excerpt(source: &Value, key: &str, limit: usize) -> Option<String> {
        source
            .get(key)
            .and_then(Value::as_str)
            .map(|value| {
                value
                    .chars()
                    .filter(|character| !character.is_control())
                    .take(limit)
                    .collect()
            })
            .filter(|value: &String| !value.trim().is_empty())
    }
    let parsed = serde_json::from_str::<Value>(text).ok();
    let Some(source) = parsed
        .as_ref()
        .filter(|value| value.get("state").and_then(Value::as_str) == Some("needs_input"))
    else {
        return json!({
            "state":"needs_input",
            "statusKey":"runtime_input_request",
            "method":"unknown",
            "reason":"Provider requires approval or user input; unattended execution stopped.",
            "detailsTruncated":true,
        });
    };
    let request_id = source.get("requestId").and_then(|value| match value {
        Value::String(text) => Some(Value::String(
            text.chars().filter(|c| !c.is_control()).take(96).collect(),
        )),
        Value::Number(_) => Some(value.clone()),
        _ => None,
    });
    let mut checkpoint = json!({
        "state":"needs_input",
        "statusKey":"runtime_input_request",
        "requestId":request_id,
        "method":excerpt(source,"method",128).unwrap_or_else(|| "unknown".into()),
        "threadId":excerpt(source,"threadId",160),
        "command":excerpt(source,"command",480),
        "cwd":excerpt(source,"cwd",320),
        "reason":excerpt(source,"reason",640),
        "availableDecisions":excerpt(source,"availableDecisions",480),
        "questions":excerpt(source,"questions",640),
    });
    if serde_json::to_vec(&checkpoint).map_or(true, |bytes| bytes.len() > 4 * 1024) {
        if let Some(object) = checkpoint.as_object_mut() {
            for key in [
                "questions",
                "availableDecisions",
                "command",
                "cwd",
                "reason",
                "threadId",
            ] {
                if let Some(Value::String(value)) = object.get_mut(key) {
                    *value = value.chars().take(96).collect();
                }
            }
            object.insert("detailsTruncated".into(), Value::Bool(true));
        }
    }
    checkpoint
}

struct ReviewRuntimeSink {
    jobs: JobsStore,
    budgets: BudgetStore,
    job_id: i64,
    attempt_number: u32,
    issue_fingerprint: String,
    reservation_id: String,
    dispatched: Mutex<(bool, bool)>,
}
impl RuntimeObserver for ReviewRuntimeSink {
    fn on_event(&self, event: &RuntimeEvent) -> RuntimeControl {
        match event.kind {
            RuntimeEventKind::ProcessStarted => {
                let Some(identity) = event.process else {
                    return RuntimeControl::Cancel;
                };
                if cfg!(windows) && identity.creation_time_100ns.is_none() {
                    return RuntimeControl::Cancel;
                }
                if self.jobs.record_verification_checkpoint(self.job_id,&self.issue_fingerprint,"verification_review_process",&json!({"state":"started","pid":identity.pid,"creationTime100ns":identity.creation_time_100ns,"reservationId":self.reservation_id,"attemptNumber":self.attempt_number}),now_ms()).is_err() { return RuntimeControl::Cancel; }
                match self
                    .budgets
                    .mark_dispatched(&self.reservation_id, now_ms().max(0) as u64)
                {
                    Ok(_) => {
                        if let Ok(mut state) = self.dispatched.lock() {
                            state.0 = true;
                        }
                        RuntimeControl::Continue
                    }
                    Err(_) => RuntimeControl::Cancel,
                }
            }
            RuntimeEventKind::ApprovalRequest | RuntimeEventKind::UserInputRequest => {
                let checkpoint = runtime_input_request_checkpoint(&event.text);
                let _ = self.jobs.record_verification_checkpoint(
                    self.job_id,
                    &self.issue_fingerprint,
                    "runtime_input_request",
                    &checkpoint,
                    now_ms(),
                );
                RuntimeControl::Cancel
            }
            RuntimeEventKind::Output | RuntimeEventKind::State => RuntimeControl::Continue,
        }
    }
    fn on_outcome(&self, outcome: &crate::runtime::RuntimeOutcome) -> Result<(), String> {
        let mut state = self.dispatched.lock().map_err(|e| e.to_string())?;
        settle_runtime_usage(
            &self.budgets,
            &self.reservation_id,
            state.0,
            Some(&outcome.usage),
            None,
            outcome.elapsed_ms / 1000,
        )
        .map_err(|e| e.to_string())?;
        state.1 = true;
        Ok(())
    }
}
impl Drop for ReviewRuntimeSink {
    fn drop(&mut self) {
        if let Ok(state) = self.dispatched.lock() {
            if !state.0 && !state.1 {
                let _ = self.budgets.release_before_dispatch(&self.reservation_id);
            }
        }
    }
}

fn review_limits(
    request: &BatchRequest,
    issue_id: &str,
) -> Result<(u64, u64, u64), SupervisorError> {
    let reviewer_id = request
        .verification
        .iter()
        .find(|v| v.issue_id == issue_id)
        .map(|v| v.reviewer.task_id.as_str())
        .ok_or_else(|| SupervisorError::NotFound(issue_id.into()))?;
    let reviewer_task = request
        .jobs
        .iter()
        .find(|j| j.issue_id == issue_id)
        .and_then(|j| {
            j.plan
                .tasks
                .iter()
                .find(|t| t.task_id == reviewer_id && t.kind == TaskKind::Reviewer)
        })
        .ok_or_else(|| {
            SupervisorError::Invalid(format!("{issue_id} has no declared Reviewer task"))
        })?;
    Ok((
        reviewer_task.limits.max_tokens.unwrap_or(0),
        reviewer_task.limits.max_cost_micros.unwrap_or(0),
        reviewer_task.limits.max_seconds.unwrap_or(0),
    ))
}

#[derive(Clone)]
struct BatchStore {
    path: PathBuf,
}
impl BatchStore {
    fn open(path: &Path) -> Result<Self, SupervisorError> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|e| SupervisorError::Io(e.to_string()))?;
        }
        let store = Self {
            path: path.to_path_buf(),
        };
        let conn = store.connection()?;
        let version: i64 = conn
            .pragma_query_value(None, "user_version", |r| r.get(0))
            .map_err(store_error)?;
        if version > BATCH_SCHEMA {
            return Err(SupervisorError::Store(format!(
                "batch schema {version} is newer than supported schema {BATCH_SCHEMA}"
            )));
        }
        if version == 0 {
            conn.execute_batch("BEGIN IMMEDIATE; CREATE TABLE batches(batch_id TEXT PRIMARY KEY, state TEXT NOT NULL, control TEXT NOT NULL, request_json TEXT NOT NULL, job_ids_json TEXT NOT NULL DEFAULT '[]', created_at_ms INTEGER NOT NULL, updated_at_ms INTEGER NOT NULL, deadline_ms INTEGER NOT NULL, control_note TEXT); CREATE TABLE batch_events(batch_id TEXT NOT NULL REFERENCES batches(batch_id), seq INTEGER NOT NULL, at_ms INTEGER NOT NULL, kind TEXT NOT NULL, payload_json TEXT NOT NULL, PRIMARY KEY(batch_id,seq)); CREATE TABLE batch_workers(worker_id TEXT PRIMARY KEY, batch_id TEXT NOT NULL REFERENCES batches(batch_id), slot INTEGER NOT NULL, pid INTEGER, creation_time_100ns INTEGER, state TEXT NOT NULL, updated_at_ms INTEGER NOT NULL, UNIQUE(batch_id,slot)); CREATE TABLE verification_leases(batch_id TEXT NOT NULL, job_id INTEGER NOT NULL, worker_id TEXT NOT NULL, lease_until_ms INTEGER NOT NULL, PRIMARY KEY(batch_id,job_id)); PRAGMA user_version=1; COMMIT;").map_err(store_error)?;
        }
        Ok(store)
    }
    fn connection(&self) -> Result<Connection, SupervisorError> {
        let c = Connection::open(&self.path).map_err(store_error)?;
        c.busy_timeout(Duration::from_secs(10))
            .map_err(store_error)?;
        c.pragma_update(None, "foreign_keys", "ON")
            .map_err(store_error)?;
        Ok(c)
    }
    fn begin(
        &self,
        request: &BatchRequest,
        encoded: &str,
        now: i64,
    ) -> Result<(), SupervisorError> {
        let c = self.connection()?;
        let tx = c.unchecked_transaction().map_err(store_error)?;
        let existing: Option<String> = tx
            .query_row(
                "SELECT request_json FROM batches WHERE batch_id=?1",
                [&request.batch_id],
                |r| r.get(0),
            )
            .optional()
            .map_err(store_error)?;
        if let Some(existing) = existing {
            if existing != encoded {
                return Err(SupervisorError::Conflict(
                    "batch ID was already used for a different immutable request".into(),
                ));
            }
            tx.commit().map_err(store_error)?;
            return Ok(());
        }
        tx.execute("INSERT INTO batches(batch_id,state,control,request_json,created_at_ms,updated_at_ms,deadline_ms) VALUES(?1,'starting','run',?2,?3,?3,?4)", params![request.batch_id, encoded, now, now.saturating_add((request.policy.window_seconds as i64).saturating_mul(1000))]).map_err(store_error)?;
        append_batch_event(
            &tx,
            &request.batch_id,
            now,
            "batch_start_requested",
            json!({"issueCount":request.jobs.len()}),
        )?;
        tx.commit().map_err(store_error)
    }
    fn link_jobs(&self, batch: &str, jobs: &[Job], now: i64) -> Result<(), SupervisorError> {
        let ids: Vec<i64> = jobs.iter().map(|j| j.id).collect();
        let encoded =
            serde_json::to_string(&ids).map_err(|e| SupervisorError::Store(e.to_string()))?;
        let c = self.connection()?;
        let tx = c.unchecked_transaction().map_err(store_error)?;
        tx.execute("UPDATE batches SET job_ids_json=?2,state=CASE WHEN control='run' THEN 'running' ELSE state END,updated_at_ms=?3 WHERE batch_id=?1", params![batch, encoded, now]).map_err(store_error)?;
        append_batch_event(&tx, batch, now, "jobs_submitted", json!({"jobIds":ids}))?;
        tx.commit().map_err(store_error)
    }
    fn get(&self, batch: &str) -> Result<BatchSnapshot, SupervisorError> {
        let c = self.connection()?;
        let row: (String, i64, i64, i64, String, Option<String>) = c.query_row("SELECT state,created_at_ms,updated_at_ms,deadline_ms,job_ids_json,control_note FROM batches WHERE batch_id=?1", [batch], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?,r.get(5)?))).optional().map_err(store_error)?.ok_or_else(|| SupervisorError::NotFound(batch.into()))?;
        Ok(BatchSnapshot {
            batch_id: batch.into(),
            state: parse_batch_state(&row.0)?,
            created_at_ms: row.1,
            updated_at_ms: row.2,
            deadline_ms: row.3,
            job_ids: serde_json::from_str(&row.4)
                .map_err(|e| SupervisorError::Store(e.to_string()))?,
            control_note: row.5,
        })
    }
    fn events(
        &self,
        batch: &str,
        cursor: u64,
        limit: u32,
    ) -> Result<Vec<BatchEvent>, SupervisorError> {
        let c = self.connection()?;
        let exists: bool = c
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM batches WHERE batch_id=?1)",
                [batch],
                |r| r.get(0),
            )
            .map_err(store_error)?;
        if !exists {
            return Err(SupervisorError::NotFound(batch.into()));
        }
        let mut s=c.prepare("SELECT seq,at_ms,kind,payload_json FROM batch_events WHERE batch_id=?1 AND seq>?2 ORDER BY seq LIMIT ?3").map_err(store_error)?;
        let rows = s
            .query_map(
                params![batch, cursor.min(i64::MAX as u64) as i64, limit],
                |r| {
                    Ok(BatchEvent {
                        sequence: r.get::<_, i64>(0)? as u64,
                        timestamp_ms: r.get(1)?,
                        kind: r.get(2)?,
                        payload: serde_json::from_str(&r.get::<_, String>(3)?)
                            .unwrap_or(Value::Null),
                    })
                },
            )
            .map_err(store_error)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(store_error)
    }
    fn control(
        &self,
        batch: &str,
        control: &str,
        note: &str,
        now: i64,
    ) -> Result<BatchSnapshot, SupervisorError> {
        if note.len() > 2048 || note.chars().any(char::is_control) {
            return Err(SupervisorError::Invalid(
                "control note must be <= 2048 printable characters".into(),
            ));
        }
        let c = self.connection()?;
        let tx = c.unchecked_transaction().map_err(store_error)?;
        let current: Option<String> = tx
            .query_row(
                "SELECT state FROM batches WHERE batch_id=?1",
                [batch],
                |r| r.get(0),
            )
            .optional()
            .map_err(store_error)?;
        let current = current.ok_or_else(|| SupervisorError::NotFound(batch.into()))?;
        let state = match control {
            "paused" => "paused",
            "running" => "running",
            "stopping" => "stopping",
            "cancelling" => "cancelling",
            _ => return Err(SupervisorError::Invalid("unknown batch control".into())),
        };
        let allowed = match state {
            "paused" => matches!(current.as_str(), "running" | "starting"),
            "running" => matches!(current.as_str(), "paused" | "waiting" | "starting"),
            "stopping" => matches!(current.as_str(), "running" | "starting" | "paused"),
            "cancelling" => matches!(
                current.as_str(),
                "running" | "starting" | "paused" | "stopping" | "waiting"
            ),
            _ => false,
        };
        if !allowed {
            return Err(SupervisorError::Conflict(format!(
                "cannot transition batch from {current} to {state}"
            )));
        }
        tx.execute("UPDATE batches SET state=?2,control=?2,control_note=?3,updated_at_ms=?4 WHERE batch_id=?1", params![batch,state,note,now]).map_err(store_error)?;
        append_batch_event(
            &tx,
            batch,
            now,
            "batch_control",
            json!({"control":control,"note":note}),
        )?;
        tx.commit().map_err(store_error)?;
        self.get(batch)
    }
    fn request(&self, batch: &str) -> Result<BatchRequest, SupervisorError> {
        let c = self.connection()?;
        let encoded: String = c
            .query_row(
                "SELECT request_json FROM batches WHERE batch_id=?1",
                [batch],
                |r| r.get(0),
            )
            .optional()
            .map_err(store_error)?
            .ok_or_else(|| SupervisorError::NotFound(batch.into()))?;
        serde_json::from_str(&encoded).map_err(|e| SupervisorError::Store(e.to_string()))
    }
    fn reserve_worker(
        &self,
        batch: &str,
        worker: &str,
        slot: u32,
        now: i64,
    ) -> Result<bool, SupervisorError> {
        let c = self.connection()?;
        let tx = c.unchecked_transaction().map_err(store_error)?;
        let state: String = tx
            .query_row(
                "SELECT state FROM batches WHERE batch_id=?1",
                [batch],
                |r| r.get(0),
            )
            .optional()
            .map_err(store_error)?
            .ok_or_else(|| SupervisorError::NotFound(batch.into()))?;
        if !matches!(state.as_str(), "running" | "starting") {
            return Err(SupervisorError::Conflict(format!(
                "cannot launch a worker while batch is {state}"
            )));
        }
        let changed=tx.execute("INSERT OR IGNORE INTO batch_workers(worker_id,batch_id,slot,state,updated_at_ms) VALUES(?1,?2,?3,'launching',?4)",params![worker,batch,slot,now]).map_err(store_error)?;
        if changed == 1 {
            append_batch_event(
                &tx,
                batch,
                now,
                "worker_launch_reserved",
                json!({"workerId":worker,"slot":slot}),
            )?;
        }
        tx.commit().map_err(store_error)?;
        Ok(changed == 1)
    }
    fn worker_started(
        &self,
        worker: &str,
        pid: u32,
        creation: Option<u64>,
        now: i64,
    ) -> Result<(), SupervisorError> {
        let c = self.connection()?;
        let changed=c.execute("UPDATE batch_workers SET pid=?2,creation_time_100ns=?3,state='running',updated_at_ms=?4 WHERE worker_id=?1 AND state='launching'",params![worker,pid,creation.map(|n|n as i64),now]).map_err(store_error)?;
        if changed != 1 {
            return Err(SupervisorError::Conflict(
                "worker launch record changed before process identity was persisted".into(),
            ));
        }
        Ok(())
    }
    fn worker_failed(&self, worker: &str, message: &str, now: i64) -> Result<(), SupervisorError> {
        let c = self.connection()?;
        c.execute(
            "UPDATE batch_workers SET state='launch_failed',updated_at_ms=?2 WHERE worker_id=?1",
            params![worker, now],
        )
        .map_err(store_error)?;
        let batch: String = c
            .query_row(
                "SELECT batch_id FROM batch_workers WHERE worker_id=?1",
                [worker],
                |r| r.get(0),
            )
            .map_err(store_error)?;
        let tx = c.unchecked_transaction().map_err(store_error)?;
        append_batch_event(
            &tx,
            &batch,
            now,
            "worker_launch_failed",
            json!({"workerId":worker,"error":message.chars().take(512).collect::<String>()}),
        )?;
        tx.commit().map_err(store_error)
    }
    fn worker_runtime_failed(
        &self,
        worker: &str,
        message: &str,
        now: i64,
    ) -> Result<(), SupervisorError> {
        let mut c = self.connection()?;
        let tx = c
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .map_err(store_error)?;
        let batch: String = tx
            .query_row(
                "SELECT batch_id FROM batch_workers WHERE worker_id=?1",
                [worker],
                |row| row.get(0),
            )
            .optional()
            .map_err(store_error)?
            .ok_or_else(|| SupervisorError::NotFound(worker.into()))?;
        tx.execute(
            "UPDATE batch_workers SET state='failed',updated_at_ms=?2 WHERE worker_id=?1 AND state IN ('running','launching')",
            params![worker, now],
        ).map_err(store_error)?;
        append_batch_event(
            &tx,
            &batch,
            now,
            "worker_failed",
            json!({"workerId":worker,"error":message.chars().take(512).collect::<String>(),"jobAttemptsPreserved":true}),
        )?;
        tx.commit().map_err(store_error)
    }
    fn worker_slots(&self, batch: &str) -> Result<Vec<WorkerSlot>, SupervisorError> {
        let c = self.connection()?;
        let mut s=c.prepare("SELECT worker_id,slot,pid,creation_time_100ns,state,updated_at_ms FROM batch_workers WHERE batch_id=?1 ORDER BY slot").map_err(store_error)?;
        let rows = s
            .query_map([batch], |r| {
                Ok(WorkerSlot {
                    worker_id: r.get(0)?,
                    slot: r.get::<_, i64>(1)? as u32,
                    pid: r.get::<_, Option<i64>>(2)?.map(|n| n as u32),
                    creation_time_100ns: r.get::<_, Option<i64>>(3)?.map(|n| n as u64),
                    state: r.get(4)?,
                    updated_at_ms: r.get(5)?,
                })
            })
            .map_err(store_error)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(store_error)
    }
    fn worker_slot(
        &self,
        batch: &str,
        worker: &str,
    ) -> Result<Option<WorkerSlot>, SupervisorError> {
        let c = self.connection()?;
        c.query_row("SELECT worker_id,slot,pid,creation_time_100ns,state,updated_at_ms FROM batch_workers WHERE batch_id=?1 AND worker_id=?2",params![batch,worker],|r|Ok(WorkerSlot{worker_id:r.get(0)?,slot:r.get::<_,i64>(1)? as u32,pid:r.get::<_,Option<i64>>(2)?.map(|v|v as u32),creation_time_100ns:r.get::<_,Option<i64>>(3)?.map(|v|v as u64),state:r.get(4)?,updated_at_ms:r.get(5)?})).optional().map_err(store_error)
    }
    fn set_terminal(&self, batch: &str, state: &str, now: i64) -> Result<(), SupervisorError> {
        if !matches!(
            state,
            "waiting" | "completed" | "failed" | "window_expired" | "cancelled"
        ) {
            return Err(SupervisorError::Invalid(
                "invalid terminal batch state".into(),
            ));
        }
        let c = self.connection()?;
        let tx = c.unchecked_transaction().map_err(store_error)?;
        tx.execute(
            "UPDATE batches SET state=?2,control=?2,updated_at_ms=?3 WHERE batch_id=?1",
            params![batch, state, now],
        )
        .map_err(store_error)?;
        append_batch_event(&tx, batch, now, "batch_finished", json!({"state":state}))?;
        tx.commit().map_err(store_error)
    }
    fn set_cancelled_if_idle(&self, batch: &str, now: i64) -> Result<bool, SupervisorError> {
        let c = self.connection()?;
        let tx = c.unchecked_transaction().map_err(store_error)?;
        let control: Option<String> = tx
            .query_row(
                "SELECT control FROM batches WHERE batch_id=?1",
                [batch],
                |row| row.get(0),
            )
            .optional()
            .map_err(store_error)?;
        if control.as_deref() != Some("cancelling") {
            tx.commit().map_err(store_error)?;
            return Ok(false);
        }
        let active_verification: bool = tx
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM verification_leases WHERE batch_id=?1 AND lease_until_ms>?2)",
                params![batch, now],
                |row| row.get(0),
            )
            .map_err(store_error)?;
        if active_verification {
            tx.commit().map_err(store_error)?;
            return Ok(false);
        }
        tx.execute(
            "UPDATE batches SET state='cancelled',control='cancelled',updated_at_ms=?2 WHERE batch_id=?1 AND control='cancelling'",
            params![batch, now],
        )
        .map_err(store_error)?;
        append_batch_event(
            &tx,
            batch,
            now,
            "batch_finished",
            json!({"state":"cancelled"}),
        )?;
        tx.commit().map_err(store_error)?;
        Ok(true)
    }
    fn acquire_verification(
        &self,
        batch: &str,
        job: i64,
        worker: &str,
        now: i64,
        lease_ms: i64,
    ) -> Result<bool, SupervisorError> {
        let c = self.connection()?;
        let tx = c.unchecked_transaction().map_err(store_error)?;
        let control: String = tx
            .query_row(
                "SELECT control FROM batches WHERE batch_id=?1",
                [batch],
                |row| row.get(0),
            )
            .optional()
            .map_err(store_error)?
            .ok_or_else(|| SupervisorError::NotFound(batch.into()))?;
        if !matches!(control.as_str(), "run" | "running" | "starting") {
            tx.commit().map_err(store_error)?;
            return Ok(false);
        }
        let changed=tx.execute("INSERT INTO verification_leases(batch_id,job_id,worker_id,lease_until_ms) VALUES(?1,?2,?3,?4) ON CONFLICT(batch_id,job_id) DO UPDATE SET worker_id=excluded.worker_id,lease_until_ms=excluded.lease_until_ms WHERE verification_leases.lease_until_ms<=?5",params![batch,job,worker,now.saturating_add(lease_ms),now]).map_err(store_error)?;
        tx.commit().map_err(store_error)?;
        Ok(changed == 1)
    }
    fn renew_verification(
        &self,
        batch: &str,
        job: i64,
        worker: &str,
        now: i64,
        lease_ms: i64,
    ) -> Result<(), SupervisorError> {
        let c = self.connection()?;
        let changed=c.execute("UPDATE verification_leases SET lease_until_ms=?4 WHERE batch_id=?1 AND job_id=?2 AND worker_id=?3",params![batch,job,worker,now.saturating_add(lease_ms)]).map_err(store_error)?;
        if changed != 1 {
            return Err(SupervisorError::Conflict(
                "verification lease ownership was lost".into(),
            ));
        }
        Ok(())
    }
    fn verification_lease_owned(
        &self,
        batch: &str,
        job: i64,
        worker: &str,
        now: i64,
    ) -> Result<bool, SupervisorError> {
        let c = self.connection()?;
        c.query_row("SELECT EXISTS(SELECT 1 FROM verification_leases WHERE batch_id=?1 AND job_id=?2 AND worker_id=?3 AND lease_until_ms>?4)",params![batch,job,worker,now],|r|r.get(0)).map_err(store_error)
    }
    fn has_active_verifications(&self, batch: &str, now: i64) -> Result<bool, SupervisorError> {
        let c = self.connection()?;
        c.query_row("SELECT EXISTS(SELECT 1 FROM verification_leases WHERE batch_id=?1 AND lease_until_ms>?2)",params![batch,now],|r|r.get(0)).map_err(store_error)
    }
    fn release_verification(
        &self,
        batch: &str,
        job: i64,
        worker: &str,
    ) -> Result<(), SupervisorError> {
        let c = self.connection()?;
        c.execute(
            "DELETE FROM verification_leases WHERE batch_id=?1 AND job_id=?2 AND worker_id=?3",
            params![batch, job, worker],
        )
        .map_err(store_error)?;
        Ok(())
    }
    fn control_state(&self, batch: &str) -> Result<String, SupervisorError> {
        let c = self.connection()?;
        c.query_row(
            "SELECT control FROM batches WHERE batch_id=?1",
            [batch],
            |r| r.get(0),
        )
        .optional()
        .map_err(store_error)?
        .ok_or_else(|| SupervisorError::NotFound(batch.into()))
    }
    fn finish_worker(&self, batch: &str, worker: &str, now: i64) -> Result<(), SupervisorError> {
        let c = self.connection()?;
        c.execute("UPDATE batch_workers SET state='exited',updated_at_ms=?3 WHERE batch_id=?1 AND worker_id=?2",params![batch,worker,now]).map_err(store_error)?;
        Ok(())
    }
}

fn validate_request(request: &BatchRequest) -> Result<(), SupervisorError> {
    validate_request_inner(request, true)
}

fn validate_request_inner(
    request: &BatchRequest,
    require_executable_routes: bool,
) -> Result<(), SupervisorError> {
    request
        .dispatch_refresh_consent
        .validate()
        .map_err(|error| {
            SupervisorError::Invalid(format!(
                "invalid dispatch metadata-refresh consent: {error}"
            ))
        })?;
    if request.batch_id.is_empty()
        || request.batch_id.len() > 128
        || !request
            .batch_id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-')
    {
        return Err(SupervisorError::Invalid(
            "batch ID must contain only ASCII letters, digits, and hyphens".into(),
        ));
    }
    if request.jobs.is_empty() || request.jobs.len() > MAX_BATCH_JOBS {
        return Err(SupervisorError::Invalid(format!(
            "batch must contain 1..={MAX_BATCH_JOBS} jobs"
        )));
    }
    if request.policy.window_seconds == 0 || request.policy.window_seconds > MAX_WINDOW_SECONDS {
        return Err(SupervisorError::Invalid(
            "batch wall-clock window must be 1 second to 7 days".into(),
        ));
    }
    if request.policy.max_concurrency == 0
        || request.policy.max_concurrency > MAX_WORKERS
        || request.policy.budget.max_concurrency != request.policy.max_concurrency
    {
        return Err(SupervisorError::Invalid(format!(
            "concurrency must be 1..={MAX_WORKERS} and match the budget policy"
        )));
    }
    if request.policy.max_auth_retries != 0 || request.policy.max_network_retries != 0 {
        return Err(SupervisorError::Invalid(
            "authentication and network retries are not implemented; both retry limits must be zero".into(),
        ));
    }
    if request.policy.budget.max_tokens == 0 || request.policy.budget.max_seconds == 0 {
        return Err(SupervisorError::Invalid(
            "token and time budget caps must be positive".into(),
        ));
    }
    if request.policy.budget.max_seconds > request.policy.window_seconds {
        return Err(SupervisorError::Invalid(
            "aggregate runtime budget cannot exceed the wall-clock window".into(),
        ));
    }
    for required in [
        PermittedEffect::ProviderExecution,
        PermittedEffect::WorkspaceWrites,
        PermittedEffect::LocalChecks,
    ] {
        if !request.policy.permitted_effects.contains(&required) {
            return Err(SupervisorError::Invalid(format!(
                "batch must explicitly permit {required:?}"
            )));
        }
    }
    let mut ids = HashSet::new();
    for job in &request.jobs {
        if !ids.insert(job.issue_id.as_str()) {
            return Err(SupervisorError::Invalid(format!(
                "duplicate issue {}",
                job.issue_id
            )));
        }
        if job.plan.tasks.is_empty() || job.plan.tasks.len() > crate::jobs::MAX_PLAN_TASKS {
            return Err(SupervisorError::Invalid(format!(
                "{} has an empty or oversized task plan",
                job.issue_id
            )));
        }
        let aggregate = job_budget(&job.plan);
        if aggregate.max_tokens > request.policy.budget.max_tokens
            || aggregate.max_cost_micros > request.policy.budget.max_cost_micros
            || aggregate.max_seconds > request.policy.budget.max_seconds
        {
            return Err(SupervisorError::Invalid(format!(
                "{} aggregate job budget exceeds the batch budget envelope",
                job.issue_id
            )));
        }
        for task in &job.plan.tasks {
            if task.limits.max_attempts == 0
                || task.limits.max_tokens.unwrap_or(0) == 0
                || task.limits.max_seconds.unwrap_or(0) == 0
            {
                return Err(SupervisorError::Invalid(format!(
                    "task {} requires positive attempt, token, and time caps",
                    task.task_id
                )));
            }
            if task
                .limits
                .max_tokens
                .is_some_and(|v| v > request.policy.budget.max_tokens)
                || task
                    .limits
                    .max_seconds
                    .is_some_and(|v| v > request.policy.budget.max_seconds)
            {
                return Err(SupervisorError::Invalid(format!(
                    "task {} limits exceed the batch budget envelope",
                    task.task_id
                )));
            }
            if task
                .limits
                .max_tokens
                .is_some_and(|v| v > aggregate.max_tokens)
                || task
                    .limits
                    .max_cost_micros
                    .is_some_and(|v| v > aggregate.max_cost_micros)
                || task
                    .limits
                    .max_seconds
                    .is_some_and(|v| v > aggregate.max_seconds)
            {
                return Err(SupervisorError::Invalid(format!(
                    "task {} limits exceed the aggregate job budget envelope",
                    task.task_id
                )));
            }
            if task.acceptance.is_empty() {
                return Err(SupervisorError::Invalid(format!(
                    "task {} needs explicit acceptance criteria",
                    task.task_id
                )));
            }
        }
    }
    let verification: HashMap<&str, &VerificationPolicy> = request
        .verification
        .iter()
        .map(|v| (v.issue_id.as_str(), v))
        .collect();
    if verification.len() != request.verification.len() {
        return Err(SupervisorError::Invalid(
            "verification policy contains duplicate issue IDs".into(),
        ));
    }
    for job in &request.jobs {
        if job.issue_type == "epic" {
            continue;
        }
        let policy = verification.get(job.issue_id.as_str()).ok_or_else(|| {
            SupervisorError::Invalid(format!(
                "{} has no persisted verification policy",
                job.issue_id
            ))
        })?;
        if policy.checks.is_empty() || policy.acceptance_criteria.is_empty() {
            return Err(SupervisorError::Invalid(format!(
                "{} needs checks and acceptance criteria",
                job.issue_id
            )));
        }
        for check in &policy.checks {
            validate_check(check).map_err(|message| {
                SupervisorError::Invalid(format!(
                    "{} has an invalid verification check: {message}",
                    job.issue_id
                ))
            })?;
        }
        if !job
            .plan
            .tasks
            .iter()
            .any(|t| t.kind == TaskKind::Implementation || t.kind == TaskKind::Worker)
        {
            return Err(SupervisorError::Invalid(format!(
                "{} has no executable implementation task",
                job.issue_id
            )));
        }
        if require_executable_routes && job.issue_type != "epic" {
            let policy = verification.get(job.issue_id.as_str()).ok_or_else(|| {
                SupervisorError::Invalid(format!("{} has no reviewer policy", job.issue_id))
            })?;
            let reviewers = job
                .plan
                .tasks
                .iter()
                .filter(|task| task.kind == TaskKind::Reviewer)
                .collect::<Vec<_>>();
            if reviewers.len() != 1 {
                return Err(SupervisorError::Invalid(format!(
                    "{} must declare exactly one Reviewer task with explicit provider, model, and resource caps",
                    job.issue_id
                )));
            }
            let reviewer = reviewers[0];
            if reviewer.task_id != policy.reviewer.task_id
                || reviewer.provider_id.as_deref() != Some(policy.reviewer.provider_id.as_str())
                || reviewer.model_id.as_deref() != Some(policy.reviewer.model_id.as_str())
                || reviewer.limits.max_tokens.is_none()
                || reviewer.limits.max_cost_micros.is_none()
                || reviewer.limits.max_seconds.is_none()
                || reviewer.limits.max_cost_micros.unwrap_or(0)
                    > request.policy.budget.max_cost_micros
            {
                return Err(SupervisorError::Invalid(format!(
                    "{} reviewer policy must exactly match its declared Reviewer task and explicit token, cost, and time caps",
                    job.issue_id
                )));
            }
        }
    }
    for dependency in request.jobs.iter().flat_map(|j| &j.dependencies) {
        if let DependencyResolution::Job {
            prerequisite_issue_id,
            ..
        } = &dependency.resolution
        {
            if !ids.contains(prerequisite_issue_id.as_str()) {
                return Err(SupervisorError::Invalid(format!(
                    "prerequisite {prerequisite_issue_id} is not included in the selected batch"
                )));
            }
        }
    }
    let project = request
        .project_path
        .canonicalize()
        .map_err(|e| SupervisorError::Invalid(format!("project path cannot be resolved: {e}")))?;
    if !project.is_dir() {
        return Err(SupervisorError::Invalid(
            "project path is not a directory".into(),
        ));
    }
    if require_executable_routes {
        let loaded =
            settings::load_settings(&request.settings_path).map_err(SupervisorError::Invalid)?;
        validate_reviewer_reasoning_efforts(&loaded, request)?;
        for job in &request.jobs {
            for task in &job.plan.tasks {
                if matches!(
                    task.kind,
                    TaskKind::Implementation | TaskKind::Worker | TaskKind::Reviewer
                ) {
                    if task.provider_id.is_some() != task.model_id.is_some() {
                        return Err(SupervisorError::Invalid(format!(
                            "task {} explicit routing must include both provider and model",
                            task.task_id
                        )));
                    }
                    if let Some(provider_id) = task.provider_id.as_deref() {
                        let profile = loaded
                            .providers
                            .iter()
                            .find(|profile| profile.id == provider_id)
                            .ok_or_else(|| {
                                SupervisorError::Invalid(format!(
                                    "provider {provider_id} is missing from current settings"
                                ))
                            })?;
                        if !profile
                            .models
                            .iter()
                            .any(|model| Some(model.id.as_str()) == task.model_id.as_deref())
                        {
                            return Err(SupervisorError::Invalid(format!(
                                "model {} is not configured for provider {provider_id}",
                                task.model_id.as_deref().unwrap_or_default()
                            )));
                        }
                    }
                }
            }
            if job.issue_type != "epic" {
                let policy = verification.get(job.issue_id.as_str()).ok_or_else(|| {
                    SupervisorError::Invalid(format!("{} has no reviewer policy", job.issue_id))
                })?;
                let reviewer = loaded
                    .providers
                    .iter()
                    .find(|profile| profile.id == policy.reviewer.provider_id)
                    .ok_or_else(|| {
                        SupervisorError::Invalid(format!(
                            "reviewer provider {} is missing",
                            policy.reviewer.provider_id
                        ))
                    })?;
                if reviewer.kind != ProviderKind::Codex
                    || !reviewer
                        .models
                        .iter()
                        .any(|model| model.id == policy.reviewer.model_id)
                {
                    return Err(SupervisorError::Invalid(format!(
                        "{} reviewer must use a configured model on the read-only Codex runtime",
                        job.issue_id
                    )));
                }
            }
        }
    }
    Ok(())
}

fn paid_policy_key(issue_id: &str, task_id: &str) -> String {
    format!("{issue_id}/{task_id}")
}

fn paid_call_token_ceiling(policy: &crate::paid_gateway::PaidModelPolicy) -> Option<u64> {
    policy
        .model_context_window_tokens
        .checked_add(policy.max_output_tokens_per_call)
        .and_then(|per_call| per_call.checked_mul(u64::from(policy.max_calls)))
}

fn execution_paid_policy(
    committed: &crate::paid_gateway::PaidModelPolicy,
    refreshed_pricing_time_ms: Option<u64>,
    refreshed_endpoint_time_ms: Option<u64>,
) -> Result<crate::paid_gateway::PaidModelPolicy, String> {
    const FRESHNESS_MS: u64 = 24 * 60 * 60 * 1000;
    let mut execution = committed.clone();
    if let Some(observed_at_ms) = refreshed_pricing_time_ms {
        let now = now_ms().max(0) as u64;
        if observed_at_ms > now || now.saturating_sub(observed_at_ms) > FRESHNESS_MS {
            return Err("fresh paid pricing timestamp is outside the 24-hour window".into());
        }
        if execution.endpoint_pin.is_none() {
            execution.pricing.observed_at_ms = observed_at_ms;
        }
        if let Some(catalog) = execution.model_catalog_pricing.as_mut() {
            catalog.observed_at_ms = observed_at_ms;
        }
    }
    if execution.endpoint_pin.is_some() {
        if let Some(observed_at_ms) = refreshed_endpoint_time_ms {
            let now = now_ms().max(0) as u64;
            if observed_at_ms > now || now.saturating_sub(observed_at_ms) > FRESHNESS_MS {
                return Err("fresh endpoint quote timestamp is outside the 24-hour window".into());
            }
            execution.pricing.observed_at_ms = observed_at_ms;
        }
    }
    Ok(execution)
}

fn validate_paid_model_policy(
    request: &BatchRequest,
    issue_id: &str,
    task: &PlannedTask,
    profile: &ProviderProfile,
    model_id: &str,
    policy: &crate::paid_gateway::PaidModelPolicy,
    refreshed_pricing_time_ms: Option<u64>,
) -> Result<(), SupervisorError> {
    if crate::economy::is_router_model_id(model_id) || crate::economy::is_free_variant_id(model_id)
    {
        return Err(SupervisorError::Invalid(format!(
            "paid task {} cannot use a router identity or free model variant",
            task.task_id
        )));
    }
    let model = profile
        .models
        .iter()
        .find(|model| model.id == model_id)
        .ok_or_else(|| {
            SupervisorError::Invalid(format!("paid model {model_id} is no longer configured"))
        })?;
    let task_max_tokens = task.limits.max_tokens.unwrap_or(0);
    let task_max_cost = task.limits.max_cost_micros.unwrap_or(0);
    let max_tokens = paid_call_token_ceiling(policy).ok_or_else(|| {
        SupervisorError::Invalid(format!(
            "paid task {} token ceiling overflows",
            task.task_id
        ))
    })?;
    let pricing = &policy.pricing;
    // Dispatch refresh may advance when the quote was verified. The quote
    // itself remains immutable and must still match the fresh observed prices
    // and source in route_task before this time override is considered.
    let freshness_timestamp_ms = refreshed_pricing_time_ms.unwrap_or(pricing.observed_at_ms);
    if profile.kind != ProviderKind::OpenRouter
        || policy.provider_id != profile.id
        || policy.model_id != model_id
        || policy.runtime_id != "aider"
        || model.context_window_tokens != Some(policy.model_context_window_tokens)
        || policy.model_context_window_tokens == 0
        || policy.max_output_tokens_per_call == 0
        || policy.max_output_tokens_per_call > policy.model_context_window_tokens
        || policy.max_calls == 0
        || policy.max_cost_micros == 0
        || policy.max_cost_micros > task_max_cost
        || policy.max_cost_micros > request.policy.budget.max_cost_micros
        || max_tokens > task_max_tokens
        || pricing.request_fee_micros != 0
        || pricing.input_price_micros_per_1m == 0
        || pricing.output_price_micros_per_1m == 0
        || pricing.source.trim().is_empty()
        || pricing.source.len() > 128
        || pricing.source.chars().any(char::is_control)
        || freshness_timestamp_ms > now_ms().max(0) as u64
        || (now_ms().max(0) as u64).saturating_sub(freshness_timestamp_ms) > 24 * 60 * 60 * 1000
        || policy.model_catalog_pricing.as_ref().is_some_and(|catalog| {
            policy.endpoint_pin.is_none()
                || catalog.input_price_micros_per_1m == 0
                || catalog.output_price_micros_per_1m == 0
                || catalog.source != "OpenRouter authenticated user model catalog"
                || catalog.source.len() > 128
                || catalog.source.chars().any(char::is_control)
                || catalog.observed_at_ms > now_ms().max(0) as u64
                || (now_ms().max(0) as u64).saturating_sub(catalog.observed_at_ms)
                    > 24 * 60 * 60 * 1000
        })
        || policy.endpoint_pin.as_ref().is_some_and(|pin| {
            pin.requires_tools
                != task
                    .route_requirements
                    .as_ref()
                    .and_then(|requirements| requirements.require_tools)
                    .unwrap_or(true)
                || !crate::economy::reviewed_paid_endpoint_policy_matches(
                    model_id,
                    &pin.canonical_model_id,
                    &pin.endpoint_id,
                    now_ms().max(0) as u64,
                )
        })
    {
        return Err(SupervisorError::Invalid(format!(
            "paid task {} requires matching provider/model/runtime, verified context, explicit token/cost/call caps, zero request fees, and fresh pricing provenance",
            task.task_id
        )));
    }
    let input_cost = u128::from(policy.model_context_window_tokens)
        .checked_mul(u128::from(pricing.input_price_micros_per_1m))
        .and_then(|value| value.checked_add(999_999))
        .map(|value| value / 1_000_000)
        .ok_or_else(|| {
            SupervisorError::Invalid("paid input price calculation overflowed".into())
        })?;
    let output_cost = u128::from(policy.max_output_tokens_per_call)
        .checked_mul(u128::from(pricing.output_price_micros_per_1m))
        .and_then(|value| value.checked_add(999_999))
        .map(|value| value / 1_000_000)
        .ok_or_else(|| {
            SupervisorError::Invalid("paid output price calculation overflowed".into())
        })?;
    let total_cost = input_cost
        .checked_add(output_cost)
        .and_then(|value| value.checked_add(u128::from(pricing.request_fee_micros)))
        .and_then(|value| value.checked_mul(u128::from(policy.max_calls)))
        .ok_or_else(|| {
            SupervisorError::Invalid("paid aggregate price calculation overflowed".into())
        })?;
    if total_cost > u128::from(policy.max_cost_micros) {
        return Err(SupervisorError::Invalid(format!(
            "paid task {issue_id}/{} cost ceiling is below the worst-case calls at the supplied price ceiling",
            task.task_id
        )));
    }
    Ok(())
}

fn normalize_request(request: &BatchRequest) -> Result<BatchRequest, SupervisorError> {
    let mut normalized = request.clone();
    normalized.project_path = request
        .project_path
        .canonicalize()
        .map_err(|e| SupervisorError::Invalid(format!("project path cannot be resolved: {e}")))?;
    for path in normalized.issue_sources.values_mut() {
        let resolved = if path.is_absolute() {
            path.clone()
        } else {
            normalized.project_path.join(&*path)
        };
        *path = resolved.canonicalize().map_err(|e| {
            SupervisorError::Invalid(format!("issue source path cannot be resolved: {e}"))
        })?;
        if !path.starts_with(&normalized.project_path) || !path.is_file() {
            return Err(SupervisorError::Invalid(
                "issue source files must be regular files under the selected project".into(),
            ));
        }
    }
    let cwd = std::env::current_dir().map_err(|e| SupervisorError::Io(e.to_string()))?;
    normalized.managed_worktree_root = absolute_from(&cwd, &request.managed_worktree_root);
    normalized.build_root = absolute_from(&cwd, &request.build_root);
    normalized.settings_path = absolute_from(&cwd, &request.settings_path);
    Ok(normalized)
}

pub fn normalize_batch_request(request: &BatchRequest) -> Result<BatchRequest, SupervisorError> {
    normalize_request(request)
}

pub fn preview_batch_request(
    data_root: &Path,
    request: &BatchRequest,
    worker_executable: &Path,
) -> Result<BatchPreview, SupervisorError> {
    preview_batch_request_at(
        data_root,
        request,
        worker_executable,
        now_ms().max(0) as u128,
    )
}

fn preview_batch_request_at(
    data_root: &Path,
    request: &BatchRequest,
    worker_executable: &Path,
    freshness_time_ms: u128,
) -> Result<BatchPreview, SupervisorError> {
    if request
        .issue_sources
        .values()
        .any(|path| !path.is_absolute())
    {
        return Err(SupervisorError::Invalid(
            "production issue source paths must be canonical absolute paths".into(),
        ));
    }
    let cwd = std::env::current_dir().map_err(|e| SupervisorError::Io(e.to_string()))?;
    let data_root = absolute_from(&cwd, data_root);
    let mut normalized = normalize_request(request)?;
    validate_issue_sources(&normalized, true)?;
    if !worker_executable.is_absolute() || !worker_executable.is_file() {
        return Err(SupervisorError::Invalid(
            "worker executable must be an existing absolute file".into(),
        ));
    }
    let worker = worker_executable
        .canonicalize()
        .map_err(|e| SupervisorError::Io(e.to_string()))?;
    let metadata = fs::metadata(&worker).map_err(|e| SupervisorError::Io(e.to_string()))?;
    let modified = metadata
        .modified()
        .ok()
        .and_then(|v| v.duration_since(UNIX_EPOCH).ok())
        .map_or(0, |v| v.as_nanos());
    let settings_bytes = fs::read(&normalized.settings_path)
        .map_err(|e| SupervisorError::Io(format!("cannot read settings snapshot: {e}")))?;
    if settings_bytes.len() > 2 * 1024 * 1024 {
        return Err(SupervisorError::Invalid(
            "settings snapshot exceeds 2 MiB".into(),
        ));
    }
    let settings_fingerprint = crate::process::git_object_hash(&settings_bytes)
        .map_err(|e| SupervisorError::Io(format!("cannot hash settings snapshot: {e}")))?;
    if normalized
        .settings_fingerprint
        .as_ref()
        .is_some_and(|old| old != &settings_fingerprint)
    {
        return Err(SupervisorError::Conflict(
            "settings changed since the previous preview; review a fresh plan".into(),
        ));
    }
    normalized.settings_fingerprint = Some(settings_fingerprint);
    let settings = settings::load_settings(&normalized.settings_path)
        .map_err(|error| SupervisorError::Invalid(format!("settings_load: {error}")))?;
    let (facts, observation_fingerprint, observation_problem) =
        load_provider_observations(&settings, &data_root, freshness_time_ms)?;
    if normalized
        .provider_observation_fingerprint
        .as_deref()
        .is_some_and(|old| old != observation_fingerprint)
    {
        return Err(SupervisorError::Conflict(
            "provider observations changed since preview; refresh and review the route plan".into(),
        ));
    }
    normalized.provider_observation_fingerprint = Some(observation_fingerprint.clone());
    let mut plan = Supervisor::dry_run_request_with_facts(&normalized, &settings, &facts)?;
    if let Some(problem) = &observation_problem {
        plan.warnings.push(format!(
            "Provider routing observations are unavailable: {problem}"
        ));
    }
    let resolved = routes_from_plan(&plan);
    if !normalized.resolved_routes.is_empty() && normalized.resolved_routes != resolved {
        return Err(SupervisorError::Conflict(
            "resolved provider routes changed since preview; review the updated plan".into(),
        ));
    }
    normalized.resolved_routes = resolved;
    plan = Supervisor::dry_run_request_with_facts(&normalized, &settings, &facts)?;
    if let Some(problem) = &observation_problem {
        plan.warnings.push(format!(
            "Provider routing observations are unavailable: {problem}"
        ));
    }
    let commitments = derive_route_commitments(&normalized, &facts)?;
    if !normalized.route_commitments.is_empty() && normalized.route_commitments != commitments {
        return Err(SupervisorError::Conflict(
            "provider route semantics changed since preview; review the updated plan".into(),
        ));
    }
    normalized.route_commitments = commitments;
    let request_json =
        serde_json::to_vec(&normalized).map_err(|e| SupervisorError::Invalid(e.to_string()))?;
    let plan_json =
        serde_json::to_vec(&plan).map_err(|e| SupervisorError::Invalid(e.to_string()))?;
    let identity = serde_json::to_vec(&(worker.to_string_lossy(), metadata.len(), modified))
        .map_err(|e| SupervisorError::Invalid(e.to_string()))?;
    let mut bytes = Vec::with_capacity(
        request_json.len()
            + plan_json.len()
            + settings_bytes.len()
            + identity.len()
            + observation_fingerprint.len()
            + 32,
    );
    for part in [
        &request_json[..],
        &plan_json[..],
        &settings_bytes[..],
        &identity[..],
        observation_fingerprint.as_bytes(),
    ] {
        bytes.extend_from_slice(&(part.len() as u64).to_le_bytes());
        bytes.extend_from_slice(part);
    }
    for (issue_id, path) in &normalized.issue_sources {
        let source = read_issue_source(path)?;
        bytes.extend_from_slice(&(issue_id.len() as u64).to_le_bytes());
        bytes.extend_from_slice(issue_id.as_bytes());
        bytes.extend_from_slice(&(source.len() as u64).to_le_bytes());
        bytes.extend_from_slice(&source);
    }
    let fingerprint = crate::process::git_object_hash(&bytes)
        .map_err(|e| SupervisorError::Io(format!("cannot hash preview binding: {e}")))?;
    Ok(BatchPreview {
        request: normalized,
        plan,
        provider_observation_fingerprint: observation_fingerprint,
        fingerprint,
    })
}

fn load_provider_observations(
    settings: &Settings,
    data_root: &Path,
    now_ms: u128,
) -> Result<(Vec<crate::routing::CandidateFacts>, String, Option<String>), SupervisorError> {
    let store = crate::provider_observations::ProviderObservationStore::open_read_only(data_root)
        .map_err(SupervisorError::Invalid)?;
    match store.snapshot(settings, now_ms) {
        Ok(snapshot) => {
            let fingerprint = snapshot.snapshot_fingerprint.clone();
            match snapshot.candidate_facts(settings, now_ms, ROUTING_OBSERVATION_MAX_AGE_MS) {
                Ok(facts) => Ok((facts, fingerprint, None)),
                Err(problem) => {
                    let fallback = unavailable_observation_fingerprint(data_root, &problem)?;
                    Ok((Vec::new(), fallback, Some(problem)))
                }
            }
        }
        Err(problem) => {
            let fingerprint = unavailable_observation_fingerprint(data_root, &problem)?;
            Ok((Vec::new(), fingerprint, Some(problem)))
        }
    }
}

fn all_selected_routes_refresh_consented(request: &BatchRequest, settings: &Settings) -> bool {
    let mut found = false;
    for job in &request.jobs {
        for task in &job.plan.tasks {
            if task.kind == TaskKind::EpicAggregation {
                continue;
            }
            found = true;
            let key = paid_policy_key(&job.issue_id, &task.task_id);
            let (Some(route), Some(commitment)) = (
                request.resolved_routes.get(&key),
                request.route_commitments.get(&key),
            ) else {
                return false;
            };
            if route.provider_id != commitment.provider_id || route.model_id != commitment.model_id
            {
                return false;
            }
            let Some(profile) = settings
                .providers
                .iter()
                .find(|profile| profile.id == route.provider_id)
            else {
                return false;
            };
            let permitted = match &request.dispatch_refresh_consent {
                crate::provider_observations::ModelFreeRefreshConsent::Disabled => false,
                crate::provider_observations::ModelFreeRefreshConsent::Providers { kinds } => {
                    kinds.contains(&profile.kind)
                }
            };
            if !permitted {
                return false;
            }
        }
    }
    found
}

fn routes_from_plan(plan: &DryRunPlan) -> BTreeMap<String, crate::routing::CandidateRoute> {
    let mut resolved = BTreeMap::new();
    for issue in &plan.issues {
        for task in &issue.tasks {
            if task.eligible {
                if let (Some(provider_id), Some(model_id)) = (&task.provider_id, &task.model_id) {
                    resolved.insert(
                        paid_policy_key(&issue.issue_id, &task.task_id),
                        crate::routing::CandidateRoute {
                            provider_id: provider_id.clone(),
                            model_id: model_id.clone(),
                        },
                    );
                }
            }
        }
    }
    resolved
}

fn unavailable_observation_fingerprint(
    data_root: &Path,
    reason: &str,
) -> Result<String, SupervisorError> {
    let path = data_root.join("provider-observations.json");
    let mut material = reason.as_bytes().to_vec();
    match fs::metadata(&path) {
        Ok(metadata) if metadata.len() <= 4 * 1024 * 1024 => {
            let bytes = fs::read(&path).map_err(|error| {
                SupervisorError::Io(format!(
                    "cannot read provider observation snapshot: {error}"
                ))
            })?;
            material.extend_from_slice(&(bytes.len() as u64).to_le_bytes());
            material.extend_from_slice(&bytes);
        }
        Ok(metadata) => {
            material.extend_from_slice(&metadata.len().to_le_bytes());
            let modified = metadata
                .modified()
                .ok()
                .and_then(|value| value.duration_since(UNIX_EPOCH).ok())
                .map_or(0, |value| value.as_nanos());
            material.extend_from_slice(&modified.to_le_bytes());
            material.extend_from_slice(b"oversized-provider-observation-snapshot");
        }
        Err(_) => material.extend_from_slice(b"provider-observation-snapshot-missing"),
    }
    crate::process::git_object_hash(&material).map_err(|error| {
        SupervisorError::Io(format!("cannot fingerprint provider observations: {error}"))
    })
}

fn validate_issue_sources(
    request: &BatchRequest,
    require_coverage: bool,
) -> Result<(), SupervisorError> {
    if require_coverage && request.issue_sources.len() != request.jobs.len() {
        return Err(SupervisorError::Invalid(
            "production batch request must bind every issue to its authoritative Markdown file"
                .into(),
        ));
    }
    for job in &request.jobs {
        let Some(path) = request.issue_sources.get(&job.issue_id) else {
            if require_coverage {
                return Err(SupervisorError::Invalid(format!(
                    "{} has no authoritative issue source path",
                    job.issue_id
                )));
            }
            continue;
        };
        let canonical = path.canonicalize().map_err(|e| {
            SupervisorError::Invalid(format!("{} source path cannot be read: {e}", job.issue_id))
        })?;
        if !canonical.starts_with(&request.project_path) || canonical != *path {
            return Err(SupervisorError::Invalid(format!(
                "{} source path must be canonical and inside the selected project",
                job.issue_id
            )));
        }
        let bytes = read_issue_source(&canonical)?;
        let raw = std::str::from_utf8(&bytes).map_err(|_| {
            SupervisorError::Invalid(format!("{} Markdown is not UTF-8", job.issue_id))
        })?;
        if crate::issues::revision(raw) != job.source_fingerprint || raw != job.issue_snapshot {
            return Err(SupervisorError::Conflict(format!(
                "{} issue Markdown changed after its snapshot was created; refresh and preview again",
                job.issue_id
            )));
        }
    }
    Ok(())
}

fn validate_job_source_binding(
    request: &BatchRequest,
    issue_id: &str,
    fingerprint: &str,
) -> Result<(), SupervisorError> {
    let entry = request
        .jobs
        .iter()
        .find(|job| job.issue_id == issue_id)
        .ok_or_else(|| SupervisorError::NotFound(issue_id.into()))?;
    let path = request.issue_sources.get(issue_id).ok_or_else(|| {
        SupervisorError::Invalid(format!("{issue_id} has no authoritative Markdown path"))
    })?;
    let canonical = path
        .canonicalize()
        .map_err(|e| SupervisorError::Io(format!("cannot resolve {issue_id} source: {e}")))?;
    if canonical != *path || !canonical.starts_with(&request.project_path) {
        return Err(SupervisorError::Invalid(format!(
            "{issue_id} source path is not canonical inside the selected project"
        )));
    }
    let bytes = read_issue_source(&canonical)?;
    let raw = std::str::from_utf8(&bytes)
        .map_err(|_| SupervisorError::Invalid(format!("{issue_id} Markdown is not UTF-8")))?;
    if crate::issues::revision(raw) != fingerprint || raw != entry.issue_snapshot {
        return Err(SupervisorError::Conflict(format!(
            "{issue_id} authoritative Markdown changed after preview"
        )));
    }
    Ok(())
}

fn authoritative_issue_revision(
    request: &BatchRequest,
    issue_id: &str,
) -> Result<String, SupervisorError> {
    let path = request.issue_sources.get(issue_id).ok_or_else(|| {
        SupervisorError::Invalid(format!("{issue_id} has no authoritative issue source"))
    })?;
    let bytes = read_issue_source(path)?;
    let raw = std::str::from_utf8(&bytes)
        .map_err(|_| SupervisorError::Invalid("issue Markdown is not UTF-8".into()))?;
    Ok(crate::issues::revision(raw))
}

fn refresh_batch_sources(
    request: &BatchRequest,
    jobs: &JobsStore,
    ids: &[i64],
) -> Result<(), SupervisorError> {
    if request.issue_sources.is_empty() {
        return Ok(());
    }
    for id in ids {
        let job = jobs.get(*id)?;
        if matches!(
            job.state,
            JobState::Leased | JobState::Running | JobState::ReconciliationRequired
        ) {
            continue;
        }
        if validate_job_source_binding(request, &job.issue_id, &job.source_fingerprint).is_ok() {
            continue;
        }
        let mut actual = authoritative_issue_revision(request, &job.issue_id)
            .unwrap_or_else(|_| format!("{}-missing", job.source_fingerprint));
        if actual == job.source_fingerprint {
            actual.push_str("-snapshot-mismatch");
        }
        let _ = jobs.check_source_fingerprint(*id, &actual, now_ms())?;
    }
    Ok(())
}

fn read_issue_source(path: &Path) -> Result<Vec<u8>, SupervisorError> {
    let metadata = fs::metadata(path)
        .map_err(|e| SupervisorError::Io(format!("cannot inspect issue Markdown: {e}")))?;
    if !metadata.is_file() || metadata.len() > 2 * 1024 * 1024 {
        return Err(SupervisorError::Invalid(
            "issue Markdown must be a regular file no larger than 2 MiB".into(),
        ));
    }
    fs::read(path).map_err(|e| SupervisorError::Io(format!("cannot read issue Markdown: {e}")))
}

fn read_settings_fingerprint(path: &Path) -> Result<String, SupervisorError> {
    let metadata = fs::metadata(path)
        .map_err(|e| SupervisorError::Io(format!("cannot inspect settings snapshot: {e}")))?;
    if !metadata.is_file() || metadata.len() > 2 * 1024 * 1024 {
        return Err(SupervisorError::Invalid(
            "settings snapshot must be a regular file no larger than 2 MiB".into(),
        ));
    }
    let bytes = fs::read(path)
        .map_err(|e| SupervisorError::Io(format!("cannot read settings snapshot: {e}")))?;
    crate::process::git_object_hash(&bytes)
        .map_err(|e| SupervisorError::Io(format!("cannot hash settings snapshot: {e}")))
}

fn validate_settings_fingerprint(request: &BatchRequest) -> Result<(), SupervisorError> {
    let Some(expected) = request.settings_fingerprint.as_deref() else {
        return Ok(());
    };
    let actual = read_settings_fingerprint(&request.settings_path)?;
    if actual != expected {
        return Err(SupervisorError::Conflict(
            "settings content no longer matches the admitted snapshot".into(),
        ));
    }
    Ok(())
}
fn absolute_from(cwd: &Path, path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd.join(path)
    }
}
fn job_budget(plan: &JobPlan) -> BudgetPolicy {
    let task_tokens = plan
        .tasks
        .iter()
        .map(|task| task.limits.max_tokens.unwrap_or(0))
        .sum::<u64>()
        .max(1);
    let task_cost = plan
        .tasks
        .iter()
        .map(|task| task.limits.max_cost_micros.unwrap_or(0))
        .sum::<u64>();
    let task_seconds = plan
        .tasks
        .iter()
        .map(|task| task.limits.max_seconds.unwrap_or(0))
        .sum::<u64>()
        .max(1);
    BudgetPolicy {
        max_tokens: plan.limits.max_tokens.unwrap_or(task_tokens),
        max_cost_micros: plan.limits.max_cost_micros.unwrap_or(task_cost),
        max_seconds: plan.limits.max_seconds.unwrap_or(task_seconds),
        max_concurrency: 1,
    }
}

pub const ROUTING_OBSERVATION_MAX_AGE_MS: u128 = 5 * 60 * 1000;
const MAX_TASK_ROUTE_CANDIDATES: usize = 32;

struct TaskRouteResult {
    provider_id: Option<String>,
    model_id: Option<String>,
    eligible: bool,
    provenance: Option<Value>,
    reasons: Vec<String>,
    economy_admission: Option<crate::economy::VerifiedEconomyAdmission>,
}

fn derive_route_commitments(
    batch: &BatchRequest,
    facts: &[crate::routing::CandidateFacts],
) -> Result<BTreeMap<String, RouteSemanticCommitment>, SupervisorError> {
    let mut commitments = BTreeMap::new();
    for job in &batch.jobs {
        for task in &job.plan.tasks {
            if task.kind == TaskKind::EpicAggregation {
                continue;
            }
            let key = paid_policy_key(&job.issue_id, &task.task_id);
            let Some(route) = batch.resolved_routes.get(&key) else {
                continue;
            };
            let fact = facts
                .iter()
                .find(|fact| {
                    fact.provider_id == route.provider_id && fact.model_id == route.model_id
                })
                .ok_or_else(|| {
                    SupervisorError::Invalid(format!(
                        "selected route {} / {} has no provider observation facts",
                        route.provider_id, route.model_id
                    ))
                })?;
            let (_, runtime_identity_fingerprint) =
                fact.runtime_id.split_once(':').ok_or_else(|| {
                    SupervisorError::Invalid("provider runtime identity is malformed".into())
                })?;
            let requirements = task.route_requirements.clone().unwrap_or_default();
            let minimum_context_tokens = requirements
                .complexity
                .map(crate::routing::Complexity::context_floor)
                .into_iter()
                .chain(requirements.context_tokens)
                .chain(requirements.max_input_tokens)
                .max();
            let mut authorized_candidates = if let (Some(provider_id), Some(model_id)) =
                (task.provider_id.as_ref(), task.model_id.as_ref())
            {
                vec![crate::routing::CandidateRoute {
                    provider_id: provider_id.clone(),
                    model_id: model_id.clone(),
                }]
            } else if !requirements.allowed_candidates.is_empty() {
                requirements.allowed_candidates.clone()
            } else {
                vec![route.clone()]
            };
            authorized_candidates.sort_by(|left, right| {
                left.provider_id
                    .cmp(&right.provider_id)
                    .then(left.model_id.cmp(&right.model_id))
            });
            let paid_quote = batch
                .paid_policies
                .get(&key)
                .filter(|policy| {
                    policy.provider_id == route.provider_id && policy.model_id == route.model_id
                })
                .map(|policy| CommittedPaidQuote {
                    input_price_micros_per_1m: policy.pricing.input_price_micros_per_1m,
                    output_price_micros_per_1m: policy.pricing.output_price_micros_per_1m,
                    request_fee_micros: policy.pricing.request_fee_micros,
                    source: policy.pricing.source.clone(),
                    model_catalog_pricing: policy.model_catalog_pricing.clone(),
                    max_calls: policy.max_calls,
                    max_cost_micros: policy.max_cost_micros,
                    context_tokens: policy.model_context_window_tokens,
                    max_output_tokens_per_call: policy.max_output_tokens_per_call,
                    endpoint_pin: policy.endpoint_pin.clone(),
                });
            commitments.insert(
                key,
                RouteSemanticCommitment {
                    provider_id: route.provider_id.clone(),
                    model_id: route.model_id.clone(),
                    runtime_identity_fingerprint: runtime_identity_fingerprint.to_owned(),
                    local_only: fact.local_only,
                    billing: fact.billing,
                    requires_tools: requirements.require_tools.unwrap_or(true),
                    minimum_context_tokens,
                    authorized_candidates,
                    paid_quote,
                    economy_endpoint_id: fact
                        .economy_admission
                        .as_ref()
                        .map(|admission| admission.endpoint_id().to_owned()),
                    economy_evidence_digest: fact
                        .economy_admission
                        .as_ref()
                        .map(|admission| admission.evidence_digest().to_owned()),
                },
            );
        }
    }
    Ok(commitments)
}

fn check_fresh_route_commitment(
    request: &BatchRequest,
    settings: &Settings,
    job: &SubmitJob,
    task: &PlannedTask,
    fresh: &crate::provider_observations::FreshCandidateObservation,
) -> Result<(), String> {
    validate_settings_fingerprint(request)
        .map_err(|error| format!("provider settings changed during dispatch refresh: {error}"))?;
    if !fresh.facts.evidence_ready {
        return Err(format!(
            "refreshed provider evidence is incomplete: {}",
            fresh.facts.evidence_reasons.join("; ")
        ));
    }
    let key = paid_policy_key(&job.issue_id, &task.task_id);
    let commitment = request
        .route_commitments
        .get(&key)
        .ok_or_else(|| "dispatch route has no immutable semantic commitment".to_owned())?;
    let Some((_, expected_fingerprint)) = fresh.facts.runtime_id.split_once(':') else {
        return Err("refreshed runtime identity is malformed".into());
    };
    if fresh.runtime_identity_fingerprint != commitment.runtime_identity_fingerprint
        || expected_fingerprint != commitment.runtime_identity_fingerprint
    {
        return Err("provider runtime identity changed since preview".into());
    }
    if fresh.facts.provider_id != commitment.provider_id
        || fresh.facts.model_id != commitment.model_id
    {
        return Err("provider refresh returned a different route".into());
    }
    commitment.validate_economy_pin(&fresh.facts)?;
    if fresh.facts.local_only != commitment.local_only || fresh.facts.billing != commitment.billing
    {
        return Err("provider privacy or billing classification changed since preview".into());
    }
    if commitment.requires_tools && fresh.facts.supports_tools != Some(true) {
        return Err("required tool capability is no longer confirmed".into());
    }
    if commitment.minimum_context_tokens.is_some_and(|minimum| {
        fresh
            .facts
            .context_tokens
            .is_none_or(|actual| actual < minimum)
    }) {
        return Err("required context capacity is no longer confirmed".into());
    }
    let committed_endpoint_pin = commitment
        .paid_quote
        .as_ref()
        .and_then(|quote| quote.endpoint_pin.as_ref());
    let current_endpoint_pin = request
        .paid_policies
        .get(&key)
        .and_then(|policy| policy.endpoint_pin.as_ref());
    if check_paid_endpoint_pin_commitment(committed_endpoint_pin, current_endpoint_pin).is_err() {
        return Err("paid endpoint pin changed since preview".into());
    }
    check_fresh_paid_quote(commitment, &fresh.facts)?;
    if !commitment.authorized_candidates.iter().any(|candidate| {
        candidate.provider_id == commitment.provider_id && candidate.model_id == commitment.model_id
    }) {
        return Err("selected route is outside the immutable candidate authorization".into());
    }
    let planned_job = request
        .jobs
        .iter()
        .find(|entry| entry.issue_id == job.issue_id)
        .ok_or_else(|| "reviewed issue is missing".to_owned())?;
    let refreshed_pricing_time = fresh
        .facts
        .pricing_verified_at_ms
        .and_then(|value| u64::try_from(value).ok());
    let routed = route_task(
        request,
        settings,
        planned_job,
        task,
        std::slice::from_ref(&fresh.facts),
        refreshed_pricing_time,
    )
    .map_err(|error| error.to_string())?;
    if !routed.eligible
        || routed.provider_id.as_deref() != Some(commitment.provider_id.as_str())
        || routed.model_id.as_deref() != Some(commitment.model_id.as_str())
    {
        return Err(format!(
            "refreshed route no longer satisfies task requirements: {}",
            routed.reasons.join("; ")
        ));
    }
    Ok(())
}

fn check_paid_endpoint_pin_commitment(
    committed: Option<&crate::paid_gateway::PaidEndpointPin>,
    current: Option<&crate::paid_gateway::PaidEndpointPin>,
) -> Result<(), ()> {
    if committed == current {
        Ok(())
    } else {
        Err(())
    }
}

fn check_fresh_paid_quote(
    commitment: &RouteSemanticCommitment,
    facts: &crate::routing::CandidateFacts,
) -> Result<(), String> {
    if let Some(quote) = &commitment.paid_quote {
        let model_quote_changed = if let Some(catalog) = quote.model_catalog_pricing.as_ref() {
            facts.input_micros_per_million_tokens != Some(catalog.input_price_micros_per_1m)
                || facts.output_micros_per_million_tokens != Some(catalog.output_price_micros_per_1m)
                || facts.pricing_source.as_deref() != Some(catalog.source.as_str())
        } else {
            facts.input_micros_per_million_tokens != Some(quote.input_price_micros_per_1m)
                || facts.output_micros_per_million_tokens != Some(quote.output_price_micros_per_1m)
                || facts.pricing_source.as_deref() != Some(quote.source.as_str())
        };
        let endpoint_quote_changed = quote.model_catalog_pricing.is_some()
            && quote.endpoint_pin.as_ref().is_some_and(|pin| {
            facts.paid_endpoint_quote.as_ref().is_none_or(|endpoint| {
                endpoint.endpoint_id != pin.endpoint_id
                    || endpoint.canonical_model_id != pin.canonical_model_id
                    || endpoint.input_micros_per_million_tokens != quote.input_price_micros_per_1m
                    || endpoint.output_micros_per_million_tokens != quote.output_price_micros_per_1m
                    || endpoint.request_fee_micros != quote.request_fee_micros
                    || endpoint.source != quote.source
            })
            });
        if facts.billing != crate::routing::BillingPolicy::Paid || model_quote_changed || endpoint_quote_changed {
            return Err(
                "verified paid quote or source changed since preview; new approval is required"
                    .into(),
            );
        }
    } else if facts.billing == crate::routing::BillingPolicy::Paid {
        return Err(
            "refreshed route is paid but the reviewed commitment contains no paid quote".into(),
        );
    }
    Ok(())
}

fn paid_candidate_quote_matches(
    facts: &crate::routing::CandidateFacts,
    policy: &crate::paid_gateway::PaidModelPolicy,
    requirements: &crate::routing::TaskRoutingRequirements,
) -> bool {
    if let Some(pin) = policy.endpoint_pin.as_ref() {
        if policy.model_catalog_pricing.is_none() {
            return facts.input_micros_per_million_tokens
                == Some(policy.pricing.input_price_micros_per_1m)
                && facts.output_micros_per_million_tokens
                    == Some(policy.pricing.output_price_micros_per_1m)
                && facts.pricing_source.as_deref() == Some(policy.pricing.source.as_str());
        }
        let Some(endpoint) = facts.paid_endpoint_quote.as_ref() else {
            return false;
        };
        let required_input = requirements.max_input_tokens.unwrap_or(16_384);
        let required_output = requirements
            .max_output_tokens
            .unwrap_or(policy.max_output_tokens_per_call);
        let Some(total_required) = required_input.checked_add(required_output) else {
            return false;
        };
        let catalog_matches = if let Some(catalog) = policy.model_catalog_pricing.as_ref() {
            facts.input_micros_per_million_tokens == Some(catalog.input_price_micros_per_1m)
                && facts.output_micros_per_million_tokens == Some(catalog.output_price_micros_per_1m)
                && facts.pricing_source.as_deref() == Some(catalog.source.as_str())
        } else { false };
        catalog_matches
            && endpoint.endpoint_id == pin.endpoint_id
            && endpoint.canonical_model_id == pin.canonical_model_id
            && endpoint.input_micros_per_million_tokens == policy.pricing.input_price_micros_per_1m
            && endpoint.output_micros_per_million_tokens == policy.pricing.output_price_micros_per_1m
            && endpoint.request_fee_micros == policy.pricing.request_fee_micros
            && endpoint.source == policy.pricing.source
            && endpoint.observed_at_ms <= now_ms().max(0) as u128
            && (now_ms().max(0) as u128).saturating_sub(endpoint.observed_at_ms)
                <= 24 * 60 * 60 * 1000
            && required_input <= endpoint.context_tokens
            && total_required <= endpoint.context_tokens
            && endpoint.max_prompt_tokens.is_none_or(|cap| required_input <= cap)
            && required_output <= endpoint.max_completion_tokens
    } else {
        policy.model_catalog_pricing.is_none()
            && facts.input_micros_per_million_tokens
                == Some(policy.pricing.input_price_micros_per_1m)
            && facts.output_micros_per_million_tokens
                == Some(policy.pricing.output_price_micros_per_1m)
            && facts.pricing_source.as_deref() == Some(policy.pricing.source.as_str())
    }
}

fn apply_paid_endpoint_quote_for_routing(
    facts: &mut crate::routing::CandidateFacts,
    policy: &crate::paid_gateway::PaidModelPolicy,
    requirements: &crate::routing::TaskRoutingRequirements,
) -> bool {
    if policy.endpoint_pin.is_none()
        || policy.model_catalog_pricing.is_none()
        || !paid_candidate_quote_matches(facts, policy, requirements)
    {
        return false;
    }
    let Some(endpoint) = facts.paid_endpoint_quote.as_ref() else {
        return false;
    };
    facts.input_micros_per_million_tokens = Some(endpoint.input_micros_per_million_tokens);
    facts.output_micros_per_million_tokens = Some(endpoint.output_micros_per_million_tokens);
    facts.pricing_source = Some(endpoint.source.clone());
    facts.pricing_verified_at_ms = Some(endpoint.observed_at_ms);
    true
}

fn route_task(
    batch: &BatchRequest,
    settings: &Settings,
    job: &SubmitJob,
    task: &PlannedTask,
    facts: &[crate::routing::CandidateFacts],
    refreshed_pricing_time_ms: Option<u64>,
) -> Result<TaskRouteResult, SupervisorError> {
    if task.kind == TaskKind::EpicAggregation {
        return Ok(TaskRouteResult {
            provider_id: None,
            model_id: None,
            eligible: true,
            provenance: None,
            reasons: Vec::new(),
            economy_admission: None,
        });
    }

    let requirements = task.route_requirements.clone().unwrap_or_default();
    if requirements.max_input_tokens == Some(0)
        || requirements.max_output_tokens == Some(0)
        || requirements.context_tokens == Some(0)
        || requirements.desired_completion_minutes == Some(0)
        || requirements.allowed_candidates.len() > MAX_TASK_ROUTE_CANDIDATES
    {
        return Err(SupervisorError::Invalid(format!(
            "task {} has invalid or oversized routing requirements",
            task.task_id
        )));
    }
    if task.provider_id.is_some() != task.model_id.is_some() {
        return Err(SupervisorError::Invalid(format!(
            "task {} explicit routing must include both provider and model",
            task.task_id
        )));
    }

    let explicit = task.provider_id.as_deref().zip(task.model_id.as_deref());
    let candidates = if let Some((provider_id, model_id)) = explicit {
        if !requirements.allowed_candidates.is_empty()
            && !requirements.allowed_candidates.iter().any(|candidate| {
                candidate.provider_id == provider_id && candidate.model_id == model_id
            })
        {
            return Err(SupervisorError::Invalid(format!(
                "task {} explicit route is outside its authorized candidate list",
                task.task_id
            )));
        }
        vec![crate::routing::CandidateRoute {
            provider_id: provider_id.to_owned(),
            model_id: model_id.to_owned(),
        }]
    } else if !requirements.allowed_candidates.is_empty() {
        requirements.allowed_candidates.clone()
    } else {
        let profile = selected_profile(settings, task)?;
        let model_id = selected_model(task, settings, profile)?;
        vec![crate::routing::CandidateRoute {
            provider_id: profile.id.clone(),
            model_id,
        }]
    };
    let mut seen = HashSet::new();
    if candidates.iter().any(|candidate| {
        candidate.provider_id.trim().is_empty()
            || candidate.model_id.trim().is_empty()
            || candidate.provider_id.len() > 96
            || candidate.model_id.len() > 200
            || candidate.provider_id.chars().any(char::is_control)
            || candidate.model_id.chars().any(char::is_control)
            || !seen.insert((candidate.provider_id.clone(), candidate.model_id.clone()))
    }) {
        return Err(SupervisorError::Invalid(format!(
            "task {} has invalid or duplicate candidate routes",
            task.task_id
        )));
    }

    let now = now_ms().max(0) as u128;
    let mut routing_candidates = Vec::with_capacity(candidates.len());
    let mut candidate_facts = Vec::with_capacity(candidates.len());
    let paid_policy = batch
        .paid_policies
        .get(&paid_policy_key(&job.issue_id, &task.task_id));
    for candidate in &candidates {
        let profile = settings
            .providers
            .iter()
            .find(|profile| profile.id == candidate.provider_id);
        let model = profile.and_then(|profile| {
            profile
                .models
                .iter()
                .find(|model| model.id == candidate.model_id)
        });
        let (
            provider_name,
            telemetry_provider_id,
            model_name,
            reasoning_efforts,
            quality_rating,
            initial_local,
        ) = match (profile, model) {
            (Some(profile), Some(model)) => (
                profile.name.clone(),
                profile.kind.telemetry_id().to_owned(),
                model.name.clone(),
                model.reasoning_efforts.clone(),
                model.quality_rating,
                profile.kind == ProviderKind::Ollama,
            ),
            _ => (
                candidate.provider_id.clone(),
                "unknown".to_owned(),
                candidate.model_id.clone(),
                Vec::new(),
                None,
                false,
            ),
        };
        routing_candidates.push(crate::routing::Candidate {
            provider_id: candidate.provider_id.clone(),
            telemetry_provider_id,
            provider_name,
            model_id: candidate.model_id.clone(),
            model_name,
            is_local: initial_local,
            context_tokens: model.and_then(|model| model.context_window_tokens),
            supports_tools: model.and_then(|model| model.supports_tools),
            reasoning_efforts,
            quality_rating,
            latency_ms: None,
            latency_collected_at_ms: None,
            economy_evidence_ready: facts.iter().any(|fact| {
                fact.provider_id == candidate.provider_id
                    && fact.model_id == candidate.model_id
                    && fact.economy_admission.is_some()
            }),
        });

        let Some(mut observed) = facts
            .iter()
            .find(|fact| {
                fact.provider_id == candidate.provider_id && fact.model_id == candidate.model_id
            })
            .cloned()
        else {
            continue;
        };
        let mut endpoint_quote_authorized = false;
        if let Some(profile) = profile {
            if profile.kind == ProviderKind::Custom {
                observed.evidence_ready = false;
                observed.evidence_reasons.push(
                    "Custom provider execution is not admitted for unattended batches.".into(),
                );
            }
            if task.kind == TaskKind::Reviewer && profile.kind != ProviderKind::Codex {
                observed.evidence_ready = false;
                observed.evidence_reasons.push(
                    "Independent reviewer tasks must use the read-only Codex runtime.".into(),
                );
            }
            if profile.kind == ProviderKind::OpenRouter {
                let economy_ok = requirements.pace.unwrap_or(crate::routing::Pace::Balanced)
                    == crate::routing::Pace::Economy
                    && observed
                        .economy_admission
                        .as_ref()
                        .is_some_and(|admission| {
                            admission.provider_profile_id() == profile.id
                                && admission.model_id() == candidate.model_id
                                && task
                                    .limits
                                    .max_tokens
                                    .is_some_and(|value| value >= admission.max_tokens())
                                && task
                                    .limits
                                    .max_seconds
                                    .is_some_and(|value| value >= admission.max_seconds())
                                && requirements
                                    .max_input_tokens
                                    .is_none_or(|value| value <= admission.context_tokens())
                                && requirements.max_output_tokens.is_none_or(|value| {
                                    value <= admission.max_output_tokens_per_call()
                                })
                                && admission
                                    .validate_pinned_policy(
                                        &profile.id,
                                        &admission.gateway_policy(),
                                        now as u64,
                                    )
                                    .is_ok()
                        });
                let policy_ok = economy_ok
                    || paid_policy.is_some_and(|policy| {
                        validate_paid_model_policy(
                            batch,
                            &job.issue_id,
                            task,
                            profile,
                            &candidate.model_id,
                            policy,
                            refreshed_pricing_time_ms,
                        )
                        .is_ok()
                            && policy.provider_id == candidate.provider_id
                            && policy.model_id == candidate.model_id
                            && policy.model_context_window_tokens
                                <= observed.context_tokens.unwrap_or(0)
                            && requirements
                                .max_input_tokens
                                .is_none_or(|tokens| tokens <= policy.model_context_window_tokens)
                            && requirements
                                .max_output_tokens
                                .is_none_or(|tokens| tokens <= policy.max_output_tokens_per_call)
                            && paid_candidate_quote_matches(&observed, policy, &requirements)
                    });
                endpoint_quote_authorized = policy_ok
                    && paid_policy.is_some_and(|policy| {
                        apply_paid_endpoint_quote_for_routing(&mut observed, policy, &requirements)
                    });
                if !policy_ok {
                    observed.evidence_ready = false;
                    observed.evidence_reasons.push("OpenRouter requires a matching explicit paid policy and current verified catalog pricing for this exact model.".into());
                }
            }
        }
        if !endpoint_quote_authorized {
            // Endpoint metadata may support a preview only when the exact
            // split-quote pin was admitted. Unpinned and legacy Paid routing
            // continue to estimate from the ordinary catalog quote.
            observed.paid_endpoint_quote = None;
        }
        candidate_facts.push(observed);
    }

    if candidates.iter().any(|candidate| {
        settings
            .providers
            .iter()
            .find(|profile| profile.id == candidate.provider_id)
            .is_some_and(|profile| profile.kind != ProviderKind::OpenRouter)
    }) && paid_policy.is_some()
    {
        let policy = paid_policy.expect("checked above");
        if !candidates.iter().any(|candidate| {
            candidate.provider_id == policy.provider_id && candidate.model_id == policy.model_id
        }) {
            return Err(SupervisorError::Invalid(format!(
                "paid policy for {} does not match an authorized task candidate",
                task.task_id
            )));
        }
    }

    let request = crate::routing::Request {
        complexity: requirements
            .complexity
            .unwrap_or(crate::routing::Complexity::Standard),
        pace: requirements.pace.unwrap_or(crate::routing::Pace::Balanced),
        require_context_evidence: requirements.complexity.is_some()
            || requirements.context_tokens.is_some()
            || requirements.max_input_tokens.is_some(),
        minimum_context_tokens: requirements
            .context_tokens
            .into_iter()
            .chain(requirements.max_input_tokens)
            .max(),
        require_tools: requirements.require_tools.unwrap_or(true),
        local_only: requirements.local_only.unwrap_or(false),
        desired_completion_minutes: requirements.desired_completion_minutes,
        manual_provider_id: task.provider_id.clone(),
        manual_model_id: task.model_id.clone(),
        candidates: routing_candidates,
        usage_signals: Vec::new(),
        now_ms: now,
    };
    let selected_economy = candidate_facts
        .iter()
        .find(|fact| {
            fact.economy_admission.is_some()
                && candidates.iter().any(|candidate| {
                    candidate.provider_id == fact.provider_id && candidate.model_id == fact.model_id
                })
        })
        .and_then(|fact| fact.economy_admission.as_ref());
    let max_input_tokens = requirements
        .max_input_tokens
        .or_else(|| {
            paid_policy
                .filter(|policy| {
                    candidates.iter().any(|candidate| {
                        policy.provider_id == candidate.provider_id
                            && policy.model_id == candidate.model_id
                    })
                })
                .map(|policy| policy.model_context_window_tokens)
        })
        .or_else(|| selected_economy.map(|admission| admission.context_tokens()));
    let max_output_tokens = requirements
        .max_output_tokens
        .or_else(|| {
            paid_policy
                .filter(|policy| {
                    candidates.iter().any(|candidate| {
                        policy.provider_id == candidate.provider_id
                            && policy.model_id == candidate.model_id
                    })
                })
                .map(|policy| policy.max_output_tokens_per_call)
        })
        .or_else(|| selected_economy.map(|admission| admission.max_output_tokens_per_call()));
    let assignment = crate::routing::AssignmentRequest {
        task_id: format!("{}/{}", job.issue_id, task.task_id),
        request,
        required_runtime_id: None,
        max_input_tokens,
        max_output_tokens,
    };
    let decision = crate::routing::route_job(crate::routing::JobRoutingRequest {
        implementer: assignment,
        reviewer: None,
        candidate_facts: candidate_facts.clone(),
    })
    .implementer;
    let mut selected = decision
        .decision
        .selected_provider_id
        .clone()
        .zip(decision.decision.selected_model_id.clone());
    let route_key = paid_policy_key(&job.issue_id, &task.task_id);
    let recorded_route = batch.resolved_routes.get(&route_key);
    let mut route_mismatch = false;
    if let Some(recorded) = recorded_route {
        route_mismatch = selected.as_ref().is_none_or(|(provider, model)| {
            provider != &recorded.provider_id || model != &recorded.model_id
        });
        if route_mismatch {
            selected = None;
        }
    }
    let eligible = selected.is_some();
    let reasons = if route_mismatch {
        vec!["The provider observation snapshot selected a different route than the reviewed preview; refresh and review the new plan.".to_owned()]
    } else if eligible {
        Vec::new()
    } else {
        decision
            .decision
            .candidates
            .iter()
            .filter(|candidate| !candidate.eligible)
            .flat_map(|candidate| candidate.reasons.clone())
            .take(16)
            .collect::<Vec<_>>()
    };
    let (provider_id, model_id) = selected.map_or((None, None), |(provider, model)| {
        (Some(provider), Some(model))
    });
    let economy_admission =
        provider_id
            .as_ref()
            .zip(model_id.as_ref())
            .and_then(|(provider, model)| {
                candidate_facts
                    .iter()
                    .find(|fact| &fact.provider_id == provider && &fact.model_id == model)
                    .and_then(|fact| fact.economy_admission.clone())
            });
    let provenance = serde_json::to_value(decision).map_err(|error| {
        SupervisorError::Invalid(format!("route provenance serialization failed: {error}"))
    })?;
    Ok(TaskRouteResult {
        provider_id,
        model_id,
        eligible,
        provenance: Some(provenance),
        economy_admission,
        reasons: if eligible {
            Vec::new()
        } else if reasons.is_empty() {
            vec!["No explicitly authorized provider/model candidate is currently eligible.".into()]
        } else {
            reasons
        },
    })
}

fn selected_profile<'a>(
    settings: &'a Settings,
    task: &PlannedTask,
) -> Result<&'a ProviderProfile, SupervisorError> {
    let id = task
        .provider_id
        .as_deref()
        .or(settings.provider_selection.provider_id.as_deref())
        .ok_or_else(|| {
            SupervisorError::Invalid(format!("task {} has no selected provider", task.task_id))
        })?;
    settings
        .providers
        .iter()
        .find(|p| p.id == id)
        .ok_or_else(|| {
            SupervisorError::Invalid(format!("provider {id} is missing from current settings"))
        })
}
fn selected_model(
    task: &PlannedTask,
    settings: &Settings,
    profile: &ProviderProfile,
) -> Result<String, SupervisorError> {
    let id = task
        .model_id
        .as_deref()
        .or_else(|| settings.provider_selection.model_id.as_deref())
        .ok_or_else(|| {
            SupervisorError::Invalid(format!("task {} has no selected model", task.task_id))
        })?;
    if !profile.models.iter().any(|model| model.id == id) {
        return Err(SupervisorError::Invalid(format!(
            "model {id} is not configured for provider {}",
            profile.id
        )));
    }
    Ok(id.to_owned())
}

/// The user's reasoning-effort selection belongs to the selected
/// provider/model pair. A task routed elsewhere must not inherit it; the
/// runtime still validates an effort explicitly selected for its own model.
fn implementation_reasoning_effort(
    settings: &Settings,
    profile: &ProviderProfile,
    model_id: &str,
) -> Option<String> {
    let selection = &settings.provider_selection;
    if selection.provider_id.as_deref() == Some(profile.id.as_str())
        && selection.model_id.as_deref() == Some(model_id)
    {
        selection.reasoning_effort.clone()
    } else {
        None
    }
}

fn validate_reviewer_reasoning_efforts(
    settings: &Settings,
    request: &BatchRequest,
) -> Result<(), SupervisorError> {
    for policy in &request.verification {
        let Some(effort) = policy.reviewer.reasoning_effort.as_deref() else {
            continue;
        };
        if effort.is_empty() || effort.len() > 32 {
            return Err(SupervisorError::Invalid(format!(
                "reviewer reasoning effort for {} must contain 1 to 32 characters",
                policy.issue_id
            )));
        }
        let profile = settings
            .providers
            .iter()
            .find(|profile| profile.id == policy.reviewer.provider_id)
            .ok_or_else(|| {
                SupervisorError::Invalid(format!(
                    "reviewer provider {} is missing from current settings",
                    policy.reviewer.provider_id
                ))
            })?;
        let model = profile
            .models
            .iter()
            .find(|model| model.id == policy.reviewer.model_id)
            .ok_or_else(|| {
                SupervisorError::Invalid(format!(
                    "reviewer model {} is not configured for provider {}",
                    policy.reviewer.model_id, profile.id
                ))
            })?;
        if !model
            .reasoning_efforts
            .iter()
            .any(|supported| supported == effort)
        {
            return Err(SupervisorError::Invalid(format!(
                "reviewer reasoning effort {effort} is not advertised for provider {} model {}",
                profile.id, model.id
            )));
        }
    }
    Ok(())
}

fn same_file_path(a: &Path, b: &Path) -> bool {
    let left = a.canonicalize().unwrap_or_else(|_| a.to_path_buf());
    let right = b.canonicalize().unwrap_or_else(|_| b.to_path_buf());
    left.to_string_lossy()
        .eq_ignore_ascii_case(&right.to_string_lossy())
}

fn read_budget_status(path: &Path, batch_id: &str) -> Result<BudgetStatusView, SupervisorError> {
    if !path.exists() {
        return Ok(BudgetStatusView::default());
    }
    let connection = Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
        .map_err(store_error)?;
    let policy=connection.query_row("SELECT max_tokens,max_cost_micros,max_seconds,tokens_used,cost_used,seconds_used FROM budget_policies WHERE scope_type='batch' AND scope_id=?1",[batch_id],|r|Ok((r.get::<_,i64>(0)?,r.get::<_,i64>(1)?,r.get::<_,i64>(2)?,r.get::<_,i64>(3)?,r.get::<_,i64>(4)?,r.get::<_,i64>(5)?))).optional().map_err(store_error)?;
    let counts=connection.query_row("SELECT COUNT(*),COALESCE(SUM(state='reserved'),0),COALESCE(SUM(state='dispatched'),0),COALESCE(SUM(state='unresolved'),0) FROM budget_reservations WHERE batch_id=?1",[batch_id],|r|Ok((r.get::<_,i64>(0)?,r.get::<_,i64>(1)?,r.get::<_,i64>(2)?,r.get::<_,i64>(3)?))).map_err(store_error)?;
    let mut status = BudgetStatusView {
        reservations: counts.0 as u64,
        reserved: counts.1 as u64,
        dispatched: counts.2 as u64,
        unresolved: counts.3 as u64,
        ..BudgetStatusView::default()
    };
    if let Some(p) = policy {
        status.max_tokens = p.0 as u64;
        status.max_cost_micros = p.1 as u64;
        status.max_seconds = p.2 as u64;
        status.tokens_used = p.3 as u64;
        status.cost_used_micros = p.4 as u64;
        status.seconds_used = p.5 as u64;
    }
    Ok(status)
}
fn parse_batch_state(value: &str) -> Result<BatchState, SupervisorError> {
    serde_json::from_str(&format!("\"{}\"", value))
        .map_err(|e| SupervisorError::Store(e.to_string()))
}
fn store_error(error: rusqlite::Error) -> SupervisorError {
    SupervisorError::Store(error.to_string())
}
fn append_batch_event(
    tx: &rusqlite::Transaction<'_>,
    batch: &str,
    now: i64,
    kind: &str,
    payload: Value,
) -> Result<(), SupervisorError> {
    let seq: i64 = tx
        .query_row(
            "SELECT COALESCE(MAX(seq),0)+1 FROM batch_events WHERE batch_id=?1",
            [batch],
            |r| r.get(0),
        )
        .map_err(store_error)?;
    tx.execute(
        "INSERT INTO batch_events(batch_id,seq,at_ms,kind,payload_json) VALUES(?1,?2,?3,?4,?5)",
        params![batch, seq, now, kind, payload.to_string()],
    )
    .map_err(store_error)?;
    Ok(())
}

#[cfg(test)]
mod supervisor_tests {
    use super::*;
    use crate::jobs::ResourceLimits;
    use tempfile::tempdir;

    #[test]
    fn reviewer_refresh_needs_input_marker_survives_typed_display_wrapping() {
        let unavailable = SupervisorError::Verification(
            "needs_input: reviewer metadata refresh failed before dispatch: exact selected model is unavailable".into(),
        );
        assert!(unavailable
            .to_string()
            .starts_with("verification error: needs_input:"));
        assert!(verification_error_requires_input(&unavailable));
        assert!(!verification_error_requires_input(
            &SupervisorError::Verification("check command failed".into(),)
        ));
        assert!(!verification_error_requires_input(
            &SupervisorError::Runtime("needs_input: unrelated runtime text".into(),)
        ));
    }

    #[test]
    fn implementation_effort_stays_bound_to_selected_provider_and_model() {
        use crate::providers::{ModelProfile, ProviderKind};

        let mut settings = Settings::default();
        settings.provider_selection.provider_id = Some("codex-local".into());
        settings.provider_selection.model_id = Some("gpt-6-luna".into());
        settings.provider_selection.reasoning_effort = Some("medium".into());
        let codex = ProviderProfile {
            id: "codex-local".into(),
            kind: ProviderKind::Codex,
            name: "Codex".into(),
            executable: "codex".into(),
            endpoint: None,
            secret_ref: None,
            models: vec![ModelProfile {
                id: "gpt-6-luna".into(),
                name: "GPT-6 Luna".into(),
                reasoning_efforts: vec!["medium".into()],
                context_window_tokens: None,
                supports_tools: None,
                quality_rating: None,
            }],
        };
        let qwen = ProviderProfile {
            id: "openRouter-373ec4c1".into(),
            kind: ProviderKind::OpenRouter,
            name: "OpenRouter".into(),
            executable: "aider".into(),
            endpoint: None,
            secret_ref: None,
            models: vec![ModelProfile {
                id: "qwen/qwen3-coder-next".into(),
                name: "Qwen 3 Coder Next".into(),
                reasoning_efforts: Vec::new(),
                context_window_tokens: None,
                supports_tools: Some(true),
                quality_rating: None,
            }],
        };

        assert_eq!(
            implementation_reasoning_effort(&settings, &qwen, "qwen/qwen3-coder-next"),
            None,
            "Codex's medium setting must not be sent to the Qwen runtime"
        );
        let same_provider_different_model = ProviderProfile {
            id: "codex-local".into(),
            ..qwen.clone()
        };
        assert_eq!(
            implementation_reasoning_effort(
                &settings,
                &same_provider_different_model,
                "qwen/qwen3-coder-next"
            ),
            None,
            "matching only the selected provider must not forward its effort"
        );
        let same_model_different_provider = ProviderProfile {
            id: "openRouter-373ec4c1".into(),
            ..codex.clone()
        };
        assert_eq!(
            implementation_reasoning_effort(
                &settings,
                &same_model_different_provider,
                "gpt-6-luna"
            ),
            None,
            "matching only the selected model must not forward its effort"
        );
        assert_eq!(
            implementation_reasoning_effort(&settings, &codex, "gpt-6-luna"),
            Some("medium".into()),
            "the selected Codex Luna effort must remain intact"
        );

        settings.provider_selection.reasoning_effort = Some("unsupported".into());
        assert_eq!(
            implementation_reasoning_effort(&settings, &codex, "gpt-6-luna"),
            Some("unsupported".into()),
            "an invalid effort on the selected identity must reach the existing runtime validator"
        );
    }

    #[test]
    fn implementation_prompt_includes_the_exact_stored_issue_snapshot() {
        let issue_snapshot = "---\nid: QWENPILOT-S001\n---\n\nNormalize names by collapsing internal whitespace and preserving case.";
        let task = PlannedTask {
            task_id: "small_bugfix-implementation".into(),
            parent_task_id: None,
            kind: TaskKind::Implementation,
            files: vec!["name_utils.py".into()],
            acceptance: vec!["Preserve case while normalizing whitespace.".into()],
            limits: ResourceLimits {
                max_attempts: 1,
                max_tokens: Some(10_000),
                max_cost_micros: Some(33_333),
                max_seconds: Some(120),
                max_concurrency: 1,
            },
            provider_id: Some("openRouter-fixture".into()),
            model_id: Some("qwen/qwen3-coder-next".into()),
            route_requirements: None,
        };
        let plan = JobPlan {
            tasks: vec![task.clone()],
            limits: task.limits.clone(),
        };

        let prompt = implementation_prompt(
            "QWENPILOT-S001",
            "stored-revision-fingerprint",
            issue_snapshot,
            &task,
            &plan,
        );

        assert!(prompt.contains(&format!(
            "Authoritative issue requirements (stored snapshot):\n{issue_snapshot}\nTask: {}",
            task.task_id
        )));
        assert!(prompt.contains("Acceptance criteria:\n- Preserve case while normalizing whitespace."));
    }

    #[test]
    fn authoritative_issue_snapshot_preserves_opaque_caller_fingerprint() {
        let raw = "---\nid: QWENPILOT-S001\n---\n\nNormalize names by collapsing internal whitespace.";
        let fingerprint = "opaque-caller-computed-fingerprint".to_owned();
        let limits = ResourceLimits {
            max_attempts: 1,
            max_tokens: Some(10_000),
            max_cost_micros: Some(33_333),
            max_seconds: Some(120),
            max_concurrency: 1,
        };
        let submitted = SubmitJob {
            idempotency_key: "qwenpilot-s001-v1".into(),
            issue_id: "QWENPILOT-S001".into(),
            issue_type: "task".into(),
            parent_issue_ids: Vec::new(),
            source_fingerprint: fingerprint.clone(),
            issue_snapshot: raw.into(),
            plan: JobPlan {
                tasks: Vec::new(),
                limits,
            },
            dependencies: Vec::new(),
        };
        let job = Job {
            id: 1,
            issue_id: submitted.issue_id.clone(),
            source_fingerprint: fingerprint,
            state: JobState::Queued,
            completion_gate: "pending".into(),
            created_at_ms: 1,
            updated_at_ms: 1,
            attempt_count: 0,
            max_attempts: 1,
            cancel_requested: false,
            lease_owner: None,
            lease_until_ms: None,
            fencing_token: 0,
        };

        assert_eq!(
            authoritative_issue_snapshot(&[submitted.clone()], &job).unwrap(),
            raw
        );
        let mut wrong_binding = submitted;
        wrong_binding.source_fingerprint = "different-caller-fingerprint".into();
        assert!(authoritative_issue_snapshot(&[wrong_binding], &job).is_err());
    }

    #[test]
    fn verify_one_envelope_forwards_reviewer_only_base_criterion_and_deduplicates() {
        let policy = VerificationPolicy {
            issue_id: "TEST-L001".into(),
            checks: vec![CheckSpec {
                id: "output-check".into(),
                program: PathBuf::from("fixture-check"),
                args: vec![],
                cwd: PathBuf::from("."),
                timeout_ms: 5_000,
                output_limit_bytes: 1024,
                adapter: None,
                contract: None,
            }],
            reviewer: ReviewerIdentity {
                task_id: "review".into(),
                provider_id: "codex-fixture".into(),
                model_id: "model-fixture".into(),
                reasoning_effort: None,
            },
            acceptance_criteria: vec!["policy criterion".into(), "shared criterion".into()],
        };
        let reviewer_task = PlannedTask {
            task_id: "review".into(),
            parent_task_id: Some("implement".into()),
            kind: TaskKind::Reviewer,
            files: vec!["src/view.rs".into()],
            acceptance: vec![
                "Activating any non-interactive area of a card opens that game's existing cockpit."
                    .into(),
                "shared criterion".into(),
                "Review full scoped diff against BASE 16617007f6a240a5e1c182579caaf1661ad7429d."
                    .into(),
            ],
            limits: ResourceLimits {
                max_attempts: 1,
                max_tokens: Some(100),
                max_cost_micros: Some(0),
                max_seconds: Some(20),
                max_concurrency: 1,
            },
            provider_id: Some("codex-fixture".into()),
            model_id: Some("model-fixture".into()),
            route_requirements: None,
        };
        let envelope = build_verification_envelope(
            &policy,
            &reviewer_task,
            "issue-fingerprint".into(),
            "implement".into(),
            crate::verification::SourceSnapshot {
                head: "explicit-base-commit".into(),
                fingerprint: "dirty-snapshot-fingerprint".into(),
            },
            3_000,
        )
        .unwrap();

        assert_eq!(
            envelope.acceptance_criteria,
            vec![
                "policy criterion".to_owned(),
                "shared criterion".to_owned(),
                "Activating any non-interactive area of a card opens that game's existing cockpit."
                    .to_owned(),
                "Review full scoped diff against BASE 16617007f6a240a5e1c182579caaf1661ad7429d."
                    .to_owned(),
            ]
        );
        assert_eq!(envelope.checks[0].timeout_ms, 3_000);

        let mut contracted = policy.clone();
        contracted.checks[0].contract = Some(crate::contracts::ContractSpec {
            criteria: vec!["policy criterion".into()],
            specimens: vec![crate::contracts::ContractSpecimen {
                id: "sample".into(),
                input: json!(1),
                expected: json!(2),
            }],
        });
        let mixed = build_verification_envelope(
            &contracted,
            &reviewer_task,
            "issue".into(),
            "implement".into(),
            envelope.code_snapshot.clone(),
            1000,
        )
        .unwrap();
        assert_eq!(mixed.checks[0].timeout_ms, 1000);
        assert_eq!(mixed.checks[0].contract, contracted.checks[0].contract);
        assert!(crate::verification::requires_independent_review(&mixed).unwrap());
        contracted.checks[0].contract.as_mut().unwrap().criteria =
            mixed.acceptance_criteria.clone();
        let deterministic = build_verification_envelope(
            &contracted,
            &reviewer_task,
            "issue".into(),
            "implement".into(),
            envelope.code_snapshot.clone(),
            1000,
        )
        .unwrap();
        assert!(!crate::verification::requires_independent_review(&deterministic).unwrap());
    }

    #[test]
    fn reviewer_acceptance_handoff_fails_closed_at_runtime_limits() {
        let policy_criteria = (0..MAX_REVIEW_ACCEPTANCE_CRITERIA)
            .map(|index| format!("criterion-{index}"))
            .collect::<Vec<_>>();
        assert_eq!(
            merge_review_acceptance_criteria(&policy_criteria, &policy_criteria)
                .unwrap()
                .len(),
            MAX_REVIEW_ACCEPTANCE_CRITERIA,
            "duplicate criteria must not consume the bounded unique-item allowance"
        );
        let mut too_many = policy_criteria.clone();
        too_many.push("new unique criterion".into());
        assert!(matches!(
            merge_review_acceptance_criteria(&policy_criteria, &too_many),
            Err(SupervisorError::Verification(message)) if message.starts_with("needs_input:")
        ));
        assert!(matches!(
            merge_review_acceptance_criteria(&[], &["x".repeat(MAX_REVIEW_ACCEPTANCE_ITEM_BYTES + 1)]),
            Err(SupervisorError::Verification(message)) if message.starts_with("needs_input:")
        ));
        assert!(matches!(
            merge_review_acceptance_criteria(&[], &["é".repeat(MAX_REVIEW_ACCEPTANCE_ITEM_BYTES / 2 + 1)]),
            Err(SupervisorError::Verification(message)) if message.starts_with("needs_input:")
        ));
        assert!(matches!(
            merge_review_acceptance_criteria(&[], &["bad\ncriterion".into()]),
            Err(SupervisorError::Verification(message)) if message.starts_with("needs_input:")
        ));
    }

    #[test]
    fn verification_needs_input_marker_surfaces_as_public_waiting_state() {
        let (folder, mut request, _worker, settings_path) = preview_fixture();
        let project = request.project_path.clone();
        git_fixture(&project, &["init"]);
        git_fixture(&project, &["config", "user.name", "Runner Fixture"]);
        git_fixture(
            &project,
            &["config", "user.email", "runner-fixture@example.invalid"],
        );
        fs::write(project.join("README.md"), "fixture base\n").unwrap();
        git_fixture(&project, &["add", "."]);
        git_fixture(&project, &["commit", "-m", "fixture base"]);
        request.verified_base_commit = git_fixture(&project, &["rev-parse", "HEAD"]);

        let state_root = folder.path().join("verification-needs-input-state");
        let isolation =
            RepoIsolation::new(&project, &request.managed_worktree_root, &state_root).unwrap();
        isolation
            .prepare_workspace(PrepareRequest {
                issue_id: request.jobs[0].issue_id.clone(),
                issue_snapshot_fingerprint: request.jobs[0].source_fingerprint.clone(),
                verified_base_commit: request.verified_base_commit.clone(),
                predecessor_commits: vec![],
                build_root: request.build_root.clone(),
            })
            .unwrap();

        let supervisor = Supervisor::open(state_root.clone()).unwrap();
        seed_fixture_provider_observations(&supervisor.paths().root, &settings_path);
        let worker = folder.path().join("worker.exe");
        let preview = preview_batch_request(&supervisor.paths().root, &request, &worker).unwrap();
        let admitted = supervisor.start(&preview.request, now_ms()).unwrap();
        let jobs = JobsStore::open(supervisor.paths().jobs.clone()).unwrap();
        let claim = jobs
            .claim_ids(
                "verification-needs-input-worker",
                &admitted.job_ids,
                now_ms(),
                MAX_LEASE_MS,
            )
            .unwrap()
            .unwrap();
        jobs.record_attempt_outcome(
            claim.job.id,
            "verification-needs-input-worker",
            claim.fencing_token,
            AttemptOutcome::ProposedComplete {
                summary: "synthetic proposal".into(),
            },
            now_ms(),
        )
        .unwrap();

        // Keep settings parseable, but change their bytes after admission. This
        // deterministically produces Verification("needs_input: …") before
        // checks, review, reservation, or provider execution.
        use std::io::Write;
        fs::OpenOptions::new()
            .append(true)
            .open(&settings_path)
            .unwrap()
            .write_all(b"\n")
            .unwrap();

        assert!(supervisor
            .verify_ready_jobs(
                &request.batch_id,
                "verification-needs-input-worker",
                &preview.request
            )
            .unwrap());
        let status = supervisor.status_view(&request.batch_id).unwrap();
        let public_job = &status.jobs[0];
        assert_eq!(public_job.state, JobState::NeedsInput);
        assert_eq!(public_job.completion_gate, "manual_acceptance");
        let manual_acceptance = jobs
            .get_checkpoint(claim.job.id, "manual_acceptance_required")
            .unwrap()
            .unwrap();
        assert!(manual_acceptance["items"][0]
            .as_str()
            .unwrap()
            .contains("needs_input:"));

        assert!(jobs
            .get_checkpoint(claim.job.id, "verification_route_refresh")
            .unwrap()
            .is_none());
        assert!(jobs
            .get_checkpoint(claim.job.id, "verification_check_0")
            .unwrap()
            .is_none());
        assert!(jobs
            .get_checkpoint(claim.job.id, "verification_review")
            .unwrap()
            .is_none());
        assert!(jobs
            .get_checkpoint(claim.job.id, "verification_repair_round_1")
            .unwrap()
            .is_none());
    }

    #[test]
    fn workspace_metadata_lock_deadline_becomes_needs_input_without_repair_or_retry() {
        let (folder, mut request, _worker, settings_path) = preview_fixture();
        let project = request.project_path.clone();
        git_fixture(&project, &["init"]);
        git_fixture(&project, &["config", "user.name", "Runner Fixture"]);
        git_fixture(
            &project,
            &["config", "user.email", "runner-fixture@example.invalid"],
        );
        fs::write(project.join("README.md"), "fixture base\n").unwrap();
        git_fixture(&project, &["add", "."]);
        git_fixture(&project, &["commit", "-m", "fixture base"]);
        request.verified_base_commit = git_fixture(&project, &["rev-parse", "HEAD"]);

        let state_root = folder.path().join("recovery-lock-needs-input-state");
        let isolation =
            RepoIsolation::new(&project, &request.managed_worktree_root, &state_root).unwrap();
        isolation
            .prepare_workspace(PrepareRequest {
                issue_id: request.jobs[0].issue_id.clone(),
                issue_snapshot_fingerprint: request.jobs[0].source_fingerprint.clone(),
                verified_base_commit: request.verified_base_commit.clone(),
                predecessor_commits: vec![],
                build_root: request.build_root.clone(),
            })
            .unwrap();

        let supervisor = Supervisor::open(state_root.clone()).unwrap();
        seed_fixture_provider_observations(&supervisor.paths().root, &settings_path);
        let worker = folder.path().join("worker.exe");
        let preview = preview_batch_request(&supervisor.paths().root, &request, &worker).unwrap();
        let admitted = supervisor.start(&preview.request, now_ms()).unwrap();
        let jobs = JobsStore::open(supervisor.paths().jobs.clone()).unwrap();
        let claim = jobs
            .claim_ids(
                "recovery-lock-needs-input-worker",
                &admitted.job_ids,
                now_ms(),
                MAX_LEASE_MS,
            )
            .unwrap()
            .unwrap();
        // Mirror production ordering and checkpoint shape: the source snapshot
        // is durably recorded while the worker lease is still active, before its
        // ProposedComplete outcome transitions the job to verification.
        let proposal_worktree = isolation
            .recover_workspace("TEST-L001")
            .unwrap()
            .worktree_path;
        let proposal_source = source_snapshot(&proposal_worktree).unwrap();
        let proposal_head = proposal_source.head.clone();
        let proposal_fingerprint = proposal_source.fingerprint.clone();
        jobs.record_checkpoint(
            claim.job.id,
            "recovery-lock-needs-input-worker",
            claim.fencing_token,
            "implementation_proposed",
            &json!({"source":proposal_source,"summary":"synthetic proposal"}),
            now_ms(),
        )
        .unwrap();
        jobs.record_attempt_outcome(
            claim.job.id,
            "recovery-lock-needs-input-worker",
            claim.fencing_token,
            AttemptOutcome::ProposedComplete {
                summary: "synthetic proposal".into(),
            },
            now_ms(),
        )
        .unwrap();

        // Start verification with a bounded five-second window, then hold the
        // repository metadata lock through the deadline and NeedsInput save.
        let batch_deadline = now_ms() + 5_000;
        Connection::open(&supervisor.paths().batches)
            .unwrap()
            .execute(
                "UPDATE batches SET deadline_ms=?2 WHERE batch_id=?1",
                params![request.batch_id, batch_deadline],
            )
            .unwrap();
        let held = isolation.lock_repository_metadata().unwrap();
        let (wait_tx, wait_rx) = std::sync::mpsc::sync_channel(1);
        let supervisor_for_verifier = supervisor.clone();
        let request_for_verifier = preview.request.clone();
        let batch_id = request.batch_id.clone();
        let verifier = thread::spawn(move || {
            crate::isolation::set_metadata_lock_wait_observer(move || {
                let _ = wait_tx.try_send(());
            });
            supervisor_for_verifier.verify_ready_jobs(
                &batch_id,
                "recovery-lock-needs-input-worker",
                &request_for_verifier,
            )
        });
        wait_rx.recv_timeout(Duration::from_secs(10)).unwrap();
        let remaining_ms = batch_deadline.saturating_sub(now_ms()).max(0) as u64;
        thread::sleep(Duration::from_millis(remaining_ms.saturating_add(100)));
        // Keep the lock held through NeedsInput persistence. That path must use
        // the already-durable proposal identity instead of retrying recovery.
        assert!(verifier.join().unwrap().unwrap());
        drop(held);
        let job = jobs.get(claim.job.id).unwrap();
        assert_eq!(job.state, JobState::NeedsInput);
        assert_eq!(
            job.attempt_count, 1,
            "infrastructure lock contention must not retry implementation"
        );
        let manual = jobs
            .get_checkpoint(job.id, "manual_acceptance_required")
            .unwrap()
            .unwrap();
        assert!(manual["items"][0]
            .as_str()
            .unwrap()
            .contains("needs_input: workspace ownership recovery"));
        assert_eq!(manual["codeHead"].as_str(), Some(proposal_head.as_str()));
        assert_eq!(
            manual["codeFingerprint"].as_str(),
            Some(proposal_fingerprint.as_str())
        );
        assert!(jobs
            .get_checkpoint(job.id, "verification_check_0")
            .unwrap()
            .is_none());
        assert!(jobs
            .get_checkpoint(job.id, "verification_review")
            .unwrap()
            .is_none());
        assert!(jobs
            .get_checkpoint(job.id, "verification_repair_round_1")
            .unwrap()
            .is_none());
    }

    #[test]
    fn execution_paid_policy_uses_fresh_timestamp_without_mutating_reviewed_quote() {
        let now = now_ms().max(0) as u64;
        let reviewed = crate::paid_gateway::PaidModelPolicy {
            mode: crate::paid_gateway::GatewayMode::Paid,
            provider_id: "openrouter-fixture".into(),
            model_id: "paid-model".into(),
            runtime_id: "aider".into(),
            model_context_window_tokens: 16_384,
            max_output_tokens_per_call: 512,
            max_calls: 2,
            max_cost_micros: 5_000,
            pricing: crate::paid_gateway::PaidPricingCeiling {
                input_price_micros_per_1m: 10_000,
                output_price_micros_per_1m: 20_000,
                request_fee_micros: 0,
                source: "verified-fixture-catalog".into(),
                observed_at_ms: now.saturating_sub(24 * 60 * 60 * 1000 + 1),
            },
            endpoint_pin: None,
            model_catalog_pricing: None,
        };
        let original = reviewed.clone();

        let execution = execution_paid_policy(&reviewed, Some(now), None).unwrap();
        assert_eq!(reviewed, original, "stored approval must remain immutable");
        assert_eq!(execution.pricing.observed_at_ms, now);
        assert_eq!(execution.provider_id, reviewed.provider_id);
        assert_eq!(execution.model_id, reviewed.model_id);
        assert_eq!(execution.runtime_id, reviewed.runtime_id);
        assert_eq!(
            execution.model_context_window_tokens,
            reviewed.model_context_window_tokens
        );
        assert_eq!(
            execution.max_output_tokens_per_call,
            reviewed.max_output_tokens_per_call
        );
        assert_eq!(execution.max_calls, reviewed.max_calls);
        assert_eq!(execution.max_cost_micros, reviewed.max_cost_micros);
        assert_eq!(
            execution.pricing,
            crate::paid_gateway::PaidPricingCeiling {
                observed_at_ms: now,
                ..reviewed.pricing.clone()
            }
        );

        let changed_facts = crate::routing::CandidateFacts {
            provider_id: reviewed.provider_id.clone(),
            model_id: reviewed.model_id.clone(),
            runtime_id: "openrouter:fixture".into(),
            runtime_installed: true,
            authenticated: true,
            healthy: true,
            health_checked_at_ms: Some(now as u128),
            model_available: true,
            local_only: false,
            billing: crate::routing::BillingPolicy::Paid,
            input_micros_per_million_tokens: Some(10_001),
            output_micros_per_million_tokens: Some(20_000),
            pricing_source: Some("verified-fixture-catalog".into()),
            pricing_verified_at_ms: Some(now as u128),
            economy_policy: None,
            economy_admission: None,
            paid_endpoint_quote: None,
            context_tokens: Some(16_384),
            supports_tools: Some(true),
            evidence_ready: true,
            evidence_reasons: Vec::new(),
        };
        let quote = CommittedPaidQuote {
            input_price_micros_per_1m: reviewed.pricing.input_price_micros_per_1m,
            output_price_micros_per_1m: reviewed.pricing.output_price_micros_per_1m,
            request_fee_micros: reviewed.pricing.request_fee_micros,
            source: reviewed.pricing.source.clone(),
            context_tokens: reviewed.model_context_window_tokens,
            max_output_tokens_per_call: reviewed.max_output_tokens_per_call,
            max_cost_micros: reviewed.max_cost_micros,
            max_calls: reviewed.max_calls,
            endpoint_pin: None,
            model_catalog_pricing: None,
        };
        let commitment = RouteSemanticCommitment {
            provider_id: reviewed.provider_id.clone(),
            model_id: reviewed.model_id.clone(),
            runtime_identity_fingerprint: "fixture".into(),
            local_only: false,
            billing: crate::routing::BillingPolicy::Paid,
            requires_tools: true,
            minimum_context_tokens: Some(16_384),
            authorized_candidates: vec![crate::routing::CandidateRoute {
                provider_id: reviewed.provider_id.clone(),
                model_id: reviewed.model_id.clone(),
            }],
            paid_quote: Some(quote),
            economy_endpoint_id: None,
            economy_evidence_digest: None,
        };
        assert!(check_fresh_paid_quote(&commitment, &changed_facts)
            .unwrap_err()
            .contains("quote or source changed"));
    }

    #[test]
    fn paid_endpoint_pin_is_serialized_in_route_commitment_and_drift_is_rejected() {
        let pin = crate::paid_gateway::PaidEndpointPin {
            endpoint_id: "parasail/bf16".into(),
            canonical_model_id: "qwen/qwen3-coder-next-2025-02-03".into(),
            requires_tools: true,
        };
        let quote = CommittedPaidQuote {
            input_price_micros_per_1m: 120_000,
            output_price_micros_per_1m: 800_000,
            request_fee_micros: 0,
            source: "authenticated-openrouter-model-and-endpoint-metadata".into(),
            model_catalog_pricing: None,
            max_calls: 3,
            max_cost_micros: 33_333,
            context_tokens: 16_384,
            max_output_tokens_per_call: 8_192,
            endpoint_pin: Some(pin.clone()),
        };
        let encoded = serde_json::to_value(&quote).unwrap();
        assert_eq!(encoded["endpointPin"]["endpointId"], "parasail/bf16");
        let restored: CommittedPaidQuote = serde_json::from_value(encoded).unwrap();
        assert_eq!(restored.endpoint_pin.as_ref(), Some(&pin));
        assert!(check_paid_endpoint_pin_commitment(Some(&pin), Some(&pin)).is_ok());
        let changed = crate::paid_gateway::PaidEndpointPin {
            endpoint_id: "parasail/other".into(),
            ..pin.clone()
        };
        assert!(check_paid_endpoint_pin_commitment(Some(&pin), Some(&changed)).is_err());
        assert!(check_paid_endpoint_pin_commitment(Some(&pin), None).is_err());
    }

    #[test]
    fn split_paid_quote_requires_both_catalog_and_endpoint_identity_but_preserves_legacy_pin() {
        let now = now_ms().max(0) as u64;
        let pin = crate::paid_gateway::PaidEndpointPin {
            endpoint_id: "parasail/fp8".into(),
            canonical_model_id: "deepseek/deepseek-v4.1-flash-20260910".into(),
            requires_tools: true,
        };
        let mut policy = crate::paid_gateway::PaidModelPolicy {
            mode: crate::paid_gateway::GatewayMode::Paid,
            provider_id: "openrouter-profile".into(),
            model_id: "deepseek/deepseek-v4.1-flash".into(),
            runtime_id: "aider".into(),
            model_context_window_tokens: 262_144,
            max_output_tokens_per_call: 8_192,
            max_calls: 2,
            max_cost_micros: 98_352,
            pricing: crate::paid_gateway::PaidPricingCeiling {
                input_price_micros_per_1m: 300_000,
                output_price_micros_per_1m: 1_200_000,
                request_fee_micros: 0,
                source: "OpenRouter authenticated model endpoint".into(),
                observed_at_ms: now,
            },
            model_catalog_pricing: Some(crate::paid_gateway::PaidModelCatalogPricing {
                input_price_micros_per_1m: 2_490,
                output_price_micros_per_1m: 305_000,
                source: "OpenRouter authenticated user model catalog".into(),
                observed_at_ms: now,
            }),
            endpoint_pin: Some(pin.clone()),
        };
        let mut fact = crate::routing::CandidateFacts {
            provider_id: policy.provider_id.clone(),
            model_id: policy.model_id.clone(),
            runtime_id: "aider:fixture".into(),
            runtime_installed: true,
            authenticated: true,
            healthy: true,
            health_checked_at_ms: Some(now as u128),
            model_available: true,
            local_only: false,
            billing: crate::routing::BillingPolicy::Paid,
            input_micros_per_million_tokens: Some(2_490),
            output_micros_per_million_tokens: Some(305_000),
            pricing_source: Some("OpenRouter authenticated user model catalog".into()),
            pricing_verified_at_ms: Some(now as u128),
            economy_policy: None,
            economy_admission: None,
            paid_endpoint_quote: Some(crate::routing::PaidEndpointQuoteObservation {
                canonical_model_id: pin.canonical_model_id.clone(),
                endpoint_id: pin.endpoint_id.clone(),
                input_micros_per_million_tokens: 300_000,
                output_micros_per_million_tokens: 1_200_000,
                request_fee_micros: 0,
                context_tokens: 1_048_576,
                max_prompt_tokens: None,
                max_completion_tokens: 943_718,
                source: "OpenRouter authenticated model endpoint".into(),
                observed_at_ms: now as u128,
            }),
            context_tokens: Some(1_048_576),
            supports_tools: Some(true),
            evidence_ready: true,
            evidence_reasons: Vec::new(),
        };
        let requirements = crate::routing::TaskRoutingRequirements {
            complexity: None,
            pace: None,
            require_tools: Some(true),
            local_only: None,
            context_tokens: None,
            desired_completion_minutes: None,
            max_input_tokens: Some(16_384),
            max_output_tokens: Some(8_192),
            allowed_candidates: Vec::new(),
        };
        assert!(paid_candidate_quote_matches(&fact, &policy, &requirements));
        let mut routed_fact = fact.clone();
        assert!(apply_paid_endpoint_quote_for_routing(
            &mut routed_fact,
            &policy,
            &requirements
        ));
        assert_eq!(routed_fact.input_micros_per_million_tokens, Some(300_000));
        assert_eq!(routed_fact.output_micros_per_million_tokens, Some(1_200_000));
        fact.input_micros_per_million_tokens = Some(2_491);
        assert!(!paid_candidate_quote_matches(&fact, &policy, &requirements));
        fact.input_micros_per_million_tokens = Some(2_490);
        fact.paid_endpoint_quote.as_mut().unwrap().output_micros_per_million_tokens += 1;
        assert!(!paid_candidate_quote_matches(&fact, &policy, &requirements));
        fact.paid_endpoint_quote.as_mut().unwrap().output_micros_per_million_tokens = 1_200_000;
        fact.paid_endpoint_quote.as_mut().unwrap().endpoint_id = "parasail/other".into();
        assert!(!paid_candidate_quote_matches(&fact, &policy, &requirements));
        fact.paid_endpoint_quote.as_mut().unwrap().endpoint_id = pin.endpoint_id.clone();
        fact.paid_endpoint_quote.as_mut().unwrap().observed_at_ms =
            u128::from(now.saturating_sub(24 * 60 * 60 * 1000 + 1));
        assert!(!paid_candidate_quote_matches(&fact, &policy, &requirements));

        policy.model_catalog_pricing = None;
        policy.pricing.input_price_micros_per_1m = 2_490;
        policy.pricing.output_price_micros_per_1m = 305_000;
        policy.pricing.source = "OpenRouter authenticated user model catalog".into();
        fact.paid_endpoint_quote = None;
        assert!(paid_candidate_quote_matches(&fact, &policy, &requirements));
        assert!(!apply_paid_endpoint_quote_for_routing(
            &mut fact,
            &policy,
            &requirements
        ));
        assert_eq!(fact.input_micros_per_million_tokens, Some(2_490));
    }

    fn budget_request(id: &str) -> ReservationRequest {
        ReservationRequest {
            reservation_id: id.into(),
            batch_id: "capacity-batch".into(),
            job_id: "capacity-job".into(),
            task_id: "implementation".into(),
            provider_id: "fixture".into(),
            model_id: "fixture-model".into(),
            runtime_id: "fixture-runtime".into(),
            max_tokens: 10,
            max_cost_micros: Some(0),
            max_seconds: 2,
            is_paid: false,
            pricing_source: None,
            pricing_verified_at_ms: None,
        }
    }

    #[test]
    fn free_policy_usage_settles_zero_cost_only_for_a_verified_zero_cost_reservation() {
        let folder = tempdir().unwrap();
        let budgets = BudgetStore::open(folder.path().join("free-usage.sqlite3")).unwrap();
        configure_test_budget(&budgets, 1_000);
        let now = now_ms().max(0) as u64;
        let mut request = budget_request("free-settled");
        request.provider_id = "openrouter-fixture".into();
        request.model_id = "vendor/test-model".into();
        request.runtime_id = "aider".into();
        request.max_tokens = 300;
        request.max_cost_micros = Some(0);
        request.max_seconds = 20;
        request.pricing_source =
            Some("authenticated-openrouter-model-and-zdr-endpoint-metadata".into());
        request.pricing_verified_at_ms = Some(now);
        budgets.reserve(&request, now).unwrap();
        budgets
            .mark_dispatched(&request.reservation_id, now + 1)
            .unwrap();
        let pricing_source = "authenticated-openrouter-model-and-zdr-endpoint-metadata";
        budgets
            .reserve_free_call(&crate::budgets::FreeCallRequest {
                parent_reservation_id: request.reservation_id.clone(),
                call_id: "free-call-settled".into(),
                provider_id: request.provider_id.clone(),
                model_id: request.model_id.clone(),
                runtime_id: request.runtime_id.clone(),
                max_tokens: 100,
                max_calls: 3,
                pricing_source: pricing_source.into(),
                pricing_verified_at_ms: now,
                now_ms: now + 1,
            })
            .unwrap();
        let canonical_model = "vendor/test-model-verified";
        let test_evaluation = crate::economy::CodingEvaluationEvidence {
            model_id: request.model_id.clone(),
            runtime_id: "aider".into(),
            runtime_version: "0.86.2".into(),
            evaluated_at_ms: now,
            fixture_set_id: "cfg-test-settlement".into(),
            fixture_set_sha256: "a".repeat(64),
            result_sha256: "b".repeat(64),
            cases: 3,
            passed: 3,
            independent_review: true,
        };
        let model = crate::providers::OpenRouterModel {
            id: request.model_id.clone(),
            canonical_slug: Some(canonical_model.into()),
            name: "fixture model".into(),
            context_length: Some(65_536),
            prompt_price: Some("0".into()),
            completion_price: Some("0".into()),
            input_modalities: Some(vec!["text".into()]),
            output_modalities: Some(vec!["text".into()]),
            supported_parameters: Some(vec!["max_tokens".into(), "tools".into()]),
        };
        let endpoint = crate::providers::OpenRouterEndpoint {
            model_id: request.model_id.clone(),
            tag: "vendor/endpoint".into(),
            context_length: Some(65_536),
            max_prompt_tokens: Some(32_768),
            max_completion_tokens: Some(8_192),
            prompt_price: Some("0".into()),
            completion_price: Some("0".into()),
            request_price: Some("0".into()),
            status: Some(0),
            zdr: None,
            supported_parameters: Some(vec!["max_tokens".into(), "tools".into()]),
        };
        let admission = crate::economy::with_test_admission_evidence(
            test_evaluation,
            &request.model_id,
            &endpoint.tag,
            now,
            || {
                crate::economy::admit_authenticated_candidate(
                    &request.provider_id,
                    &request.model_id,
                    "aider",
                    "0.86.2",
                    &crate::providers::OpenRouterMetadataObservation {
                        authenticated_for_execution: Some(true),
                        models: vec![model],
                        free_model_daily_requests: None,
                    },
                    &[endpoint.clone()],
                    &[(request.model_id.clone(), endpoint.tag.clone())]
                        .into_iter()
                        .collect(),
                    None,
                    None,
                    true,
                    122_880,
                    120,
                    now,
                )
                .unwrap()
            },
        );
        budgets
            .settle_free_call(
                "free-call-settled",
                Some(42),
                &format!("openrouter-response-usage|model={canonical_model}"),
            )
            .unwrap();
        let known = RuntimeUsage {
            input_tokens: None,
            output_tokens: None,
            total_tokens: Some(42),
            cost_usd: None,
            source: Some("openrouter-free-ledger-token-usage".into()),
            confidence: Some("provider-reported-token-usage".into()),
            billing: BillingStatus::ZeroCostPolicy,
        };
        settle_runtime_usage(
            &budgets,
            &request.reservation_id,
            true,
            Some(&known),
            Some(&admission),
            3,
        )
        .unwrap();
        let settled = budgets.get(&request.reservation_id).unwrap();
        assert_eq!(settled.state, crate::budgets::ReservationState::Settled);

        let mut unknown_request = request;
        unknown_request.reservation_id = "free-unknown".into();
        budgets.reserve(&unknown_request, now + 2).unwrap();
        budgets
            .mark_dispatched(&unknown_request.reservation_id, now + 3)
            .unwrap();
        budgets
            .reserve_free_call(&crate::budgets::FreeCallRequest {
                parent_reservation_id: unknown_request.reservation_id.clone(),
                call_id: "free-call-unknown".into(),
                provider_id: unknown_request.provider_id.clone(),
                model_id: unknown_request.model_id.clone(),
                runtime_id: unknown_request.runtime_id.clone(),
                max_tokens: 100,
                max_calls: 3,
                pricing_source: pricing_source.into(),
                pricing_verified_at_ms: now,
                now_ms: now + 3,
            })
            .unwrap();
        budgets
            .settle_free_call("free-call-unknown", None, "provider-usage-unknown")
            .unwrap();
        let unknown = RuntimeUsage {
            input_tokens: None,
            output_tokens: None,
            total_tokens: Some(42),
            cost_usd: None,
            source: None,
            confidence: None,
            billing: BillingStatus::Unknown,
        };
        settle_runtime_usage(
            &budgets,
            &unknown_request.reservation_id,
            true,
            Some(&unknown),
            None,
            3,
        )
        .unwrap();
        assert_eq!(
            budgets.get(&unknown_request.reservation_id).unwrap().state,
            crate::budgets::ReservationState::Unresolved
        );
    }

    fn configure_test_budget(store: &BudgetStore, max_tokens: u64) {
        let policy = BudgetPolicy {
            max_tokens,
            max_cost_micros: 100,
            max_seconds: 100,
            max_concurrency: 1,
        };
        store
            .configure("capacity-batch", "capacity-job", &policy, &policy)
            .unwrap();
    }

    #[test]
    fn capacity_wait_retries_temporary_concurrency_without_creating_extra_reservations() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("budget.sqlite3");
        let budgets = BudgetStore::open(&path).unwrap();
        configure_test_budget(&budgets, 100);
        budgets.reserve(&budget_request("holder"), 1).unwrap();

        let release_store = BudgetStore::open(path).unwrap();
        let release = thread::spawn(move || {
            thread::sleep(Duration::from_millis(180));
            release_store.release_before_dispatch("holder").unwrap();
        });
        let started = std::time::Instant::now();
        let reserved = reserve_with_capacity_wait(
            &budgets,
            &budget_request("waiter"),
            now_ms() + 3_000,
            || Ok(()),
        )
        .unwrap();
        release.join().unwrap();
        assert!(started.elapsed() >= Duration::from_millis(100));
        assert_eq!(reserved.reservation_id, "waiter");
        assert_eq!(
            budgets.get("waiter").unwrap().state,
            crate::budgets::ReservationState::Reserved
        );
        assert_eq!(
            budgets.get("holder").unwrap().state,
            crate::budgets::ReservationState::Released
        );
    }

    #[test]
    fn capacity_wait_surfaces_permanent_budget_exhaustion_as_needs_input() {
        let dir = tempdir().unwrap();
        let budgets = BudgetStore::open(dir.path().join("budget.sqlite3")).unwrap();
        configure_test_budget(&budgets, 5);
        let result = reserve_with_capacity_wait(
            &budgets,
            &budget_request("oversized"),
            now_ms() + 3_000,
            || Ok(()),
        );
        assert!(
            matches!(result, Err(SupervisorError::Verification(message)) if message.starts_with("needs_input: provider budget cannot admit"))
        );
        assert!(budgets.get_optional("oversized").unwrap().is_none());
    }

    #[test]
    fn observed_changed_scope_allows_subset_and_rejects_outside_files() {
        let allowed = vec![
            "src-tauri/src/game_view.rs".to_owned(),
            "web/theme.css".to_owned(),
        ];
        assert!(validate_observed_change_scope(
            &["src-tauri\\src\\game_view.rs".to_owned()],
            &allowed,
        )
        .is_ok());
        let error =
            validate_observed_change_scope(&["web/app.js".to_owned()], &allowed).unwrap_err();
        assert!(error.contains("outside declared task scope"));
        assert!(error.contains("web/app.js"));
    }

    #[test]
    fn verification_checkpoint_identity_reads_started_and_completed_receipts() {
        let started = json!({"state":"started","codeHead":"abc","codeFingerprint":"def"});
        assert_eq!(checkpoint_code_identity(&started), Some(("abc", "def")));
        let completed =
            json!({"state":"needs_input","receipt":{"source":{"head":"ghi","fingerprint":"jkl"}}});
        assert_eq!(checkpoint_code_identity(&completed), Some(("ghi", "jkl")));
    }

    fn preview_fixture() -> (tempfile::TempDir, BatchRequest, PathBuf, PathBuf) {
        use crate::providers::{ModelProfile, ProviderProfile};
        let folder = tempdir().unwrap();
        let project = folder.path().join("project");
        fs::create_dir(&project).unwrap();
        let issue_path = project.join("TEST-L001.md");
        let issue = "---\nid: TEST-L001\ntitle: fixture\n---\n\n## Acceptance criteria\n- [ ] output exists\n";
        fs::write(&issue_path, issue).unwrap();
        let worker = folder.path().join("worker.exe");
        fs::write(&worker, b"worker-v1").unwrap();
        let settings_path = folder.path().join("settings.json");
        let mut settings = Settings::default();
        settings.providers.push(ProviderProfile {
            id: "codex-fixture".into(),
            kind: ProviderKind::Codex,
            name: "Codex fixture".into(),
            executable: worker.to_string_lossy().into_owned(),
            endpoint: None,
            secret_ref: None,
            models: vec![ModelProfile {
                id: "model-fixture".into(),
                name: "Fixture model".into(),
                reasoning_efforts: vec![],
                context_window_tokens: Some(16_384),
                supports_tools: Some(true),
                quality_rating: None,
            }],
        });
        fs::write(&settings_path, serde_json::to_vec(&settings).unwrap()).unwrap();
        let limits = ResourceLimits {
            max_attempts: 1,
            max_tokens: Some(100),
            max_cost_micros: Some(0),
            max_seconds: Some(20),
            max_concurrency: 1,
        };
        let acceptance = vec!["output.txt exists".to_owned()];
        let task = |task_id: &str, kind: TaskKind| PlannedTask {
            task_id: task_id.into(),
            parent_task_id: (kind == TaskKind::Reviewer).then(|| "implement".into()),
            kind,
            files: vec!["output.txt".into()],
            acceptance: acceptance.clone(),
            limits: limits.clone(),
            provider_id: Some("codex-fixture".into()),
            model_id: Some("model-fixture".into()),
            route_requirements: None,
        };
        let issue_path = issue_path.canonicalize().unwrap();
        let request = BatchRequest {
            batch_id: "preview-fixture".into(),
            project_path: project.canonicalize().unwrap(),
            managed_worktree_root: folder.path().join("worktrees"),
            build_root: folder.path().join("builds"),
            settings_path: settings_path.clone(),
            settings_fingerprint: None,
            verified_base_commit: "fixture-head".into(),
            jobs: vec![SubmitJob {
                idempotency_key: "preview-fixture-TEST-L001".into(),
                issue_id: "TEST-L001".into(),
                issue_type: "task".into(),
                parent_issue_ids: vec![],
                source_fingerprint: crate::issues::revision(issue),
                issue_snapshot: issue.into(),
                dependencies: vec![],
                plan: JobPlan {
                    tasks: vec![
                        task("implement", TaskKind::Implementation),
                        task("review", TaskKind::Reviewer),
                    ],
                    limits: ResourceLimits {
                        max_attempts: 1,
                        max_tokens: Some(200),
                        max_cost_micros: Some(0),
                        max_seconds: Some(40),
                        max_concurrency: 1,
                    },
                },
            }],
            verification: vec![VerificationPolicy {
                issue_id: "TEST-L001".into(),
                checks: vec![CheckSpec {
                    id: "output-check".into(),
                    program: PathBuf::from("fixture-check"),
                    args: vec![],
                    cwd: PathBuf::from("."),
                    timeout_ms: 1000,
                    output_limit_bytes: 1024,
                    adapter: None,
                    contract: None,
                }],
                reviewer: ReviewerIdentity {
                    task_id: "review".into(),
                    provider_id: "codex-fixture".into(),
                    model_id: "model-fixture".into(),
                    reasoning_effort: None,
                },
                acceptance_criteria: acceptance,
            }],
            issue_sources: BTreeMap::from([("TEST-L001".into(), issue_path)]),
            paid_policies: BTreeMap::new(),
            resolved_routes: BTreeMap::new(),
            provider_observation_fingerprint: None,
            dispatch_refresh_consent:
                crate::provider_observations::ModelFreeRefreshConsent::Disabled,
            route_commitments: BTreeMap::new(),
            policy: BatchPolicy {
                window_seconds: 60,
                max_concurrency: 1,
                max_auth_retries: 0,
                max_network_retries: 0,
                budget: BudgetPolicy {
                    max_tokens: 1000,
                    max_cost_micros: 0,
                    max_seconds: 60,
                    max_concurrency: 1,
                },
                permitted_effects: vec![
                    PermittedEffect::ProviderExecution,
                    PermittedEffect::WorkspaceWrites,
                    PermittedEffect::LocalChecks,
                ],
            },
        };
        (folder, request, worker, settings_path)
    }

    fn seed_fixture_provider_observations(data_root: &Path, settings_path: &Path) {
        use crate::provider_observations::{
            Observation, ProviderModelObservation, ProviderObservation,
        };
        let settings = settings::load_settings(settings_path).unwrap();
        let observed_at = now_ms().max(0) as u128;
        let providers = settings
            .providers
            .iter()
            .map(|profile| {
                let identity = crate::providers::runtime_identity_fingerprint(profile).unwrap();
                ProviderObservation {
                    provider_id: profile.id.clone(),
                    kind: profile.kind,
                    runtime_identity_fingerprint: identity,
                    runtime_installed: Observation::known(
                        true,
                        "synthetic-unit-fixture",
                        observed_at,
                    ),
                    authenticated: Observation::known(true, "synthetic-unit-fixture", observed_at),
                    healthy: Observation::known(true, "synthetic-unit-fixture", observed_at),
                    health_checked_at_ms: Some(observed_at),
                    models: profile
                        .models
                        .iter()
                        .map(|model| ProviderModelObservation {
                            model_id: model.id.clone(),
                            model_available: Observation::known(
                                true,
                                "synthetic-unit-fixture",
                                observed_at,
                            ),
                            context_tokens: Observation::known(
                                32_768,
                                "synthetic-unit-fixture",
                                observed_at,
                            ),
                            supports_tools: Observation::known(
                                true,
                                "synthetic-unit-fixture",
                                observed_at,
                            ),
                            local_only: Observation::known(
                                profile.kind == ProviderKind::Ollama,
                                "synthetic-unit-fixture",
                                observed_at,
                            ),
                            billing: Observation::known(
                                match profile.kind {
                                    ProviderKind::Codex => {
                                        crate::routing::BillingPolicy::Subscription
                                    }
                                    ProviderKind::Ollama => crate::routing::BillingPolicy::Local,
                                    _ => crate::routing::BillingPolicy::Unknown,
                                },
                                "synthetic-unit-fixture",
                                observed_at,
                            ),
                            input_micros_per_million_tokens: Observation::unknown(
                                "synthetic fixture does not authorize paid pricing",
                            ),
                            output_micros_per_million_tokens: Observation::unknown(
                                "synthetic fixture does not authorize paid pricing",
                            ),
                            pricing_source: Observation::unknown(
                                "synthetic fixture does not authorize paid pricing",
                            ),
                            pricing_verified_at_ms: None,
                            economy_metadata: None,
                            paid_endpoint_metadata: None,
                        })
                        .collect(),
                }
            })
            .collect();
        let snapshot =
            crate::provider_observations::seal_snapshot(&settings, observed_at, providers).unwrap();
        crate::provider_observations::persist_snapshot(data_root, &snapshot).unwrap();
    }

    #[test]
    fn cancel_without_workers_cancels_queued_jobs_and_finishes_batch() {
        let (_folder, request, _worker, _settings_path) = preview_fixture();
        let state = tempdir().unwrap();
        let supervisor = Supervisor::open(state.path().join("cancel-queued")).unwrap();
        seed_fixture_provider_observations(&supervisor.paths().root, &request.settings_path);
        let batch = supervisor.start(&request, 1).unwrap();
        let jobs = JobsStore::open(supervisor.paths().jobs.clone()).unwrap();
        let before = jobs.get(batch.job_ids[0]).unwrap();
        assert_eq!(before.state, JobState::Queued);
        assert_eq!(before.attempt_count, 0);

        let cancelled = supervisor
            .cancel(&request.batch_id, "user requested cancellation", 2)
            .unwrap();
        assert_eq!(cancelled.state, BatchState::Cancelled);
        let job = jobs.get(batch.job_ids[0]).unwrap();
        assert_eq!(job.state, JobState::Cancelled);
        assert_eq!(job.attempt_count, 0);
        assert!(!BatchStore::open(&supervisor.paths().batches)
            .unwrap()
            .acquire_verification(&request.batch_id, job.id, "late-verifier", 3, MAX_LEASE_MS)
            .unwrap());
        assert_eq!(
            supervisor.status(&request.batch_id).unwrap().state,
            BatchState::Cancelled
        );
    }

    #[test]
    fn cancel_needs_input_preserves_unknown_usage_hold_and_attempt_history() {
        let (_folder, request, _worker, _settings_path) = preview_fixture();
        let state = tempdir().unwrap();
        let supervisor = Supervisor::open(state.path().join("cancel-needs-input")).unwrap();
        seed_fixture_provider_observations(&supervisor.paths().root, &request.settings_path);
        let batch = supervisor.start(&request, 1).unwrap();
        let jobs = JobsStore::open(supervisor.paths().jobs.clone()).unwrap();
        let claim = jobs
            .claim_ids("needs-input-worker", &batch.job_ids, 2, 10_000)
            .unwrap()
            .unwrap();
        jobs.record_attempt_outcome(
            claim.job.id,
            "needs-input-worker",
            claim.fencing_token,
            AttemptOutcome::NeedsInput {
                summary: "provider metadata changed".into(),
            },
            3,
        )
        .unwrap();

        let reservation_id = "cancel-unknown-usage-hold";
        let budgets = BudgetStore::open(supervisor.paths().budgets.clone()).unwrap();
        budgets
            .reserve(
                &ReservationRequest {
                    reservation_id: reservation_id.into(),
                    batch_id: request.batch_id.clone(),
                    job_id: claim.job.id.to_string(),
                    task_id: "implement".into(),
                    provider_id: "codex-fixture".into(),
                    model_id: "model-fixture".into(),
                    runtime_id: "codex".into(),
                    max_tokens: 50,
                    max_cost_micros: Some(0),
                    max_seconds: 10,
                    is_paid: false,
                    pricing_source: None,
                    pricing_verified_at_ms: None,
                },
                4,
            )
            .unwrap();
        budgets.mark_dispatched(reservation_id, 5).unwrap();
        let hold = budgets
            .settle(
                reservation_id,
                None,
                None,
                None,
                "synthetic-unknown-usage",
                6,
            )
            .unwrap();
        assert_eq!(hold.state, crate::budgets::ReservationState::Unresolved);

        let cancelled = supervisor
            .cancel(&request.batch_id, "cancel while awaiting input", 7)
            .unwrap();
        assert_eq!(cancelled.state, BatchState::Cancelled);
        let job = jobs.get(claim.job.id).unwrap();
        assert_eq!(job.state, JobState::Cancelled);
        assert_eq!(job.attempt_count, 1);
        assert_eq!(
            jobs.latest_attempt_summary(job.id).unwrap().as_deref(),
            Some("provider metadata changed")
        );
        assert_eq!(
            budgets.get(reservation_id).unwrap().state,
            crate::budgets::ReservationState::Unresolved
        );
    }

    #[test]
    fn cancel_needs_input_with_incomplete_runtime_identity_stays_blocked() {
        let (_folder, request, _worker, _settings_path) = preview_fixture();
        let state = tempdir().unwrap();
        let supervisor = Supervisor::open(state.path().join("cancel-unknown-process")).unwrap();
        seed_fixture_provider_observations(&supervisor.paths().root, &request.settings_path);
        let batch = supervisor.start(&request, 1).unwrap();
        let jobs = JobsStore::open(supervisor.paths().jobs.clone()).unwrap();
        let claim = jobs
            .claim_ids("unknown-process-worker", &batch.job_ids, 2, 10_000)
            .unwrap()
            .unwrap();
        jobs.record_checkpoint(
            claim.job.id,
            "unknown-process-worker",
            claim.fencing_token,
            "runtime_process_identity",
            &json!({"pid":4242,"creationTime100ns":null}),
            3,
        )
        .unwrap();
        jobs.record_attempt_outcome(
            claim.job.id,
            "unknown-process-worker",
            claim.fencing_token,
            AttemptOutcome::NeedsInput {
                summary: "provider response interrupted".into(),
            },
            4,
        )
        .unwrap();

        let batches = BatchStore::open(&supervisor.paths().batches).unwrap();
        assert!(batches
            .reserve_worker(&request.batch_id, "cancel-unknown-worker", 0, 4)
            .unwrap());
        let worker_identity = crate::process::current_process_identity().unwrap();
        batches
            .worker_started(
                "cancel-unknown-worker",
                worker_identity.pid,
                worker_identity.creation_time_100ns,
                4,
            )
            .unwrap();

        let result = supervisor
            .cancel(&request.batch_id, "cancel with unknown process", 5)
            .unwrap();
        assert_eq!(result.state, BatchState::Cancelling);
        assert_eq!(jobs.get(claim.job.id).unwrap().state, JobState::NeedsInput);
        assert_eq!(
            supervisor
                .run_worker(&request.batch_id, "cancel-unknown-worker", worker_identity)
                .unwrap(),
            WorkerExit::Cancelled,
        );
        assert_eq!(
            supervisor.status(&request.batch_id).unwrap().state,
            BatchState::Cancelling
        );
        assert_eq!(jobs.get(claim.job.id).unwrap().state, JobState::NeedsInput);
        // Test-only manual reconciliation removes the synthetic malformed
        // identity after confirming this fixture never launched that PID.
        Connection::open(supervisor.paths().jobs.clone())
            .unwrap()
            .execute(
                "DELETE FROM job_checkpoints WHERE job_id=?1 AND name='runtime_process_identity'",
                [claim.job.id],
            )
            .unwrap();
        assert_eq!(
            supervisor
                .cancel(&request.batch_id, "retry after identity reconciliation", 6)
                .unwrap()
                .state,
            BatchState::Cancelled,
        );
        assert_eq!(jobs.get(claim.job.id).unwrap().state, JobState::Cancelled);
    }

    #[test]
    fn cancel_needs_input_with_orphaned_process_or_review_intent_stays_blocked() {
        for (suffix, checkpoint_name, payload) in [
            (
                "orphan-task-process",
                "runtime_process_implement",
                json!({"state":"started","pid":4242,"creationTime100ns":null}),
            ),
            (
                "review-start-without-identity",
                "verification_review",
                json!({"state":"started","taskId":"review"}),
            ),
        ] {
            let (_folder, request, _worker, _settings_path) = preview_fixture();
            let state = tempdir().unwrap();
            let supervisor =
                Supervisor::open(state.path().join(format!("cancel-{suffix}"))).unwrap();
            seed_fixture_provider_observations(&supervisor.paths().root, &request.settings_path);
            let batch = supervisor.start(&request, 1).unwrap();
            let jobs = JobsStore::open(supervisor.paths().jobs.clone()).unwrap();
            let claim = jobs
                .claim_ids("orphaned-process-worker", &batch.job_ids, 2, 10_000)
                .unwrap()
                .unwrap();
            jobs.record_checkpoint(
                claim.job.id,
                "orphaned-process-worker",
                claim.fencing_token,
                checkpoint_name,
                &payload,
                3,
            )
            .unwrap();
            jobs.record_attempt_outcome(
                claim.job.id,
                "orphaned-process-worker",
                claim.fencing_token,
                AttemptOutcome::NeedsInput {
                    summary: "process ownership is incomplete".into(),
                },
                4,
            )
            .unwrap();

            let result = supervisor
                .cancel(
                    &request.batch_id,
                    "cancel with incomplete process journal",
                    5,
                )
                .unwrap();
            assert_eq!(result.state, BatchState::Cancelling, "case {suffix}");
            assert_eq!(
                jobs.get(claim.job.id).unwrap().state,
                JobState::NeedsInput,
                "case {suffix}"
            );
        }
    }

    #[cfg(windows)]
    #[test]
    fn cancel_needs_input_with_live_owned_child_stays_blocked() {
        use std::ffi::OsString;

        let child_cwd = tempdir().unwrap();
        let command = PathBuf::from(std::env::var_os("SystemRoot").unwrap())
            .join("System32")
            .join("cmd.exe");
        let (mut child, identity) = crate::process::spawn_interactive(
            &command,
            &["/C".into(), "ping -t 127.0.0.1 >NUL".into()],
            child_cwd.path(),
            &[(OsString::from("FABRICA_CANCEL_CHILD"), OsString::from("1"))],
            &[],
        )
        .unwrap();

        let (_folder, request, _worker, _settings_path) = preview_fixture();
        let state = tempdir().unwrap();
        let supervisor = Supervisor::open(state.path().join("cancel-live-process")).unwrap();
        seed_fixture_provider_observations(&supervisor.paths().root, &request.settings_path);
        let batch = supervisor.start(&request, 1).unwrap();
        let jobs = JobsStore::open(supervisor.paths().jobs.clone()).unwrap();
        let claim = jobs
            .claim_ids("live-process-worker", &batch.job_ids, 2, 10_000)
            .unwrap()
            .unwrap();
        jobs.record_checkpoint(
            claim.job.id,
            "live-process-worker",
            claim.fencing_token,
            "runtime_process_identity",
            &json!({"pid":identity.pid,"creationTime100ns":identity.creation_time_100ns}),
            3,
        )
        .unwrap();
        jobs.record_attempt_outcome(
            claim.job.id,
            "live-process-worker",
            claim.fencing_token,
            AttemptOutcome::NeedsInput {
                summary: "provider requested input".into(),
            },
            4,
        )
        .unwrap();

        let live_before = crate::process::process_identity_matches(identity).unwrap_or(false);
        let result = supervisor.cancel(&request.batch_id, "cancel with live process", 5);
        let stopped_after = !crate::process::process_identity_matches(identity).unwrap_or(true);
        let _ = crate::process::terminate_owned_process(identity);
        let _ = child.kill();
        let _ = child.wait();
        let result = result.unwrap();
        assert!(
            live_before,
            "fixture child must be alive before cancellation"
        );
        assert!(
            stopped_after,
            "cancel must stop and confirm the owned child"
        );
        assert_eq!(result.state, BatchState::Cancelled);
        assert_eq!(jobs.get(claim.job.id).unwrap().state, JobState::Cancelled);
        assert_eq!(
            supervisor
                .cancel(&request.batch_id, "repeat completed cancellation", 6)
                .unwrap()
                .state,
            BatchState::Cancelled,
        );
        assert_eq!(jobs.get(claim.job.id).unwrap().state, JobState::Cancelled);
    }

    #[test]
    fn cancel_mixed_batch_keeps_active_work_cancelling_and_preserves_ambiguity() {
        let (_folder, mut request, _worker, _settings_path) = preview_fixture();
        let source_path = request.issue_sources["TEST-L001"].clone();
        let mut second = request.jobs[0].clone();
        second.issue_id = "TEST-L002".into();
        second.idempotency_key = "cancel-mixed-TEST-L002".into();
        request.jobs.push(second);
        let mut verification = request.verification[0].clone();
        verification.issue_id = "TEST-L002".into();
        request.verification.push(verification);
        request
            .issue_sources
            .insert("TEST-L002".into(), source_path);

        let state = tempdir().unwrap();
        let supervisor = Supervisor::open(state.path().join("cancel-mixed")).unwrap();
        seed_fixture_provider_observations(&supervisor.paths().root, &request.settings_path);
        let batch = supervisor.start(&request, 1).unwrap();
        let jobs = JobsStore::open(supervisor.paths().jobs.clone()).unwrap();
        let active = jobs
            .claim_ids("active-cancel-worker", &[batch.job_ids[1]], 2, 10_000)
            .unwrap()
            .unwrap();

        let result = supervisor
            .cancel(&request.batch_id, "cancel mixed work", 3)
            .unwrap();
        assert_eq!(result.state, BatchState::Cancelling);
        assert_eq!(
            jobs.get(batch.job_ids[0]).unwrap().state,
            JobState::Cancelled
        );
        let active_job = jobs.get(active.job.id).unwrap();
        assert_eq!(active_job.state, JobState::Leased);
        assert!(active_job.cancel_requested);
        assert_eq!(
            supervisor.status(&request.batch_id).unwrap().state,
            BatchState::Cancelling
        );
    }

    #[test]
    fn cancel_does_not_terminalize_pending_verification_or_ambiguous_jobs() {
        let (_folder, request, _worker, _settings_path) = preview_fixture();
        let state = tempdir().unwrap();
        let supervisor = Supervisor::open(state.path().join("cancel-verification")).unwrap();
        seed_fixture_provider_observations(&supervisor.paths().root, &request.settings_path);
        let batch = supervisor.start(&request, 1).unwrap();
        let jobs = JobsStore::open(supervisor.paths().jobs.clone()).unwrap();
        let claim = jobs
            .claim_ids("verification-cancel-worker", &batch.job_ids, 2, 10_000)
            .unwrap()
            .unwrap();
        jobs.record_attempt_outcome(
            claim.job.id,
            "verification-cancel-worker",
            claim.fencing_token,
            AttemptOutcome::ProposedComplete {
                summary: "proposal awaits checks".into(),
            },
            3,
        )
        .unwrap();
        let result = supervisor
            .cancel(&request.batch_id, "cancel before verification", 4)
            .unwrap();
        assert_eq!(result.state, BatchState::Cancelling);
        assert_eq!(
            jobs.get(claim.job.id).unwrap().state,
            JobState::AwaitingVerification
        );

        let (_folder, request, _worker, _settings_path) = preview_fixture();
        let state = tempdir().unwrap();
        let supervisor = Supervisor::open(state.path().join("cancel-reconciliation")).unwrap();
        seed_fixture_provider_observations(&supervisor.paths().root, &request.settings_path);
        let batch = supervisor.start(&request, 1).unwrap();
        let jobs = JobsStore::open(supervisor.paths().jobs.clone()).unwrap();
        let claim = jobs
            .claim_ids("ambiguous-cancel-worker", &batch.job_ids, 2, 10_000)
            .unwrap()
            .unwrap();
        jobs.record_attempt_outcome(
            claim.job.id,
            "ambiguous-cancel-worker",
            claim.fencing_token,
            AttemptOutcome::Failed {
                summary: "outcome requires reconciliation".into(),
            },
            3,
        )
        .unwrap();
        assert_eq!(
            jobs.get(claim.job.id).unwrap().state,
            JobState::ReconciliationRequired
        );
        let result = supervisor
            .cancel(&request.batch_id, "cancel with unresolved effects", 4)
            .unwrap();
        assert_eq!(result.state, BatchState::Cancelling);
        assert_eq!(
            jobs.get(claim.job.id).unwrap().state,
            JobState::ReconciliationRequired
        );
    }

    fn git_fixture(repo: &Path, args: &[&str]) -> String {
        let output = std::process::Command::new("git")
            .args(args)
            .current_dir(repo)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).trim().to_owned()
    }

    #[test]
    fn conflicting_owned_build_root_is_durable_needs_input_without_dispatch() {
        let (folder, mut request, _worker, _settings_path) = preview_fixture();
        let project = request.project_path.clone();
        git_fixture(&project, &["init"]);
        git_fixture(&project, &["config", "user.name", "Runner Fixture"]);
        git_fixture(
            &project,
            &["config", "user.email", "runner-fixture@example.invalid"],
        );
        fs::write(project.join("README.md"), "fixture base\n").unwrap();
        git_fixture(&project, &["add", "."]);
        git_fixture(&project, &["commit", "-m", "fixture base"]);
        let base = git_fixture(&project, &["rev-parse", "HEAD"]);
        request.verified_base_commit = base.clone();

        let state_root = folder.path().join("runner-state");
        let isolation =
            RepoIsolation::new(&project, &request.managed_worktree_root, &state_root).unwrap();
        isolation
            .prepare_workspace(PrepareRequest {
                issue_id: request.jobs[0].issue_id.clone(),
                issue_snapshot_fingerprint: request.jobs[0].source_fingerprint.clone(),
                verified_base_commit: base.clone(),
                predecessor_commits: vec![],
                build_root: request.build_root.clone(),
            })
            .unwrap();
        let original_record = isolation
            .recover_workspace(&request.jobs[0].issue_id)
            .unwrap();
        let ownership_path = project
            .join(".git")
            .join("fabrica-runner-ownership")
            .join("TEST-L001.json");
        let original_ownership = fs::read(&ownership_path).unwrap();
        let original_source = fs::read(project.join("TEST-L001.md")).unwrap();

        request.build_root = folder.path().join("conflicting-build-root");
        let supervisor = Supervisor::open(state_root).unwrap();
        seed_fixture_provider_observations(&supervisor.paths().root, &request.settings_path);
        let batch = supervisor.start(&request, now_ms()).unwrap();
        let persisted_request = BatchStore::open(&supervisor.paths().batches)
            .unwrap()
            .request(&request.batch_id)
            .unwrap();
        let job_id = batch.job_ids[0];
        let jobs = JobsStore::open(supervisor.paths().jobs.clone()).unwrap();
        let claim = jobs
            .claim_ids("fixture-worker", &[job_id], now_ms(), MAX_LEASE_MS)
            .unwrap()
            .unwrap();
        jobs.start_attempt(job_id, "fixture-worker", claim.fencing_token, now_ms())
            .unwrap();

        supervisor
            .execute_claim(&persisted_request, "fixture-worker", claim)
            .unwrap();

        let job = jobs.get(job_id).unwrap();
        assert_eq!(job.state, JobState::NeedsInput);
        assert_eq!(job.attempt_count, 1);
        assert!(job.lease_owner.is_none());
        let failure = jobs
            .get_checkpoint(job_id, "workspace_preparation_failure")
            .unwrap()
            .unwrap();
        assert_eq!(failure["statusKey"], "workspace_preparation_failure");
        assert_eq!(failure["stage"], "prepare_workspace");
        assert!(failure["error"]
            .as_str()
            .unwrap()
            .contains("existing issue checkout"));
        assert!(failure["guidance"].as_str().unwrap().contains("inspect"));
        assert!(failure.to_string().len() < 2048);
        let status = supervisor.status_view(&request.batch_id).unwrap();
        let job_status = &status.jobs[0];
        assert_eq!(job_status.state, JobState::NeedsInput);
        assert!(job_status
            .checkpoints
            .iter()
            .any(|checkpoint| checkpoint.key == "workspace_preparation_failure"));
        assert!(supervisor
            .morning_summary(&request.batch_id)
            .unwrap()
            .needs_input
            .contains(&"TEST-L001".into()));

        let reservation = reservation_id(&request.batch_id, job_id, "implement", 1);
        assert!(BudgetStore::open(&supervisor.paths().budgets)
            .unwrap()
            .get_optional(&reservation)
            .unwrap()
            .is_none());
        assert!(jobs
            .get_checkpoint(job_id, "runtime_process_implement")
            .unwrap()
            .is_none());
        assert!(jobs
            .get_checkpoint(job_id, "dispatch_implement")
            .unwrap()
            .is_none());
        assert_eq!(fs::read(&ownership_path).unwrap(), original_ownership);
        assert_eq!(
            fs::read(project.join("TEST-L001.md")).unwrap(),
            original_source
        );
        assert_eq!(git_fixture(&project, &["rev-parse", "HEAD"]), base);
        assert_eq!(
            isolation
                .recover_workspace(&request.jobs[0].issue_id)
                .unwrap(),
            original_record
        );
    }

    #[test]
    fn verification_cancel_watcher_stops_managed_check_after_batch_cancel() {
        let (_folder, request, _worker, _settings_path) = preview_fixture();
        let state = tempdir().unwrap();
        let supervisor = Supervisor::open(state.path().join("cancel-watch-state")).unwrap();
        seed_fixture_provider_observations(&supervisor.paths().root, &request.settings_path);
        let batch = supervisor.start(&request, now_ms()).unwrap();
        BatchStore::open(&supervisor.paths().batches)
            .unwrap()
            .control(
                &request.batch_id,
                "cancelling",
                "fixture cancellation",
                now_ms(),
            )
            .unwrap();

        let cancel = Arc::new(AtomicBool::new(false));
        let watcher = VerificationCancelWatcher::start(
            supervisor.paths().clone(),
            request.batch_id,
            batch.job_ids[0],
            "fixture-worker".into(),
            now_ms() + 2_000,
            Arc::new(AtomicBool::new(false)),
            Arc::clone(&cancel),
        );
        let deadline = std::time::Instant::now() + Duration::from_secs(1);
        while !cancel.load(Ordering::Acquire) && std::time::Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }
        drop(watcher);
        assert!(cancel.load(Ordering::Acquire));
    }

    #[test]
    fn detached_worker_error_is_durable_bounded_and_preserves_ambiguous_state() {
        let (_folder, request, _worker, _settings_path) = preview_fixture();
        let state = tempdir().unwrap();
        let supervisor = Supervisor::open(state.path().join("worker-error-state")).unwrap();
        seed_fixture_provider_observations(&supervisor.paths().root, &request.settings_path);
        let batch = supervisor.start(&request, 10).unwrap();
        let job_id = batch.job_ids[0];
        let batches = BatchStore::open(&supervisor.paths().batches).unwrap();
        batches
            .reserve_worker(&request.batch_id, "fixture-worker", 0, 11)
            .unwrap();
        batches
            .worker_started("fixture-worker", 424_242, Some(123_456), 12)
            .unwrap();
        let jobs = JobsStore::open(supervisor.paths().jobs.clone()).unwrap();
        let claim = jobs
            .claim_ids("fixture-worker", &[job_id], 13, 60_000)
            .unwrap()
            .unwrap();
        jobs.start_attempt(job_id, "fixture-worker", claim.fencing_token, 14)
            .unwrap();
        let budgets = BudgetStore::open(&supervisor.paths().budgets).unwrap();
        budgets
            .reserve(
                &ReservationRequest {
                    reservation_id: "fixture-dispatched-call".into(),
                    batch_id: request.batch_id.clone(),
                    job_id: job_id.to_string(),
                    task_id: "implement".into(),
                    provider_id: "codex-fixture".into(),
                    model_id: "model-fixture".into(),
                    runtime_id: "codex".into(),
                    max_tokens: 10,
                    max_cost_micros: Some(0),
                    max_seconds: 5,
                    is_paid: false,
                    pricing_source: None,
                    pricing_verified_at_ms: None,
                },
                14,
            )
            .unwrap();
        budgets
            .mark_dispatched("fixture-dispatched-call", 15)
            .unwrap();

        let internal_error =
            SupervisorError::Runtime("workspace_inspection: sk-secret-must-not-persist".into());
        record_worker_runtime_failure(&supervisor, "fixture-worker", &internal_error, 16);

        assert_eq!(
            batches
                .worker_slot(&request.batch_id, "fixture-worker")
                .unwrap()
                .unwrap()
                .state,
            "failed"
        );
        let persisted_job = jobs.get(job_id).unwrap();
        assert_eq!(persisted_job.state, JobState::Running);
        assert_eq!(persisted_job.attempt_count, 1);
        assert!(persisted_job.lease_until_ms.is_some());
        assert_eq!(
            budgets.get("fixture-dispatched-call").unwrap().state,
            crate::budgets::ReservationState::Dispatched
        );
        let event = supervisor
            .events(&request.batch_id, 0, 100)
            .unwrap()
            .events
            .into_iter()
            .find(|event| event.kind == "worker_failed")
            .unwrap();
        let serialized = event.payload.to_string();
        assert!(serialized.contains("workspace_inspection"));
        assert!(serialized.contains("active attempts and reservations remain unreconciled"));
        assert!(!serialized.contains("sk-secret"));
        assert!(serialized.len() <= 512);
    }

    #[test]
    fn preview_binding_changes_with_request_and_worker_and_rejects_drifted_sources() {
        let (folder, request, worker, settings_path) = preview_fixture();
        let data_root = folder.path().join("routing-data");
        let initial = preview_batch_request(&data_root, &request, &worker).unwrap();
        assert!(
            !data_root.exists(),
            "a routing preview must remain storage-free"
        );
        assert!(initial.request.provider_observation_fingerprint.is_some());
        assert_eq!(
            initial.provider_observation_fingerprint,
            initial
                .request
                .provider_observation_fingerprint
                .clone()
                .unwrap()
        );

        let mut changed_request = initial.request.clone();
        changed_request.jobs[0].plan.tasks[0].acceptance[0] = "different outcome".into();
        let changed = preview_batch_request(&data_root, &changed_request, &worker).unwrap();
        assert_ne!(initial.fingerprint, changed.fingerprint);

        let other_worker = folder.path().join("other-worker.exe");
        fs::write(&other_worker, b"different worker bytes").unwrap();
        let changed_worker =
            preview_batch_request(&data_root, &initial.request, &other_worker).unwrap();
        assert_ne!(initial.fingerprint, changed_worker.fingerprint);

        fs::create_dir_all(&data_root).unwrap();
        fs::write(
            data_root.join("provider-observations.json"),
            b"changed evidence",
        )
        .unwrap();
        assert!(matches!(
            preview_batch_request(&data_root, &initial.request, &worker),
            Err(SupervisorError::Conflict(_))
        ));
        let mut refreshed_review = initial.request.clone();
        refreshed_review.provider_observation_fingerprint = None;
        refreshed_review.resolved_routes.clear();
        let refreshed = preview_batch_request(&data_root, &refreshed_review, &worker).unwrap();
        assert_ne!(
            initial.provider_observation_fingerprint,
            refreshed.provider_observation_fingerprint
        );

        fs::write(&settings_path, b"{}").unwrap();
        assert!(matches!(
            preview_batch_request(&data_root, &initial.request, &worker),
            Err(SupervisorError::Conflict(_))
        ));

        let source = initial.request.issue_sources["TEST-L001"].clone();
        fs::write(source, "---\nid: TEST-L001\ntitle: edited\n---\n").unwrap();
        assert!(matches!(
            preview_batch_request(&data_root, &request, &worker),
            Err(SupervisorError::Conflict(_))
        ));
    }

    #[test]
    fn dispatch_route_commitment_is_immutable_and_refresh_is_opt_in() {
        let (folder, request, worker, settings_path) = preview_fixture();
        let data_root = folder.path().join("route-commitment-data");
        seed_fixture_provider_observations(&data_root, &settings_path);
        let preview = preview_batch_request(&data_root, &request, &worker).unwrap();
        let settings = settings::load_settings(&settings_path).unwrap();
        let (facts, snapshot_fingerprint, problem) =
            load_provider_observations(&settings, &data_root, now_ms().max(0) as u128).unwrap();
        assert!(problem.is_none());
        let job = &preview.request.jobs[0];
        let task = &job.plan.tasks[0];
        let route_key = paid_policy_key(&job.issue_id, &task.task_id);
        let commitment = preview.request.route_commitments.get(&route_key).unwrap();
        let fact = facts
            .iter()
            .find(|fact| {
                fact.provider_id == commitment.provider_id && fact.model_id == commitment.model_id
            })
            .unwrap()
            .clone();
        let (_, runtime_identity_fingerprint) = fact.runtime_id.split_once(':').unwrap();
        let fresh = crate::provider_observations::FreshCandidateObservation {
            facts: fact.clone(),
            snapshot_fingerprint,
            settings_fingerprint: preview.request.settings_fingerprint.clone().unwrap(),
            runtime_identity_fingerprint: runtime_identity_fingerprint.to_owned(),
            observed_at_ms: now_ms().max(0) as u128,
        };
        assert!(
            check_fresh_route_commitment(&preview.request, &settings, job, task, &fresh).is_ok()
        );

        let mut changed_privacy = fresh.clone();
        changed_privacy.facts.local_only = !commitment.local_only;
        assert!(check_fresh_route_commitment(
            &preview.request,
            &settings,
            job,
            task,
            &changed_privacy
        )
        .unwrap_err()
        .contains("privacy or billing"));

        let mut changed_capability = fresh.clone();
        changed_capability.facts.supports_tools = Some(false);
        assert!(check_fresh_route_commitment(
            &preview.request,
            &settings,
            job,
            task,
            &changed_capability
        )
        .unwrap_err()
        .contains("tool capability"));

        assert!(matches!(
            crate::provider_observations::ModelFreeRefreshConsent::default(),
            crate::provider_observations::ModelFreeRefreshConsent::Disabled
        ));
        let mut legacy = serde_json::to_value(&preview.request).unwrap();
        legacy
            .as_object_mut()
            .unwrap()
            .remove("dispatchRefreshConsent");
        let decoded: BatchRequest = serde_json::from_value(legacy).unwrap();
        assert!(matches!(
            decoded.dispatch_refresh_consent,
            crate::provider_observations::ModelFreeRefreshConsent::Disabled
        ));

        let future_now = now_ms().max(0) as u128 + ROUTING_OBSERVATION_MAX_AGE_MS + 1;
        let (stale_facts, _, _) =
            load_provider_observations(&settings, &data_root, future_now).unwrap();
        for task_id in ["implement", "review"] {
            let task = job
                .plan
                .tasks
                .iter()
                .find(|task| task.task_id == task_id)
                .unwrap();
            let stale =
                route_task(&preview.request, &settings, job, task, &stale_facts, None).unwrap();
            assert!(!stale.eligible, "stale saved evidence must block {task_id}");

            let key = paid_policy_key(&job.issue_id, task_id);
            let committed = preview.request.route_commitments.get(&key).unwrap();
            let fact = facts
                .iter()
                .find(|fact| {
                    fact.provider_id == committed.provider_id && fact.model_id == committed.model_id
                })
                .unwrap()
                .clone();
            let (_, runtime_identity_fingerprint) = fact.runtime_id.split_once(':').unwrap();
            let runtime_identity_fingerprint = runtime_identity_fingerprint.to_owned();
            let refreshed = crate::provider_observations::FreshCandidateObservation {
                facts: fact,
                snapshot_fingerprint: "ephemeral-refresh".into(),
                settings_fingerprint: fresh.settings_fingerprint.clone(),
                runtime_identity_fingerprint,
                observed_at_ms: future_now,
            };
            assert!(
                check_fresh_route_commitment(&preview.request, &settings, job, task, &refreshed)
                    .is_ok(),
                "a semantically unchanged fresh route should remain valid for {task_id}"
            );

            let mut alternate = refreshed;
            alternate.facts.provider_id = "unreviewed-provider".into();
            assert!(check_fresh_route_commitment(
                &preview.request,
                &settings,
                job,
                task,
                &alternate
            )
            .unwrap_err()
            .contains("different route"));
        }
    }

    #[test]
    fn failed_dispatch_refresh_enters_needs_input_before_any_reservation_or_provider_launch() {
        let (folder, mut request, worker, settings_path) = preview_fixture();
        let project = request.project_path.clone();
        git_fixture(&project, &["init"]);
        git_fixture(&project, &["config", "user.name", "Runner Fixture"]);
        git_fixture(
            &project,
            &["config", "user.email", "runner-fixture@example.invalid"],
        );
        fs::write(project.join("README.md"), "fixture base\n").unwrap();
        git_fixture(&project, &["add", "."]);
        git_fixture(&project, &["commit", "-m", "fixture base"]);
        request.verified_base_commit = git_fixture(&project, &["rev-parse", "HEAD"]);
        request.dispatch_refresh_consent =
            crate::provider_observations::ModelFreeRefreshConsent::Providers {
                kinds: vec![ProviderKind::Codex],
            };

        let supervisor = Supervisor::open(folder.path().join("refresh-failure-state")).unwrap();
        seed_fixture_provider_observations(&supervisor.paths().root, &settings_path);
        let preview = preview_batch_request(&supervisor.paths().root, &request, &worker).unwrap();
        let admitted = supervisor.start(&preview.request, now_ms()).unwrap();
        let jobs = JobsStore::open(supervisor.paths().jobs.clone()).unwrap();
        let claim = jobs
            .claim_ids(
                "refresh-failure-worker",
                &admitted.job_ids,
                now_ms(),
                MAX_LEASE_MS,
            )
            .unwrap()
            .unwrap();

        // The settings and reviewed route remain unchanged, but runtime identity
        // can no longer be verified; the bounded refresh must fail before reserve.
        fs::remove_file(&worker).unwrap();
        supervisor
            .execute_claim(&preview.request, "refresh-failure-worker", claim.clone())
            .unwrap();

        let job = jobs.get(claim.job.id).unwrap();
        assert_eq!(job.state, JobState::NeedsInput);
        assert_eq!(job.attempt_count, 1);
        let audit = jobs
            .get_checkpoint(job.id, "dispatch_route_refresh")
            .unwrap()
            .unwrap();
        assert_eq!(audit["state"], "needs_input");
        assert!(audit["reason"].as_str().unwrap().contains("runtime"));
        assert!(BudgetStore::open(&supervisor.paths().budgets)
            .unwrap()
            .get_optional(&reservation_id(&request.batch_id, job.id, "implement", 1))
            .unwrap()
            .is_none());
        assert!(jobs
            .get_checkpoint(job.id, "dispatch_implement")
            .unwrap()
            .is_none());
        assert!(jobs
            .get_checkpoint(job.id, "runtime_process_implement")
            .unwrap()
            .is_none());
    }

    #[test]
    fn runtime_input_request_checkpoint_preserves_bounded_reviewable_details() {
        let event = json!({
            "state":"needs_input",
            "requestId":17,
            "method":"item/commandExecution/requestApproval",
            "threadId":"thread-local-fixture",
            "command":"cargo test --workspace",
            "cwd":"C:/fixture",
            "reason":"Codex requires approval before command execution.",
            "availableDecisions":"accept, decline",
            "questions":"Run the requested local checks?"
        })
        .to_string();
        let checkpoint = runtime_input_request_checkpoint(&event);
        let encoded = serde_json::to_vec(&checkpoint).unwrap();
        assert!(encoded.len() <= 4 * 1024);
        assert_eq!(checkpoint["state"], "needs_input");
        assert_eq!(checkpoint["statusKey"], "runtime_input_request");
        assert_eq!(checkpoint["threadId"], "thread-local-fixture");
        assert_eq!(checkpoint["requestId"], 17);
        assert!(checkpoint["method"]
            .as_str()
            .unwrap()
            .contains("requestApproval"));
    }

    #[test]
    fn status_view_exposes_bounded_needs_input_attempt_summary() {
        let (_folder, request, _worker, _settings_path) = preview_fixture();
        let state = tempdir().unwrap();
        let supervisor = Supervisor::open(state.path().join("status-needs-input")).unwrap();
        seed_fixture_provider_observations(&supervisor.paths().root, &request.settings_path);
        let batch = supervisor.start(&request, 1).unwrap();
        let jobs = JobsStore::open(supervisor.paths().jobs.clone()).unwrap();
        let claim = jobs
            .claim_ids("status-needs-input-worker", &batch.job_ids, 2, 10_000)
            .unwrap()
            .unwrap();
        jobs.record_attempt_outcome(
            claim.job.id,
            "status-needs-input-worker",
            claim.fencing_token,
            AttemptOutcome::NeedsInput {
                summary: format!(
                    "Repository HEAD changed; Bearer status-secret {}",
                    "🙂".repeat(1500)
                ),
            },
            3,
        )
        .unwrap();
        let controlled_summary = format!(
            "Repository HEAD changed\0\nBearer status-secret {}",
            "🙂".repeat(1500)
        );
        let connection = rusqlite::Connection::open(&supervisor.paths().jobs).unwrap();
        connection
            .execute(
                "UPDATE job_attempts SET result_json=?2 WHERE job_id=?1",
                params![
                    claim.job.id,
                    json!({"summary": controlled_summary}).to_string()
                ],
            )
            .unwrap();

        let status = supervisor.status_view(&request.batch_id).unwrap();
        let job = &status.jobs[0];
        assert_eq!(job.state, JobState::NeedsInput);
        let summary = job
            .last_attempt_summary
            .as_deref()
            .expect("blocked attempt reason is projected");
        assert!(summary.len() <= 2 * 1024);
        assert!(summary.ends_with("… [truncated]"));
        assert!(!summary.contains("status-secret"));
        assert!(!summary.chars().any(char::is_control));
        assert!(summary.contains("Repository HEAD changed"));

        let legacy: JobStatusView = serde_json::from_value(json!({
            "id": job.id,
            "issueId": job.issue_id,
            "state": "needs_input",
            "completionGate": "pending",
            "attemptCount": 1,
            "maxAttempts": 3,
            "updatedAtMs": 3,
            "checkpointsTruncated": false,
            "checkpoints": []
        }))
        .unwrap();
        assert_eq!(legacy.last_attempt_summary, None);
    }

    #[test]
    fn crashed_attempt_recovery_is_batch_bound_and_preserves_attempt_limit() {
        let (_folder, request, _worker, _settings_path) = preview_fixture();
        let state = tempdir().unwrap();
        let supervisor = Supervisor::open(state.path().join("recovery-state")).unwrap();
        seed_fixture_provider_observations(&supervisor.paths().root, &request.settings_path);
        let batch = supervisor.start(&request, 1).unwrap();
        let jobs = JobsStore::open(supervisor.paths().jobs.clone()).unwrap();
        let claim = jobs
            .claim_ids("crashed-worker", &batch.job_ids, 2, 10)
            .unwrap()
            .unwrap();
        jobs.reconcile_expired_ids(&batch.job_ids, 20).unwrap();

        let recovery = RecoveryRequest {
            job_id: claim.job.id,
            decision: RecoveryAction::SafeToRetry,
            evidence: "synthetic crash; effects inspected".into(),
            process_confirmed_stopped: true,
            effects_reconciled: true,
        };
        assert!(
            matches!(
                supervisor.recover_job(&request.batch_id, recovery.clone(), 21),
                Err(SupervisorError::Jobs(JobsError::Conflict(_)))
            ),
            "the store attempt cap must survive explicit recovery"
        );
        let result = supervisor
            .recover_job(
                &request.batch_id,
                RecoveryRequest {
                    decision: RecoveryAction::KeepNeedsInput,
                    ..recovery
                },
                22,
            )
            .unwrap();
        assert!(result.batch_membership_verified);
        assert_eq!(result.job.state, JobState::NeedsInput);
        assert_eq!(result.job.attempt_count, 1);
        assert!(matches!(
            supervisor.recover_job(
                "other-batch",
                RecoveryRequest {
                    job_id: claim.job.id,
                    decision: RecoveryAction::MarkFailed,
                    evidence: "wrong batch".into(),
                    process_confirmed_stopped: true,
                    effects_reconciled: true,
                },
                23
            ),
            Err(SupervisorError::NotFound(_))
        ));
    }

    #[cfg(windows)]
    #[test]
    fn reconnecting_running_batch_reuses_live_worker_slot_after_restart() {
        let (_fixture, request, worker, _settings_path) = preview_fixture();
        let state = tempdir().unwrap();
        let supervisor = Supervisor::open(state.path().join("reattach-state")).unwrap();
        seed_fixture_provider_observations(&supervisor.paths().root, &request.settings_path);
        supervisor.start(&request, 1).unwrap();
        let batches = BatchStore::open(&supervisor.paths().batches).unwrap();
        let identity = crate::process::current_process_identity().unwrap();
        assert!(batches
            .reserve_worker(&request.batch_id, "persisted-live-worker", 0, 2)
            .unwrap());
        batches
            .worker_started(
                "persisted-live-worker",
                identity.pid,
                identity.creation_time_100ns,
                3,
            )
            .unwrap();

        // Opening a new Supervisor models process restart. Both reconnects must
        // find the same owned worker rather than launching into its capacity slot.
        let restarted = Supervisor::open(state.path().join("reattach-state")).unwrap();
        let first = restarted
            .resume_and_launch(&request.batch_id, &worker, 4)
            .unwrap();
        let second = restarted
            .resume_and_launch(&request.batch_id, &worker, 5)
            .unwrap();
        let slots = batches.worker_slots(&request.batch_id).unwrap();
        assert_eq!(first.state, BatchState::Running);
        assert_eq!(second.state, BatchState::Running);
        assert_eq!(slots.len(), 1);
        assert_eq!(slots[0].worker_id, "persisted-live-worker");
    }

    #[test]
    fn expired_batch_deadline_blocks_check_dispatch_and_unsupported_retries() {
        let check = CheckSpec {
            id: "must-not-start".into(),
            program: PathBuf::from("missing-check-program"),
            args: vec![],
            cwd: PathBuf::from("."),
            timeout_ms: 60_000,
            output_limit_bytes: 1024,
            adapter: None,
            contract: None,
        };
        let executor = DeadlineCheckExecutor {
            deadline_ms: now_ms() - 1,
            build_output: PathBuf::new(),
        };
        assert!(executor
            .execute(Path::new("."), &check)
            .unwrap_err()
            .contains("deadline expired"));

        let (_folder, mut request, _worker, _settings_path) = preview_fixture();
        request.policy.max_network_retries = 1;
        assert!(matches!(
            validate_request(&request),
            Err(SupervisorError::Invalid(message)) if message.contains("not implemented")
        ));

        let (_folder, mut request, _worker, _settings_path) = preview_fixture();
        request.verification[0].checks[0].adapter =
            Some(crate::verification::CheckAdapter::AgentsDeliveryContract);
        assert!(matches!(
            validate_request(&request),
            Err(SupervisorError::Invalid(message)) if message.contains("invalid verification check")
        ));
    }

    #[test]
    fn foreground_host_exits_promptly_at_persisted_absolute_deadline() {
        let (_folder, request, _worker, _settings_path) = preview_fixture();
        let state = tempdir().unwrap();
        let supervisor = Supervisor::open(state.path().join("foreground-deadline")).unwrap();
        seed_fixture_provider_observations(&supervisor.paths().root, &request.settings_path);
        supervisor.start(&request, now_ms()).unwrap();
        let batches = BatchStore::open(&supervisor.paths().batches).unwrap();
        batches
            .connection()
            .unwrap()
            .execute(
                "UPDATE batches SET state='running',deadline_ms=?2 WHERE batch_id=?1",
                params![request.batch_id, now_ms() - 1],
            )
            .unwrap();
        let started = std::time::Instant::now();
        assert!(matches!(
            supervisor.wait_for_hosted_batch(&request.batch_id),
            Err(SupervisorError::Conflict(message)) if message.contains("absolute batch deadline")
        ));
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn reviewer_reasoning_effort_must_be_advertised_by_selected_model() {
        let (_folder, mut request, _worker, settings_path) = preview_fixture();
        request.verification[0].reviewer.reasoning_effort = Some("medium".into());
        let error = Supervisor::dry_run_request(&request).unwrap_err();
        assert!(error.to_string().contains("not advertised"));

        let mut settings: Value =
            serde_json::from_slice(&fs::read(&settings_path).unwrap()).unwrap();
        settings["providers"][0]["models"][0]["reasoningEfforts"] = json!(["medium"]);
        fs::write(&settings_path, serde_json::to_vec(&settings).unwrap()).unwrap();
        assert!(Supervisor::dry_run_request(&request).is_ok());
    }

    #[test]
    fn explicit_job_budget_caps_bound_reservations_and_must_fit_batch() {
        let (_folder, mut request, _worker, _settings_path) = preview_fixture();
        request.policy.window_seconds = 120;
        request.policy.budget.max_seconds = 120;
        request.jobs[0].plan.limits.max_tokens = Some(1_500);
        request.jobs[0].plan.limits.max_cost_micros = Some(0);
        request.jobs[0].plan.limits.max_seconds = Some(100);
        let aggregate = job_budget(&request.jobs[0].plan);
        assert_eq!(aggregate.max_tokens, 1_500);
        assert_eq!(aggregate.max_cost_micros, 0);
        assert_eq!(aggregate.max_seconds, 100);

        request.policy.budget.max_tokens = 1_000;
        assert!(Supervisor::dry_run_request(&request)
            .unwrap_err()
            .to_string()
            .contains("aggregate job budget exceeds the batch"));

        request.policy.budget.max_tokens = 10_000;
        assert!(Supervisor::dry_run_request(&request).is_ok());

        request.jobs[0].plan.limits.max_tokens = None;
        request.jobs[0].plan.limits.max_cost_micros = None;
        request.jobs[0].plan.limits.max_seconds = None;
        let legacy = job_budget(&request.jobs[0].plan);
        assert_eq!(legacy.max_tokens, 200);
        assert_eq!(legacy.max_cost_micros, 0);
        assert_eq!(legacy.max_seconds, 40);
    }

    #[test]
    fn recovery_releases_only_deterministic_pre_dispatch_reservations_without_checkpoints() {
        for (reviewer, attempt_number, stale_checkpoint) in [
            (false, 1, false),
            (true, 1, false),
            (false, 2, true),
            (true, 2, true),
        ] {
            let (_folder, request, _worker, _settings_path) = preview_fixture();
            let state = tempdir().unwrap();
            let supervisor = Supervisor::open(
                state
                    .path()
                    .join(format!("reserved-before-checkpoint-{reviewer}")),
            )
            .unwrap();
            seed_fixture_provider_observations(&supervisor.paths().root, &request.settings_path);
            let batch = supervisor.start(&request, now_ms()).unwrap();
            let jobs = JobsStore::open(supervisor.paths().jobs.clone()).unwrap();
            let job = jobs.get(batch.job_ids[0]).unwrap();
            let budgets = BudgetStore::open(supervisor.paths().budgets.clone()).unwrap();
            let task_id = if reviewer { "review" } else { "implement" };
            let id = if reviewer {
                format!(
                    "{}-{}-review-r0-a{attempt_number}",
                    request.batch_id, job.id
                )
            } else {
                reservation_id(&request.batch_id, job.id, task_id, attempt_number)
            };
            budgets
                .reserve(
                    &ReservationRequest {
                        reservation_id: id.clone(),
                        batch_id: request.batch_id.clone(),
                        job_id: job.id.to_string(),
                        task_id: task_id.into(),
                        provider_id: "codex-fixture".into(),
                        model_id: "model-fixture".into(),
                        runtime_id: "codex".into(),
                        max_tokens: 100,
                        max_cost_micros: Some(0),
                        max_seconds: 20,
                        is_paid: false,
                        pricing_source: None,
                        pricing_verified_at_ms: None,
                    },
                    now_ms().max(0) as u64,
                )
                .unwrap();

            if stale_checkpoint {
                let old_id = if reviewer {
                    format!("{}-{}-review-r0-a1", request.batch_id, job.id)
                } else {
                    reservation_id(&request.batch_id, job.id, task_id, 1)
                };
                let (name, payload) = if reviewer {
                    (
                        "verification_review_process".to_owned(),
                        json!({"state":"started","reservationId":old_id,"attemptNumber":1,"pid":1,"creationTime100ns":1}),
                    )
                } else {
                    (
                        format!("dispatch_{}", safe_checkpoint_key(task_id)),
                        json!({"state":"intent_persisted","reservationId":old_id,"taskId":task_id}),
                    )
                };
                Connection::open(supervisor.paths().jobs.clone())
                    .unwrap()
                    .execute(
                        "INSERT INTO job_checkpoints(job_id,name,fencing_token,at_ms,payload_json) VALUES(?1,?2,1,?3,?4)",
                        params![job.id, name, now_ms(), payload.to_string()],
                    )
                    .unwrap();
            }

            preserve_recovered_reservation_envelopes(
                &budgets,
                &jobs,
                &request,
                &job,
                &crate::jobs::RecoveryAttemptInfo {
                    attempt_number,
                    worker_id: "synthetic-worker".into(),
                    process_ref: None,
                },
                now_ms().max(0) as u64,
            )
            .unwrap();
            assert_eq!(
                budgets.get(&id).unwrap().state,
                crate::budgets::ReservationState::Released,
                "the reserve-before-checkpoint window must not strand concurrency"
            );
        }
    }

    #[test]
    fn foreground_host_bounds_paused_live_worker_but_returns_terminal_snapshot() {
        let (_folder, mut request, _worker, _settings_path) = preview_fixture();
        let state = tempdir().unwrap();
        let supervisor = Supervisor::open(state.path().join("foreground-paused-deadline")).unwrap();
        seed_fixture_provider_observations(&supervisor.paths().root, &request.settings_path);
        supervisor.start(&request, now_ms()).unwrap();
        let batches = BatchStore::open(&supervisor.paths().batches).unwrap();
        assert!(batches
            .reserve_worker(&request.batch_id, "expired-paused-worker", 0, now_ms())
            .unwrap());
        batches
            .worker_started("expired-paused-worker", 4_000_000_000, Some(1), now_ms())
            .unwrap();
        batches
            .connection()
            .unwrap()
            .execute(
                "UPDATE batches SET state='paused',deadline_ms=?2 WHERE batch_id=?1",
                params![request.batch_id, now_ms() - 1],
            )
            .unwrap();
        assert!(matches!(
            supervisor.wait_for_hosted_batch(&request.batch_id),
            Err(SupervisorError::Conflict(message)) if message.contains("absolute batch deadline")
        ));

        batches
            .connection()
            .unwrap()
            .execute(
                "UPDATE batches SET state='completed',deadline_ms=?2 WHERE batch_id=?1",
                params![request.batch_id, now_ms() - 1],
            )
            .unwrap();
        assert_eq!(
            supervisor
                .wait_for_hosted_batch(&request.batch_id)
                .unwrap()
                .state,
            BatchState::Completed
        );
    }

    #[test]
    fn post_admission_worker_launch_failure_is_typed_and_batch_remains_readable() {
        let (_folder, request, _worker, settings_path) = preview_fixture();
        let state = tempdir().unwrap();
        let supervisor = Supervisor::open(state.path().join("launch-error-state")).unwrap();
        seed_fixture_provider_observations(&supervisor.paths().root, &settings_path);
        let missing_worker = state.path().join("missing-worker.exe");
        let outcome = supervisor.start_and_launch(&request, &missing_worker, now_ms());
        assert!(
            matches!(&outcome, Err(SupervisorError::WorkerLaunch(_))),
            "expected a typed worker launch failure after admission, got {outcome:?}"
        );
        let admitted = supervisor.status(&request.batch_id).unwrap();
        assert_eq!(admitted.job_ids.len(), 1);
        assert!(matches!(
            admitted.state,
            BatchState::Starting | BatchState::Running
        ));
    }

    #[test]
    fn run_batch_resumes_paused_batch_hosted_without_resetting_attempts_or_deadline() {
        let (_folder, request, _worker, _settings_path) = preview_fixture();
        let state = tempdir().unwrap();
        let supervisor = Supervisor::open(state.path().join("hosted-pause-resume")).unwrap();
        seed_fixture_provider_observations(&supervisor.paths().root, &request.settings_path);
        let admitted = supervisor.start(&request, now_ms()).unwrap();
        let deadline = admitted.deadline_ms;
        supervisor
            .pause(&request.batch_id, "fixture pause", now_ms())
            .unwrap();

        let missing_worker = state.path().join("missing-hosted-worker.exe");
        assert!(matches!(
            supervisor.run_batch_foreground(&request, &missing_worker, now_ms()),
            Err(SupervisorError::WorkerLaunch(_))
        ));
        let resumed = supervisor.status(&request.batch_id).unwrap();
        assert_eq!(resumed.state, BatchState::Running);
        assert_eq!(resumed.deadline_ms, deadline);
        assert_eq!(resumed.job_ids, admitted.job_ids);
        assert_eq!(
            JobsStore::open(&supervisor.paths().jobs)
                .unwrap()
                .get(resumed.job_ids[0])
                .unwrap()
                .attempt_count,
            0
        );
    }

    #[test]
    fn public_run_batch_reattaches_consented_stale_admission_without_resetting_state() {
        let (folder, mut request, worker, _settings_path) = preview_fixture();
        request.policy.window_seconds = 600;
        request.dispatch_refresh_consent =
            crate::provider_observations::ModelFreeRefreshConsent::Providers {
                kinds: vec![ProviderKind::Codex],
            };
        let state = tempdir().unwrap();
        seed_fixture_provider_observations(state.path(), &request.settings_path);
        let preview = preview_batch_request(state.path(), &request, &worker).unwrap();
        let supervisor = Supervisor::open(state.path()).unwrap();
        let admitted = supervisor.start(&preview.request, now_ms()).unwrap();
        let deadline = admitted.deadline_ms;
        let future = admitted.created_at_ms + ROUTING_OBSERVATION_MAX_AGE_MS as i64 + 1_000;

        // The public protocol reconstructs the admission preview at its durable
        // creation time, then leaves current evidence validation to dispatch.
        // The fixture worker is intentionally not executable, so reaching its
        // launch error proves no stale-preview rejection or new admission ran.
        let result = crate::supervisor_protocol::dispatch_at(
            state.path(),
            crate::supervisor_protocol::SupervisorOperation::RunBatch,
            Some(preview.request.clone()),
            None,
            0,
            20,
            Some(&worker),
            Some(&preview.fingerprint),
            None,
            future,
        );
        assert!(
            matches!(result, Err(SupervisorError::WorkerLaunch(_))),
            "unexpected attach result: {result:?}"
        );
        let after = supervisor.status(&preview.request.batch_id).unwrap();
        assert_eq!(after.created_at_ms, admitted.created_at_ms);
        assert_eq!(after.deadline_ms, deadline);
        assert_eq!(after.job_ids, admitted.job_ids);
        let jobs = JobsStore::open(&supervisor.paths().jobs).unwrap();
        let job = jobs.get(admitted.job_ids[0]).unwrap();
        assert_eq!(job.state, JobState::Queued);
        assert_eq!(job.attempt_count, 0);
        let budgets = Connection::open(&supervisor.paths().budgets).unwrap();
        let reservation_count: i64 = budgets
            .query_row(
                "SELECT count(*) FROM budget_reservations WHERE batch_id=?1",
                [&preview.request.batch_id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(reservation_count, 0);
        drop(folder);
    }

    #[test]
    fn public_run_batch_reattach_binds_worker_and_rejects_drift_or_terminal_state() {
        let (_folder, mut request, worker, settings_path) = preview_fixture();
        request.policy.window_seconds = 600;
        request.dispatch_refresh_consent =
            crate::provider_observations::ModelFreeRefreshConsent::Providers {
                kinds: vec![ProviderKind::Codex],
            };
        let state = tempdir().unwrap();
        seed_fixture_provider_observations(state.path(), &settings_path);
        let preview = preview_batch_request(state.path(), &request, &worker).unwrap();
        let supervisor = Supervisor::open(state.path()).unwrap();
        let admitted = supervisor.start(&preview.request, now_ms()).unwrap();
        let future = admitted.created_at_ms + ROUTING_OBSERVATION_MAX_AGE_MS as i64 + 1_000;
        let dispatch = |input: BatchRequest, executable: &Path| {
            crate::supervisor_protocol::dispatch_at(
                state.path(),
                crate::supervisor_protocol::SupervisorOperation::RunBatch,
                Some(input),
                None,
                0,
                20,
                Some(executable),
                Some(&preview.fingerprint),
                None,
                future,
            )
        };

        let changed_worker = state.path().join("different-worker.exe");
        fs::write(&changed_worker, b"different worker bytes").unwrap();
        assert!(matches!(
            dispatch(preview.request.clone(), &changed_worker),
            Err(SupervisorError::Conflict(_))
        ));

        let mut changed_semantics = preview.request.clone();
        changed_semantics
            .route_commitments
            .get_mut("TEST-L001/implement")
            .unwrap()
            .runtime_identity_fingerprint = "changed-runtime".into();
        assert!(matches!(
            dispatch(changed_semantics, &worker),
            Err(SupervisorError::Conflict(_))
        ));

        let original_settings = fs::read(&settings_path).unwrap();
        let mut changed_settings = original_settings.clone();
        changed_settings.push(b' ');
        fs::write(&settings_path, changed_settings).unwrap();
        assert!(matches!(
            dispatch(preview.request.clone(), &worker),
            Err(SupervisorError::Conflict(_))
        ));
        fs::write(&settings_path, original_settings).unwrap();

        let connection = Connection::open(&supervisor.paths().batches).unwrap();
        connection
            .execute(
                "UPDATE batches SET state='completed',control='completed' WHERE batch_id=?1",
                [&preview.request.batch_id],
            )
            .unwrap();
        drop(connection);
        assert!(matches!(
            dispatch(preview.request.clone(), &worker),
            Err(SupervisorError::Conflict(_))
        ));
        assert_eq!(
            JobsStore::open(&supervisor.paths().jobs)
                .unwrap()
                .get(admitted.job_ids[0])
                .unwrap()
                .attempt_count,
            0
        );
        let budgets = Connection::open(&supervisor.paths().budgets).unwrap();
        let reservation_count: i64 = budgets
            .query_row(
                "SELECT count(*) FROM budget_reservations WHERE batch_id=?1",
                [&preview.request.batch_id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(reservation_count, 0);
    }

    #[test]
    fn public_run_batch_without_refresh_consent_still_refuses_expired_observations() {
        let (_folder, mut request, worker, settings_path) = preview_fixture();
        request.policy.window_seconds = 600;
        let state = tempdir().unwrap();
        seed_fixture_provider_observations(state.path(), &settings_path);
        let preview = preview_batch_request(state.path(), &request, &worker).unwrap();
        let supervisor = Supervisor::open(state.path()).unwrap();
        let admitted = supervisor.start(&preview.request, now_ms()).unwrap();
        let future = admitted.created_at_ms + ROUTING_OBSERVATION_MAX_AGE_MS as i64 + 1_000;
        let result = crate::supervisor_protocol::dispatch_at(
            state.path(),
            crate::supervisor_protocol::SupervisorOperation::RunBatch,
            Some(preview.request.clone()),
            None,
            0,
            20,
            Some(&worker),
            Some(&preview.fingerprint),
            None,
            future,
        );
        assert!(matches!(result, Err(SupervisorError::Conflict(_))));
        let after = supervisor.status(&preview.request.batch_id).unwrap();
        assert_eq!(after.deadline_ms, admitted.deadline_ms);
        assert_eq!(
            JobsStore::open(&supervisor.paths().jobs)
                .unwrap()
                .get(admitted.job_ids[0])
                .unwrap()
                .attempt_count,
            0
        );
        let budgets = Connection::open(&supervisor.paths().budgets).unwrap();
        let reservation_count: i64 = budgets
            .query_row(
                "SELECT count(*) FROM budget_reservations WHERE batch_id=?1",
                [&preview.request.batch_id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(reservation_count, 0);
    }

    #[cfg(windows)]
    #[test]
    fn foreground_host_detects_dead_registered_worker_without_provider_work() {
        let (_folder, request, _worker, _settings_path) = preview_fixture();
        let state = tempdir().unwrap();
        let supervisor = Supervisor::open(state.path().join("foreground-dead-worker")).unwrap();
        seed_fixture_provider_observations(&supervisor.paths().root, &request.settings_path);
        supervisor.start(&request, now_ms()).unwrap();
        let batches = BatchStore::open(&supervisor.paths().batches).unwrap();
        assert!(batches
            .reserve_worker(&request.batch_id, "dead-hosted-worker", 0, now_ms())
            .unwrap());
        batches
            .worker_started("dead-hosted-worker", 4_000_000_000, Some(1), now_ms())
            .unwrap();
        let started = std::time::Instant::now();
        assert!(matches!(
            supervisor.wait_for_hosted_batch(&request.batch_id),
            Err(SupervisorError::Conflict(message)) if message.contains("all hosted workers exited")
        ));
        assert!(started.elapsed() < Duration::from_secs(1));
        assert_eq!(
            batches.worker_slots(&request.batch_id).unwrap()[0].state,
            "failed"
        );
        assert_eq!(
            JobsStore::open(&supervisor.paths().jobs)
                .unwrap()
                .get(supervisor.status(&request.batch_id).unwrap().job_ids[0])
                .unwrap()
                .attempt_count,
            0
        );
    }

    #[test]
    fn foreground_host_refreshes_batch_after_worker_exit_probe() {
        let (_folder, request, _worker, _settings_path) = preview_fixture();
        let state = tempdir().unwrap();
        let supervisor = Supervisor::open(state.path().join("foreground-exit-race")).unwrap();
        seed_fixture_provider_observations(&supervisor.paths().root, &request.settings_path);
        supervisor.start(&request, now_ms()).unwrap();
        let mut committed_terminal = false;
        let finished = supervisor
            .wait_for_hosted_batch_with_liveness(&request.batch_id, |_, batches, snapshot| {
                assert_eq!(snapshot.state, BatchState::Running);
                // Reproduce the interleaving at the exact probe boundary: the
                // worker commits terminal state, exits, and liveness is false.
                batches.set_terminal(&request.batch_id, "completed", now_ms())?;
                committed_terminal = true;
                Ok(false)
            })
            .unwrap();
        assert!(committed_terminal);
        assert_eq!(finished.state, BatchState::Completed);
    }

    #[test]
    fn preview_explains_manual_routes_and_marks_unavailable_capabilities_blocked() {
        let (folder, request, _worker, settings_path) = preview_fixture();
        seed_fixture_provider_observations(folder.path(), &settings_path);
        let preview =
            Supervisor::dry_run_request_with_observations(&request, folder.path()).unwrap();
        assert_eq!(preview.issues[0].eligibility, PreviewEligibility::Eligible);
        assert_eq!(preview.issues[0].tasks[0].assignment_source, "explicit");
        assert_eq!(
            preview.issues[0].tasks[0].provider_id.as_deref(),
            Some("codex-fixture")
        );

        let mut blocked = request;
        blocked.jobs[0].plan.tasks[0].provider_id = Some("missing-provider".into());
        let preview =
            Supervisor::dry_run_request_with_observations(&blocked, folder.path()).unwrap();
        assert_eq!(preview.issues[0].eligibility, PreviewEligibility::Blocked);
        assert_eq!(preview.issues[0].tasks[0].assignment_source, "unavailable");
        assert!(!preview.issues[0].tasks[0].reasons.is_empty());

        let supervisor = Supervisor::open(folder.path().join("blocked-submit-state")).unwrap();
        assert!(matches!(
            supervisor.start(&blocked, 1),
            Err(SupervisorError::Invalid(_))
        ));
        assert!(supervisor.status(&blocked.batch_id).is_err());
    }

    #[test]
    fn status_returns_all_admitted_jobs_and_filters_oversized_checkpoints() {
        let (folder, mut request, _worker, _settings_path) = preview_fixture();
        let source_path = request.issue_sources["TEST-L001"].clone();
        for index in 2..=MAX_BATCH_JOBS {
            let issue_id = format!("TEST-L{index:03}");
            let mut job = request.jobs[0].clone();
            job.issue_id = issue_id.clone();
            job.idempotency_key = format!("status-fixture-{issue_id}");
            request.verification.push({
                let mut policy = request.verification[0].clone();
                policy.issue_id = issue_id.clone();
                policy
            });
            request.issue_sources.insert(issue_id, source_path.clone());
            request.jobs.push(job);
        }
        let data_root = folder.path().join("status-state");
        let supervisor = Supervisor::open(&data_root).unwrap();
        seed_fixture_provider_observations(&supervisor.paths().root, &request.settings_path);
        let started = supervisor.start(&request, 1).unwrap();
        let status = supervisor.status_view(&request.batch_id).unwrap();
        assert_eq!(started.job_ids.len(), MAX_BATCH_JOBS);
        assert_eq!(status.jobs.len(), MAX_BATCH_JOBS);

        let jobs = JobsStore::open(supervisor.paths().jobs.clone()).unwrap();
        let claim = jobs
            .claim_ids("status-worker", &[started.job_ids[0]], 2, 10_000)
            .unwrap()
            .unwrap();
        jobs.record_checkpoint(
            claim.job.id,
            "status-worker",
            claim.fencing_token,
            "implementation_proposed",
            &json!("x".repeat(5000)),
            3,
        )
        .unwrap();
        jobs.record_checkpoint(
            claim.job.id,
            "status-worker",
            claim.fencing_token,
            "runtime_usage",
            &json!({"tokens": 0}),
            4,
        )
        .unwrap();
        let status = supervisor.status_view(&request.batch_id).unwrap();
        assert!(status.jobs[0]
            .checkpoints
            .iter()
            .all(|checkpoint| checkpoint.key != "implementation_proposed"));
        assert!(status.jobs[0]
            .checkpoints
            .iter()
            .any(|checkpoint| checkpoint.key == "runtime_usage"));
        assert!(status.jobs[0].checkpoints_truncated);
        assert_eq!(status.jobs.len(), MAX_BATCH_JOBS);
    }

    #[test]
    fn status_marks_runtime_output_omitted_by_global_checkpoint_budget() {
        let (folder, mut request, _worker, _settings_path) = preview_fixture();
        let source_path = request.issue_sources["TEST-L001"].clone();
        for index in 2..=17 {
            let issue_id = format!("TEST-L{index:03}");
            let mut job = request.jobs[0].clone();
            job.issue_id = issue_id.clone();
            job.idempotency_key = format!("status-output-budget-{issue_id}");
            request.verification.push({
                let mut policy = request.verification[0].clone();
                policy.issue_id = issue_id.clone();
                policy
            });
            request.issue_sources.insert(issue_id, source_path.clone());
            request.jobs.push(job);
        }
        let supervisor = Supervisor::open(folder.path().join("status-output-budget")).unwrap();
        seed_fixture_provider_observations(&supervisor.paths().root, &request.settings_path);
        let admitted = supervisor.start(&request, 1).unwrap();
        let jobs = JobsStore::open(supervisor.paths().jobs.clone()).unwrap();
        for (index, job_id) in admitted.job_ids.iter().take(16).enumerate() {
            let worker_id = format!("status-budget-worker-{index}");
            let claim = jobs
                .claim_ids(&worker_id, &[*job_id], 2, 10_000)
                .unwrap()
                .unwrap();
            jobs.record_checkpoint(
                claim.job.id,
                &worker_id,
                claim.fencing_token,
                "implementation_proposed",
                &json!("x".repeat(4_000)),
                3,
            )
            .unwrap();
        }
        let output_id = admitted.job_ids[16];
        let output_claim = jobs
            .claim_ids("status-budget-output-worker", &[output_id], 2, 10_000)
            .unwrap()
            .unwrap();
        let suffix = "UnicodeEncodeError suffix🙂";
        let mut tail = "\0\u{1f}\t\r\n-é🙂".repeat(4_000);
        let tail_limit = 32 * 1024 - suffix.len();
        if tail.len() > tail_limit {
            let excess = tail.len() - tail_limit;
            trim_utf8_prefix(&mut tail, excess);
        }
        tail.push_str(suffix);
        assert!(tail.len() <= 32 * 1024);
        let key = format!(
            "runtime_output_{}",
            safe_checkpoint_key(&output_claim.job.source_fingerprint)
        );
        jobs.record_checkpoint(
            output_id,
            "status-budget-output-worker",
            output_claim.fencing_token,
            &key,
            &json!({"tail": tail, "stream": "stderr"}),
            3,
        )
        .unwrap();

        let status = supervisor.status_view(&request.batch_id).unwrap();
        assert_eq!(status.jobs.len(), 17);
        assert!(status.jobs[16].checkpoints_truncated);
        assert!(status.jobs[16]
            .checkpoints
            .iter()
            .all(|checkpoint| checkpoint.key != key));
        let total_status_bytes: usize = status
            .jobs
            .iter()
            .flat_map(|job| job.checkpoints.iter())
            .map(|checkpoint| serde_json::to_vec(&checkpoint.value).unwrap().len())
            .sum();
        assert!(total_status_bytes <= 64 * 1024);
    }

    #[test]
    fn status_projects_oversized_runtime_output_with_unicode_suffix_and_keeps_source_checkpoint() {
        let (folder, request, _worker, _settings_path) = preview_fixture();
        let supervisor = Supervisor::open(folder.path().join("status-output-state")).unwrap();
        seed_fixture_provider_observations(&supervisor.paths().root, &request.settings_path);
        let admitted = supervisor.start(&request, 1).unwrap();
        let jobs = JobsStore::open(supervisor.paths().jobs.clone()).unwrap();
        let claim = jobs
            .claim_ids("status-output-worker", &admitted.job_ids, 2, 10_000)
            .unwrap()
            .unwrap();

        let suffix = "UnicodeEncodeError: cp1252 could not encode '🙂' café";
        let pattern = "\0\u{1f}\t\r\n-é🙂";
        let mut tail = pattern.repeat(4_000);
        let tail_limit = 32 * 1024 - suffix.len();
        if tail.len() > tail_limit {
            let excess = tail.len() - tail_limit;
            trim_utf8_prefix(&mut tail, excess);
        }
        tail.push_str(suffix);
        assert!(tail.len() <= 32 * 1024);
        assert!(tail.chars().any(char::is_control));
        assert!(tail.contains('é') && tail.contains('🙂'));
        let original = json!({"tail": tail, "stream": "stdout"});
        assert!(serde_json::to_vec(&original).unwrap().len() > 4096);
        let key = format!(
            "runtime_output_{}",
            safe_checkpoint_key(&claim.job.source_fingerprint)
        );
        jobs.record_checkpoint(
            claim.job.id,
            "status-output-worker",
            claim.fencing_token,
            &key,
            &original,
            3,
        )
        .unwrap();

        let status = supervisor.status_view(&request.batch_id).unwrap();
        let checkpoint = status.jobs[0]
            .checkpoints
            .iter()
            .find(|checkpoint| checkpoint.key == key)
            .expect("bounded runtime output must remain visible in status");
        let projected = &checkpoint.value;
        let encoded = serde_json::to_vec(projected).unwrap();
        assert!(encoded.len() <= 4096);
        assert_eq!(projected["truncated"], true);
        assert_eq!(projected["originalBytes"], tail.len());
        assert!(projected["tail"].as_str().unwrap().ends_with(suffix));
        assert!(projected["tail"].as_str().unwrap().contains('🙂'));
        assert_eq!(projected["stream"], "stdout");
        let total_status_bytes: usize = status
            .jobs
            .iter()
            .flat_map(|job| job.checkpoints.iter())
            .map(|checkpoint| serde_json::to_vec(&checkpoint.value).unwrap().len())
            .sum();
        assert!(total_status_bytes <= 64 * 1024);
        assert_eq!(
            jobs.get_checkpoint(claim.job.id, &key).unwrap(),
            Some(original)
        );
    }

    #[test]
    fn output_tail_trimming_preserves_utf8_boundaries() {
        let mut output = "🙂".repeat(9_000);
        let requested = output.len() - 32 * 1024;
        trim_utf8_prefix(&mut output, requested);
        assert!(output.len() <= 32 * 1024);
        assert!(output.len() >= 32 * 1024 - 4);
        assert!(output.chars().all(|ch| ch == '🙂'));
    }

    #[test]
    fn worker_slot_launch_registration_is_durable_and_identity_bound() {
        let folder = tempdir().unwrap();
        let store = BatchStore::open(&folder.path().join("batches.sqlite3")).unwrap();
        store.connection().unwrap().execute(
            "INSERT INTO batches(batch_id,state,control,request_json,job_ids_json,created_at_ms,updated_at_ms,deadline_ms) VALUES('batch-a','running','run','{}','[]',1,1,999999)",
            [],
        ).unwrap();
        assert!(store.reserve_worker("batch-a", "worker-a", 0, 2).unwrap());
        let launching = store.worker_slot("batch-a", "worker-a").unwrap().unwrap();
        assert_eq!(launching.state, "launching");
        store.worker_started("worker-a", 42, Some(1234), 3).unwrap();
        let running = store.worker_slot("batch-a", "worker-a").unwrap().unwrap();
        assert_eq!(
            (
                running.pid,
                running.creation_time_100ns,
                running.state.as_str()
            ),
            (Some(42), Some(1234), "running")
        );
        assert!(!store.reserve_worker("batch-a", "worker-b", 0, 4).unwrap());
        // The launch handshake is one-shot: a duplicate or mismatched registration
        // must not overwrite the process identity the parent already persisted.
        assert!(matches!(
            store.worker_started("worker-a", 99, Some(8888), 5),
            Err(SupervisorError::Conflict(_))
        ));
        let still_registered = store.worker_slot("batch-a", "worker-a").unwrap().unwrap();
        assert_eq!(still_registered.pid, Some(42));
        assert_eq!(still_registered.creation_time_100ns, Some(1234));
    }

    #[test]
    fn batch_controls_reject_terminal_restart_and_keep_stop_distinct_from_cancel() {
        let folder = tempdir().unwrap();
        let store = BatchStore::open(&folder.path().join("batches.sqlite3")).unwrap();
        store.connection().unwrap().execute("INSERT INTO batches(batch_id,state,control,request_json,job_ids_json,created_at_ms,updated_at_ms,deadline_ms) VALUES('batch-a','running','run','{}','[]',1,1,999999)",[]).unwrap();
        assert_eq!(
            store
                .control("batch-a", "stopping", "graceful", 2)
                .unwrap()
                .state,
            BatchState::Stopping
        );
        assert!(store.control("batch-a", "cancelling", "cancel", 3).is_ok());
        store.set_terminal("batch-a", "cancelled", 4).unwrap();
        assert!(matches!(
            store.control("batch-a", "running", "restart", 5),
            Err(SupervisorError::Conflict(_))
        ));
    }

    #[test]
    fn pause_and_graceful_stop_block_new_worker_claim_slots() {
        let folder = tempdir().unwrap();
        let store = BatchStore::open(&folder.path().join("batches.sqlite3")).unwrap();
        store.connection().unwrap().execute(
            "INSERT INTO batches(batch_id,state,control,request_json,job_ids_json,created_at_ms,updated_at_ms,deadline_ms) VALUES('batch-a','running','run','{}','[]',1,1,999999)",
            [],
        ).unwrap();

        assert_eq!(
            store
                .control("batch-a", "paused", "hold current work", 2)
                .unwrap()
                .state,
            BatchState::Paused
        );
        assert!(matches!(
            store.reserve_worker("batch-a", "paused-worker", 0, 3),
            Err(SupervisorError::Conflict(_))
        ));
        assert_eq!(
            store
                .control("batch-a", "running", "resume", 4)
                .unwrap()
                .state,
            BatchState::Running
        );
        assert!(store
            .reserve_worker("batch-a", "running-worker", 0, 5)
            .unwrap());
        assert_eq!(
            store
                .control("batch-a", "stopping", "finish current work", 6)
                .unwrap()
                .state,
            BatchState::Stopping
        );
        assert!(matches!(
            store.reserve_worker("batch-a", "stopped-worker", 1, 7),
            Err(SupervisorError::Conflict(_))
        ));
        assert_eq!(
            store
                .control("batch-a", "cancelling", "cancel active work", 8)
                .unwrap()
                .state,
            BatchState::Cancelling
        );
    }

    fn post_provider_capture_fixture() -> (
        tempfile::TempDir,
        Supervisor,
        ClaimedJob,
        RepoIsolation,
        PathBuf,
        Vec<String>,
    ) {
        let (folder, mut request, _worker, _settings_path) = preview_fixture();
        let project = request.project_path.clone();
        git_fixture(&project, &["init"]);
        git_fixture(&project, &["config", "user.name", "Runner Fixture"]);
        git_fixture(
            &project,
            &["config", "user.email", "runner-fixture@example.invalid"],
        );
        fs::write(project.join("README.md"), "capture fixture base\n").unwrap();
        git_fixture(&project, &["add", "."]);
        git_fixture(&project, &["commit", "-m", "capture fixture base"]);
        request.verified_base_commit = git_fixture(&project, &["rev-parse", "HEAD"]);

        let state_root = folder.path().join("capture-state");
        let supervisor = Supervisor::open(state_root.clone()).unwrap();
        let jobs = JobsStore::open(&supervisor.paths().jobs).unwrap();
        jobs.submit(request.jobs[0].clone(), now_ms()).unwrap();
        let claim = jobs
            .claim("capture-worker", now_ms(), 60_000)
            .unwrap()
            .unwrap();
        let isolation =
            RepoIsolation::new(&project, &request.managed_worktree_root, &state_root).unwrap();
        let workspace = isolation
            .prepare_workspace(PrepareRequest {
                issue_id: claim.job.issue_id.clone(),
                issue_snapshot_fingerprint: claim.job.source_fingerprint.clone(),
                verified_base_commit: request.verified_base_commit.clone(),
                predecessor_commits: vec![],
                build_root: request.build_root.clone(),
            })
            .unwrap();
        let worktree = workspace.record.worktree_path.clone();
        let allowed = vec!["output.txt".to_owned(), "notes.txt".to_owned()];
        drop(workspace);
        (folder, supervisor, claim, isolation, worktree, allowed)
    }

    #[test]
    fn post_provider_out_of_scope_change_is_durable_needs_input_before_verification() {
        let (_folder, supervisor, claim, isolation, worktree, allowed) =
            post_provider_capture_fixture();
        fs::write(worktree.join("output.txt"), "allowed change").unwrap();
        fs::write(worktree.join("unexpected.txt"), "outside declared scope").unwrap();

        let proposed = supervisor
            .capture_post_provider_changes(
                &claim,
                &claim.worker_id,
                &isolation,
                &worktree,
                &allowed,
                "synthetic provider output",
                now_ms() + 60_000,
                &AtomicBool::new(false),
            )
            .unwrap();
        assert!(!proposed, "scope failure must stop before verification");

        let jobs = JobsStore::open(&supervisor.paths().jobs).unwrap();
        let job = jobs.get(claim.job.id).unwrap();
        assert_eq!(job.state, JobState::NeedsInput);
        assert_eq!(job.attempt_count, 1);
        let failure = jobs
            .get_checkpoint(job.id, "post_provider_capture_failure")
            .unwrap()
            .unwrap();
        assert_eq!(failure["statusKey"], "post_provider_capture_failure");
        assert_eq!(failure["state"], "needs_input");
        assert_eq!(failure["stage"], "allowed_scope_validation");
        assert!(failure["error"]
            .as_str()
            .unwrap()
            .contains("unexpected.txt"));
        assert!(jobs
            .get_checkpoint(job.id, "implementation_proposed")
            .unwrap()
            .is_none());
        assert!(jobs
            .get_checkpoint(job.id, "verification_review")
            .unwrap()
            .is_none());
        assert!(jobs
            .get_checkpoint(job.id, "verification_complete")
            .unwrap()
            .is_none());
    }

    #[test]
    fn post_provider_capture_records_one_of_two_allowed_changes() {
        let (_folder, supervisor, claim, isolation, worktree, allowed) =
            post_provider_capture_fixture();
        fs::write(worktree.join("output.txt"), "only changed file").unwrap();

        let proposed = supervisor
            .capture_post_provider_changes(
                &claim,
                &claim.worker_id,
                &isolation,
                &worktree,
                &allowed,
                "synthetic provider output",
                now_ms() + 60_000,
                &AtomicBool::new(false),
            )
            .unwrap();
        assert!(proposed);
        let record = isolation.recover_workspace(&claim.job.issue_id).unwrap();
        assert_eq!(record.owned_changes, vec!["output.txt"]);
        let jobs = JobsStore::open(&supervisor.paths().jobs).unwrap();
        assert!(jobs
            .get_checkpoint(claim.job.id, "implementation_proposed")
            .unwrap()
            .is_some());
        assert!(jobs
            .get_checkpoint(claim.job.id, "post_provider_capture_failure")
            .unwrap()
            .is_none());
    }

    #[test]
    fn post_provider_capture_cancel_wait_is_durable_needs_input_before_verification() {
        let (_folder, supervisor, claim, isolation, worktree, allowed) =
            post_provider_capture_fixture();
        fs::write(worktree.join("output.txt"), "provider result retained").unwrap();
        let held = isolation.lock_repository_metadata().unwrap();
        let cancelled = AtomicBool::new(true);

        let proposed = supervisor
            .capture_post_provider_changes(
                &claim,
                &claim.worker_id,
                &isolation,
                &worktree,
                &allowed,
                "synthetic provider output",
                now_ms() + 60_000,
                &cancelled,
            )
            .unwrap();
        assert!(!proposed);
        drop(held);

        let jobs = JobsStore::open(&supervisor.paths().jobs).unwrap();
        let job = jobs.get(claim.job.id).unwrap();
        assert_eq!(job.state, JobState::NeedsInput);
        let failure = jobs
            .get_checkpoint(job.id, "post_provider_capture_failure")
            .unwrap()
            .unwrap();
        assert_eq!(failure["stage"], "observed_changed_paths");
        assert!(failure["error"].as_str().unwrap().contains("cancelled"));
        assert!(jobs
            .get_checkpoint(job.id, "implementation_proposed")
            .unwrap()
            .is_none());
        assert!(jobs
            .get_checkpoint(job.id, "verification_review")
            .unwrap()
            .is_none());
        assert_eq!(
            fs::read_to_string(worktree.join("output.txt")).unwrap(),
            "provider result retained"
        );
    }

    #[test]
    fn post_provider_capture_detects_content_drift_during_metadata_wait() {
        let (_folder, supervisor, claim, isolation, worktree, allowed) =
            post_provider_capture_fixture();
        fs::write(worktree.join("output.txt"), "provider result").unwrap();
        let other = RepoIsolation::new(
            isolation.repository_root(),
            worktree.parent().unwrap().to_path_buf(),
            supervisor.paths().root.clone(),
        )
        .unwrap();
        let changed_file = worktree.join("output.txt");
        let (held_tx, held_rx) = std::sync::mpsc::channel();
        let (wait_tx, wait_rx) = std::sync::mpsc::sync_channel(1);
        let mutator = thread::spawn(move || {
            let _metadata = other.lock_repository_metadata().unwrap();
            held_tx.send(()).unwrap();
            wait_rx.recv_timeout(Duration::from_secs(10)).unwrap();
            fs::write(changed_file, "external content drift").unwrap();
        });
        held_rx.recv().unwrap();

        let capture_wait_signal = wait_tx.clone();
        let supervisor_for_capture = supervisor.clone();
        let claim_for_capture = claim.clone();
        let worktree_for_capture = worktree.clone();
        let capture = thread::spawn(move || {
            crate::isolation::set_metadata_lock_wait_observer(move || {
                let _ = capture_wait_signal.try_send(());
            });
            supervisor_for_capture
                .capture_post_provider_changes(
                    &claim_for_capture,
                    &claim_for_capture.worker_id,
                    &isolation,
                    &worktree_for_capture,
                    &allowed,
                    "synthetic provider output",
                    now_ms() + 60_000,
                    &AtomicBool::new(false),
                )
                .unwrap()
        });
        let proposed = capture.join().unwrap();
        mutator.join().unwrap();
        assert!(!proposed, "changed source must not enter verification");

        let jobs = JobsStore::open(&supervisor.paths().jobs).unwrap();
        let job = jobs.get(claim.job.id).unwrap();
        assert_eq!(job.state, JobState::NeedsInput);
        let failure = jobs
            .get_checkpoint(job.id, "post_provider_capture_failure")
            .unwrap()
            .unwrap();
        assert_eq!(failure["stage"], "source_changed_during_metadata_wait");
        assert!(jobs
            .get_checkpoint(job.id, "implementation_proposed")
            .unwrap()
            .is_none());
        assert!(jobs
            .get_checkpoint(job.id, "verification_review")
            .unwrap()
            .is_none());
        assert_eq!(
            fs::read_to_string(worktree.join("output.txt")).unwrap(),
            "external content drift"
        );
    }
}
fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::ZERO)
        .as_millis()
        .min(i64::MAX as u128) as i64
}
