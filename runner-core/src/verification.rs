//! Exact-source verification and review gates for issue jobs.
//!
//! This module records evidence only after observing the check itself. A worker's
//! claim that checks passed is never accepted as verification evidence.

use crate::contracts::{ContractEvidence, ContractSpec, ContractStatus};
use crate::jobs::{JobsError, JobsStore, VerificationEvidence};
use rusqlite::OptionalExtension;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::io::Read;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::time::Duration;

pub const MAX_REPAIR_ROUNDS: u8 = 2;
pub const DEFAULT_OUTPUT_LIMIT: usize = 64 * 1024;
const MAX_SOURCE_FILES: usize = 20_000;
const MAX_SOURCE_BYTES: usize = 64 * 1024 * 1024;
const MAX_TRACKED_DIFF_BYTES: usize = 16 * 1024 * 1024;
const MAX_ISSUE_MARKDOWN_BYTES: usize = 2 * 1024 * 1024;
const MAX_DELIVERY_REPORT_BYTES: usize = 256 * 1024;

#[derive(Debug)]
pub enum VerificationError {
    Invalid(String),
    Io(String),
    NeedsInput(String),
    CheckFailed(String),
    StaleSource { before: String, after: String },
    StaleIssueSource { before: String, after: String },
    GateGuardFailed(String),
    ReviewFailed(String),
    ManualAcceptanceRequired(Vec<String>),
    Jobs(JobsError),
}

impl std::fmt::Display for VerificationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Invalid(s) => write!(f, "invalid verification request: {s}"),
            Self::Io(s) => write!(f, "verification I/O failed: {s}"),
            Self::NeedsInput(s) => write!(f, "needs_input: {s}"),
            Self::CheckFailed(s) => write!(f, "verification check failed: {s}"),
            Self::StaleSource { .. } => {
                f.write_str("source changed while verification was running")
            }
            Self::StaleIssueSource { .. } => {
                f.write_str("authoritative issue Markdown changed during verification")
            }
            Self::GateGuardFailed(message) => {
                write!(f, "verification gate ownership check failed: {message}")
            }
            Self::ReviewFailed(s) => write!(f, "independent review did not pass: {s}"),
            Self::ManualAcceptanceRequired(items) => {
                write!(f, "manual acceptance required: {}", items.join(", "))
            }
            Self::Jobs(e) => write!(f, "job verification failed: {e}"),
        }
    }
}
impl std::error::Error for VerificationError {}
impl From<JobsError> for VerificationError {
    fn from(e: JobsError) -> Self {
        Self::Jobs(e)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct SourceSnapshot {
    pub head: String,
    /// Digest over HEAD, tracked diff, untracked path list, and untracked contents.
    pub fingerprint: String,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CheckAdapter {
    AgentsDeliveryContract,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct CheckSpec {
    pub id: String,
    pub program: PathBuf,
    pub args: Vec<String>,
    /// Repository-relative working directory. Absolute paths and parent traversal are rejected.
    pub cwd: PathBuf,
    pub timeout_ms: u64,
    pub output_limit_bytes: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub adapter: Option<CheckAdapter>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub contract: Option<ContractSpec>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct CheckReceipt {
    pub check_id: String,
    pub source: SourceSnapshot,
    pub exit_code: Option<i32>,
    pub timed_out: bool,
    #[serde(default)]
    pub cancelled: bool,
    #[serde(default)]
    pub output_incomplete: bool,
    pub truncated: bool,
    pub output_bytes: usize,
    pub passed: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub contract: Option<ContractEvidence>,
}

pub trait CheckExecutor: Send + Sync {
    fn execute(&self, repository: &Path, check: &CheckSpec) -> Result<CheckOutput, String>;

    fn execute_managed(
        &self,
        repository: &Path,
        check: &CheckSpec,
        _cancel: &AtomicBool,
        _on_spawn: &mut dyn FnMut(crate::process::ProcessIdentity) -> Result<(), String>,
    ) -> Result<CheckOutput, String> {
        self.execute(repository, check)
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct CheckOutput {
    pub exit_code: Option<i32>,
    pub timed_out: bool,
    #[serde(default)]
    pub cancelled: bool,
    #[serde(default)]
    pub output_incomplete: bool,
    pub truncated: bool,
    pub stdout: String,
    pub stderr: String,
}

pub struct ProcessCheckExecutor;
impl ProcessCheckExecutor {
    pub fn execute_managed_in_build_output(
        &self,
        repository: &Path,
        check: &CheckSpec,
        build_output: &Path,
        cancel: &AtomicBool,
        mut on_spawn: impl FnMut(crate::process::ProcessIdentity) -> Result<(), String>,
    ) -> Result<CheckOutput, String> {
        validate_check(check)?;
        let root = repository.canonicalize().map_err(|e| e.to_string())?;
        let cwd = root
            .join(&check.cwd)
            .canonicalize()
            .map_err(|e| e.to_string())?;
        if !cwd.starts_with(&root) || !cwd.is_dir() {
            return Err("check cwd escapes repository or is not a directory".into());
        }
        let build_output = build_output.canonicalize().map_err(|e| e.to_string())?;
        if !build_output.is_dir()
            || build_output.starts_with(&root)
            || root.starts_with(&build_output)
        {
            return Err("build output must be a directory separate from the repository".into());
        }
        let output_limit = check
            .output_limit_bytes
            .min(DEFAULT_OUTPUT_LIMIT * 16)
            .div_ceil(2);
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let mut truncated = false;
        let mut spawn_error = None;
        let result = crate::process::run_streamed(
            &check.program,
            &check.args,
            &cwd,
            &[(
                "CARGO_TARGET_DIR".into(),
                build_output.as_os_str().to_owned(),
            )],
            &[],
            Duration::from_millis(check.timeout_ms),
            output_limit,
            cancel,
            |identity| match on_spawn(identity) {
                Ok(()) => crate::process::ProcessControl::Continue,
                Err(error) => {
                    spawn_error = Some(error);
                    crate::process::ProcessControl::Cancel
                }
            },
            |output| {
                truncated |= output.truncated;
                match output.stream {
                    crate::process::OutputStream::Stdout => stdout.extend_from_slice(&output.bytes),
                    crate::process::OutputStream::Stderr => stderr.extend_from_slice(&output.bytes),
                }
                crate::process::ProcessControl::Continue
            },
        )
        .map_err(|e| e.to_string())?;
        if let Some(error) = spawn_error {
            return Err(format!("check_process_registration_failed: {error}"));
        }
        Ok(CheckOutput {
            exit_code: result.status.and_then(|s| s.code()),
            timed_out: result.timed_out,
            cancelled: result.cancelled,
            output_incomplete: result.output_incomplete,
            truncated,
            stdout: String::from_utf8_lossy(&stdout).into_owned(),
            stderr: String::from_utf8_lossy(&stderr).into_owned(),
        })
    }
    /// Execute a check with Cargo artifacts routed to the supervisor-owned build
    /// output directory. This remains a backend-only context; callers cannot
    /// select the directory through a serialized check request.
    pub fn execute_in_build_output(
        &self,
        repository: &Path,
        check: &CheckSpec,
        build_output: &Path,
    ) -> Result<CheckOutput, String> {
        validate_check(check)?;
        let root = repository.canonicalize().map_err(|e| e.to_string())?;
        let cwd = root
            .join(&check.cwd)
            .canonicalize()
            .map_err(|e| e.to_string())?;
        if !cwd.starts_with(&root) || !cwd.is_dir() {
            return Err("check cwd escapes repository or is not a directory".into());
        }
        let build_output = build_output.canonicalize().map_err(|e| e.to_string())?;
        if !build_output.is_dir()
            || build_output.starts_with(&root)
            || root.starts_with(&build_output)
        {
            return Err("build output must be a directory separate from the repository".into());
        }
        let result = crate::process::capture_in_build_output(
            &check.program,
            &check.args,
            &cwd,
            Duration::from_millis(check.timeout_ms),
            check
                .output_limit_bytes
                .min(DEFAULT_OUTPUT_LIMIT * 16)
                .div_ceil(2),
            &build_output,
        )
        .map_err(|e| e.to_string())?;
        Ok(CheckOutput {
            exit_code: result.status.and_then(|s| s.code()),
            timed_out: result.timed_out,
            cancelled: false,
            output_incomplete: false,
            truncated: result.truncated,
            stdout: String::from_utf8_lossy(&result.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&result.stderr).into_owned(),
        })
    }
}
impl CheckExecutor for ProcessCheckExecutor {
    fn execute(&self, repository: &Path, check: &CheckSpec) -> Result<CheckOutput, String> {
        validate_check(check)?;
        let root = repository.canonicalize().map_err(|e| e.to_string())?;
        let cwd = root
            .join(&check.cwd)
            .canonicalize()
            .map_err(|e| e.to_string())?;
        if !cwd.starts_with(&root) || !cwd.is_dir() {
            return Err("check cwd escapes repository or is not a directory".into());
        }
        let result = crate::process::capture(
            &check.program,
            &check.args,
            &cwd,
            Duration::from_millis(check.timeout_ms),
            check
                .output_limit_bytes
                .min(DEFAULT_OUTPUT_LIMIT * 16)
                .div_ceil(2),
        )
        .map_err(|e| e.to_string())?;
        Ok(CheckOutput {
            exit_code: result.status.and_then(|s| s.code()),
            timed_out: result.timed_out,
            cancelled: false,
            output_incomplete: false,
            truncated: result.truncated,
            stdout: String::from_utf8_lossy(&result.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&result.stderr).into_owned(),
        })
    }
}

fn validate_authoritative_issue_source(
    store: &JobsStore,
    job_id: i64,
    path: &Path,
    expected_revision: &str,
) -> Result<(), VerificationError> {
    if !path.is_absolute() {
        return Err(VerificationError::Invalid(
            "authoritative issue source path must be absolute".into(),
        ));
    }
    let canonical = path
        .canonicalize()
        .map_err(|error| VerificationError::Io(error.to_string()))?;
    let mut bytes = Vec::new();
    std::fs::File::open(&canonical)
        .map_err(|error| VerificationError::Io(error.to_string()))?
        .take((MAX_ISSUE_MARKDOWN_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|error| VerificationError::Io(error.to_string()))?;
    if bytes.len() > MAX_ISSUE_MARKDOWN_BYTES {
        return Err(VerificationError::Invalid(
            "authoritative issue Markdown exceeds the 2 MiB limit".into(),
        ));
    }
    let raw = std::str::from_utf8(&bytes)
        .map_err(|error| VerificationError::Invalid(error.to_string()))?;
    let actual_revision = crate::issues::revision(raw);
    if actual_revision != expected_revision {
        return Err(VerificationError::StaleIssueSource {
            before: expected_revision.to_owned(),
            after: actual_revision,
        });
    }
    let parsed = crate::issues::parse_issue_markdown(raw)
        .map_err(|error| VerificationError::Invalid(error.to_string()))?;
    let expected_id = store.get(job_id)?.issue_id;
    if parsed.id() != expected_id {
        return Err(VerificationError::StaleIssueSource {
            before: format!("queued issue ID {expected_id}"),
            after: format!("Markdown issue ID {}", parsed.id()),
        });
    }
    Ok(())
}

pub(crate) fn validate_check(check: &CheckSpec) -> Result<(), String> {
    if check.id.trim().is_empty() || check.id.len() > 128 || check.program.as_os_str().is_empty() {
        return Err("check id and program are required".into());
    }
    if check.timeout_ms == 0 || check.timeout_ms > 30 * 60 * 1000 {
        return Err("timeout must be between 1 ms and 30 minutes".into());
    }
    if check.output_limit_bytes == 0 || check.output_limit_bytes > DEFAULT_OUTPUT_LIMIT * 16 {
        return Err("output limit must be between 1 byte and 1 MiB".into());
    }
    if let Some(contract) = &check.contract {
        if check.adapter.is_some() {
            return Err("contract cannot be combined with a legacy adapter".into());
        }
        crate::contracts::validate_spec(contract, &contract.criteria)?;
    }
    if check.adapter == Some(CheckAdapter::AgentsDeliveryContract) {
        if !check.program.is_absolute()
            || check.cwd != Path::new(".")
            || check.output_limit_bytes > MAX_DELIVERY_REPORT_BYTES
            || check.args.len() != 6
            || !Path::new(&check.args[0]).is_absolute()
            || check.args[1] != "report"
            || check.args[2] != "--repo"
            || check.args[3] != "."
            || check.args[4] != "--record"
            || !Path::new(&check.args[5]).is_absolute()
        {
            return Err("agents_delivery_contract requires absolute Python/helper/record paths, cwd '.', exact report argv, and output limit at most 256 KiB".into());
        }
    }
    if check.cwd.is_absolute()
        || check.cwd.components().any(|c| {
            matches!(
                c,
                Component::ParentDir | Component::RootDir | Component::Prefix(_)
            )
        })
    {
        return Err("check cwd must stay inside the repository".into());
    }
    Ok(())
}

fn validate_adapter_record_path(repository: &Path, check: &CheckSpec) -> Result<(), String> {
    if check.adapter != Some(CheckAdapter::AgentsDeliveryContract) {
        return Ok(());
    }
    let repository = repository.canonicalize().map_err(|e| e.to_string())?;
    let helper = Path::new(&check.args[0])
        .canonicalize()
        .map_err(|e| format!("delivery-contract helper is unavailable: {e}"))?;
    if !helper.is_file() {
        return Err("delivery-contract helper must be a file".into());
    }
    let record = Path::new(&check.args[5])
        .canonicalize()
        .map_err(|e| format!("delivery-contract record is unavailable: {e}"))?;
    if !record.is_file() || record.starts_with(&repository) {
        return Err(
            "delivery-contract record must be an existing file outside the worktree".into(),
        );
    }
    Ok(())
}

fn validate_delivery_contract_report(
    stdout: &str,
    repository: &Path,
    expected: &SourceSnapshot,
) -> Result<(), VerificationError> {
    if stdout.len() > MAX_DELIVERY_REPORT_BYTES {
        return Err(VerificationError::NeedsInput(
            "delivery-contract report exceeded its output bound".into(),
        ));
    }
    let report: serde_json::Value = serde_json::from_str(stdout).map_err(|_| {
        VerificationError::NeedsInput("delivery-contract report was malformed".into())
    })?;
    let ready = report.get("ready").and_then(serde_json::Value::as_bool) == Some(true);
    let status = report.get("status").and_then(serde_json::Value::as_str) == Some("ready");
    if !ready || !status {
        return Err(VerificationError::NeedsInput(
            "delivery-contract report is not ready".into(),
        ));
    }
    let reported_repository = report.get("repository").ok_or_else(|| {
        VerificationError::NeedsInput("delivery-contract repository evidence is missing".into())
    })?;
    let reported_path = reported_repository
        .get("path")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| {
            VerificationError::NeedsInput("delivery-contract repository path is missing".into())
        })?;
    let reported_path = Path::new(reported_path).canonicalize().map_err(|_| {
        VerificationError::NeedsInput("delivery-contract repository path is unavailable".into())
    })?;
    let actual_path = repository
        .canonicalize()
        .map_err(|e| VerificationError::Io(e.to_string()))?;
    if reported_path != actual_path {
        return Err(VerificationError::NeedsInput(
            "delivery-contract report belongs to another repository".into(),
        ));
    }
    let reported_head = reported_repository
        .get("head")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| {
            VerificationError::NeedsInput("delivery-contract repository HEAD is missing".into())
        })?;
    if reported_head != expected.head {
        return Err(VerificationError::NeedsInput(
            "delivery-contract report is for another source HEAD".into(),
        ));
    }
    if reported_repository
        .get("fingerprint")
        .and_then(serde_json::Value::as_str)
        .map_or(true, |fingerprint| fingerprint.trim().is_empty())
    {
        return Err(VerificationError::NeedsInput(
            "delivery-contract source fingerprint is missing".into(),
        ));
    }
    Ok(())
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ReviewerIdentity {
    pub task_id: String,
    pub provider_id: String,
    pub model_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ReviewRequest {
    pub job_id: i64,
    pub worker_task_id: String,
    /// Identity of the authoritative queued issue Markdown, distinct from checkout contents.
    pub issue_fingerprint: String,
    pub source: SourceSnapshot,
    pub reviewer: ReviewerIdentity,
    pub check_receipts: Vec<CheckReceipt>,
    pub acceptance_criteria: Vec<String>,
}

/// Serializable request envelope for the supervisor/configuration adapter. The issue
/// fingerprint identifies the queued Markdown; `code_snapshot` independently identifies
/// the checkout contents that checks and review must observe.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct VerificationEnvelope {
    pub issue_fingerprint: String,
    pub worker_task_id: String,
    pub code_snapshot: SourceSnapshot,
    pub checks: Vec<CheckSpec>,
    pub reviewer: ReviewerIdentity,
    pub acceptance_criteria: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ReviewResult {
    pub passed: bool,
    pub findings: Vec<String>,
    pub manual_acceptance: Vec<String>,
    pub details: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum VerificationOutcome {
    Verified {
        source: SourceSnapshot,
        checks: Vec<CheckReceipt>,
        review: ReviewResult,
    },
    NeedsManualAcceptance {
        source: SourceSnapshot,
        checks: Vec<CheckReceipt>,
        review: ReviewResult,
    },
    NeedsInput {
        reason: String,
    },
}

pub trait ReviewExecutor: Send + Sync {
    fn review(&self, request: &ReviewRequest) -> Result<ReviewResult, String>;
}

impl<T: ReviewExecutor + ?Sized> ReviewExecutor for Box<T> {
    fn review(&self, request: &ReviewRequest) -> Result<ReviewResult, String> {
        (**self).review(request)
    }
}

/// Static scope planning only; execution still must prove every contract.
pub(crate) fn requires_independent_review(envelope: &VerificationEnvelope) -> Result<bool, String> {
    let mut owned = std::collections::HashSet::new();
    for check in &envelope.checks {
        validate_check(check)?;
        if let Some(contract) = &check.contract {
            crate::contracts::validate_spec(contract, &envelope.acceptance_criteria)?;
            for criterion in &contract.criteria {
                if !owned.insert(criterion.as_str()) {
                    return Err("contract criterion has overlapping ownership".into());
                }
            }
        }
    }
    Ok(owned.is_empty()
        || envelope
            .acceptance_criteria
            .iter()
            .any(|criterion| !owned.contains(criterion.as_str())))
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct RepairProgress {
    pub round: u8,
    pub state: RepairState,
    /// Snapshot at repair dispatch; completion records the resulting source separately.
    pub source_fingerprint: String,
    pub result_source_fingerprint: Option<String>,
    pub summary: String,
}
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RepairState {
    Started,
    Completed,
    Interrupted,
    Exhausted,
}

pub struct VerificationService<E, R> {
    pub checks: E,
    pub reviewer: R,
}
impl<E: CheckExecutor, R: ReviewExecutor> VerificationService<E, R> {
    pub fn run_check(
        &self,
        repository: &Path,
        expected: &SourceSnapshot,
        spec: &CheckSpec,
    ) -> Result<CheckReceipt, VerificationError> {
        self.run_check_with_executor(repository, expected, spec, |checks| {
            checks.execute(repository, spec)
        })
    }

    fn run_check_managed(
        &self,
        repository: &Path,
        expected: &SourceSnapshot,
        spec: &CheckSpec,
        cancel: &AtomicBool,
        on_spawn: &mut dyn FnMut(crate::process::ProcessIdentity) -> Result<(), String>,
    ) -> Result<CheckReceipt, VerificationError> {
        self.run_check_with_executor(repository, expected, spec, |checks| {
            checks.execute_managed(repository, spec, cancel, on_spawn)
        })
    }

    fn run_check_with_executor(
        &self,
        repository: &Path,
        expected: &SourceSnapshot,
        spec: &CheckSpec,
        execute: impl FnOnce(&E) -> Result<CheckOutput, String>,
    ) -> Result<CheckReceipt, VerificationError> {
        validate_check(spec).map_err(VerificationError::Invalid)?;
        if spec.adapter == Some(CheckAdapter::AgentsDeliveryContract) {
            validate_adapter_record_path(repository, spec).map_err(|error| {
                VerificationError::NeedsInput(format!(
                    "delivery-contract dependency or record is unavailable: {error}"
                ))
            })?;
        }
        let before = source_snapshot(repository)?;
        if &before != expected {
            return Err(VerificationError::StaleSource {
                before: expected.fingerprint.clone(),
                after: before.fingerprint,
            });
        }
        let out = execute(&self.checks).map_err(|error| {
            if spec.adapter == Some(CheckAdapter::AgentsDeliveryContract) {
                VerificationError::NeedsInput(
                    "delivery-contract adapter command failed or was unavailable; inspect the configured interpreter and paths".into(),
                )
            } else if spec.contract.is_some() {
                VerificationError::NeedsInput("contract command was unavailable; no contract conclusion is established".into())
            } else if let Some(detail) = error.strip_prefix("check_process_registration_failed:") {
                VerificationError::NeedsInput(format!(
                    "check process identity could not be persisted; cancellation of the owned process was requested; inspect process evidence: {}",
                    detail.trim()
                ))
            } else {
                VerificationError::Io(error)
            }
        })?;
        let output_bytes = out.stdout.len().saturating_add(out.stderr.len());
        if spec.adapter == Some(CheckAdapter::AgentsDeliveryContract) {
            if out.exit_code != Some(0)
                || out.timed_out
                || out.cancelled
                || out.output_incomplete
                || out.truncated
                || output_bytes > spec.output_limit_bytes.min(MAX_DELIVERY_REPORT_BYTES)
            {
                return Err(VerificationError::NeedsInput(
                    "delivery-contract report command did not complete with bounded output".into(),
                ));
            }
            validate_delivery_contract_report(&out.stdout, repository, &before)?;
        }
        if spec.contract.is_none()
            && output_bytes > spec.output_limit_bytes.min(DEFAULT_OUTPUT_LIMIT * 16)
        {
            return Err(VerificationError::Invalid(
                "check executor exceeded its configured output limit".into(),
            ));
        }
        let after = source_snapshot(repository)?;
        if before != after {
            return Err(VerificationError::StaleSource {
                before: before.fingerprint,
                after: after.fingerprint,
            });
        }
        let contract = spec.contract.as_ref().map(|declaration| {
            let complete = out.exit_code == Some(0)
                && !out.timed_out
                && !out.cancelled
                && !out.output_incomplete
                && !out.truncated
                && output_bytes
                    <= spec
                        .output_limit_bytes
                        .min(crate::contracts::MAX_CONTRACT_REPORT_BYTES);
            crate::contracts::evaluate(declaration, &out.stdout, complete)
        });
        let passed = match &contract {
            Some(evidence) => evidence.status == ContractStatus::Passed,
            None => {
                out.exit_code == Some(0)
                    && !out.timed_out
                    && !out.cancelled
                    && !out.output_incomplete
            }
        };
        Ok(CheckReceipt {
            check_id: spec.id.clone(),
            source: before,
            exit_code: out.exit_code,
            timed_out: out.timed_out,
            cancelled: out.cancelled,
            output_incomplete: out.output_incomplete,
            truncated: out.truncated,
            output_bytes,
            passed,
            contract,
        })
    }

    pub fn review(&self, request: &ReviewRequest) -> Result<ReviewResult, VerificationError> {
        if request.worker_task_id.trim().is_empty()
            || request.reviewer.task_id.trim().is_empty()
            || request.worker_task_id == request.reviewer.task_id
            || request.issue_fingerprint.trim().is_empty()
            || request.reviewer.provider_id.trim().is_empty()
            || request.reviewer.model_id.trim().is_empty()
            || request.acceptance_criteria.len() > 256
            || request
                .acceptance_criteria
                .iter()
                .any(|c| c.trim().is_empty() || c.len() > 16 * 1024)
        {
            return Err(VerificationError::Invalid(
                "reviewer must have a distinct task identity and configured provider/model".into(),
            ));
        }
        if request.check_receipts.is_empty()
            || request
                .check_receipts
                .iter()
                .any(|r| !r.passed || r.source != request.source)
        {
            return Err(VerificationError::Invalid(
                "review requires passing checks for the exact source snapshot".into(),
            ));
        }
        let mut owned = std::collections::HashSet::new();
        for receipt in &request.check_receipts {
            if let Some(evidence) = &receipt.contract {
                if receipt.exit_code != Some(0)
                    || receipt.timed_out
                    || receipt.cancelled
                    || receipt.output_incomplete
                    || receipt.truncated
                {
                    return Err(VerificationError::Invalid(
                        "contract process evidence is incomplete".into(),
                    ));
                }
                crate::contracts::validate_evidence(evidence, &request.acceptance_criteria)
                    .map_err(VerificationError::Invalid)?;
                for criterion in &evidence.spec.criteria {
                    if !owned.insert(criterion.clone()) {
                        return Err(VerificationError::Invalid(
                            "contract criterion has overlapping ownership".into(),
                        ));
                    }
                }
            }
        }
        let mut remaining = request.clone();
        remaining
            .acceptance_criteria
            .retain(|criterion| !owned.contains(criterion));
        if remaining.acceptance_criteria.is_empty() && !owned.is_empty() {
            return Ok(ReviewResult { passed: true, findings: vec![], manual_acceptance: vec![],
                details: "All declared acceptance criteria passed their finite contracts; no uncovered review criteria remain. No broader correctness claim is made.".into() });
        }
        let result = self
            .reviewer
            .review(&remaining)
            .map_err(VerificationError::ReviewFailed)?;
        validate_review_result(&result)?;
        Ok(result)
    }

    /// Executes checks and the independent review itself, then persists the observed evidence.
    /// No caller-provided `passed` flag or receipt can authorize the gate.
    pub fn verify_job(
        &self,
        repository: &Path,
        store: &JobsStore,
        job_id: i64,
        envelope: &VerificationEnvelope,
        now_ms: i64,
    ) -> Result<VerificationOutcome, VerificationError> {
        self.verify_job_inner(repository, store, job_id, envelope, None, None, now_ms)
    }

    /// Production verifier entry point bound to the authoritative issue Markdown file.
    /// The issue is reread before checks begin and immediately before any terminal gate
    /// mutation, so edits during checks or review cannot authorize stale evidence.
    pub fn verify_job_with_issue_source(
        &self,
        repository: &Path,
        store: &JobsStore,
        job_id: i64,
        envelope: &VerificationEnvelope,
        issue_path: &Path,
        now_ms: i64,
    ) -> Result<VerificationOutcome, VerificationError> {
        self.verify_job_inner(
            repository,
            store,
            job_id,
            envelope,
            Some(issue_path),
            None,
            now_ms,
        )
    }

    /// Production verifier entry point with a final ownership/fencing check directly
    /// before the completion gate is persisted.
    pub fn verify_job_with_issue_source_and_gate_guard(
        &self,
        repository: &Path,
        store: &JobsStore,
        job_id: i64,
        envelope: &VerificationEnvelope,
        issue_path: Option<&Path>,
        gate_guard: impl Fn() -> Result<(), String>,
        now_ms: i64,
    ) -> Result<VerificationOutcome, VerificationError> {
        self.verify_job_inner(
            repository,
            store,
            job_id,
            envelope,
            issue_path,
            Some(&gate_guard),
            now_ms,
        )
    }

    pub fn verify_job_with_issue_source_and_gate_guard_managed(
        &self,
        repository: &Path,
        store: &JobsStore,
        job_id: i64,
        envelope: &VerificationEnvelope,
        issue_path: Option<&Path>,
        gate_guard: impl Fn() -> Result<(), String>,
        cancel: &AtomicBool,
        on_check_spawn: &mut dyn FnMut(
            usize,
            crate::process::ProcessIdentity,
        ) -> Result<(), String>,
        now_ms: i64,
    ) -> Result<VerificationOutcome, VerificationError> {
        self.verify_job_inner_managed(
            repository,
            store,
            job_id,
            envelope,
            issue_path,
            Some(&gate_guard),
            Some(cancel),
            Some(on_check_spawn),
            now_ms,
        )
    }

    fn verify_job_inner(
        &self,
        repository: &Path,
        store: &JobsStore,
        job_id: i64,
        envelope: &VerificationEnvelope,
        issue_path: Option<&Path>,
        gate_guard: Option<&dyn Fn() -> Result<(), String>>,
        now_ms: i64,
    ) -> Result<VerificationOutcome, VerificationError> {
        self.verify_job_inner_managed(
            repository, store, job_id, envelope, issue_path, gate_guard, None, None, now_ms,
        )
    }

    fn verify_job_inner_managed(
        &self,
        repository: &Path,
        store: &JobsStore,
        job_id: i64,
        envelope: &VerificationEnvelope,
        issue_path: Option<&Path>,
        gate_guard: Option<&dyn Fn() -> Result<(), String>>,
        cancel: Option<&AtomicBool>,
        mut on_check_spawn: Option<
            &mut dyn FnMut(usize, crate::process::ProcessIdentity) -> Result<(), String>,
        >,
        now_ms: i64,
    ) -> Result<VerificationOutcome, VerificationError> {
        if envelope.issue_fingerprint.trim().is_empty()
            || envelope.worker_task_id.trim().is_empty()
            || envelope.worker_task_id == envelope.reviewer.task_id
            || envelope.checks.is_empty()
            || envelope.checks.len() > 128
            || envelope.acceptance_criteria.len() > 256
            || envelope
                .acceptance_criteria
                .iter()
                .any(|c| c.trim().is_empty() || c.len() > 16 * 1024)
            || envelope
                .checks
                .iter()
                .map(|c| c.id.as_str())
                .collect::<std::collections::HashSet<_>>()
                .len()
                != envelope.checks.len()
        {
            return Err(VerificationError::Invalid(
                "at least one declared check is required".into(),
            ));
        }
        let mut owned = std::collections::HashSet::new();
        for check in &envelope.checks {
            if let Some(contract) = &check.contract {
                crate::contracts::validate_spec(contract, &envelope.acceptance_criteria)
                    .map_err(VerificationError::Invalid)?;
                for criterion in &contract.criteria {
                    if !owned.insert(criterion) {
                        return Err(VerificationError::Invalid(
                            "contract criterion has overlapping ownership".into(),
                        ));
                    }
                }
            }
        }
        if let Some(path) = issue_path {
            validate_authoritative_issue_source(store, job_id, path, &envelope.issue_fingerprint)?;
        }
        let mut receipts = Vec::with_capacity(envelope.checks.len());
        for (index, spec) in envelope.checks.iter().enumerate() {
            if cancel.is_some_and(|signal| signal.load(std::sync::atomic::Ordering::Acquire)) {
                return Ok(VerificationOutcome::NeedsInput {
                    reason: "verification was cancelled before check execution".into(),
                });
            }
            let step_name = format!("verification_check_{index}");
            store.record_verification_checkpoint(
                job_id,
                &envelope.issue_fingerprint,
                &step_name,
                &json!({
                    "state":"started", "checkId":&spec.id, "codeHead":&envelope.code_snapshot.head,
                    "codeFingerprint":&envelope.code_snapshot.fingerprint
                }),
                now_ms,
            )?;
            let receipt_result = if let (Some(cancel), Some(on_check_spawn)) =
                (cancel, on_check_spawn.as_deref_mut())
            {
                let mut on_spawn = |identity| on_check_spawn(index, identity);
                self.run_check_managed(
                    repository,
                    &envelope.code_snapshot,
                    spec,
                    cancel,
                    &mut on_spawn,
                )
            } else {
                self.run_check(repository, &envelope.code_snapshot, spec)
            };
            let receipt = match receipt_result {
                Ok(receipt) => receipt,
                Err(VerificationError::NeedsInput(reason)) => {
                    if spec.contract.is_some() {
                        store.record_verification_checkpoint(
                            job_id,
                            &envelope.issue_fingerprint,
                            &step_name,
                            &json!({"state":"needs_input", "contractStatus":"inconclusive",
                                "checkId":&spec.id,"reason":&reason}),
                            now_ms,
                        )?;
                    }
                    return Ok(VerificationOutcome::NeedsInput { reason });
                }
                Err(error) => return Err(error),
            };
            store.record_verification_checkpoint(
                job_id,
                &envelope.issue_fingerprint,
                &step_name,
                &json!({
                    "state":if receipt.cancelled || receipt.output_incomplete || receipt.contract.as_ref().is_some_and(|c| c.status == ContractStatus::Inconclusive) { "needs_input" } else { "completed" }, "receipt":&receipt
                }),
                now_ms,
            )?;
            if let Some(contract) = &receipt.contract {
                if contract.status == ContractStatus::Inconclusive {
                    return Ok(VerificationOutcome::NeedsInput {
                        reason: format!(
                            "contract {} is inconclusive: {}",
                            receipt.check_id,
                            contract
                                .reason
                                .as_deref()
                                .unwrap_or("insufficient evidence")
                        ),
                    });
                }
            }
            if receipt.cancelled || receipt.output_incomplete {
                return Ok(VerificationOutcome::NeedsInput {
                    reason: if receipt.cancelled {
                        "verification check was cancelled".into()
                    } else {
                        "verification check output was incomplete".into()
                    },
                });
            }
            if !receipt.passed {
                if let Some(contract) = &receipt.contract {
                    return Err(VerificationError::CheckFailed(format!(
                        "contract {}: {}",
                        receipt.check_id,
                        contract
                            .reason
                            .as_deref()
                            .unwrap_or("demonstrated specimen mismatch")
                    )));
                }
                return Err(VerificationError::CheckFailed(format!(
                    "{} exit={:?} timed_out={}",
                    receipt.check_id, receipt.exit_code, receipt.timed_out
                )));
            }
            receipts.push(receipt);
        }
        let source = source_snapshot(repository)?;
        if source != envelope.code_snapshot {
            return Err(VerificationError::StaleSource {
                before: envelope.code_snapshot.fingerprint.clone(),
                after: source.fingerprint,
            });
        }
        let contract_only =
            !requires_independent_review(envelope).map_err(VerificationError::Invalid)?;
        let review_method = if contract_only {
            "deterministic_contracts"
        } else {
            "independent_reviewer"
        };
        let review_request = ReviewRequest {
            job_id,
            worker_task_id: envelope.worker_task_id.clone(),
            issue_fingerprint: envelope.issue_fingerprint.clone(),
            source: source.clone(),
            reviewer: envelope.reviewer.clone(),
            check_receipts: receipts.clone(),
            acceptance_criteria: envelope.acceptance_criteria.clone(),
        };
        store.record_verification_checkpoint(
            job_id,
            &envelope.issue_fingerprint,
            "verification_review",
            &json!({
                "state":if contract_only { "skipped_contract_only" } else { "started" }, "method":review_method, "reviewer":&envelope.reviewer, "codeHead":&source.head,
                "codeFingerprint":&source.fingerprint
            }),
            now_ms,
        )?;
        let review = self.review(&review_request)?;
        store.record_verification_checkpoint(
            job_id,
            &envelope.issue_fingerprint,
            "verification_review",
            &json!({
                "state":"completed", "method":review_method, "passed":review.passed, "findings":&review.findings,
                "manualAcceptance":&review.manual_acceptance, "details":&review.details
            }),
            now_ms,
        )?;
        if !review.passed {
            return Err(VerificationError::ReviewFailed(review.details.clone()));
        }
        if let Some(path) = issue_path {
            validate_authoritative_issue_source(store, job_id, path, &envelope.issue_fingerprint)?;
        }
        let current = source_snapshot(repository)?;
        if current != source {
            return Err(VerificationError::StaleSource {
                before: source.fingerprint,
                after: current.fingerprint,
            });
        }
        if !review.manual_acceptance.is_empty() {
            store.require_manual_acceptance(
                job_id,
                &envelope.issue_fingerprint,
                &source.head,
                &source.fingerprint,
                &review.manual_acceptance,
                now_ms,
            )?;
            return Ok(VerificationOutcome::NeedsManualAcceptance {
                source,
                checks: receipts,
                review,
            });
        }
        let evidence = receipts
            .iter()
            .map(|r| VerificationEvidence {
                check_id: r.check_id.clone(),
                source_fingerprint: r.source.fingerprint.clone(),
                passed: true,
                details: serde_json::json!({
                    "codeHead": &source.head,
                    "codeFingerprint": &source.fingerprint,
                    "exitCode": r.exit_code,
                    "reviewMethod": review_method,
                    "reviewerTask": &envelope.reviewer.task_id,
                    "reviewerProvider": &envelope.reviewer.provider_id,
                    "reviewerModel": &envelope.reviewer.model_id,
                    "review": &review.details,
                })
                .to_string(),
            })
            .collect::<Vec<_>>();
        if let Some(path) = issue_path {
            validate_authoritative_issue_source(store, job_id, path, &envelope.issue_fingerprint)?;
        }
        if let Some(guard) = gate_guard {
            guard().map_err(VerificationError::GateGuardFailed)?;
        }
        store.verify_gate_with_code_snapshot(
            job_id,
            &envelope.issue_fingerprint,
            &source.head,
            &source.fingerprint,
            &evidence,
            now_ms,
        )?;
        Ok(VerificationOutcome::Verified {
            source,
            checks: receipts,
            review,
        })
    }

    pub fn save_repair_progress(
        store: &JobsStore,
        job_id: i64,
        issue_fingerprint: &str,
        progress: &RepairProgress,
        now_ms: i64,
    ) -> Result<(), VerificationError> {
        if progress.round == 0 || progress.round > MAX_REPAIR_ROUNDS {
            return Err(VerificationError::Invalid(
                "repair rounds are limited to two".into(),
            ));
        }
        let name = format!("verification_repair_round_{}", progress.round);
        let existing = store.get_checkpoint(job_id, &name)?;
        match (existing, progress.state) {
            (None, RepairState::Started) => {
                if progress.source_fingerprint.trim().is_empty()
                    || progress.result_source_fingerprint.is_some()
                {
                    return Err(VerificationError::Invalid(
                        "repair start requires a source snapshot and no result snapshot".into(),
                    ));
                }
                if progress.round > 1 {
                    let prior_name = format!("verification_repair_round_{}", progress.round - 1);
                    let prior = store.get_checkpoint(job_id, &prior_name)?.ok_or_else(|| {
                        VerificationError::Invalid("repair rounds must be sequential".into())
                    })?;
                    let prior: RepairProgress = serde_json::from_value(prior)
                        .map_err(|e| VerificationError::Invalid(e.to_string()))?;
                    if prior.state == RepairState::Started {
                        return Err(VerificationError::Invalid("prior repair round is still active; checkpoint interruption before resuming".into()));
                    }
                }
            }
            (
                Some(value),
                RepairState::Completed | RepairState::Interrupted | RepairState::Exhausted,
            ) => {
                let prior: RepairProgress = serde_json::from_value(value)
                    .map_err(|e| VerificationError::Invalid(e.to_string()))?;
                if prior.state != RepairState::Started
                    || prior.round != progress.round
                    || prior.source_fingerprint != progress.source_fingerprint
                {
                    return Err(VerificationError::Invalid(
                        "repair result must close the matching started round and source snapshot"
                            .into(),
                    ));
                }
                if matches!(
                    progress.state,
                    RepairState::Completed | RepairState::Exhausted
                ) && progress
                    .result_source_fingerprint
                    .as_deref()
                    .unwrap_or("")
                    .trim()
                    .is_empty()
                {
                    return Err(VerificationError::Invalid(
                        "completed repair requires its resulting source fingerprint".into(),
                    ));
                }
                if !matches!(
                    progress.state,
                    RepairState::Completed | RepairState::Exhausted
                ) && progress.result_source_fingerprint.is_some()
                {
                    return Err(VerificationError::Invalid(
                        "only a completed or exhausted repair may record a result source fingerprint".into(),
                    ));
                }
            }
            _ => {
                return Err(VerificationError::Invalid(
                    "repair state transition is not allowed".into(),
                ))
            }
        }
        store.record_verification_checkpoint(
            job_id,
            issue_fingerprint,
            &name,
            &json!(progress),
            now_ms,
        )?;
        Ok(())
    }
}

fn validate_review_result(result: &ReviewResult) -> Result<(), VerificationError> {
    const MAX_FINDINGS: usize = 256;
    const MAX_REVIEW_TEXT_BYTES: usize = 16 * 1024;
    if result.details.len() > MAX_REVIEW_TEXT_BYTES
        || result.findings.len() > MAX_FINDINGS
        || result.manual_acceptance.len() > MAX_FINDINGS
        || result
            .findings
            .iter()
            .chain(result.manual_acceptance.iter())
            .any(|text| text.trim().is_empty() || text.len() > MAX_REVIEW_TEXT_BYTES)
    {
        return Err(VerificationError::ReviewFailed(
            "review result exceeds the bounded response schema".into(),
        ));
    }
    Ok(())
}

/// Computes a portable source identity from Git's exact HEAD, tracked diff, and untracked files.
/// The fingerprint uses Git's object hash over deterministic bytes; it does not write objects.
pub fn source_snapshot(repository: &Path) -> Result<SourceSnapshot, VerificationError> {
    let repository = repository
        .canonicalize()
        .map_err(|e| VerificationError::Io(e.to_string()))?;
    let head = git(&repository, &["rev-parse", "HEAD"])?;
    let status = git_bytes(
        &repository,
        &[
            "-c",
            "core.fsmonitor=false",
            "status",
            "--porcelain=v1",
            "-z",
            "--untracked-files=all",
        ],
    )?;
    let tracked = git_bytes(
        &repository,
        &[
            "-c",
            "core.quotepath=false",
            "diff",
            "--no-ext-diff",
            "--no-textconv",
            "--binary",
            "HEAD",
        ],
    )?;
    if tracked.len() > MAX_TRACKED_DIFF_BYTES {
        return Err(VerificationError::Invalid(
            "tracked diff exceeds verification snapshot limit".into(),
        ));
    }
    let mut material = Vec::new();
    append_fingerprint_field(&mut material, b"head", head.trim().as_bytes());
    append_fingerprint_field(&mut material, b"status", &status);
    append_fingerprint_field(&mut material, b"tracked-diff", &tracked);
    // Include untracked content so a newly added source file changes the fingerprint.
    let files = git_bytes(
        &repository,
        &[
            "-c",
            "core.quotepath=false",
            "ls-files",
            "--others",
            "--exclude-standard",
            "-z",
        ],
    )?;
    let names = files
        .split(|b| *b == 0)
        .filter(|name| !name.is_empty())
        .collect::<Vec<_>>();
    if names.len() > MAX_SOURCE_FILES {
        return Err(VerificationError::Invalid(
            "untracked file count exceeds verification snapshot limit".into(),
        ));
    }
    let mut total_bytes = fingerprint_field_size(b"head", head.trim().as_bytes())
        .saturating_add(fingerprint_field_size(b"status", &status))
        .saturating_add(fingerprint_field_size(b"tracked-diff", &tracked));
    if total_bytes > MAX_SOURCE_BYTES {
        return Err(VerificationError::Invalid(
            "source snapshot exceeds 64 MiB limit".into(),
        ));
    }
    for raw_name in names {
        let name = std::str::from_utf8(raw_name).map_err(|e| {
            VerificationError::Invalid(format!("untracked filename is not UTF-8: {e}"))
        })?;
        let relative = Path::new(name);
        if relative.is_absolute()
            || relative.components().any(|c| {
                matches!(
                    c,
                    Component::ParentDir | Component::RootDir | Component::Prefix(_)
                )
            })
        {
            return Err(VerificationError::Invalid(
                "untracked path escapes repository".into(),
            ));
        }
        let path = repository.join(relative);
        let metadata =
            std::fs::symlink_metadata(&path).map_err(|e| VerificationError::Io(e.to_string()))?;
        if !metadata.file_type().is_file() {
            return Err(VerificationError::Invalid(format!(
                "untracked path is not a regular file: {name}"
            )));
        }
        let resolved = path
            .canonicalize()
            .map_err(|e| VerificationError::Io(e.to_string()))?;
        if !resolved.starts_with(&repository) {
            return Err(VerificationError::Invalid(
                "untracked path resolves outside repository".into(),
            ));
        }
        total_bytes = total_bytes
            .saturating_add(fingerprint_field_size(b"untracked-path", name.as_bytes()))
            .saturating_add(fingerprint_field_size(b"untracked-content", &[]));
        if total_bytes > MAX_SOURCE_BYTES {
            return Err(VerificationError::Invalid(
                "source snapshot exceeds 64 MiB limit".into(),
            ));
        }
        let remaining = MAX_SOURCE_BYTES.saturating_sub(total_bytes);
        let mut file =
            std::fs::File::open(&resolved).map_err(|e| VerificationError::Io(e.to_string()))?;
        let mut contents = Vec::with_capacity((metadata.len() as usize).min(remaining));
        file.by_ref()
            .take(remaining.saturating_add(1) as u64)
            .read_to_end(&mut contents)
            .map_err(|e| VerificationError::Io(e.to_string()))?;
        if contents.len() > remaining {
            return Err(VerificationError::Invalid(
                "source snapshot exceeds 64 MiB limit".into(),
            ));
        }
        total_bytes = total_bytes.saturating_add(contents.len());
        append_fingerprint_field(&mut material, b"untracked-path", name.as_bytes());
        append_fingerprint_field(&mut material, b"untracked-content", &contents);
    }
    let digest = git_hash(&repository, &material)?;
    Ok(SourceSnapshot {
        head: head.trim().to_owned(),
        fingerprint: digest,
    })
}

/// Length-prefix every fingerprint component. File contents are arbitrary bytes (including
/// NUL), so delimiter-only framing could make different source trees hash the same stream.
fn append_fingerprint_field(material: &mut Vec<u8>, tag: &[u8], value: &[u8]) {
    material.extend_from_slice(&(tag.len() as u64).to_be_bytes());
    material.extend_from_slice(tag);
    material.extend_from_slice(&(value.len() as u64).to_be_bytes());
    material.extend_from_slice(value);
}
fn fingerprint_field_size(tag: &[u8], value: &[u8]) -> usize {
    16usize
        .saturating_add(tag.len())
        .saturating_add(value.len())
}

fn git(repository: &Path, args: &[&str]) -> Result<String, VerificationError> {
    let bytes = git_bytes(repository, args)?;
    String::from_utf8(bytes).map_err(|e| VerificationError::Io(e.to_string()))
}
fn git_bytes(repository: &Path, args: &[&str]) -> Result<Vec<u8>, VerificationError> {
    let result = crate::process::capture(
        Path::new("git"),
        &args.iter().map(|s| s.to_string()).collect::<Vec<_>>(),
        repository,
        Duration::from_secs(20),
        16 * 1024 * 1024,
    )
    .map_err(|e| VerificationError::Io(e.to_string()))?;
    if result.timed_out || result.truncated || result.status.and_then(|s| s.code()) != Some(0) {
        return Err(VerificationError::Io(format!(
            "git {} failed or exceeded output limit",
            args.join(" ")
        )));
    }
    Ok(result.stdout)
}
fn git_hash(_repository: &Path, material: &[u8]) -> Result<String, VerificationError> {
    crate::process::git_object_hash(material).map_err(|e| VerificationError::Io(e.to_string()))
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum EffectKind {
    Commit,
    Push,
    PullRequest,
    Merge,
    Deploy,
    Publish,
}
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum EffectState {
    Planned,
    Started,
    Confirmed,
    Failed,
    Ambiguous,
    Reconciled,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct EffectReceipt {
    pub idempotency_key: String,
    pub kind: EffectKind,
    pub state: EffectState,
    pub reference: Option<String>,
    pub evidence: String,
    pub source: Option<SourceSnapshot>,
    pub check_ids: Vec<String>,
}
pub trait EffectLedger {
    fn record(&self, receipt: &EffectReceipt) -> Result<(), String>;
    fn get(&self, key: &str) -> Result<Option<EffectReceipt>, String>;
}

/// Durable append-only effect ledger. Keep this database path explicit and separate from
/// the job store unless the application deliberately supplies its job database path.
pub struct SqliteEffectLedger {
    path: PathBuf,
}
impl SqliteEffectLedger {
    pub fn open(path: impl Into<PathBuf>) -> Result<Self, String> {
        let path = path.into();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        let ledger = Self { path };
        let conn = rusqlite::Connection::open(&ledger.path).map_err(|e| e.to_string())?;
        conn.pragma_update(None, "journal_mode", "WAL")
            .map_err(|e| e.to_string())?;
        conn.execute_batch("PRAGMA foreign_keys=ON;
            CREATE TABLE IF NOT EXISTS runner_effects(
              idempotency_key TEXT PRIMARY KEY NOT NULL,
              kind TEXT NOT NULL,
              state TEXT NOT NULL,
              receipt_json TEXT NOT NULL,
              created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
              updated_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
            );
            CREATE TABLE IF NOT EXISTS runner_effect_transitions(
              transition_id INTEGER PRIMARY KEY AUTOINCREMENT,
              idempotency_key TEXT NOT NULL REFERENCES runner_effects(idempotency_key),
              state TEXT NOT NULL,
              receipt_json TEXT NOT NULL,
              recorded_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
            );
            CREATE INDEX IF NOT EXISTS runner_effect_transitions_key ON runner_effect_transitions(idempotency_key, transition_id);").map_err(|e| e.to_string())?;
        Ok(ledger)
    }
}
impl EffectLedger for SqliteEffectLedger {
    fn record(&self, receipt: &EffectReceipt) -> Result<(), String> {
        if receipt.idempotency_key.trim().is_empty() || receipt.evidence.trim().is_empty() {
            return Err("effect key and evidence are required".into());
        }
        if receipt.source.is_none() || receipt.check_ids.is_empty() {
            return Err(
                "effect receipt must bind the source snapshot and passing check IDs".into(),
            );
        }
        let mut conn = rusqlite::Connection::open(&self.path).map_err(|e| e.to_string())?;
        let tx = conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .map_err(|e| e.to_string())?;
        let existing: Option<(String, String, String)> = tx
            .query_row(
                "SELECT kind,state,receipt_json FROM runner_effects WHERE idempotency_key=?1",
                [&receipt.idempotency_key],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()
            .map_err(|e| e.to_string())?;
        let kind = serde_json::to_value(receipt.kind)
            .map_err(|e| e.to_string())?
            .as_str()
            .unwrap()
            .to_owned();
        let state = serde_json::to_value(receipt.state)
            .map_err(|e| e.to_string())?
            .as_str()
            .unwrap()
            .to_owned();
        let json = serde_json::to_string(receipt).map_err(|e| e.to_string())?;
        if let Some((old_kind, old_state, old_json)) = existing {
            if old_kind != kind {
                return Err("idempotency key already belongs to a different effect kind".into());
            }
            let previous: EffectReceipt =
                serde_json::from_str(&old_json).map_err(|e| e.to_string())?;
            if previous.source != receipt.source || previous.check_ids != receipt.check_ids {
                return Err(
                    "effect result cannot change its original source or check binding".into(),
                );
            }
            if !allowed_effect_transition(&old_state, &state) {
                return Err(format!("invalid effect transition {old_state} -> {state}"));
            }
            tx.execute("UPDATE runner_effects SET state=?2,receipt_json=?3,updated_at=CURRENT_TIMESTAMP WHERE idempotency_key=?1", rusqlite::params![receipt.idempotency_key,state,json]).map_err(|e| e.to_string())?;
        } else {
            if receipt.state != EffectState::Planned && receipt.state != EffectState::Started {
                return Err("new effect must begin in planned or started state".into());
            }
            tx.execute("INSERT INTO runner_effects(idempotency_key,kind,state,receipt_json) VALUES(?1,?2,?3,?4)", rusqlite::params![receipt.idempotency_key,kind,state,json]).map_err(|e| e.to_string())?;
        }
        tx.execute("INSERT INTO runner_effect_transitions(idempotency_key,state,receipt_json) VALUES(?1,?2,?3)", rusqlite::params![receipt.idempotency_key,state,json]).map_err(|e| e.to_string())?;
        tx.commit().map_err(|e| e.to_string())?;
        Ok(())
    }
    fn get(&self, key: &str) -> Result<Option<EffectReceipt>, String> {
        let conn = rusqlite::Connection::open(&self.path).map_err(|e| e.to_string())?;
        let value: Option<String> = conn
            .query_row(
                "SELECT receipt_json FROM runner_effects WHERE idempotency_key=?1",
                [key],
                |r| r.get(0),
            )
            .optional()
            .map_err(|e| e.to_string())?;
        value
            .map(|v| serde_json::from_str(&v).map_err(|e| e.to_string()))
            .transpose()
    }
}

fn allowed_effect_transition(from: &str, to: &str) -> bool {
    matches!(
        (from, to),
        ("planned", "started")
            | ("planned", "failed")
            | ("started", "confirmed")
            | ("started", "failed")
            | ("started", "ambiguous")
            | ("ambiguous", "reconciled")
            | ("ambiguous", "failed")
    )
}

/// Enforces an explicitly requested local contract adapter without making it a portable dependency.
pub fn require_contract_adapter(available: bool) -> Result<(), VerificationError> {
    if available {
        Ok(())
    } else {
        Err(VerificationError::Invalid(
            "optional Agents Issue Delivery Contract adapter unavailable".into(),
        ))
    }
}

/// Rejects blind retries after an uncertain external effect. The caller must reconcile first.
pub fn begin_effect<L: EffectLedger>(
    ledger: &L,
    key: &str,
    kind: EffectKind,
    intent: &str,
    source: SourceSnapshot,
    check_ids: Vec<String>,
) -> Result<EffectReceipt, VerificationError> {
    if key.trim().is_empty() || intent.trim().is_empty() || check_ids.is_empty() {
        return Err(VerificationError::Invalid(
            "effect key, intent, source snapshot, and passing check IDs are required".into(),
        ));
    }
    if check_ids.iter().any(|id| id.trim().is_empty())
        || check_ids
            .iter()
            .collect::<std::collections::HashSet<_>>()
            .len()
            != check_ids.len()
    {
        return Err(VerificationError::Invalid(
            "effect check IDs must be nonempty and unique".into(),
        ));
    }
    if let Some(existing) = ledger.get(key).map_err(VerificationError::Io)? {
        if existing.kind != kind
            || existing.source.as_ref() != Some(&source)
            || existing.check_ids != check_ids
        {
            return Err(VerificationError::Invalid(
                "effect idempotency key is already bound to different source or checks".into(),
            ));
        }
        if matches!(
            existing.state,
            EffectState::Started | EffectState::Ambiguous
        ) {
            return Err(VerificationError::Invalid(
                "effect has uncertain outcome; reconcile before retry".into(),
            ));
        }
        if matches!(
            existing.state,
            EffectState::Confirmed | EffectState::Reconciled
        ) {
            return Ok(existing);
        }
        if existing.state == EffectState::Failed {
            return Err(VerificationError::Invalid(
                "failed effect cannot be retried under the same idempotency key".into(),
            ));
        }
    }
    let receipt = EffectReceipt {
        idempotency_key: key.to_owned(),
        kind,
        state: EffectState::Started,
        reference: None,
        evidence: intent.to_owned(),
        source: Some(source),
        check_ids,
    };
    ledger.record(&receipt).map_err(VerificationError::Io)?;
    Ok(receipt)
}

/// Record the observed outcome of a started side effect without changing its source/check binding.
pub fn record_effect_result<L: EffectLedger>(
    ledger: &L,
    started: &EffectReceipt,
    state: EffectState,
    reference: Option<String>,
    evidence: String,
) -> Result<EffectReceipt, VerificationError> {
    if started.state != EffectState::Started
        || !matches!(
            state,
            EffectState::Confirmed | EffectState::Failed | EffectState::Ambiguous
        )
        || evidence.trim().is_empty()
    {
        return Err(VerificationError::Invalid(
            "effect result must close a started intent with an observed outcome".into(),
        ));
    }
    if ledger
        .get(&started.idempotency_key)
        .map_err(VerificationError::Io)?
        .as_ref()
        != Some(started)
    {
        return Err(VerificationError::Invalid(
            "effect result must match the currently persisted started intent".into(),
        ));
    }
    let result = EffectReceipt {
        state,
        reference,
        evidence,
        ..started.clone()
    };
    ledger.record(&result).map_err(VerificationError::Io)?;
    Ok(result)
}

/// Reconcile an ambiguous effect. Reconciled receipts are terminal and idempotent.
pub fn reconcile_effect<L: EffectLedger>(
    ledger: &L,
    key: &str,
    evidence: String,
    reference: Option<String>,
) -> Result<EffectReceipt, VerificationError> {
    if evidence.trim().is_empty() {
        return Err(VerificationError::Invalid(
            "effect reconciliation evidence is required".into(),
        ));
    }
    let prior = ledger
        .get(key)
        .map_err(VerificationError::Io)?
        .ok_or_else(|| VerificationError::Invalid("effect intent was not found".into()))?;
    if prior.state != EffectState::Ambiguous {
        return Err(VerificationError::Invalid(
            "only an ambiguous effect can be reconciled".into(),
        ));
    }
    let reconciled = EffectReceipt {
        state: EffectState::Reconciled,
        reference,
        evidence,
        ..prior
    };
    ledger.record(&reconciled).map_err(VerificationError::Io)?;
    Ok(reconciled)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::jobs::{
        AttemptOutcome, JobPlan, JobsStore, PlannedTask, ResourceLimits, SubmitJob, TaskKind,
    };
    struct FixtureCheck {
        mutate: bool,
        fail: bool,
    }
    impl CheckExecutor for FixtureCheck {
        fn execute(&self, repo: &Path, _: &CheckSpec) -> Result<CheckOutput, String> {
            if self.mutate {
                std::fs::write(repo.join("change.txt"), "changed").unwrap();
            }
            Ok(CheckOutput {
                exit_code: Some(if self.fail { 1 } else { 0 }),
                ..Default::default()
            })
        }
    }
    struct FixtureReview {
        result: ReviewResult,
    }
    impl ReviewExecutor for FixtureReview {
        fn review(&self, _: &ReviewRequest) -> Result<ReviewResult, String> {
            Ok(self.result.clone())
        }
    }
    struct IssueChangingReview {
        issue_path: PathBuf,
        result: ReviewResult,
    }
    impl ReviewExecutor for IssueChangingReview {
        fn review(&self, _: &ReviewRequest) -> Result<ReviewResult, String> {
            std::fs::write(
                &self.issue_path,
                "---\nid: \"FAB-L008\"\ntitle: \"Changed\"\nstatus: \"Open\"\ntype: \"enhancement\"\n---\nchanged during review\n",
            )
            .map_err(|error| error.to_string())?;
            Ok(self.result.clone())
        }
    }
    fn repo() -> (tempfile::TempDir, SourceSnapshot) {
        let d = tempfile::tempdir().unwrap();
        let run = |args: &[&str]| {
            std::process::Command::new("git")
                .args(args)
                .current_dir(d.path())
                .output()
                .unwrap()
        };
        assert!(run(&["init", "-q"]).status.success());
        assert!(run(&[
            "-c",
            "user.name=Fixture",
            "-c",
            "user.email=fixture@example.invalid",
            "commit",
            "--allow-empty",
            "-qm",
            "initial"
        ])
        .status
        .success());
        let s = source_snapshot(d.path()).unwrap();
        (d, s)
    }
    fn spec() -> CheckSpec {
        CheckSpec {
            id: "fixture".into(),
            program: "true".into(),
            args: vec![],
            cwd: ".".into(),
            timeout_ms: 1000,
            output_limit_bytes: 1024,
            adapter: None,
            contract: None,
        }
    }

    #[test]
    fn reviewer_reasoning_effort_is_optional_for_older_serialized_policies() {
        let reviewer: ReviewerIdentity = serde_json::from_value(serde_json::json!({
            "task_id": "review",
            "provider_id": "codex",
            "model_id": "model"
        }))
        .unwrap();
        assert_eq!(reviewer.reasoning_effort, None);
        assert!(serde_json::to_value(reviewer)
            .unwrap()
            .get("reasoning_effort")
            .is_none());
    }

    #[test]
    #[ignore = "subprocess fixture invoked by execute_in_build_output_overrides_ambient_cargo_target_dir"]
    fn process_check_build_output_child_writes_target_witness() {
        let target = PathBuf::from(
            std::env::var_os("CARGO_TARGET_DIR")
                .expect("the build-output executor must set CARGO_TARGET_DIR"),
        );
        let witness = std::env::args()
            .last()
            .expect("parent test passes a unique witness filename");
        std::fs::write(target.join(witness), b"owned build output").unwrap();
    }

    #[test]
    #[ignore = "subprocess fixture invoked by managed_check_registers_identity_and_cancels_only_owned_process_tree"]
    fn process_check_child_waits_for_owned_cancellation() {
        if let Some(marker) = std::env::var_os("NAVIS_UNRELATED_CHECK_MARKER") {
            std::fs::write(marker, b"ready").expect("write unrelated child readiness marker");
        }
        std::thread::sleep(Duration::from_secs(30));
    }

    #[test]
    fn managed_check_registers_identity_and_cancels_only_owned_process_tree() {
        use std::process::Command;
        use std::sync::atomic::Ordering;

        let repository = tempfile::tempdir().unwrap();
        let build_output = tempfile::tempdir().unwrap();
        let executable = std::env::current_exe().unwrap();
        let unrelated_marker = build_output.path().join("unrelated-child-ready");
        let mut unrelated = Command::new(&executable)
            .args([
                "--exact",
                "verification::tests::process_check_child_waits_for_owned_cancellation",
                "--ignored",
                "--nocapture",
            ])
            .env("NAVIS_UNRELATED_CHECK_MARKER", &unrelated_marker)
            .spawn()
            .unwrap();
        let ready_deadline = std::time::Instant::now() + Duration::from_secs(5);
        while !unrelated_marker.exists() && std::time::Instant::now() < ready_deadline {
            if let Some(status) = unrelated.try_wait().unwrap() {
                panic!("unrelated fixture exited before becoming ready: {status}");
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        if !unrelated_marker.exists() {
            let _ = unrelated.kill();
            let _ = unrelated.wait();
            panic!("unrelated fixture did not signal that it was running");
        }

        let check = CheckSpec {
            id: "managed-cancellation".into(),
            program: executable,
            args: vec![
                "--exact".into(),
                "verification::tests::process_check_child_waits_for_owned_cancellation".into(),
                "--ignored".into(),
                "--nocapture".into(),
            ],
            cwd: ".".into(),
            timeout_ms: 10_000,
            output_limit_bytes: DEFAULT_OUTPUT_LIMIT,
            adapter: None,
            contract: None,
        };
        let cancel = AtomicBool::new(false);
        let mut spawned_identity = None;
        let output = ProcessCheckExecutor
            .execute_managed_in_build_output(
                repository.path(),
                &check,
                build_output.path(),
                &cancel,
                |identity| {
                    spawned_identity = Some(identity);
                    cancel.store(true, Ordering::Release);
                    Ok(())
                },
            )
            .unwrap();
        assert!(output.cancelled);
        let spawned_identity =
            spawned_identity.expect("spawn callback must receive child identity");
        assert!(!crate::process::process_identity_matches(spawned_identity).unwrap());
        assert!(
            unrelated.try_wait().unwrap().is_none(),
            "unrelated child was terminated"
        );

        unrelated.kill().unwrap();
        unrelated.wait().unwrap();
    }

    #[test]
    fn execute_in_build_output_overrides_ambient_cargo_target_dir() {
        let repository = tempfile::tempdir().unwrap();
        let build_output = tempfile::tempdir().unwrap();
        let ambient_target = std::env::var_os("CARGO_TARGET_DIR").map(PathBuf::from);
        let witness = format!("build-output-witness-{}.txt", std::process::id());
        let executable = std::env::current_exe().unwrap();
        let check = CheckSpec {
            id: "build-output-witness".into(),
            program: executable,
            args: vec![
                "--exact".into(),
                "verification::tests::process_check_build_output_child_writes_target_witness"
                    .into(),
                "--ignored".into(),
                "--nocapture".into(),
                "--".into(),
                witness.clone(),
            ],
            cwd: ".".into(),
            timeout_ms: 30_000,
            output_limit_bytes: DEFAULT_OUTPUT_LIMIT,
            adapter: None,
            contract: None,
        };

        let result = ProcessCheckExecutor
            .execute_in_build_output(repository.path(), &check, build_output.path())
            .unwrap();
        assert_eq!(result.exit_code, Some(0), "{}", result.stderr);
        assert!(!result.timed_out);
        assert!(build_output.path().join(&witness).is_file());
        assert!(!repository.path().join("target").join(&witness).exists());
        if let Some(ambient_target) = ambient_target {
            let ambient_target = ambient_target.canonicalize().unwrap_or(ambient_target);
            let build_root = build_output.path().canonicalize().unwrap();
            if ambient_target != build_root {
                assert!(!ambient_target.join(&witness).exists());
            }
        }
    }

    fn awaiting_job(store: &JobsStore, id: &str, issue_fp: &str) -> i64 {
        let limits = ResourceLimits {
            max_attempts: 3,
            max_tokens: None,
            max_cost_micros: None,
            max_seconds: Some(300),
            max_concurrency: 1,
        };
        let job = store
            .submit(
                SubmitJob {
                    idempotency_key: format!("{id}-key"),
                    issue_id: id.into(),
                    issue_type: "enhancement".into(),
                    parent_issue_ids: vec![],
                    source_fingerprint: issue_fp.into(),
                    issue_snapshot: format!("snapshot {id}"),
                    plan: JobPlan {
                        tasks: vec![PlannedTask {
                            task_id: "implement".into(),
                            parent_task_id: None,
                            kind: TaskKind::Implementation,
                            files: vec!["src/lib.rs".into()],
                            acceptance: vec!["pass".into()],
                            limits: limits.clone(),
                            provider_id: None,
                            model_id: None,
                            route_requirements: None,
                        }],
                        limits,
                    },
                    dependencies: vec![],
                },
                1,
            )
            .unwrap();
        let claimed = store.claim("worker", 2, 60_000).unwrap().unwrap();
        store
            .start_attempt(job.id, "worker", claimed.fencing_token, 3)
            .unwrap();
        store
            .record_attempt_outcome(
                job.id,
                "worker",
                claimed.fencing_token,
                AttemptOutcome::ProposedComplete {
                    summary: "proposed".into(),
                },
                4,
            )
            .unwrap();
        job.id
    }

    #[test]
    fn stale_check_before_start_is_rejected_and_source_mutation_during_check_fails() {
        let (d, source) = repo();
        let service = VerificationService {
            checks: FixtureCheck {
                mutate: false,
                fail: false,
            },
            reviewer: FixtureReview {
                result: ReviewResult {
                    passed: true,
                    findings: vec![],
                    manual_acceptance: vec![],
                    details: "ok".into(),
                },
            },
        };
        let stale = SourceSnapshot {
            head: source.head.clone(),
            fingerprint: "stale".into(),
        };
        assert!(matches!(
            service.run_check(d.path(), &stale, &spec()),
            Err(VerificationError::StaleSource { .. })
        ));
        let mutating = VerificationService {
            checks: FixtureCheck {
                mutate: true,
                fail: false,
            },
            reviewer: service.reviewer,
        };
        assert!(matches!(
            mutating.run_check(d.path(), &source, &spec()),
            Err(VerificationError::StaleSource { .. })
        ));
    }
    #[test]
    fn failed_check_and_timeout_never_create_pass_receipts() {
        let (d, source) = repo();
        let service = VerificationService {
            checks: FixtureCheck {
                mutate: false,
                fail: true,
            },
            reviewer: FixtureReview {
                result: ReviewResult {
                    passed: true,
                    findings: vec![],
                    manual_acceptance: vec![],
                    details: "ok".into(),
                },
            },
        };
        assert!(
            !service
                .run_check(d.path(), &source, &spec())
                .unwrap()
                .passed
        );
        assert!(validate_check(&CheckSpec {
            timeout_ms: 0,
            ..spec()
        })
        .is_err());
    }

    #[test]
    fn check_executor_cannot_exceed_the_declared_output_limit() {
        struct LoudCheck;
        impl CheckExecutor for LoudCheck {
            fn execute(&self, _: &Path, _: &CheckSpec) -> Result<CheckOutput, String> {
                Ok(CheckOutput {
                    exit_code: Some(0),
                    stdout: "x".repeat(2048),
                    ..Default::default()
                })
            }
        }
        let (d, source) = repo();
        let service = VerificationService {
            checks: LoudCheck,
            reviewer: FixtureReview {
                result: ReviewResult {
                    passed: true,
                    findings: vec![],
                    manual_acceptance: vec![],
                    details: "ok".into(),
                },
            },
        };
        assert!(matches!(
            service.run_check(
                d.path(),
                &source,
                &CheckSpec {
                    output_limit_bytes: 1024,
                    ..spec()
                }
            ),
            Err(VerificationError::Invalid(_))
        ));
    }

    #[test]
    fn incomplete_check_output_never_counts_as_passing() {
        struct IncompleteCheck;
        impl CheckExecutor for IncompleteCheck {
            fn execute(&self, _: &Path, _: &CheckSpec) -> Result<CheckOutput, String> {
                Ok(CheckOutput {
                    exit_code: Some(0),
                    output_incomplete: true,
                    ..Default::default()
                })
            }
        }

        let (repository, source) = repo();
        let service = VerificationService {
            checks: IncompleteCheck,
            reviewer: FixtureReview {
                result: ReviewResult {
                    passed: true,
                    findings: vec![],
                    manual_acceptance: vec![],
                    details: "unused".into(),
                },
            },
        };
        let receipt = service
            .run_check(repository.path(), &source, &spec())
            .unwrap();
        assert!(!receipt.passed);
        assert!(receipt.output_incomplete);
    }

    #[test]
    fn delivery_contract_adapter_requires_current_ready_report() {
        struct ReportCheck(CheckOutput);
        impl CheckExecutor for ReportCheck {
            fn execute(&self, _: &Path, _: &CheckSpec) -> Result<CheckOutput, String> {
                Ok(self.0.clone())
            }
        }

        let (repository, source) = repo();
        let external = tempfile::tempdir().unwrap();
        let helper = external.path().join("workflow_evidence.py");
        let record = external.path().join("run.json");
        std::fs::write(&helper, b"# synthetic report helper\n").unwrap();
        std::fs::write(&record, b"{}\n").unwrap();
        let check = CheckSpec {
            id: "delivery-contract-report".into(),
            program: std::env::current_exe().unwrap(),
            args: vec![
                helper.to_string_lossy().into_owned(),
                "report".into(),
                "--repo".into(),
                ".".into(),
                "--record".into(),
                record.to_string_lossy().into_owned(),
            ],
            cwd: ".".into(),
            timeout_ms: 10_000,
            output_limit_bytes: MAX_DELIVERY_REPORT_BYTES,
            adapter: Some(CheckAdapter::AgentsDeliveryContract),
            contract: None,
        };
        let repository_identity = repository.path().canonicalize().unwrap();
        let ready_report = serde_json::json!({
            "ready": true,
            "status": "ready",
            "repository": {
                "path": repository_identity.display().to_string(),
                "head": source.head.clone(),
                "fingerprint": "synthetic-helper-fingerprint"
            },
            "remote": null,
            "effects": {}
        })
        .to_string();
        let service = |stdout: String, exit_code: Option<i32>| VerificationService {
            checks: ReportCheck(CheckOutput {
                exit_code,
                stdout,
                ..Default::default()
            }),
            reviewer: FixtureReview {
                result: ReviewResult {
                    passed: true,
                    findings: vec![],
                    manual_acceptance: vec![],
                    details: "unused".into(),
                },
            },
        };

        let receipt = service(ready_report.clone(), Some(0))
            .run_check(repository.path(), &source, &check)
            .unwrap();
        assert!(receipt.passed);

        let draft_report = serde_json::json!({
            "ready": false,
            "status": "draft",
            "reasons": ["check unit missing or not passed"],
            "repository": {
                "path": repository_identity.display().to_string(),
                "head": source.head.clone(),
                "fingerprint": "synthetic-helper-fingerprint"
            }
        })
        .to_string();
        let jobs_dir = tempfile::tempdir().unwrap();
        let store = JobsStore::open(jobs_dir.path().join("jobs.sqlite3")).unwrap();
        let issue_fp = "delivery-contract-draft-issue";
        let job_id = awaiting_job(&store, "FAB-L008-DELIVERY-CONTRACT", issue_fp);
        let envelope = VerificationEnvelope {
            issue_fingerprint: issue_fp.into(),
            worker_task_id: "implementation-task".into(),
            code_snapshot: source.clone(),
            checks: vec![check.clone()],
            reviewer: ReviewerIdentity {
                task_id: "independent-review".into(),
                provider_id: "review-profile".into(),
                model_id: "review-model".into(),
                reasoning_effort: None,
            },
            acceptance_criteria: vec!["delivery report is ready".into()],
        };
        let outcome = service(draft_report, Some(0))
            .verify_job(repository.path(), &store, job_id, &envelope, 5)
            .unwrap();
        assert!(matches!(
            outcome,
            VerificationOutcome::NeedsInput { ref reason }
                if reason == "delivery-contract report is not ready"
        ));
        assert_eq!(
            store.get(job_id).unwrap().state,
            crate::jobs::JobState::AwaitingVerification
        );
        assert!(store
            .get_checkpoint(job_id, "verification_review")
            .unwrap()
            .is_none());
        assert!(store
            .get_checkpoint(job_id, "verification_repair_round_1")
            .unwrap()
            .is_none());

        let missing_dependency_report = serde_json::json!({
            "ready": false,
            "status": "draft",
            "reasons": ["required check dependency unavailable"],
            "repository": {
                "path": repository_identity.display().to_string(),
                "head": source.head.clone(),
                "fingerprint": "synthetic-helper-fingerprint"
            }
        })
        .to_string();
        let error = service(missing_dependency_report, Some(0))
            .run_check(repository.path(), &source, &check)
            .unwrap_err();
        assert!(matches!(error, VerificationError::NeedsInput(_)));
        assert!(!error.to_string().contains("dependency unavailable"));

        let error = service(String::new(), Some(2))
            .run_check(repository.path(), &source, &check)
            .unwrap_err();
        assert!(matches!(error, VerificationError::NeedsInput(_)));

        let error = service("not json".into(), Some(0))
            .run_check(repository.path(), &source, &check)
            .unwrap_err();
        assert!(matches!(error, VerificationError::NeedsInput(_)));

        let wrong_repository_report = serde_json::json!({
            "ready": true,
            "status": "ready",
            "repository": {
                "path": external.path().display().to_string(),
                "head": source.head.clone(),
                "fingerprint": "synthetic-helper-fingerprint"
            }
        })
        .to_string();
        let error = service(wrong_repository_report, Some(0))
            .run_check(repository.path(), &source, &check)
            .unwrap_err();
        assert!(matches!(error, VerificationError::NeedsInput(_)));

        let wrong_head_report = serde_json::json!({
            "ready": true,
            "status": "ready",
            "repository": {
                "path": repository_identity.display().to_string(),
                "head": "different-head",
                "fingerprint": "synthetic-helper-fingerprint"
            }
        })
        .to_string();
        let error = service(wrong_head_report, Some(0))
            .run_check(repository.path(), &source, &check)
            .unwrap_err();
        assert!(matches!(error, VerificationError::NeedsInput(_)));

        let mut missing_helper = check.clone();
        missing_helper.args[0] = external
            .path()
            .join("missing-helper.py")
            .to_string_lossy()
            .into_owned();
        let error = service(ready_report.clone(), Some(0))
            .run_check(repository.path(), &source, &missing_helper)
            .unwrap_err();
        assert!(matches!(error, VerificationError::NeedsInput(_)));

        let mut missing_interpreter = check.clone();
        missing_interpreter.program = external.path().join("missing-python.exe");
        let process_service = VerificationService {
            checks: ProcessCheckExecutor,
            reviewer: FixtureReview {
                result: ReviewResult {
                    passed: true,
                    findings: vec![],
                    manual_acceptance: vec![],
                    details: "unused".into(),
                },
            },
        };
        let error = process_service
            .run_check(repository.path(), &source, &missing_interpreter)
            .unwrap_err();
        assert!(matches!(error, VerificationError::NeedsInput(_)));

        let legacy: CheckSpec = serde_json::from_value(serde_json::json!({
            "id": "legacy", "program": "python", "args": [], "cwd": ".",
            "timeout_ms": 1000, "output_limit_bytes": 1024
        }))
        .unwrap();
        assert_eq!(legacy.adapter, None);
        assert!(serde_json::to_value(legacy)
            .unwrap()
            .get("adapter")
            .is_none());
    }

    #[test]
    fn process_registration_failure_needs_input_without_entering_review_or_repair() {
        struct RegistrationFailure;
        impl CheckExecutor for RegistrationFailure {
            fn execute(&self, _: &Path, _: &CheckSpec) -> Result<CheckOutput, String> {
                Ok(CheckOutput {
                    exit_code: Some(0),
                    ..Default::default()
                })
            }

            fn execute_managed(
                &self,
                _: &Path,
                _: &CheckSpec,
                _: &AtomicBool,
                _: &mut dyn FnMut(crate::process::ProcessIdentity) -> Result<(), String>,
            ) -> Result<CheckOutput, String> {
                Err("check_process_registration_failed: injected journal failure".into())
            }
        }

        let d = tempfile::tempdir().unwrap();
        let store = JobsStore::open(d.path().join("jobs.sqlite3")).unwrap();
        let issue_fp = "registration-failure-issue";
        let job_id = awaiting_job(&store, "FAB-L009-PID-REGISTRATION", issue_fp);
        let (repository, code_snapshot) = repo();
        let service = VerificationService {
            checks: RegistrationFailure,
            reviewer: FixtureReview {
                result: ReviewResult {
                    passed: true,
                    findings: vec![],
                    manual_acceptance: vec![],
                    details: "review must not run".into(),
                },
            },
        };
        let envelope = VerificationEnvelope {
            issue_fingerprint: issue_fp.into(),
            worker_task_id: "implementation-task".into(),
            code_snapshot,
            checks: vec![spec()],
            reviewer: ReviewerIdentity {
                task_id: "independent-review".into(),
                provider_id: "review-profile".into(),
                model_id: "review-model".into(),
                reasoning_effort: None,
            },
            acceptance_criteria: vec!["pass".into()],
        };
        let cancel = AtomicBool::new(false);
        let mut on_check_spawn = |_: usize, _: crate::process::ProcessIdentity| Ok(());
        let outcome = service
            .verify_job_with_issue_source_and_gate_guard_managed(
                repository.path(),
                &store,
                job_id,
                &envelope,
                None,
                || Ok(()),
                &cancel,
                &mut on_check_spawn,
                5,
            )
            .unwrap();

        assert!(matches!(
            outcome,
            VerificationOutcome::NeedsInput { ref reason }
                if reason.contains("check process identity could not be persisted")
        ));
        assert_eq!(
            store.get(job_id).unwrap().state,
            crate::jobs::JobState::AwaitingVerification
        );
        assert_eq!(
            store
                .get_checkpoint(job_id, "verification_check_0")
                .unwrap()
                .unwrap()["state"],
            "started"
        );
        assert!(store
            .get_checkpoint(job_id, "verification_repair_round_1")
            .unwrap()
            .is_none());
        assert!(store
            .get_checkpoint(job_id, "verification_review")
            .unwrap()
            .is_none());
    }
    #[test]
    fn review_requires_independent_identity_and_manual_acceptance_remains_blocking() {
        let (d, source) = repo();
        let service = VerificationService {
            checks: FixtureCheck {
                mutate: false,
                fail: false,
            },
            reviewer: FixtureReview {
                result: ReviewResult {
                    passed: true,
                    findings: vec![],
                    manual_acceptance: vec!["human demo".into()],
                    details: "awaiting demo".into(),
                },
            },
        };
        let receipt = service.run_check(d.path(), &source, &spec()).unwrap();
        let req = ReviewRequest {
            job_id: 1,
            worker_task_id: "implementation-task".into(),
            issue_fingerprint: "issue-fp".into(),
            source: source.clone(),
            reviewer: ReviewerIdentity {
                task_id: "review-task".into(),
                provider_id: "reviewer".into(),
                model_id: "model".into(),
                reasoning_effort: None,
            },
            check_receipts: vec![receipt],
            acceptance_criteria: vec!["verified".into()],
        };
        assert_eq!(
            service.review(&req).unwrap().manual_acceptance,
            vec!["human demo"]
        );
        let mut invalid = req;
        invalid.reviewer.task_id.clear();
        assert!(service.review(&invalid).is_err());
    }

    #[test]
    fn manual_acceptance_is_persisted_as_needs_input_and_never_verified() {
        let d = tempfile::tempdir().unwrap();
        let store = JobsStore::open(d.path().join("jobs.sqlite3")).unwrap();
        let issue_fp = "manual-acceptance-issue";
        let job_id = awaiting_job(&store, "FAB-L008-MANUAL", issue_fp);
        let (repo, code_snapshot) = repo();
        let service = VerificationService {
            checks: FixtureCheck {
                mutate: false,
                fail: false,
            },
            reviewer: FixtureReview {
                result: ReviewResult {
                    passed: true,
                    findings: vec![],
                    manual_acceptance: vec!["human demo".into()],
                    details: "awaiting demo".into(),
                },
            },
        };
        let envelope = VerificationEnvelope {
            issue_fingerprint: issue_fp.into(),
            worker_task_id: "implementation-task".into(),
            code_snapshot,
            checks: vec![spec()],
            reviewer: ReviewerIdentity {
                task_id: "review-task".into(),
                provider_id: "reviewer".into(),
                model_id: "model".into(),
                reasoning_effort: None,
            },
            acceptance_criteria: vec!["demo".into()],
        };
        assert!(matches!(
            service
                .verify_job(repo.path(), &store, job_id, &envelope, 5)
                .unwrap(),
            VerificationOutcome::NeedsManualAcceptance { .. }
        ));
        let job = store.get(job_id).unwrap();
        assert_eq!(job.state, crate::jobs::JobState::NeedsInput);
        assert_eq!(job.completion_gate, "manual_acceptance");
        assert!(store
            .get_checkpoint(job_id, "manual_acceptance_required")
            .unwrap()
            .is_some());
    }

    #[test]
    fn failed_independent_review_never_verifies_the_job() {
        let d = tempfile::tempdir().unwrap();
        let store = JobsStore::open(d.path().join("jobs.sqlite3")).unwrap();
        let issue_fp = "failed-review-issue";
        let job_id = awaiting_job(&store, "FAB-L008-FAILED-REVIEW", issue_fp);
        let (repo, code_snapshot) = repo();
        let service = VerificationService {
            checks: FixtureCheck {
                mutate: false,
                fail: false,
            },
            reviewer: FixtureReview {
                result: ReviewResult {
                    passed: false,
                    findings: vec!["missing behavior".into()],
                    manual_acceptance: vec!["keyboard behavior requires direct evidence".into()],
                    details: "review rejected the implementation".into(),
                },
            },
        };
        let envelope = VerificationEnvelope {
            issue_fingerprint: issue_fp.into(),
            worker_task_id: "implementation-task".into(),
            code_snapshot,
            checks: vec![spec()],
            reviewer: ReviewerIdentity {
                task_id: "review-task".into(),
                provider_id: "reviewer".into(),
                model_id: "model".into(),
                reasoning_effort: None,
            },
            acceptance_criteria: vec!["required behavior".into()],
        };
        assert!(matches!(
            service.verify_job(repo.path(), &store, job_id, &envelope, 5),
            Err(VerificationError::ReviewFailed(_))
        ));
        assert_eq!(
            store.get(job_id).unwrap().state,
            crate::jobs::JobState::AwaitingVerification
        );
        let review = store
            .get_checkpoint(job_id, "verification_review")
            .unwrap()
            .unwrap();
        assert_eq!(review["state"], "completed");
        assert_eq!(review["passed"], false);
        assert_eq!(
            review["manualAcceptance"][0],
            "keyboard behavior requires direct evidence"
        );
        assert!(store
            .get_checkpoint(job_id, "manual_acceptance_required")
            .unwrap()
            .is_none());
    }

    #[test]
    fn repair_rounds_are_sequential_durable_and_requeue_then_fail_closed() {
        let d = tempfile::tempdir().unwrap();
        let store = JobsStore::open(d.path().join("jobs.sqlite3")).unwrap();
        let issue_fp = "repair-issue";
        let job_id = awaiting_job(&store, "FAB-L008-REPAIR", issue_fp);
        let first = RepairProgress {
            round: 1,
            state: RepairState::Started,
            source_fingerprint: "code-1".into(),
            result_source_fingerprint: None,
            summary: "repair queued".into(),
        };
        VerificationService::<FixtureCheck, FixtureReview>::save_repair_progress(
            &store, job_id, issue_fp, &first, 5,
        )
        .unwrap();
        let queued = store
            .requeue_for_verification_repair(job_id, issue_fp, 1, "review findings", 7)
            .unwrap();
        assert_eq!(queued.state, crate::jobs::JobState::Queued);
        assert!(store
            .requeue_for_verification_repair(job_id, issue_fp, 1, "duplicate", 8)
            .is_err());
        // Requeue brings the job back through an implementation attempt before another verification gate.
        let claimed = store.claim("repair-worker", 9, 60_000).unwrap().unwrap();
        store
            .start_attempt(job_id, "repair-worker", claimed.fencing_token, 10)
            .unwrap();
        store
            .record_attempt_outcome(
                job_id,
                "repair-worker",
                claimed.fencing_token,
                AttemptOutcome::ProposedComplete {
                    summary: "second proposal".into(),
                },
                11,
            )
            .unwrap();
        let first_completed = RepairProgress {
            state: RepairState::Completed,
            result_source_fingerprint: Some("code-2".into()),
            summary: "repair proposal completed".into(),
            ..first
        };
        VerificationService::<FixtureCheck, FixtureReview>::save_repair_progress(
            &store,
            job_id,
            issue_fp,
            &first_completed,
            12,
        )
        .unwrap();
        let second = RepairProgress {
            round: 2,
            state: RepairState::Started,
            source_fingerprint: "code-2".into(),
            result_source_fingerprint: None,
            summary: "second repair queued".into(),
        };
        VerificationService::<FixtureCheck, FixtureReview>::save_repair_progress(
            &store, job_id, issue_fp, &second, 12,
        )
        .unwrap();
        let exhausted = RepairProgress {
            state: RepairState::Exhausted,
            result_source_fingerprint: Some("code-3".into()),
            summary: "repair limit exhausted".into(),
            ..second
        };
        VerificationService::<FixtureCheck, FixtureReview>::save_repair_progress(
            &store, job_id, issue_fp, &exhausted, 13,
        )
        .unwrap();
        let failed = store
            .fail_verification(
                job_id,
                issue_fp,
                "head-3",
                "code-3",
                "review still failed after bounded repairs",
                14,
            )
            .unwrap();
        assert_eq!(failed.state, crate::jobs::JobState::Failed);
        assert!(store
            .get_checkpoint(job_id, "verification_failed")
            .unwrap()
            .is_some());
    }
    #[test]
    fn repair_round_limit_is_explicit() {
        assert_eq!(MAX_REPAIR_ROUNDS, 2);
    }

    #[test]
    fn interrupted_repair_is_durable_and_allows_only_the_next_bounded_round() {
        let d = tempfile::tempdir().unwrap();
        let store = JobsStore::open(d.path().join("jobs.sqlite3")).unwrap();
        let issue_fp = "interrupted-repair-issue";
        let job_id = awaiting_job(&store, "FAB-L008-INTERRUPTED", issue_fp);
        let start = RepairProgress {
            round: 1,
            state: RepairState::Started,
            source_fingerprint: "code-before".into(),
            result_source_fingerprint: None,
            summary: "repair dispatched".into(),
        };
        VerificationService::<FixtureCheck, FixtureReview>::save_repair_progress(
            &store, job_id, issue_fp, &start, 5,
        )
        .unwrap();
        store
            .require_manual_acceptance(
                job_id,
                issue_fp,
                "head-before",
                "code-before",
                &["reviewer requested human input".into()],
                6,
            )
            .unwrap();
        let interrupted = RepairProgress {
            state: RepairState::Interrupted,
            summary: "worker lost; reconcile its process and effects".into(),
            ..start
        };
        VerificationService::<FixtureCheck, FixtureReview>::save_repair_progress(
            &store,
            job_id,
            issue_fp,
            &interrupted,
            7,
        )
        .unwrap();
        assert_eq!(
            store
                .get_checkpoint(job_id, "verification_repair_round_1")
                .unwrap()
                .unwrap()["state"],
            "interrupted"
        );
        assert_eq!(
            store.get(job_id).unwrap().state,
            crate::jobs::JobState::NeedsInput
        );
    }

    #[test]
    fn source_snapshot_includes_untracked_nul_bytes() {
        let (d, before) = repo();
        std::fs::write(d.path().join("nul.bin"), b"first\0payload").unwrap();
        let first = source_snapshot(d.path()).unwrap();
        assert_ne!(first.fingerprint, before.fingerprint);
        std::fs::write(d.path().join("nul.bin"), b"first\0payload\0").unwrap();
        let second = source_snapshot(d.path()).unwrap();
        assert_ne!(first.fingerprint, second.fingerprint);
    }

    #[test]
    fn fingerprint_field_framing_is_unambiguous_for_arbitrary_bytes() {
        let mut left = Vec::new();
        append_fingerprint_field(&mut left, b"file", b"a\0b");
        append_fingerprint_field(&mut left, b"file", b"c");
        let mut right = Vec::new();
        append_fingerprint_field(&mut right, b"file", b"a");
        append_fingerprint_field(&mut right, b"file", b"b\0c");
        assert_ne!(left, right);
    }

    #[test]
    fn verified_code_snapshot_is_separate_from_submitted_issue_fingerprint() {
        let d = tempfile::tempdir().unwrap();
        let store = JobsStore::open(d.path().join("jobs.sqlite3")).unwrap();
        let limits = ResourceLimits {
            max_attempts: 3,
            max_tokens: None,
            max_cost_micros: None,
            max_seconds: Some(300),
            max_concurrency: 1,
        };
        let issue_fp = "markdown-source-sha256";
        let job = store
            .submit(
                SubmitJob {
                    idempotency_key: "verify-distinct-snapshots".into(),
                    issue_id: "FAB-L008".into(),
                    issue_type: "enhancement".into(),
                    parent_issue_ids: vec![],
                    source_fingerprint: issue_fp.into(),
                    issue_snapshot: "authoritative issue markdown".into(),
                    plan: JobPlan {
                        tasks: vec![PlannedTask {
                            task_id: "implement".into(),
                            parent_task_id: None,
                            kind: TaskKind::Implementation,
                            files: vec!["src/lib.rs".into()],
                            acceptance: vec!["pass".into()],
                            limits: limits.clone(),
                            provider_id: None,
                            model_id: None,
                            route_requirements: None,
                        }],
                        limits,
                    },
                    dependencies: vec![],
                },
                1,
            )
            .unwrap();
        let claimed = store.claim("worker", 2, 60_000).unwrap().unwrap();
        store
            .start_attempt(job.id, "worker", claimed.fencing_token, 3)
            .unwrap();
        store
            .record_attempt_outcome(
                job.id,
                "worker",
                claimed.fencing_token,
                AttemptOutcome::ProposedComplete {
                    summary: "proposed".into(),
                },
                4,
            )
            .unwrap();
        let (repo, code_snapshot) = repo();
        let service = VerificationService {
            checks: FixtureCheck {
                mutate: false,
                fail: false,
            },
            reviewer: FixtureReview {
                result: ReviewResult {
                    passed: true,
                    findings: vec![],
                    manual_acceptance: vec![],
                    details: "review passed".into(),
                },
            },
        };
        let envelope = VerificationEnvelope {
            issue_fingerprint: issue_fp.into(),
            worker_task_id: "implementation-task".into(),
            code_snapshot,
            checks: vec![spec()],
            reviewer: ReviewerIdentity {
                task_id: "independent-review".into(),
                provider_id: "review-profile".into(),
                model_id: "review-model".into(),
                reasoning_effort: None,
            },
            acceptance_criteria: vec!["pass".into()],
        };
        assert!(matches!(
            service
                .verify_job(repo.path(), &store, job.id, &envelope, 5)
                .unwrap(),
            VerificationOutcome::Verified { .. }
        ));
        assert_eq!(
            store.get(job.id).unwrap().state,
            crate::jobs::JobState::Verified
        );
    }

    #[test]
    fn authoritative_issue_edit_during_review_cannot_verify_job() {
        let d = tempfile::tempdir().unwrap();
        let store = JobsStore::open(d.path().join("jobs.sqlite3")).unwrap();
        let issue_path = d.path().join("FAB-L008.md");
        let raw = "---\nid: \"FAB-L008\"\ntitle: \"Original\"\nstatus: \"Open\"\ntype: \"enhancement\"\n---\noriginal body\n";
        std::fs::write(&issue_path, raw).unwrap();
        let issue_fingerprint = crate::issues::revision(raw);
        let job_id = awaiting_job(&store, "FAB-L008", &issue_fingerprint);
        let (repository, code_snapshot) = repo();
        let service = VerificationService {
            checks: FixtureCheck {
                mutate: false,
                fail: false,
            },
            reviewer: IssueChangingReview {
                issue_path: issue_path.clone(),
                result: ReviewResult {
                    passed: true,
                    findings: vec![],
                    manual_acceptance: vec![],
                    details: "review passed".into(),
                },
            },
        };
        let envelope = VerificationEnvelope {
            issue_fingerprint,
            worker_task_id: "implementation-task".into(),
            code_snapshot,
            checks: vec![spec()],
            reviewer: ReviewerIdentity {
                task_id: "independent-review".into(),
                provider_id: "review-profile".into(),
                model_id: "review-model".into(),
                reasoning_effort: None,
            },
            acceptance_criteria: vec!["pass".into()],
        };
        assert!(matches!(
            service.verify_job_with_issue_source(
                repository.path(),
                &store,
                job_id,
                &envelope,
                &issue_path,
                5,
            ),
            Err(VerificationError::StaleIssueSource { .. })
        ));
        assert_eq!(
            store.get(job_id).unwrap().state,
            crate::jobs::JobState::AwaitingVerification
        );
    }

    #[test]
    fn gate_guard_failure_does_not_verify_job() {
        let d = tempfile::tempdir().unwrap();
        let store = JobsStore::open(d.path().join("jobs.sqlite3")).unwrap();
        let issue_fingerprint = "trusted-fixture-issue-revision";
        let job_id = awaiting_job(&store, "FAB-L008-GATE-GUARD", issue_fingerprint);
        let (repository, code_snapshot) = repo();
        let service = VerificationService {
            checks: FixtureCheck {
                mutate: false,
                fail: false,
            },
            reviewer: FixtureReview {
                result: ReviewResult {
                    passed: true,
                    findings: vec![],
                    manual_acceptance: vec![],
                    details: "review passed".into(),
                },
            },
        };
        let envelope = VerificationEnvelope {
            issue_fingerprint: issue_fingerprint.into(),
            worker_task_id: "implementation-task".into(),
            code_snapshot,
            checks: vec![spec()],
            reviewer: ReviewerIdentity {
                task_id: "independent-review".into(),
                provider_id: "review-profile".into(),
                model_id: "review-model".into(),
                reasoning_effort: None,
            },
            acceptance_criteria: vec!["pass".into()],
        };
        assert!(matches!(
            service.verify_job_with_issue_source_and_gate_guard(
                repository.path(),
                &store,
                job_id,
                &envelope,
                None,
                || Err("lease lost".into()),
                5,
            ),
            Err(VerificationError::GateGuardFailed(_))
        ));
        assert_eq!(
            store.get(job_id).unwrap().state,
            crate::jobs::JobState::AwaitingVerification
        );
    }

    struct MemoryLedger(std::sync::Mutex<Option<EffectReceipt>>);
    impl EffectLedger for MemoryLedger {
        fn record(&self, r: &EffectReceipt) -> Result<(), String> {
            *self.0.lock().unwrap() = Some(r.clone());
            Ok(())
        }
        fn get(&self, _: &str) -> Result<Option<EffectReceipt>, String> {
            Ok(self.0.lock().unwrap().clone())
        }
    }
    #[test]
    fn ambiguous_publication_is_never_retried_blindly() {
        let ledger = MemoryLedger(std::sync::Mutex::new(Some(EffectReceipt {
            idempotency_key: "publish-1".into(),
            kind: EffectKind::Publish,
            state: EffectState::Ambiguous,
            reference: None,
            evidence: "connection dropped".into(),
            source: None,
            check_ids: Vec::new(),
        })));
        assert!(begin_effect(
            &ledger,
            "publish-1",
            EffectKind::Publish,
            "retry",
            SourceSnapshot {
                head: "abc".into(),
                fingerprint: "code-fp".into()
            },
            vec!["tests".into()]
        )
        .is_err());
    }

    #[test]
    fn sqlite_effect_ledger_is_durable_append_only_and_rejects_invalid_transitions() {
        let d = tempfile::tempdir().unwrap();
        let path = d.path().join("effects.sqlite3");
        let ledger = SqliteEffectLedger::open(&path).unwrap();
        let started = EffectReceipt {
            idempotency_key: "push-42".into(),
            kind: EffectKind::Push,
            state: EffectState::Started,
            reference: None,
            evidence: "intent".into(),
            source: Some(SourceSnapshot {
                head: "abc".into(),
                fingerprint: "sha256-def".into(),
            }),
            check_ids: vec!["tests".into()],
        };
        ledger.record(&started).unwrap();
        let ambiguous = EffectReceipt {
            state: EffectState::Ambiguous,
            evidence: "connection dropped".into(),
            ..started.clone()
        };
        ledger.record(&ambiguous).unwrap();
        assert_eq!(ledger.get("push-42").unwrap(), Some(ambiguous.clone()));
        assert!(ledger
            .record(&EffectReceipt {
                state: EffectState::Started,
                ..ambiguous.clone()
            })
            .is_err());
        let resumed = SqliteEffectLedger::open(&path).unwrap();
        assert_eq!(resumed.get("push-42").unwrap(), Some(ambiguous));
        assert!(resumed
            .record(&EffectReceipt {
                state: EffectState::Reconciled,
                source: Some(SourceSnapshot {
                    head: "different-head".into(),
                    fingerprint: "different-fingerprint".into(),
                }),
                ..started
            })
            .is_err());
    }
    #[test]
    fn optional_contract_adapter_fails_explicitly_when_unavailable() {
        assert!(require_contract_adapter(false).is_err());
    }

    fn finite_contract_spec() -> CheckSpec {
        let mut check = spec();
        check.contract = Some(ContractSpec {
            criteria: vec!["pass".into()],
            specimens: vec![crate::contracts::ContractSpecimen {
                id: "sample".into(),
                input: json!(1),
                expected: json!(2),
            }],
        });
        check
    }
    fn finite_output(actual: i32) -> CheckOutput {
        CheckOutput { exit_code:Some(0),stdout:json!({"schema_version":1,"observations":[
            {"specimen_id":"sample","input":1,"assertion":"deep_equal","expected":2,"actual":actual,"executed":true}
        ]}).to_string(),..Default::default() }
    }
    struct ContractCheck {
        output: CheckOutput,
        mutate: bool,
        unavailable: bool,
    }
    impl CheckExecutor for ContractCheck {
        fn execute(&self, repository: &Path, _: &CheckSpec) -> Result<CheckOutput, String> {
            if self.mutate {
                std::fs::write(repository.join("changed.txt"), "changed").unwrap();
            }
            if self.unavailable {
                return Err("missing command".into());
            }
            Ok(self.output.clone())
        }
    }
    struct NoContractReviewer;
    impl ReviewExecutor for NoContractReviewer {
        fn review(&self, _: &ReviewRequest) -> Result<ReviewResult, String> {
            panic!("contract-only criteria must not dispatch review")
        }
    }
    fn contract_envelope(source: SourceSnapshot) -> VerificationEnvelope {
        VerificationEnvelope {
            issue_fingerprint: "contract-issue".into(),
            worker_task_id: "worker".into(),
            code_snapshot: source,
            checks: vec![finite_contract_spec()],
            reviewer: ReviewerIdentity {
                task_id: "review".into(),
                provider_id: "fixture".into(),
                model_id: "fixture".into(),
                reasoning_effort: None,
            },
            acceptance_criteria: vec!["pass".into()],
        }
    }
    #[test]
    fn old_check_configuration_has_no_implicit_contract() {
        let mut value = serde_json::to_value(spec()).unwrap();
        value.as_object_mut().unwrap().remove("contract");
        let check: CheckSpec = serde_json::from_value(value).unwrap();
        assert!(check.contract.is_none());
    }
    #[test]
    fn contract_only_gate_persists_evidence_without_model_review() {
        let (repository, source) = repo();
        let data = tempfile::tempdir().unwrap();
        let store = JobsStore::open(data.path().join("jobs.db")).unwrap();
        let id = awaiting_job(&store, "contract-only", "contract-issue");
        let service = VerificationService {
            checks: ContractCheck {
                output: finite_output(2),
                mutate: false,
                unavailable: false,
            },
            reviewer: NoContractReviewer,
        };
        let outcome = service
            .verify_job(repository.path(), &store, id, &contract_envelope(source), 5)
            .unwrap();
        assert!(matches!(outcome, VerificationOutcome::Verified { .. }));
        let checkpoint = store
            .get_checkpoint(id, "verification_check_0")
            .unwrap()
            .unwrap();
        assert_eq!(checkpoint["receipt"]["contract"]["status"], "passed");
        assert_eq!(
            checkpoint["receipt"]["contract"]["observations"][0]["actual"],
            2
        );
    }
    #[test]
    fn contract_failures_and_inconclusive_reports_never_verify() {
        let mut missing = finite_output(2);
        missing.stdout = json!({"schema_version":1,"observations":[]}).to_string();
        let outputs = vec![
            finite_output(3),
            missing,
            CheckOutput {
                exit_code: Some(1),
                ..finite_output(2)
            },
            CheckOutput {
                timed_out: true,
                ..finite_output(2)
            },
            CheckOutput {
                cancelled: true,
                ..finite_output(2)
            },
            CheckOutput {
                truncated: true,
                ..finite_output(2)
            },
            CheckOutput {
                output_incomplete: true,
                ..finite_output(2)
            },
        ];
        for (index, output) in outputs.into_iter().enumerate() {
            let (repository, source) = repo();
            let data = tempfile::tempdir().unwrap();
            let store = JobsStore::open(data.path().join("jobs.db")).unwrap();
            let id = awaiting_job(&store, &format!("contract-{index}"), "contract-issue");
            let service = VerificationService {
                checks: ContractCheck {
                    output,
                    mutate: false,
                    unavailable: false,
                },
                reviewer: NoContractReviewer,
            };
            let result =
                service.verify_job(repository.path(), &store, id, &contract_envelope(source), 5);
            if index == 0 {
                assert!(matches!(result, Err(VerificationError::CheckFailed(_))));
            } else {
                assert!(matches!(result, Ok(VerificationOutcome::NeedsInput { .. })));
            }
            assert_eq!(
                store.get(id).unwrap().state,
                crate::jobs::JobState::AwaitingVerification
            );
            let cp = store
                .get_checkpoint(id, "verification_check_0")
                .unwrap()
                .unwrap();
            assert!(cp["receipt"]["contract"].is_object());
            assert!(store
                .get_checkpoint(id, "verification_review")
                .unwrap()
                .is_none());
        }
    }
    #[test]
    fn unavailable_contract_command_is_checkpointed_as_inconclusive() {
        let (repository, source) = repo();
        let data = tempfile::tempdir().unwrap();
        let store = JobsStore::open(data.path().join("jobs.db")).unwrap();
        let id = awaiting_job(&store, "contract-unavailable", "contract-issue");
        let service = VerificationService {
            checks: ContractCheck {
                output: finite_output(2),
                mutate: false,
                unavailable: true,
            },
            reviewer: NoContractReviewer,
        };
        assert!(matches!(
            service.verify_job(repository.path(), &store, id, &contract_envelope(source), 5),
            Ok(VerificationOutcome::NeedsInput { .. })
        ));
        let cp = store
            .get_checkpoint(id, "verification_check_0")
            .unwrap()
            .unwrap();
        assert_eq!(cp["contractStatus"], "inconclusive");
    }
    #[test]
    fn contract_source_mutation_cannot_create_passing_receipt() {
        let (repository, source) = repo();
        let service = VerificationService {
            checks: ContractCheck {
                output: finite_output(2),
                mutate: true,
                unavailable: false,
            },
            reviewer: NoContractReviewer,
        };
        assert!(matches!(
            service.run_check(repository.path(), &source, &finite_contract_spec()),
            Err(VerificationError::StaleSource { .. })
        ));
    }
    struct RemainingReview {
        result: ReviewResult,
    }
    impl ReviewExecutor for RemainingReview {
        fn review(&self, request: &ReviewRequest) -> Result<ReviewResult, String> {
            assert_eq!(
                request.acceptance_criteria,
                vec!["outside contract".to_string()]
            );
            Ok(self.result.clone())
        }
    }
    #[test]
    fn mixed_scope_preserves_reviewer_failure_and_manual_acceptance() {
        for failed in [false, true] {
            let (repository, source) = repo();
            let data = tempfile::tempdir().unwrap();
            let store = JobsStore::open(data.path().join("jobs.db")).unwrap();
            let id = awaiting_job(&store, "contract-mixed", "contract-issue");
            let service = VerificationService {
                checks: ContractCheck {
                    output: finite_output(2),
                    mutate: false,
                    unavailable: false,
                },
                reviewer: RemainingReview {
                    result: ReviewResult {
                        passed: !failed,
                        findings: if failed {
                            vec!["defect".into()]
                        } else {
                            vec![]
                        },
                        manual_acceptance: if failed {
                            vec![]
                        } else {
                            vec!["human acceptance".into()]
                        },
                        details: "outside review".into(),
                    },
                },
            };
            let mut envelope = contract_envelope(source);
            envelope.acceptance_criteria.push("outside contract".into());
            let result = service.verify_job(repository.path(), &store, id, &envelope, 5);
            if failed {
                assert!(matches!(result, Err(VerificationError::ReviewFailed(_))));
            } else {
                assert!(matches!(
                    result,
                    Ok(VerificationOutcome::NeedsManualAcceptance { .. })
                ));
            }
            assert_ne!(
                store.get(id).unwrap().state,
                crate::jobs::JobState::Verified
            );
        }
    }
    #[test]
    fn contract_cannot_claim_unknown_or_overlapping_acceptance() {
        let (repository, source) = repo();
        let data = tempfile::tempdir().unwrap();
        let store = JobsStore::open(data.path().join("jobs.db")).unwrap();
        let id = awaiting_job(&store, "contract-scope", "contract-issue");
        let service = VerificationService {
            checks: ContractCheck {
                output: finite_output(2),
                mutate: false,
                unavailable: false,
            },
            reviewer: NoContractReviewer,
        };
        let mut envelope = contract_envelope(source);
        envelope.acceptance_criteria = vec!["other".into()];
        assert!(matches!(
            service.verify_job(repository.path(), &store, id, &envelope, 5),
            Err(VerificationError::Invalid(_))
        ));
        envelope.acceptance_criteria = vec!["pass".into()];
        let mut duplicate = finite_contract_spec();
        duplicate.id = "second".into();
        envelope.checks.push(duplicate);
        assert!(matches!(
            service.verify_job(repository.path(), &store, id, &envelope, 5),
            Err(VerificationError::Invalid(_))
        ));
    }
    #[cfg(windows)]
    #[test]
    fn real_process_contract_report_is_consumed_by_the_host() {
        let (repository, source) = repo();
        let mut check = finite_contract_spec();
        check.program = "powershell.exe".into();
        check.args = vec![
            "-NoProfile".into(),
            "-NonInteractive".into(),
            "-Command".into(),
            format!(
                "[Console]::Out.Write('{}')",
                finite_output(2).stdout.replace('\'', "''")
            ),
        ];
        check.timeout_ms = 10_000;
        let service = VerificationService {
            checks: ProcessCheckExecutor,
            reviewer: NoContractReviewer,
        };
        let receipt = service
            .run_check(repository.path(), &source, &check)
            .unwrap();
        assert!(receipt.passed);
        assert_eq!(receipt.contract.unwrap().status, ContractStatus::Passed);
    }

    #[test]
    fn mixed_contracts_cannot_bypass_a_failed_ordinary_check() {
        struct MixedCheck;
        impl CheckExecutor for MixedCheck {
            fn execute(&self, _: &Path, check: &CheckSpec) -> Result<CheckOutput, String> {
                Ok(if check.contract.is_some() {
                    finite_output(2)
                } else {
                    CheckOutput {
                        exit_code: Some(1),
                        ..Default::default()
                    }
                })
            }
        }
        let (repository, source) = repo();
        let data = tempfile::tempdir().unwrap();
        let store = JobsStore::open(data.path().join("jobs.db")).unwrap();
        let id = awaiting_job(&store, "mixed-ordinary", "contract-issue");
        let service = VerificationService {
            checks: MixedCheck,
            reviewer: NoContractReviewer,
        };
        let mut envelope = contract_envelope(source);
        let mut ordinary = spec();
        ordinary.id = "ordinary".into();
        envelope.checks.push(ordinary);
        assert!(matches!(
            service.verify_job(repository.path(), &store, id, &envelope, 5),
            Err(VerificationError::CheckFailed(_))
        ));
        assert_eq!(
            store.get(id).unwrap().state,
            crate::jobs::JobState::AwaitingVerification
        );
        assert!(store
            .get_checkpoint(id, "verification_review")
            .unwrap()
            .is_none());
    }
    #[test]
    fn direct_review_rejects_a_forged_passing_contract_flag() {
        let (repository, source) = repo();
        let service = VerificationService {
            checks: ContractCheck {
                output: finite_output(2),
                mutate: false,
                unavailable: false,
            },
            reviewer: NoContractReviewer,
        };
        let mut receipt = service
            .run_check(repository.path(), &source, &finite_contract_spec())
            .unwrap();
        receipt.contract.as_mut().unwrap().observations[0].actual = json!(999);
        let envelope = contract_envelope(source.clone());
        let request = ReviewRequest {
            job_id: 1,
            worker_task_id: envelope.worker_task_id,
            issue_fingerprint: envelope.issue_fingerprint,
            source,
            reviewer: envelope.reviewer,
            check_receipts: vec![receipt],
            acceptance_criteria: envelope.acceptance_criteria,
        };
        assert!(matches!(
            service.review(&request),
            Err(VerificationError::Invalid(_))
        ));
    }
}
