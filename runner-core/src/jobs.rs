//! Durable, provider-agnostic issue jobs for the shared runner.
//!
//! This module owns job state and recovery facts only. It does not start agents,
//! infer that an issue is complete from a process exit, or retry ambiguous work.

use rusqlite::{
    params, Connection, ErrorCode, OptionalExtension, Transaction, TransactionBehavior,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

const SCHEMA_VERSION: i64 = 1;
pub const MAX_PLAN_TASKS: usize = 32;
pub const MAX_PLAN_DEPTH: usize = 1;
pub const MAX_TASK_FILES: usize = 128;
pub const MAX_TEXT_BYTES: usize = 16 * 1024;
pub const MAX_CHECKPOINT_BYTES: usize = 256 * 1024;

#[derive(Debug)]
pub enum JobsError {
    Sql(rusqlite::Error),
    Invalid(String),
    NotFound(String),
    Conflict(String),
    UnsupportedSchema(i64),
    Initialization(String),
    LeaseLost,
}

impl std::fmt::Display for JobsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Sql(e) => match e {
                rusqlite::Error::SqliteFailure(failure, _) => write!(
                    f,
                    "job store error: {e} (SQLite {:?}, extended code {})",
                    failure.code, failure.extended_code
                ),
                _ => write!(f, "job store error: {e}"),
            },
            Self::Invalid(s) => write!(f, "invalid job request: {s}"),
            Self::NotFound(s) => write!(f, "job not found: {s}"),
            Self::Conflict(s) => write!(f, "job conflict: {s}"),
            Self::Initialization(stage) => write!(f, "job store initialization failed at {stage}"),
            Self::UnsupportedSchema(v) => write!(
                f,
                "job store schema {v} is newer than supported schema {SCHEMA_VERSION}"
            ),
            Self::LeaseLost => f.write_str("job lease is no longer owned by this worker"),
        }
    }
}
impl std::error::Error for JobsError {}
impl From<rusqlite::Error> for JobsError {
    fn from(e: rusqlite::Error) -> Self {
        Self::Sql(e)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JobState {
    Blocked,
    Queued,
    Leased,
    Running,
    NeedsInput,
    AwaitingVerification,
    Verified,
    Failed,
    Cancelled,
    ReconciliationRequired,
    Stale,
}
impl JobState {
    fn as_str(self) -> &'static str {
        match self {
            Self::Blocked => "blocked",
            Self::Queued => "queued",
            Self::Leased => "leased",
            Self::Running => "running",
            Self::NeedsInput => "needs_input",
            Self::AwaitingVerification => "awaiting_verification",
            Self::Verified => "verified",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
            Self::ReconciliationRequired => "reconciliation_required",
            Self::Stale => "stale",
        }
    }
    fn parse(s: &str) -> Result<Self, JobsError> {
        match s {
            "blocked" => Ok(Self::Blocked),
            "queued" => Ok(Self::Queued),
            "leased" => Ok(Self::Leased),
            "running" => Ok(Self::Running),
            "needs_input" => Ok(Self::NeedsInput),
            "awaiting_verification" => Ok(Self::AwaitingVerification),
            "verified" => Ok(Self::Verified),
            "failed" => Ok(Self::Failed),
            "cancelled" => Ok(Self::Cancelled),
            "reconciliation_required" => Ok(Self::ReconciliationRequired),
            "stale" => Ok(Self::Stale),
            other => Err(JobsError::Invalid(format!(
                "unknown stored state {other:?}"
            ))),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ResourceLimits {
    pub max_attempts: u8,
    pub max_tokens: Option<u64>,
    pub max_cost_micros: Option<u64>,
    pub max_seconds: Option<u64>,
    pub max_concurrency: u8,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskKind {
    Implementation,
    Worker,
    Reviewer,
    EpicAggregation,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PlannedTask {
    pub task_id: String,
    pub parent_task_id: Option<String>,
    pub kind: TaskKind,
    pub files: Vec<String>,
    pub acceptance: Vec<String>,
    pub limits: ResourceLimits,
    pub provider_id: Option<String>,
    pub model_id: Option<String>,
    /// Optional explicit routing needs and alternate-candidate allow-list.
    /// Older requests omit this and retain the documented safe defaults.
    #[serde(default)]
    pub route_requirements: Option<crate::routing::TaskRoutingRequirements>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct JobPlan {
    pub tasks: Vec<PlannedTask>,
    pub limits: ResourceLimits,
}

/// Resolution for one `depends_on` ID from the Markdown issue.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum DependencyResolution {
    /// Link to a specific job by issue ID and source fingerprint. In a submitted batch,
    /// the issue may refer to another job in that same batch.
    Job {
        prerequisite_issue_id: String,
        required_fingerprint: String,
    },
    /// Explicit closed external prerequisite evidence. Absence alone never satisfies it.
    ExternalClosed {
        source: String,
        reference: String,
        observed_at: String,
        evidence: String,
    },
    Unresolved,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DependencySpec {
    pub issue_id: String,
    pub resolution: DependencyResolution,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SubmitJob {
    pub idempotency_key: String,
    pub issue_id: String,
    pub issue_type: String,
    pub parent_issue_ids: Vec<String>,
    /// Caller-computed digest of the exact authoritative Markdown bytes.
    pub source_fingerprint: String,
    /// Immutable serialized issue snapshot; this store never writes back to Markdown.
    pub issue_snapshot: String,
    pub plan: JobPlan,
    pub dependencies: Vec<DependencySpec>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Job {
    pub id: i64,
    pub issue_id: String,
    pub source_fingerprint: String,
    pub state: JobState,
    pub completion_gate: String,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
    pub attempt_count: u32,
    pub max_attempts: u8,
    pub cancel_requested: bool,
    pub lease_owner: Option<String>,
    pub lease_until_ms: Option<i64>,
    pub fencing_token: u64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct EpicProgress {
    pub child_count: usize,
    pub verified_children: usize,
    pub incomplete_children: Vec<String>,
    pub complete: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct JobEvent {
    pub sequence: u64,
    pub timestamp_ms: i64,
    pub kind: String,
    pub payload: Value,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ClaimedJob {
    pub job: Job,
    pub worker_id: String,
    pub fencing_token: u64,
    pub attempt_number: u32,
    pub plan: JobPlan,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ReconciliationItem {
    pub job_id: i64,
    pub attempt_number: u32,
    pub worker_id: String,
    pub process_ref: Option<String>,
    pub lease_until_ms: i64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RecoveryAttemptInfo {
    pub attempt_number: u32,
    pub worker_id: String,
    pub process_ref: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct VerificationEvidence {
    pub check_id: String,
    pub source_fingerprint: String,
    pub passed: bool,
    pub details: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum AttemptOutcome {
    ProposedComplete { summary: String },
    NeedsInput { summary: String },
    Failed { summary: String },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum RecoveryDecision {
    SafeToRetry {
        evidence: String,
        process_confirmed_stopped: bool,
        effects_reconciled: bool,
    },
    KeepNeedsInput {
        evidence: String,
    },
    MarkFailed {
        evidence: String,
    },
}

pub struct JobsStore {
    path: PathBuf,
}

impl JobsStore {
    /// Open or initialize a shared job database. Callers choose a path outside tracked source.
    pub fn open(path: impl Into<PathBuf>) -> Result<Self, JobsError> {
        let path = path.into();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| {
                JobsError::Invalid(format!("cannot create job-store directory: {e}"))
            })?;
        }
        let store = Self { path };
        let mut c = store.connection()?;
        // journal_mode is persistent database state, not a per-connection option.
        // Initialize it once, with bounded Busy/Locked handling for concurrent first opens.
        ensure_wal_journal_mode(&c)?;
        c.pragma_update(None, "foreign_keys", "ON")?;
        let version: i64 = c.pragma_query_value(None, "user_version", |r| r.get(0))?;
        if version > SCHEMA_VERSION {
            return Err(JobsError::UnsupportedSchema(version));
        }
        if version == 0 {
            match migrate_v1(&mut c) {
                Ok(()) => {}
                Err(error @ JobsError::UnsupportedSchema(_)) => return Err(error),
                Err(error) => {
                    return Err(JobsError::Initialization(format!(
                        "schema migration: {error}"
                    )))
                }
            }
        }
        Ok(store)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
    fn connection(&self) -> Result<Connection, JobsError> {
        let c = Connection::open(&self.path)?;
        c.busy_timeout(Duration::from_secs(10))?;
        c.pragma_update(None, "foreign_keys", "ON")?;
        Ok(c)
    }

    /// Submit an idempotent batch. Dependencies within the batch are linked only after
    /// every job row has an ID; a cycle or invalid plan aborts the whole transaction.
    pub fn submit_batch(&self, requests: &[SubmitJob], now_ms: i64) -> Result<Vec<Job>, JobsError> {
        if requests.is_empty() {
            return Err(JobsError::Invalid(
                "batch must contain at least one job".into(),
            ));
        }
        let mut ids = HashSet::new();
        for r in requests {
            validate_submit(r)?;
            if !ids.insert(r.issue_id.as_str()) {
                return Err(JobsError::Invalid(format!(
                    "duplicate issue {} in batch",
                    r.issue_id
                )));
            }
        }
        let order = topological_issue_order(requests)?;
        let mut c = self.connection()?;
        let tx = c.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut job_ids: HashMap<String, i64> = HashMap::new();
        for r in requests {
            let encoded =
                serde_json::to_string(r).map_err(|e| JobsError::Invalid(e.to_string()))?;
            let old: Option<(i64, String)> = tx
                .query_row(
                    "SELECT id, request_json FROM jobs WHERE idempotency_key=?1",
                    [&r.idempotency_key],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .optional()?;
            if let Some((id, prior)) = old {
                if prior != encoded {
                    return Err(JobsError::Conflict(format!(
                        "idempotency key {} was already used for a different request",
                        r.idempotency_key
                    )));
                }
                job_ids.insert(r.issue_id.clone(), id);
                continue;
            }
            let unresolved = r
                .dependencies
                .iter()
                .any(|d| matches!(d.resolution, DependencyResolution::Unresolved));
            let state = if unresolved {
                JobState::Blocked
            } else {
                JobState::Queued
            };
            tx.execute("INSERT INTO jobs(issue_id,issue_type,parent_ids_json,idempotency_key,source_fingerprint,issue_snapshot,plan_json,state,completion_gate,request_json,created_at_ms,updated_at_ms,max_attempts) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,'pending',?9,?10,?10,?11)", params![r.issue_id,r.issue_type,serde_json::to_string(&r.parent_issue_ids).unwrap(),r.idempotency_key,r.source_fingerprint,r.issue_snapshot,serde_json::to_string(&r.plan).unwrap(),state.as_str(),encoded,now_ms,r.plan.limits.max_attempts])?;
            job_ids.insert(r.issue_id.clone(), tx.last_insert_rowid());
        }
        for issue_id in order {
            let r = requests
                .iter()
                .find(|r| r.issue_id == issue_id)
                .expect("topological order derives from requests");
            let id = job_ids[&r.issue_id];
            // An idempotent resubmission must not duplicate edges or events.
            let request_matches: bool = tx.query_row(
                "SELECT request_json=?2 FROM jobs WHERE id=?1",
                params![id, serde_json::to_string(r).unwrap()],
                |row| row.get(0),
            )?;
            if !request_matches {
                continue;
            }
            let edge_count: i64 = tx.query_row(
                "SELECT COUNT(*) FROM job_dependencies WHERE job_id=?1",
                [id],
                |row| row.get(0),
            )?;
            if edge_count > 0
                || tx.query_row::<i64, _, _>(
                    "SELECT COUNT(*) FROM job_events WHERE job_id=?1",
                    [id],
                    |row| row.get(0),
                )? > 0
            {
                continue;
            }
            for d in &r.dependencies {
                let (target, fingerprint, resolution, evidence) = match &d.resolution {
                    DependencyResolution::Job {
                        prerequisite_issue_id,
                        required_fingerprint,
                    } => {
                        if d.issue_id != *prerequisite_issue_id {
                            return Err(JobsError::Invalid(
                                "dependency issue ID does not match its prerequisite target".into(),
                            ));
                        }
                        let target = match job_ids.get(prerequisite_issue_id).copied() {
                            Some(id) => Some(id),
                            None => tx.query_row("SELECT id FROM jobs WHERE issue_id=?1 AND source_fingerprint=?2 AND state!='stale' ORDER BY id DESC LIMIT 1", params![prerequisite_issue_id,required_fingerprint], |row| row.get(0)).optional()?,
                        };
                        let resolution = if target.is_some() {
                            "job"
                        } else {
                            "unresolved"
                        };
                        (
                            target,
                            Some(required_fingerprint.as_str()),
                            resolution,
                            None,
                        )
                    }
                    DependencyResolution::ExternalClosed {
                        source,
                        reference,
                        observed_at,
                        evidence,
                    } => {
                        if source.trim().is_empty()
                            || reference.trim().is_empty()
                            || observed_at.trim().is_empty()
                            || evidence.trim().is_empty()
                        {
                            return Err(JobsError::Invalid("external closed dependency requires source, reference, timestamp and evidence".into()));
                        }
                        (
                            None,
                            None,
                            "external_verified",
                            Some(
                                serde_json::json!({"source":source,"reference":reference,"observedAt":observed_at,"status":"closed","evidence":evidence}),
                            ),
                        )
                    }
                    DependencyResolution::Unresolved => (None, None, "unresolved", None),
                };
                if target == Some(id) {
                    return Err(JobsError::Invalid("job cannot depend on itself".into()));
                }
                tx.execute("INSERT INTO job_dependencies(job_id,dependency_issue_id,prerequisite_job_id,required_fingerprint,resolution,evidence_json) VALUES(?1,?2,?3,?4,?5,?6)", params![id,d.issue_id,target,fingerprint,resolution,evidence.map(|v|v.to_string())])?;
            }
            append_event(
                &tx,
                id,
                now_ms,
                "submitted",
                serde_json::json!({"issueId":r.issue_id,"sourceFingerprint":r.source_fingerprint}),
            )?;
        }
        // Link forward references once the matching issue snapshot is submitted.
        for r in requests {
            if let Some(target) = job_ids.get(&r.issue_id) {
                let dependents = {
                    let mut s = tx.prepare("SELECT job_id FROM job_dependencies WHERE dependency_issue_id=?1 AND required_fingerprint=?2 AND resolution='unresolved'")?;
                    let rows = s
                        .query_map(params![r.issue_id, r.source_fingerprint], |row| {
                            row.get::<_, i64>(0)
                        })?
                        .collect::<Result<Vec<_>, _>>()?;
                    rows
                };
                for dependent in dependents {
                    tx.execute("UPDATE job_dependencies SET prerequisite_job_id=?3,resolution='job' WHERE job_id=?1 AND dependency_issue_id=?2",params![dependent,r.issue_id,target])?;
                }
            }
        }
        validate_persisted_dependency_graph(&tx)?;
        // An internally linked prerequisite is not executable until its gate verifies.
        for id in job_ids.values() {
            refresh_eligibility(&tx, *id, now_ms)?;
        }
        tx.commit()?;
        job_ids
            .values()
            .map(|id| read_job(&self.connection()?, *id))
            .collect()
    }

    pub fn submit(&self, request: SubmitJob, now_ms: i64) -> Result<Job, JobsError> {
        self.submit_batch(&[request], now_ms)?
            .into_iter()
            .next()
            .ok_or_else(|| JobsError::Invalid("submission returned no job".into()))
    }

    pub fn get(&self, id: i64) -> Result<Job, JobsError> {
        read_job(&self.connection()?, id)
    }
    /// Return only the summary field from the latest durable attempt result.
    /// The rest of `result_json` is intentionally not exposed through status.
    pub fn latest_attempt_summary(&self, id: i64) -> Result<Option<String>, JobsError> {
        let c = self.connection()?;
        ensure_job(&c, id)?;
        let raw: Option<String> = c
            .query_row(
                "SELECT result_json FROM job_attempts \
                 WHERE job_id=?1 ORDER BY attempt_no DESC,id DESC LIMIT 1",
                [id],
                |row| row.get(0),
            )
            .optional()?
            .flatten();
        Ok(raw
            .filter(|encoded| encoded.len() <= MAX_TEXT_BYTES.saturating_mul(6).saturating_add(128))
            .and_then(|encoded| serde_json::from_str::<Value>(&encoded).ok())
            .and_then(|value| {
                value
                    .get("summary")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
            })
            .filter(|summary| !summary.trim().is_empty()))
    }
    pub fn list(&self, limit: u32) -> Result<Vec<Job>, JobsError> {
        if limit == 0 || limit > 10_000 {
            return Err(JobsError::Invalid("list limit must be 1..=10000".into()));
        }
        let c = self.connection()?;
        let mut s =
            c.prepare("SELECT id FROM jobs ORDER BY created_at_ms DESC,id DESC LIMIT ?1")?;
        let ids = s
            .query_map([limit], |r| r.get::<_, i64>(0))?
            .collect::<Result<Vec<_>, _>>()?;
        ids.into_iter().map(|id| read_job(&c, id)).collect()
    }
    pub fn events(
        &self,
        id: i64,
        after_sequence: u64,
        limit: u32,
    ) -> Result<Vec<JobEvent>, JobsError> {
        if limit == 0 || limit > 1000 {
            return Err(JobsError::Invalid("event limit must be 1..=1000".into()));
        }
        let c = self.connection()?;
        ensure_job(&c, id)?;
        let mut s=c.prepare("SELECT seq,at_ms,kind,payload_json FROM job_events WHERE job_id=?1 AND seq>?2 ORDER BY seq LIMIT ?3")?;
        let rows = s
            .query_map(params![id, after_sequence as i64, limit], |r| {
                Ok(JobEvent {
                    sequence: r.get::<_, i64>(0)? as u64,
                    timestamp_ms: r.get(1)?,
                    kind: r.get(2)?,
                    payload: serde_json::from_str(&r.get::<_, String>(3)?).unwrap_or(Value::Null),
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Atomically claims the oldest currently eligible queued job and creates an attempt.
    pub fn claim(
        &self,
        worker_id: &str,
        now_ms: i64,
        lease_ms: i64,
    ) -> Result<Option<ClaimedJob>, JobsError> {
        self.claim_scoped(worker_id, None, now_ms, lease_ms)
    }

    /// Atomically claim the oldest eligible non-epic job in an explicitly allowed set.
    /// Empty sets claim nothing; unknown IDs are rejected, and known but no-longer-eligible
    /// IDs remain unclaimable. This prevents one batch worker from stealing another batch's work.
    pub fn claim_ids(
        &self,
        worker_id: &str,
        allowed_ids: &[i64],
        now_ms: i64,
        lease_ms: i64,
    ) -> Result<Option<ClaimedJob>, JobsError> {
        if allowed_ids.len() > 10_000
            || allowed_ids.iter().any(|id| *id <= 0)
            || allowed_ids.iter().collect::<HashSet<_>>().len() != allowed_ids.len()
        {
            return Err(JobsError::Invalid(
                "allowed job IDs must be positive, unique, and limited to 10,000".into(),
            ));
        }
        if allowed_ids.is_empty() {
            return Ok(None);
        }
        self.claim_scoped(worker_id, Some(allowed_ids), now_ms, lease_ms)
    }

    fn claim_scoped(
        &self,
        worker_id: &str,
        allowed_ids: Option<&[i64]>,
        now_ms: i64,
        lease_ms: i64,
    ) -> Result<Option<ClaimedJob>, JobsError> {
        validate_text("worker_id", worker_id, 256)?;
        if lease_ms <= 0 || lease_ms > 86_400_000 {
            return Err(JobsError::Invalid(
                "lease duration must be 1..=86400000ms".into(),
            ));
        }
        let mut c = self.connection()?;
        let tx = c.transaction_with_behavior(TransactionBehavior::Immediate)?;
        if let Some(allowed) = allowed_ids {
            for id in allowed {
                ensure_job(&tx, *id)?;
            }
        }
        let allowed_set = allowed_ids.map(|ids| ids.iter().copied().collect::<HashSet<_>>());
        let ids = {
            let mut s = tx.prepare(
                "SELECT id FROM jobs WHERE state IN ('queued','blocked') AND issue_type!='epic' ORDER BY created_at_ms,id",
            )?;
            let rows = s
                .query_map([], |r| r.get::<_, i64>(0))?
                .collect::<Result<Vec<_>, _>>()?;
            rows
        };
        let mut chosen = None;
        for id in ids {
            if allowed_set.as_ref().is_some_and(|set| !set.contains(&id)) {
                continue;
            }
            refresh_eligibility(&tx, id, now_ms)?;
            let state: String =
                tx.query_row("SELECT state FROM jobs WHERE id=?1", [id], |r| r.get(0))?;
            if state == "queued" {
                chosen = Some(id);
                break;
            }
        }
        let Some(id) = chosen else {
            tx.commit()?;
            return Ok(None);
        };
        let (fence, attempt, plan_json, attempt_limit): (i64, i64, String, i64) = tx.query_row(
            "SELECT fencing_token+1,attempt_count+1,plan_json,max_attempts FROM jobs WHERE id=?1",
            [id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )?;
        if attempt > attempt_limit {
            tx.execute(
                "UPDATE jobs SET state='failed',updated_at_ms=?2 WHERE id=?1",
                params![id, now_ms],
            )?;
            append_event(&tx, id, now_ms, "attempt_limit", serde_json::json!({}))?;
            tx.commit()?;
            return Ok(None);
        }
        let plan: JobPlan =
            serde_json::from_str(&plan_json).map_err(|e| JobsError::Invalid(e.to_string()))?;
        let root = plan
            .tasks
            .iter()
            .find(|t| matches!(t.kind, TaskKind::Implementation));
        tx.execute("UPDATE jobs SET state='leased',lease_owner=?2,lease_until_ms=?3,fencing_token=?4,attempt_count=?5,updated_at_ms=?6 WHERE id=?1 AND state='queued'",params![id,worker_id,now_ms.saturating_add(lease_ms),fence,attempt,now_ms])?;
        tx.execute("INSERT INTO job_attempts(job_id,attempt_no,state,worker_id,fencing_token,started_at_ms,assigned_provider_id,assigned_model_id) VALUES(?1,?2,'leased',?3,?4,?5,?6,?7)",params![id,attempt,worker_id,fence,now_ms,root.and_then(|t|t.provider_id.as_deref()),root.and_then(|t|t.model_id.as_deref())])?;
        append_event(
            &tx,
            id,
            now_ms,
            "claimed",
            serde_json::json!({"workerId":worker_id,"fencingToken":fence,"attempt":attempt}),
        )?;
        tx.commit()?;
        Ok(Some(ClaimedJob {
            job: self.get(id)?,
            worker_id: worker_id.into(),
            fencing_token: fence as u64,
            attempt_number: attempt as u32,
            plan,
        }))
    }

    pub fn start_attempt(
        &self,
        id: i64,
        worker: &str,
        fence: u64,
        now: i64,
    ) -> Result<(), JobsError> {
        let mut c = self.connection()?;
        let tx = c.transaction_with_behavior(TransactionBehavior::Immediate)?;
        assert_lease(&tx, id, worker, fence, now)?;
        tx.execute(
            "UPDATE jobs SET state='running',updated_at_ms=?2 WHERE id=?1",
            params![id, now],
        )?;
        tx.execute(
            "UPDATE job_attempts SET state='running' WHERE job_id=?1 AND fencing_token=?2",
            params![id, fence as i64],
        )?;
        append_event(
            &tx,
            id,
            now,
            "attempt_started",
            serde_json::json!({"workerId":worker,"fencingToken":fence}),
        )?;
        tx.commit()?;
        Ok(())
    }

    pub fn heartbeat(
        &self,
        id: i64,
        worker: &str,
        fence: u64,
        now: i64,
        lease_ms: i64,
    ) -> Result<i64, JobsError> {
        if lease_ms <= 0 || lease_ms > 86_400_000 {
            return Err(JobsError::Invalid("invalid lease duration".into()));
        }
        let mut c = self.connection()?;
        let tx = c.transaction_with_behavior(TransactionBehavior::Immediate)?;
        assert_lease(&tx, id, worker, fence, now)?;
        let until = now.saturating_add(lease_ms);
        tx.execute(
            "UPDATE jobs SET lease_until_ms=?2,updated_at_ms=?3 WHERE id=?1",
            params![id, until, now],
        )?;
        tx.execute(
            "UPDATE job_attempts SET heartbeat_at_ms=?3 WHERE job_id=?1 AND fencing_token=?2",
            params![id, fence as i64, now],
        )?;
        tx.commit()?;
        Ok(until)
    }

    /// Persist an opaque owned-process identity so recovery can inspect only that process.
    pub fn record_process_reference(
        &self,
        id: i64,
        worker: &str,
        fence: u64,
        process_ref: &str,
        now: i64,
    ) -> Result<(), JobsError> {
        validate_text("process reference", process_ref, 512)?;
        let mut c = self.connection()?;
        let tx = c.transaction_with_behavior(TransactionBehavior::Immediate)?;
        assert_lease(&tx, id, worker, fence, now)?;
        tx.execute(
            "UPDATE job_attempts SET process_ref=?3 WHERE job_id=?1 AND fencing_token=?2",
            params![id, fence as i64, process_ref],
        )?;
        append_event(
            &tx,
            id,
            now,
            "owned_process_recorded",
            serde_json::json!({"processRef":process_ref}),
        )?;
        tx.commit()?;
        Ok(())
    }

    pub fn cancellation_requested(
        &self,
        id: i64,
        worker: &str,
        fence: u64,
        now: i64,
    ) -> Result<bool, JobsError> {
        let c = self.connection()?;
        let tx = c.unchecked_transaction()?;
        assert_lease(&tx, id, worker, fence, now)?;
        let requested: bool = tx
            .query_row("SELECT cancel_requested FROM jobs WHERE id=?1", [id], |r| {
                r.get::<_, i64>(0)
            })
            .map(|n| n != 0)?;
        tx.commit()?;
        Ok(requested)
    }

    /// Records worker output as a proposal. Proposed completion still needs verification.
    pub fn record_attempt_outcome(
        &self,
        id: i64,
        worker: &str,
        fence: u64,
        outcome: AttemptOutcome,
        now: i64,
    ) -> Result<Job, JobsError> {
        let mut c = self.connection()?;
        let tx = c.transaction_with_behavior(TransactionBehavior::Immediate)?;
        assert_lease(&tx, id, worker, fence, now)?;
        let cancel_requested: bool = tx
            .query_row("SELECT cancel_requested FROM jobs WHERE id=?1", [id], |r| {
                r.get::<_, i64>(0)
            })
            .map(|n| n != 0)?;
        let (state, gate, kind, summary) = if cancel_requested {
            let summary = match outcome {
                AttemptOutcome::ProposedComplete { summary }
                | AttemptOutcome::NeedsInput { summary }
                | AttemptOutcome::Failed { summary } => summary,
            };
            (
                JobState::ReconciliationRequired,
                "pending",
                "cancelled_attempt_reconcile",
                summary,
            )
        } else {
            match outcome {
                AttemptOutcome::ProposedComplete { summary } => (
                    JobState::AwaitingVerification,
                    "pending",
                    "completion_proposed",
                    summary,
                ),
                AttemptOutcome::NeedsInput { summary } => {
                    (JobState::NeedsInput, "pending", "needs_input", summary)
                }
                AttemptOutcome::Failed { summary } => (
                    JobState::ReconciliationRequired,
                    "pending",
                    "attempt_failed_reconcile",
                    summary,
                ),
            }
        };
        validate_text("summary", &summary, MAX_TEXT_BYTES)?;
        tx.execute("UPDATE jobs SET state=?2,completion_gate=?3,lease_owner=NULL,lease_until_ms=NULL,updated_at_ms=?4 WHERE id=?1",params![id,state.as_str(),gate,now])?;
        tx.execute("UPDATE job_attempts SET state=?3,finished_at_ms=?4,result_json=?5 WHERE job_id=?1 AND fencing_token=?2",params![id,fence as i64,state.as_str(),now,serde_json::json!({"summary":summary}).to_string()])?;
        append_event(&tx, id, now, kind, serde_json::json!({"summary":summary}))?;
        tx.commit()?;
        self.get(id)
    }

    /// Expired leases are never requeued automatically; the process/effects may still exist.
    pub fn reconcile_expired(&self, now: i64) -> Result<Vec<ReconciliationItem>, JobsError> {
        self.reconcile_expired_scoped(None, now)
    }

    /// Reconcile only expired leases from a caller-owned job set in a shared store.
    pub fn reconcile_expired_ids(
        &self,
        allowed_ids: &[i64],
        now: i64,
    ) -> Result<Vec<ReconciliationItem>, JobsError> {
        if allowed_ids.len() > 10_000
            || allowed_ids.iter().any(|id| *id <= 0)
            || allowed_ids.iter().collect::<HashSet<_>>().len() != allowed_ids.len()
        {
            return Err(JobsError::Invalid(
                "allowed job IDs must be positive, unique, and limited to 10,000".into(),
            ));
        }
        if allowed_ids.is_empty() {
            return Ok(Vec::new());
        }
        self.reconcile_expired_scoped(Some(allowed_ids), now)
    }

    fn reconcile_expired_scoped(
        &self,
        allowed_ids: Option<&[i64]>,
        now: i64,
    ) -> Result<Vec<ReconciliationItem>, JobsError> {
        let mut c = self.connection()?;
        let tx = c.transaction_with_behavior(TransactionBehavior::Immediate)?;
        if let Some(allowed) = allowed_ids {
            for id in allowed {
                ensure_job(&tx, *id)?;
            }
        }
        let allowed_set = allowed_ids.map(|ids| ids.iter().copied().collect::<HashSet<_>>());
        let items = {
            let mut s=tx.prepare("SELECT j.id,a.attempt_no,a.worker_id,a.process_ref,j.lease_until_ms FROM jobs j JOIN job_attempts a ON a.job_id=j.id AND a.fencing_token=j.fencing_token WHERE j.state IN ('leased','running') AND j.lease_until_ms<=?1")?;
            let rows = s
                .query_map([now], |r| {
                    Ok(ReconciliationItem {
                        job_id: r.get(0)?,
                        attempt_number: r.get::<_, i64>(1)? as u32,
                        worker_id: r.get(2)?,
                        process_ref: r.get(3)?,
                        lease_until_ms: r.get(4)?,
                    })
                })?
                .collect::<Result<Vec<_>, _>>()?;
            rows.into_iter()
                .filter(|item| {
                    allowed_set
                        .as_ref()
                        .is_none_or(|set| set.contains(&item.job_id))
                })
                .collect::<Vec<_>>()
        };
        for item in &items {
            tx.execute("UPDATE jobs SET state='reconciliation_required',completion_gate='pending',lease_owner=NULL,lease_until_ms=NULL,updated_at_ms=?2 WHERE id=?1",params![item.job_id,now])?;
            tx.execute("UPDATE job_attempts SET state='expired_ambiguous',finished_at_ms=?2 WHERE job_id=?1 AND finished_at_ms IS NULL",params![item.job_id,now])?;
            append_event(
                &tx,
                item.job_id,
                now,
                "lease_expired_reconcile_required",
                serde_json::json!({"attempt":item.attempt_number,"workerId":item.worker_id,"processRef":item.process_ref}),
            )?;
        }
        tx.commit()?;
        Ok(items)
    }

    /// Only an explicit reconciliation decision may release ambiguous work for retry.
    pub fn resolve_recovery(
        &self,
        id: i64,
        decision: RecoveryDecision,
        now: i64,
    ) -> Result<Job, JobsError> {
        let mut c = self.connection()?;
        let tx = c.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let state: String = tx
            .query_row("SELECT state FROM jobs WHERE id=?1", [id], |r| r.get(0))
            .optional()?
            .ok_or_else(|| JobsError::NotFound(id.to_string()))?;
        if state != "reconciliation_required" && state != "needs_input" {
            return Err(JobsError::Conflict("job is not awaiting recovery".into()));
        }
        let (next, gate, evidence) = match decision {
            RecoveryDecision::SafeToRetry {
                evidence,
                process_confirmed_stopped,
                effects_reconciled,
            } => {
                if !process_confirmed_stopped || !effects_reconciled {
                    return Err(JobsError::Invalid(
                        "safe retry requires confirmed process termination and reconciled effects"
                            .into(),
                    ));
                }
                (JobState::Queued, "pending", evidence)
            }
            RecoveryDecision::KeepNeedsInput { evidence } => {
                (JobState::NeedsInput, "pending", evidence)
            }
            RecoveryDecision::MarkFailed { evidence } => (JobState::Failed, "pending", evidence),
        };
        validate_text("recovery evidence", &evidence, MAX_TEXT_BYTES)?;
        if next == JobState::Queued {
            let (count, max): (i64, i64) = tx.query_row(
                "SELECT attempt_count,max_attempts FROM jobs WHERE id=?1",
                [id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )?;
            if count >= max {
                return Err(JobsError::Conflict("attempt limit reached".into()));
            }
        }
        tx.execute(
            "UPDATE jobs SET state=?2,completion_gate=?3,cancel_requested=CASE WHEN ?2='queued' THEN 0 ELSE cancel_requested END,updated_at_ms=?4 WHERE id=?1",
            params![id, next.as_str(), gate, now],
        )?;
        append_event(
            &tx,
            id,
            now,
            "recovery_resolved",
            serde_json::json!({"state":next.as_str(),"evidence":evidence}),
        )?;
        tx.commit()?;
        self.get(id)
    }

    /// Return the current fenced attempt's process and worker identity for explicit recovery.
    pub fn recovery_attempt_info(&self, id: i64) -> Result<RecoveryAttemptInfo, JobsError> {
        let c = self.connection()?;
        c.query_row(
            "SELECT a.attempt_no,a.worker_id,a.process_ref FROM job_attempts a JOIN jobs j ON j.id=a.job_id AND j.fencing_token=a.fencing_token WHERE j.id=?1",
            [id],
            |row| Ok(RecoveryAttemptInfo {
                attempt_number: row.get::<_, i64>(0)? as u32,
                worker_id: row.get(1)?,
                process_ref: row.get(2)?,
            }),
        )
        .optional()?
        .ok_or_else(|| JobsError::Conflict("job has no attempt to reconcile".into()))
    }

    pub fn record_checkpoint(
        &self,
        id: i64,
        worker: &str,
        fence: u64,
        name: &str,
        payload: &Value,
        now: i64,
    ) -> Result<(), JobsError> {
        validate_text("checkpoint name", name, 128)?;
        let encoded =
            serde_json::to_string(payload).map_err(|e| JobsError::Invalid(e.to_string()))?;
        if encoded.len() > MAX_CHECKPOINT_BYTES {
            return Err(JobsError::Invalid("checkpoint exceeds size limit".into()));
        }
        let mut c = self.connection()?;
        let tx = c.transaction_with_behavior(TransactionBehavior::Immediate)?;
        assert_lease(&tx, id, worker, fence, now)?;
        tx.execute("INSERT INTO job_checkpoints(job_id,name,fencing_token,at_ms,payload_json) VALUES(?1,?2,?3,?4,?5) ON CONFLICT(job_id,name) DO UPDATE SET fencing_token=excluded.fencing_token,at_ms=excluded.at_ms,payload_json=excluded.payload_json",params![id,name,fence as i64,now,encoded])?;
        append_event(
            &tx,
            id,
            now,
            "checkpoint_saved",
            serde_json::json!({"name":name}),
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Read one durable checkpoint for recovery coordinators.
    pub fn get_checkpoint(&self, id: i64, name: &str) -> Result<Option<Value>, JobsError> {
        let c = self.connection()?;
        let encoded: Option<String> = c
            .query_row(
                "SELECT payload_json FROM job_checkpoints WHERE job_id=?1 AND name=?2",
                params![id, name],
                |r| r.get(0),
            )
            .optional()?;
        encoded
            .map(|value| {
                serde_json::from_str(&value).map_err(|e| JobsError::Invalid(e.to_string()))
            })
            .transpose()
    }

    /// Persist verifier-owned evidence while a job is awaiting verification (its worker lease
    /// has intentionally been closed by ProposedComplete).
    pub fn record_verification_checkpoint(
        &self,
        id: i64,
        issue_fingerprint: &str,
        name: &str,
        payload: &Value,
        now: i64,
    ) -> Result<(), JobsError> {
        validate_text("verification checkpoint name", name, 128)?;
        let encoded =
            serde_json::to_string(payload).map_err(|e| JobsError::Invalid(e.to_string()))?;
        if encoded.len() > MAX_CHECKPOINT_BYTES {
            return Err(JobsError::Invalid("checkpoint exceeds size limit".into()));
        }
        let mut c = self.connection()?;
        let tx = c.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let (stored, state): (String, String) = tx
            .query_row(
                "SELECT source_fingerprint,state FROM jobs WHERE id=?1",
                [id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?
            .ok_or_else(|| JobsError::NotFound(id.to_string()))?;
        let interrupted_repair = name.starts_with("verification_repair_round_")
            && payload.get("state").and_then(Value::as_str) == Some("interrupted");
        if stored != issue_fingerprint
            || (state != "awaiting_verification"
                && !(matches!(state.as_str(), "reconciliation_required" | "needs_input")
                    && interrupted_repair))
        {
            return Err(JobsError::Conflict("verification evidence requires the current issue snapshot and awaiting_verification state".into()));
        }
        tx.execute("INSERT INTO job_checkpoints(job_id,name,fencing_token,at_ms,payload_json) SELECT id,?2,fencing_token,?3,?4 FROM jobs WHERE id=?1 ON CONFLICT(job_id,name) DO UPDATE SET fencing_token=excluded.fencing_token,at_ms=excluded.at_ms,payload_json=excluded.payload_json", params![id,name,now,encoded])?;
        append_event(
            &tx,
            id,
            now,
            "verification_checkpoint_saved",
            serde_json::json!({"name":name,"payload":payload}),
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Put a verification-failed job back in the normal queue for one bounded repair attempt.
    pub fn requeue_for_verification_repair(
        &self,
        id: i64,
        issue_fingerprint: &str,
        round: u8,
        summary: &str,
        now: i64,
    ) -> Result<Job, JobsError> {
        if !(1..=2).contains(&round) {
            return Err(JobsError::Invalid("repair round must be 1 or 2".into()));
        }
        validate_text("repair summary", summary, MAX_TEXT_BYTES)?;
        let mut c = self.connection()?;
        let tx = c.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let (stored, state): (String, String) = tx
            .query_row(
                "SELECT source_fingerprint,state FROM jobs WHERE id=?1",
                [id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?
            .ok_or_else(|| JobsError::NotFound(id.to_string()))?;
        if stored != issue_fingerprint || state != "awaiting_verification" {
            return Err(JobsError::Conflict(
                "repair requires the current issue snapshot in awaiting_verification".into(),
            ));
        }
        let checkpoint: Option<String> = tx
            .query_row(
                "SELECT payload_json FROM job_checkpoints WHERE job_id=?1 AND name=?2",
                params![id, format!("verification_repair_round_{round}")],
                |r| r.get(0),
            )
            .optional()?;
        let checkpoint: Value = checkpoint
            .map(|json| serde_json::from_str(&json))
            .transpose()
            .map_err(|e| JobsError::Invalid(e.to_string()))?
            .ok_or_else(|| {
                JobsError::Conflict("repair must be checkpointed before requeue".into())
            })?;
        if checkpoint.get("round").and_then(Value::as_u64) != Some(round as u64)
            || checkpoint.get("state").and_then(Value::as_str) != Some("started")
        {
            return Err(JobsError::Conflict(
                "requeue requires the matching started repair checkpoint".into(),
            ));
        }
        tx.execute("UPDATE jobs SET state='queued',completion_gate='repair_required',lease_owner=NULL,lease_until_ms=NULL,updated_at_ms=?2 WHERE id=?1", params![id,now])?;
        append_event(
            &tx,
            id,
            now,
            "verification_repair_queued",
            serde_json::json!({"round":round,"summary":summary}),
        )?;
        tx.commit()?;
        self.get(id)
    }

    /// Persist an outstanding human acceptance gate as NeedsInput, retaining both issue and code identities.
    pub fn require_manual_acceptance(
        &self,
        id: i64,
        issue_fingerprint: &str,
        code_head: &str,
        code_fingerprint: &str,
        items: &[String],
        now: i64,
    ) -> Result<Job, JobsError> {
        if items.is_empty() || code_head.trim().is_empty() || code_fingerprint.trim().is_empty() {
            return Err(JobsError::Invalid(
                "manual acceptance evidence is incomplete".into(),
            ));
        }
        if items.len() > 256 {
            return Err(JobsError::Invalid(
                "manual acceptance item count exceeds limit".into(),
            ));
        }
        for item in items {
            validate_text("manual acceptance item", item, MAX_TEXT_BYTES)?;
        }
        let mut c = self.connection()?;
        let tx = c.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let (stored, state): (String, String) = tx
            .query_row(
                "SELECT source_fingerprint,state FROM jobs WHERE id=?1",
                [id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?
            .ok_or_else(|| JobsError::NotFound(id.to_string()))?;
        if stored != issue_fingerprint || state != "awaiting_verification" {
            return Err(JobsError::Conflict(
                "manual acceptance requires the current issue snapshot awaiting verification"
                    .into(),
            ));
        }
        let details=serde_json::json!({"codeHead":code_head,"codeFingerprint":code_fingerprint,"items":items}).to_string();
        if details.len() > MAX_CHECKPOINT_BYTES {
            return Err(JobsError::Invalid(
                "manual acceptance checkpoint exceeds size limit".into(),
            ));
        }
        tx.execute("INSERT INTO job_checkpoints(job_id,name,fencing_token,at_ms,payload_json) SELECT id,'manual_acceptance_required',fencing_token,?2,?3 FROM jobs WHERE id=?1 ON CONFLICT(job_id,name) DO UPDATE SET fencing_token=excluded.fencing_token,at_ms=excluded.at_ms,payload_json=excluded.payload_json",params![id,now,details])?;
        tx.execute("UPDATE jobs SET state='needs_input',completion_gate='manual_acceptance',lease_owner=NULL,lease_until_ms=NULL,updated_at_ms=?2 WHERE id=?1",params![id,now])?;
        append_event(
            &tx,
            id,
            now,
            "manual_acceptance_required",
            serde_json::json!({"codeHead":code_head,"codeFingerprint":code_fingerprint,"items":items}),
        )?;
        tx.commit()?;
        self.get(id)
    }

    /// Close a failed verification gate with durable issue/code identities and diagnostics.
    pub fn fail_verification(
        &self,
        id: i64,
        issue_fingerprint: &str,
        code_head: &str,
        code_fingerprint: &str,
        summary: &str,
        now: i64,
    ) -> Result<Job, JobsError> {
        validate_text("verification failure summary", summary, MAX_TEXT_BYTES)?;
        if code_head.trim().is_empty() || code_fingerprint.trim().is_empty() {
            return Err(JobsError::Invalid(
                "code snapshot identity is required".into(),
            ));
        }
        let mut c = self.connection()?;
        let tx = c.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let (stored, state): (String, String) = tx
            .query_row(
                "SELECT source_fingerprint,state FROM jobs WHERE id=?1",
                [id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?
            .ok_or_else(|| JobsError::NotFound(id.to_string()))?;
        if stored != issue_fingerprint || state != "awaiting_verification" {
            return Err(JobsError::Conflict(
                "verification failure requires the current issue snapshot awaiting verification"
                    .into(),
            ));
        }
        tx.execute("INSERT INTO job_checkpoints(job_id,name,fencing_token,at_ms,payload_json) SELECT id,'verification_failed',fencing_token,?2,?3 FROM jobs WHERE id=?1 ON CONFLICT(job_id,name) DO UPDATE SET fencing_token=excluded.fencing_token,at_ms=excluded.at_ms,payload_json=excluded.payload_json",params![id,now,serde_json::json!({"codeHead":code_head,"codeFingerprint":code_fingerprint,"summary":summary}).to_string()])?;
        tx.execute("UPDATE jobs SET state='failed',completion_gate='verification_failed',lease_owner=NULL,lease_until_ms=NULL,updated_at_ms=?2 WHERE id=?1",params![id,now])?;
        append_event(
            &tx,
            id,
            now,
            "verification_failed",
            serde_json::json!({"codeHead":code_head,"codeFingerprint":code_fingerprint,"summary":summary}),
        )?;
        tx.commit()?;
        self.get(id)
    }

    /// Mark the job stale when authoritative Markdown no longer matches its snapshot.
    pub fn check_source_fingerprint(
        &self,
        id: i64,
        current: &str,
        now: i64,
    ) -> Result<bool, JobsError> {
        let mut c = self.connection()?;
        let tx = c.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let (stored, state): (String, String) = tx
            .query_row(
                "SELECT source_fingerprint,state FROM jobs WHERE id=?1",
                [id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?
            .ok_or_else(|| JobsError::NotFound(id.to_string()))?;
        if stored == current {
            tx.commit()?;
            return Ok(false);
        }
        if matches!(
            state.as_str(),
            "leased" | "running" | "reconciliation_required"
        ) {
            return Err(JobsError::Conflict(
                "active or ambiguous work must be reconciled before invalidating its source".into(),
            ));
        }
        tx.execute("UPDATE jobs SET state='stale',completion_gate='invalidated',lease_owner=NULL,lease_until_ms=NULL,updated_at_ms=?2 WHERE id=?1",params![id,now])?;
        append_event(
            &tx,
            id,
            now,
            "source_changed",
            serde_json::json!({"storedFingerprint":stored,"currentFingerprint":current}),
        )?;
        tx.commit()?;
        Ok(true)
    }

    /// Invalidate a changed source only while the exact owner still holds a
    /// newly claimed lease and before any attempt has started. Once execution
    /// starts, source changes require normal reconciliation because a process
    /// may have been dispatched before its identity was persisted.
    pub fn mark_stale_before_dispatch(
        &self,
        id: i64,
        worker: &str,
        fence: u64,
        current: &str,
        now: i64,
    ) -> Result<bool, JobsError> {
        validate_text("source fingerprint", current, 256)?;
        let mut c = self.connection()?;
        let tx = c.transaction_with_behavior(TransactionBehavior::Immediate)?;
        assert_lease(&tx, id, worker, fence, now)?;
        let (stored, state, process_ref): (String, String, Option<String>) = tx.query_row(
            "SELECT j.source_fingerprint,j.state,a.process_ref FROM jobs j JOIN job_attempts a ON a.job_id=j.id AND a.fencing_token=j.fencing_token WHERE j.id=?1",
            [id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        ).optional()?.ok_or_else(|| JobsError::NotFound(id.to_string()))?;
        if stored == current {
            tx.commit()?;
            return Ok(false);
        }
        if state != "leased" || process_ref.is_some() {
            return Err(JobsError::Conflict(
                "source changed after dispatch may have started; reconcile the owned attempt"
                    .into(),
            ));
        }
        tx.execute("UPDATE jobs SET state='stale',completion_gate='invalidated',lease_owner=NULL,lease_until_ms=NULL,updated_at_ms=?2 WHERE id=?1", params![id, now])?;
        tx.execute("UPDATE job_attempts SET state='stale',finished_at_ms=?3,result_json=?4 WHERE job_id=?1 AND fencing_token=?2", params![id, fence as i64, now, serde_json::json!({"reason":"source_changed_before_dispatch","storedFingerprint":stored,"currentFingerprint":current}).to_string()])?;
        append_event(
            &tx,
            id,
            now,
            "source_changed_before_dispatch",
            serde_json::json!({"workerId":worker,"fencingToken":fence,"storedFingerprint":stored,"currentFingerprint":current}),
        )?;
        tx.commit()?;
        Ok(true)
    }

    /// Accept explicit verification evidence for this exact source snapshot.
    pub fn verify_gate(
        &self,
        id: i64,
        current_fingerprint: &str,
        evidence: &[VerificationEvidence],
        now: i64,
    ) -> Result<Job, JobsError> {
        if evidence.is_empty() {
            return Err(JobsError::Invalid(
                "verification requires at least one evidence item".into(),
            ));
        }
        let mut c = self.connection()?;
        let tx = c.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let (stored, state): (String, String) = tx
            .query_row(
                "SELECT source_fingerprint,state FROM jobs WHERE id=?1",
                [id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?
            .ok_or_else(|| JobsError::NotFound(id.to_string()))?;
        if stored != current_fingerprint {
            return Err(JobsError::Conflict(
                "source fingerprint changed; verification is stale".into(),
            ));
        }
        if state != "awaiting_verification" {
            return Err(JobsError::Conflict(
                "job is not awaiting verification".into(),
            ));
        }
        for e in evidence {
            if e.source_fingerprint != stored
                || !e.passed
                || e.check_id.trim().is_empty()
                || e.details.trim().is_empty()
            {
                return Err(JobsError::Invalid(
                    "all verification evidence must pass and match the source fingerprint".into(),
                ));
            }
        }
        let encoded =
            serde_json::to_string(evidence).map_err(|e| JobsError::Invalid(e.to_string()))?;
        tx.execute("INSERT INTO job_checkpoints(job_id,name,fencing_token,at_ms,payload_json) SELECT id,'verified_completion',fencing_token,?2,?3 FROM jobs WHERE id=?1 ON CONFLICT(job_id,name) DO UPDATE SET fencing_token=excluded.fencing_token,at_ms=excluded.at_ms,payload_json=excluded.payload_json",params![id,now,encoded])?;
        tx.execute("UPDATE jobs SET state='verified',completion_gate='verified',updated_at_ms=?2 WHERE id=?1",params![id,now])?;
        append_event(
            &tx,
            id,
            now,
            "completion_verified",
            serde_json::json!({"evidence":evidence}),
        )?;
        let dependents = {
            let mut s = tx.prepare(
                "SELECT DISTINCT job_id FROM job_dependencies WHERE prerequisite_job_id=?1",
            )?;
            let rows = s
                .query_map([id], |r| r.get::<_, i64>(0))?
                .collect::<Result<Vec<_>, _>>()?;
            rows
        };
        for dependent in dependents {
            refresh_eligibility(&tx, dependent, now)?;
        }
        tx.commit()?;
        self.get(id)
    }

    /// Verify code against a separate Git snapshot while retaining the authoritative
    /// issue-Markdown fingerprint as the job gate identity. The code snapshot is
    /// persisted inside each evidence detail for audit and recovery.
    pub fn verify_gate_with_code_snapshot(
        &self,
        id: i64,
        issue_fingerprint: &str,
        code_head: &str,
        code_fingerprint: &str,
        evidence: &[VerificationEvidence],
        now: i64,
    ) -> Result<Job, JobsError> {
        if code_head.trim().is_empty() || code_fingerprint.trim().is_empty() {
            return Err(JobsError::Invalid(
                "code snapshot identity is required".into(),
            ));
        }
        let bound = evidence
            .iter()
            .map(|item| VerificationEvidence {
                check_id: item.check_id.clone(),
                source_fingerprint: issue_fingerprint.to_owned(),
                passed: item.passed,
                details: serde_json::json!({
                    "codeHead": code_head,
                    "codeFingerprint": code_fingerprint,
                    "checkEvidence": item.details,
                })
                .to_string(),
            })
            .collect::<Vec<_>>();
        self.verify_gate(id, issue_fingerprint, &bound, now)
    }

    /// Queued work can be cancelled immediately. Active work receives a durable request;
    /// the process owner must acknowledge it after reconciling effects.
    pub fn request_cancel(&self, id: i64, now: i64) -> Result<Job, JobsError> {
        let mut c = self.connection()?;
        let tx = c.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let state: String = tx
            .query_row("SELECT state FROM jobs WHERE id=?1", [id], |r| r.get(0))
            .optional()?
            .ok_or_else(|| JobsError::NotFound(id.to_string()))?;
        match state.as_str() {
            "queued" | "blocked" => {
                tx.execute(
                    "UPDATE jobs SET state='cancelled',updated_at_ms=?2 WHERE id=?1",
                    params![id, now],
                )?;
            }
            "leased" | "running" => {
                tx.execute(
                    "UPDATE jobs SET cancel_requested=1,updated_at_ms=?2 WHERE id=?1",
                    params![id, now],
                )?;
            }
            "needs_input" => {
                tx.execute(
                    "UPDATE jobs SET state='cancelled',updated_at_ms=?2 WHERE id=?1",
                    params![id, now],
                )?;
            }
            "reconciliation_required" => {
                return Err(JobsError::Conflict(
                    "ambiguous work must be reconciled before cancellation".into(),
                ))
            }
            "verified" | "failed" | "cancelled" | "stale" => {
                return Err(JobsError::Conflict(
                    "terminal job cannot be cancelled".into(),
                ))
            }
            _ => {
                return Err(JobsError::Conflict(
                    "job cannot be cancelled in its current state".into(),
                ))
            }
        }
        append_event(
            &tx,
            id,
            now,
            "cancel_requested",
            serde_json::json!({"previousState":state}),
        )?;
        tx.commit()?;
        self.get(id)
    }

    pub fn acknowledge_cancel(
        &self,
        id: i64,
        worker: &str,
        fence: u64,
        process_stopped: bool,
        effects_reconciled: bool,
        evidence: &str,
        now: i64,
    ) -> Result<Job, JobsError> {
        if !process_stopped || !effects_reconciled {
            return Err(JobsError::Invalid(
                "cancellation needs confirmed process stop and reconciled effects".into(),
            ));
        }
        validate_text("cancellation evidence", evidence, MAX_TEXT_BYTES)?;
        let mut c = self.connection()?;
        let tx = c.transaction_with_behavior(TransactionBehavior::Immediate)?;
        assert_lease(&tx, id, worker, fence, now)?;
        let requested: bool = tx
            .query_row("SELECT cancel_requested FROM jobs WHERE id=?1", [id], |r| {
                r.get::<_, i64>(0)
            })
            .map(|n| n != 0)?;
        if !requested {
            return Err(JobsError::Conflict("cancellation was not requested".into()));
        }
        tx.execute("UPDATE jobs SET state='cancelled',lease_owner=NULL,lease_until_ms=NULL,updated_at_ms=?2 WHERE id=?1",params![id,now])?;
        tx.execute("UPDATE job_attempts SET state='cancelled',finished_at_ms=?3,result_json=?4 WHERE job_id=?1 AND fencing_token=?2",params![id,fence as i64,now,serde_json::json!({"reconciliation":evidence}).to_string()])?;
        append_event(
            &tx,
            id,
            now,
            "cancellation_reconciled",
            serde_json::json!({"evidence":evidence}),
        )?;
        tx.commit()?;
        self.get(id)
    }

    /// Resolve a previously unknown dependency with explicit closed-state evidence.
    pub fn record_external_closed_evidence(
        &self,
        id: i64,
        dependency_issue_id: &str,
        source: &str,
        reference: &str,
        observed_at: &str,
        evidence: &str,
        now: i64,
    ) -> Result<Job, JobsError> {
        for (name, value) in [
            ("dependency issue ID", dependency_issue_id),
            ("evidence source", source),
            ("evidence reference", reference),
            ("observation time", observed_at),
            ("evidence", evidence),
        ] {
            validate_text(name, value, MAX_TEXT_BYTES)?
        }
        let proof=serde_json::json!({"source":source,"reference":reference,"observedAt":observed_at,"status":"closed","evidence":evidence}).to_string();
        let mut c = self.connection()?;
        let tx = c.transaction_with_behavior(TransactionBehavior::Immediate)?;
        ensure_job(&tx, id)?;
        let changed=tx.execute("UPDATE job_dependencies SET resolution='external_verified',prerequisite_job_id=NULL,required_fingerprint=NULL,evidence_json=?3 WHERE job_id=?1 AND dependency_issue_id=?2 AND resolution='unresolved'",params![id,dependency_issue_id,proof])?;
        if changed == 0 {
            return Err(JobsError::Conflict(
                "dependency is absent or already resolved".into(),
            ));
        }
        append_event(
            &tx,
            id,
            now,
            "external_dependency_verified",
            serde_json::json!({"issueId":dependency_issue_id,"proof":serde_json::from_str::<Value>(&proof).unwrap()}),
        )?;
        refresh_eligibility(&tx, id, now)?;
        tx.commit()?;
        self.get(id)
    }

    /// Aggregate child job gates without silently verifying the epic itself.
    pub fn epic_progress(&self, epic_issue_id: &str) -> Result<EpicProgress, JobsError> {
        validate_text("epic issue ID", epic_issue_id, 128)?;
        let c = self.connection()?;
        let epic_type: Option<String> = c
            .query_row(
                "SELECT issue_type FROM jobs WHERE issue_id=?1 ORDER BY id DESC LIMIT 1",
                [epic_issue_id],
                |r| r.get(0),
            )
            .optional()?;
        if epic_type.as_deref() != Some("epic") {
            return Err(JobsError::Invalid("issue has no submitted epic job".into()));
        }
        let rows = {
            let mut s=c.prepare("SELECT issue_id,state,completion_gate FROM jobs WHERE id IN (SELECT MAX(id) FROM jobs GROUP BY issue_id) AND EXISTS (SELECT 1 FROM json_each(parent_ids_json) WHERE value=?1)")?;
            let rows = s
                .query_map([epic_issue_id], |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, String>(2)?,
                    ))
                })?
                .collect::<Result<Vec<_>, _>>()?;
            rows
        };
        let mut incomplete = Vec::new();
        for (issue, state, gate) in &rows {
            if state != "verified" || gate != "verified" {
                incomplete.push(issue.clone())
            }
        }
        incomplete.sort();
        let count = rows.len();
        let verified = count - incomplete.len();
        Ok(EpicProgress {
            child_count: count,
            verified_children: verified,
            incomplete_children: incomplete,
            complete: count > 0 && verified == count,
        })
    }
}

fn ensure_wal_journal_mode(connection: &Connection) -> Result<(), JobsError> {
    ensure_wal_journal_mode_with_timeout(connection, Duration::from_secs(10))
}

fn ensure_wal_journal_mode_with_timeout(
    connection: &Connection,
    timeout: Duration,
) -> Result<(), JobsError> {
    let deadline = Instant::now() + timeout;
    // Keep each SQLite lock wait short so Busy/Locked can be retried within one bounded window.
    connection.busy_timeout(Duration::from_millis(100))?;
    loop {
        if Instant::now() >= deadline {
            return Err(JobsError::Initialization(
                "journal_mode did not converge before the bounded initialization deadline".into(),
            ));
        }
        let mode: String = match connection
            .pragma_query_value(None, "journal_mode", |row| row.get(0))
        {
            Ok(mode) => mode,
            Err(error) if is_sqlite_busy_or_locked(&error) => {
                if Instant::now() >= deadline {
                    return Err(JobsError::Initialization(format!(
                        "journal_mode query remained locked until the bounded initialization deadline: {error}"
                    )));
                }
                std::thread::sleep(Duration::from_millis(10));
                continue;
            }
            Err(error) => {
                return Err(JobsError::Initialization(format!(
                    "journal_mode query: {error}"
                )))
            }
        };
        if mode.eq_ignore_ascii_case("wal") {
            connection.busy_timeout(Duration::from_secs(10))?;
            return Ok(());
        }

        match connection.pragma_update(None, "journal_mode", "WAL") {
            Ok(()) => {}
            Err(error) if is_sqlite_busy_or_locked(&error) => {
                if Instant::now() >= deadline {
                    return Err(JobsError::Initialization(format!(
                        "journal_mode update remained locked until the bounded initialization deadline: {error}"
                    )));
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(error) => {
                return Err(JobsError::Initialization(format!(
                    "journal_mode update: {error}"
                )))
            }
        }
    }
}

fn is_sqlite_busy_or_locked(error: &rusqlite::Error) -> bool {
    matches!(
        error.sqlite_error_code(),
        Some(ErrorCode::DatabaseBusy) | Some(ErrorCode::DatabaseLocked)
    )
}
fn migrate_v1(c: &mut Connection) -> Result<(), JobsError> {
    let tx = c.transaction_with_behavior(TransactionBehavior::Immediate)?;
    // Another opener may have completed the first migration while this caller was
    // waiting for the immediate transaction. Recheck under the write lock before DDL.
    let version: i64 = tx.pragma_query_value(None, "user_version", |row| row.get(0))?;
    if version > SCHEMA_VERSION {
        return Err(JobsError::UnsupportedSchema(version));
    }
    if version == SCHEMA_VERSION {
        tx.commit()?;
        return Ok(());
    }
    tx.execute_batch("CREATE TABLE jobs(id INTEGER PRIMARY KEY AUTOINCREMENT,issue_id TEXT NOT NULL,issue_type TEXT NOT NULL,parent_ids_json TEXT NOT NULL,idempotency_key TEXT NOT NULL UNIQUE,source_fingerprint TEXT NOT NULL,issue_snapshot TEXT NOT NULL,plan_json TEXT NOT NULL,state TEXT NOT NULL,completion_gate TEXT NOT NULL,request_json TEXT NOT NULL,created_at_ms INTEGER NOT NULL,updated_at_ms INTEGER NOT NULL,attempt_count INTEGER NOT NULL DEFAULT 0,max_attempts INTEGER NOT NULL,cancel_requested INTEGER NOT NULL DEFAULT 0,lease_owner TEXT,lease_until_ms INTEGER,fencing_token INTEGER NOT NULL DEFAULT 0); CREATE INDEX jobs_state_created ON jobs(state,created_at_ms,id); CREATE INDEX jobs_issue ON jobs(issue_id); CREATE TABLE job_dependencies(job_id INTEGER NOT NULL REFERENCES jobs(id) ON DELETE CASCADE,dependency_issue_id TEXT NOT NULL,prerequisite_job_id INTEGER REFERENCES jobs(id),required_fingerprint TEXT,resolution TEXT NOT NULL,evidence_json TEXT,PRIMARY KEY(job_id,dependency_issue_id)); CREATE TABLE job_attempts(id INTEGER PRIMARY KEY AUTOINCREMENT,job_id INTEGER NOT NULL REFERENCES jobs(id) ON DELETE CASCADE,attempt_no INTEGER NOT NULL,state TEXT NOT NULL,worker_id TEXT NOT NULL,fencing_token INTEGER NOT NULL,started_at_ms INTEGER NOT NULL,heartbeat_at_ms INTEGER,finished_at_ms INTEGER,result_json TEXT,assigned_provider_id TEXT,assigned_model_id TEXT,process_ref TEXT,UNIQUE(job_id,attempt_no)); CREATE TABLE job_events(job_id INTEGER NOT NULL REFERENCES jobs(id) ON DELETE CASCADE,seq INTEGER NOT NULL,at_ms INTEGER NOT NULL,kind TEXT NOT NULL,payload_json TEXT NOT NULL,PRIMARY KEY(job_id,seq)); CREATE TABLE job_checkpoints(job_id INTEGER NOT NULL REFERENCES jobs(id) ON DELETE CASCADE,name TEXT NOT NULL,fencing_token INTEGER NOT NULL,at_ms INTEGER NOT NULL,payload_json TEXT NOT NULL,PRIMARY KEY(job_id,name)); PRAGMA user_version=1;")?;
    tx.commit()?;
    Ok(())
}

fn validate_submit(r: &SubmitJob) -> Result<(), JobsError> {
    validate_text("idempotency key", &r.idempotency_key, 256)?;
    validate_text("issue ID", &r.issue_id, 128)?;
    validate_text("issue type", &r.issue_type, 128)?;
    validate_text("fingerprint", &r.source_fingerprint, 256)?;
    if r.issue_snapshot.len() > MAX_CHECKPOINT_BYTES {
        return Err(JobsError::Invalid(
            "issue snapshot exceeds size limit".into(),
        ));
    }
    for parent in &r.parent_issue_ids {
        validate_text("parent issue ID", parent, 128)?
    }
    validate_plan(&r.plan)?;
    let aggregation = r.plan.tasks.len() == 1
        && r.plan.tasks[0].kind == TaskKind::EpicAggregation
        && r.plan.tasks[0].parent_task_id.is_none()
        && r.plan.tasks[0].provider_id.is_none()
        && r.plan.tasks[0].model_id.is_none();
    if (r.issue_type == "epic") != aggregation {
        return Err(JobsError::Invalid(
            "epic issues require one non-executable aggregation task; other issues require an implementation plan".into(),
        ));
    }
    let mut seen = HashSet::new();
    for d in &r.dependencies {
        validate_text("dependency issue ID", &d.issue_id, 128)?;
        if !seen.insert(d.issue_id.as_str()) {
            return Err(JobsError::Invalid(format!(
                "duplicate dependency {}",
                d.issue_id
            )));
        }
    }
    Ok(())
}
fn validate_plan(p: &JobPlan) -> Result<(), JobsError> {
    if p.tasks.is_empty() || p.tasks.len() > MAX_PLAN_TASKS {
        return Err(JobsError::Invalid(format!(
            "plan must contain 1..={MAX_PLAN_TASKS} tasks"
        )));
    }
    validate_limits(&p.limits)?;
    let mut by = HashMap::new();
    for t in &p.tasks {
        validate_text("task ID", &t.task_id, 128)?;
        if by.insert(t.task_id.as_str(), t).is_some() {
            return Err(JobsError::Invalid("duplicate task ID".into()));
        }
        if t.files.len() > MAX_TASK_FILES || t.acceptance.is_empty() {
            return Err(JobsError::Invalid(
                "each task needs acceptance criteria and a bounded file list".into(),
            ));
        }
        for f in &t.files {
            validate_relative_file(f)?
        }
        for a in &t.acceptance {
            validate_text("task acceptance", a, MAX_TEXT_BYTES)?
        }
        validate_limits(&t.limits)?;
        if t.limits.max_attempts > p.limits.max_attempts
            || t.limits.max_concurrency > p.limits.max_concurrency
            || t.limits
                .max_tokens
                .zip(p.limits.max_tokens)
                .is_some_and(|(task, total)| task > total)
            || t.limits
                .max_cost_micros
                .zip(p.limits.max_cost_micros)
                .is_some_and(|(task, total)| task > total)
            || t.limits
                .max_seconds
                .zip(p.limits.max_seconds)
                .is_some_and(|(task, total)| task > total)
        {
            return Err(JobsError::Invalid(
                "task resource limits may not exceed job limits".into(),
            ));
        }
    }
    if p.tasks.len() == 1
        && p.tasks[0].kind == TaskKind::EpicAggregation
        && p.tasks[0].parent_task_id.is_none()
    {
        return Ok(());
    }
    let roots: Vec<_> = p
        .tasks
        .iter()
        .filter(|t| t.kind == TaskKind::Implementation)
        .collect();
    if roots.len() != 1 {
        return Err(JobsError::Invalid(
            "plan requires exactly one implementation root".into(),
        ));
    }
    for t in &p.tasks {
        match (&t.kind, &t.parent_task_id) {
            (TaskKind::Implementation, None) => {}
            (TaskKind::Worker | TaskKind::Reviewer, Some(parent)) => {
                let p = by
                    .get(parent.as_str())
                    .ok_or_else(|| JobsError::Invalid("subtask parent is missing".into()))?;
                if p.kind != TaskKind::Implementation {
                    return Err(JobsError::Invalid(
                        "worker/reviewer tasks cannot recursively spawn subtasks".into(),
                    ));
                }
            }
            _ => {
                return Err(JobsError::Invalid(
                    "plan requires root implementation tasks and direct worker/reviewer children"
                        .into(),
                ))
            }
        }
    }
    let root = &roots[0].limits;
    let children: Vec<_> = p
        .tasks
        .iter()
        .filter(|t| t.parent_task_id.is_some())
        .collect();
    let sum_opt = |f: fn(&ResourceLimits) -> Option<u64>| -> Option<u64> {
        children
            .iter()
            .try_fold(0u64, |sum, t| sum.checked_add(f(&t.limits)?))
    };
    for (name, total, cap) in [
        ("tokens", sum_opt(|l| l.max_tokens), root.max_tokens),
        ("cost", sum_opt(|l| l.max_cost_micros), root.max_cost_micros),
        ("seconds", sum_opt(|l| l.max_seconds), root.max_seconds),
    ] {
        if cap.is_some() && (total.is_none() || total.unwrap() > cap.unwrap()) {
            return Err(JobsError::Invalid(format!(
                "aggregate subtask {name} budget exceeds implementation parent cap"
            )));
        }
    }
    let concurrency: u16 = children
        .iter()
        .map(|t| u16::from(t.limits.max_concurrency))
        .sum();
    if concurrency > u16::from(root.max_concurrency) {
        return Err(JobsError::Invalid(
            "aggregate subtask concurrency exceeds implementation parent cap".into(),
        ));
    }
    if p.tasks.iter().any(|t| t.parent_task_id.is_some())
        && p.tasks
            .iter()
            .filter(|t| t.parent_task_id.is_some())
            .count()
            > MAX_PLAN_TASKS - 1
    {
        return Err(JobsError::Invalid("subtask count exceeds limit".into()));
    }
    Ok(())
}
fn validate_limits(l: &ResourceLimits) -> Result<(), JobsError> {
    if l.max_attempts == 0 || l.max_attempts > 5 || l.max_concurrency == 0 || l.max_concurrency > 8
    {
        return Err(JobsError::Invalid(
            "attempts must be 1..=5 and concurrency 1..=8".into(),
        ));
    }
    Ok(())
}
fn validate_relative_file(f: &str) -> Result<(), JobsError> {
    let p = Path::new(f);
    if f.trim().is_empty()
        || p.is_absolute()
        || p.components().any(|c| {
            matches!(
                c,
                std::path::Component::ParentDir | std::path::Component::Prefix(_)
            )
        })
    {
        return Err(JobsError::Invalid(format!(
            "task file must be a safe relative path: {f:?}"
        )));
    }
    Ok(())
}
fn validate_text(name: &str, s: &str, max: usize) -> Result<(), JobsError> {
    if s.trim().is_empty() || s.len() > max || s.chars().any(char::is_control) {
        return Err(JobsError::Invalid(format!(
            "{name} must be non-empty, bounded text without control characters"
        )));
    }
    Ok(())
}
fn topological_issue_order(reqs: &[SubmitJob]) -> Result<Vec<String>, JobsError> {
    let by: HashMap<_, _> = reqs.iter().map(|r| (r.issue_id.as_str(), r)).collect();
    let mut out = Vec::new();
    let mut visiting = HashSet::new();
    let mut visited = HashSet::new();
    fn visit(
        id: &str,
        by: &HashMap<&str, &SubmitJob>,
        visiting: &mut HashSet<String>,
        visited: &mut HashSet<String>,
        out: &mut Vec<String>,
    ) -> Result<(), JobsError> {
        if visited.contains(id) {
            return Ok(());
        }
        if !visiting.insert(id.into()) {
            return Err(JobsError::Invalid(format!(
                "job dependency cycle includes {id}"
            )));
        }
        if let Some(r) = by.get(id) {
            for d in &r.dependencies {
                if let DependencyResolution::Job {
                    prerequisite_issue_id,
                    ..
                } = &d.resolution
                {
                    if by.contains_key(prerequisite_issue_id.as_str()) {
                        visit(prerequisite_issue_id, by, visiting, visited, out)?
                    }
                }
            }
        }
        visiting.remove(id);
        visited.insert(id.into());
        out.push(id.into());
        Ok(())
    }
    for id in by.keys() {
        visit(id, &by, &mut visiting, &mut visited, &mut out)?
    }
    Ok(out)
}
fn validate_persisted_dependency_graph(tx: &Transaction<'_>) -> Result<(), JobsError> {
    let mut edges: HashMap<i64, Vec<i64>> = HashMap::new();
    let mut s = tx.prepare(
        "SELECT job_id,prerequisite_job_id FROM job_dependencies WHERE resolution='job'",
    )?;
    let rows = s.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?)))?;
    for row in rows {
        let (job, pre) = row?;
        edges.entry(job).or_default().push(pre);
    }
    fn visit(
        id: i64,
        edges: &HashMap<i64, Vec<i64>>,
        visiting: &mut HashSet<i64>,
        visited: &mut HashSet<i64>,
    ) -> Result<(), JobsError> {
        if visited.contains(&id) {
            return Ok(());
        }
        if !visiting.insert(id) {
            return Err(JobsError::Invalid(format!(
                "persisted dependency cycle includes job {id}"
            )));
        }
        if let Some(next) = edges.get(&id) {
            for dep in next {
                visit(*dep, edges, visiting, visited)?
            }
        }
        visiting.remove(&id);
        visited.insert(id);
        Ok(())
    }
    let mut visiting = HashSet::new();
    let mut visited = HashSet::new();
    for id in edges.keys() {
        visit(*id, &edges, &mut visiting, &mut visited)?
    }
    Ok(())
}
fn append_event(
    tx: &Transaction<'_>,
    id: i64,
    at: i64,
    kind: &str,
    payload: Value,
) -> Result<(), JobsError> {
    let encoded = serde_json::to_string(&payload).map_err(|e| JobsError::Invalid(e.to_string()))?;
    if encoded.len() > MAX_TEXT_BYTES {
        return Err(JobsError::Invalid(
            "event payload exceeds size limit".into(),
        ));
    }
    let seq: i64 = tx.query_row(
        "SELECT COALESCE(MAX(seq),0)+1 FROM job_events WHERE job_id=?1",
        [id],
        |r| r.get(0),
    )?;
    tx.execute(
        "INSERT INTO job_events(job_id,seq,at_ms,kind,payload_json) VALUES(?1,?2,?3,?4,?5)",
        params![id, seq, at, kind, encoded],
    )?;
    Ok(())
}
fn ensure_job(c: &Connection, id: i64) -> Result<(), JobsError> {
    let found: bool = c.query_row("SELECT EXISTS(SELECT 1 FROM jobs WHERE id=?1)", [id], |r| {
        r.get(0)
    })?;
    if found {
        Ok(())
    } else {
        Err(JobsError::NotFound(id.to_string()))
    }
}
fn read_job(c: &Connection, id: i64) -> Result<Job, JobsError> {
    c.query_row("SELECT id,issue_id,source_fingerprint,state,completion_gate,created_at_ms,updated_at_ms,attempt_count,max_attempts,cancel_requested,lease_owner,lease_until_ms,fencing_token FROM jobs WHERE id=?1",[id],|r|Ok((r.get::<_,i64>(0)?,r.get::<_,String>(1)?,r.get::<_,String>(2)?,r.get::<_,String>(3)?,r.get::<_,String>(4)?,r.get::<_,i64>(5)?,r.get::<_,i64>(6)?,r.get::<_,i64>(7)?,r.get::<_,i64>(8)?,r.get::<_,i64>(9)?,r.get::<_,Option<String>>(10)?,r.get::<_,Option<i64>>(11)?,r.get::<_,i64>(12)?))).optional()?.ok_or_else(||JobsError::NotFound(id.to_string())).and_then(|x|Ok(Job{id:x.0,issue_id:x.1,source_fingerprint:x.2,state:JobState::parse(&x.3)?,completion_gate:x.4,created_at_ms:x.5,updated_at_ms:x.6,attempt_count:x.7 as u32,max_attempts:x.8 as u8,cancel_requested:x.9!=0,lease_owner:x.10,lease_until_ms:x.11,fencing_token:x.12 as u64}))
}
fn assert_lease(
    tx: &Transaction<'_>,
    id: i64,
    worker: &str,
    fence: u64,
    now: i64,
) -> Result<(), JobsError> {
    let ok:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM jobs WHERE id=?1 AND state IN ('leased','running') AND lease_owner=?2 AND fencing_token=?3 AND lease_until_ms>?4)",params![id,worker,fence as i64,now],|r|r.get(0))?;
    if ok {
        Ok(())
    } else {
        Err(JobsError::LeaseLost)
    }
}
fn refresh_eligibility(tx: &Transaction<'_>, id: i64, now: i64) -> Result<(), JobsError> {
    let state: String = tx
        .query_row("SELECT state FROM jobs WHERE id=?1", [id], |r| r.get(0))
        .optional()?
        .ok_or_else(|| JobsError::NotFound(id.to_string()))?;
    if state != "queued" && state != "blocked" {
        return Ok(());
    }
    let mut s=tx.prepare("SELECT resolution,prerequisite_job_id,required_fingerprint FROM job_dependencies WHERE job_id=?1")?;
    let rows = s
        .query_map([id], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, Option<i64>>(1)?,
                r.get::<_, Option<String>>(2)?,
            ))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    let mut ready = true;
    for (res, target, fp) in rows {
        match res.as_str() {
            "unresolved" => ready = false,
            "external_verified" => {}
            "job" => {
                let Some(target) = target else {
                    ready = false;
                    continue;
                };
                let ok:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM jobs WHERE id=?1 AND state='verified' AND completion_gate='verified' AND source_fingerprint=?2)",params![target,fp],|r|r.get(0))?;
                if !ok {
                    ready = false
                }
            }
            _ => {
                return Err(JobsError::Invalid(
                    "unknown dependency resolution in store".into(),
                ))
            }
        }
    }
    let next = if ready { "queued" } else { "blocked" };
    if next != state {
        tx.execute(
            "UPDATE jobs SET state=?2,updated_at_ms=?3 WHERE id=?1",
            params![id, next, now],
        )?;
        append_event(
            tx,
            id,
            now,
            "eligibility_changed",
            serde_json::json!({"state":next}),
        )?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn limits() -> ResourceLimits {
        ResourceLimits {
            max_attempts: 3,
            max_tokens: Some(1000),
            max_cost_micros: None,
            max_seconds: Some(300),
            max_concurrency: 1,
        }
    }
    fn request(issue: &str, key: &str, deps: Vec<DependencySpec>) -> SubmitJob {
        SubmitJob {
            idempotency_key: key.into(),
            issue_id: issue.into(),
            issue_type: "enhancement".into(),
            parent_issue_ids: vec![],
            source_fingerprint: format!("fp-{issue}"),
            issue_snapshot: format!("snapshot {issue}"),
            plan: JobPlan {
                tasks: vec![PlannedTask {
                    task_id: "implement".into(),
                    parent_task_id: None,
                    kind: TaskKind::Implementation,
                    files: vec!["src/lib.rs".into()],
                    acceptance: vec!["synthetic acceptance".into()],
                    limits: limits(),
                    provider_id: None,
                    model_id: None,
                    route_requirements: None,
                }],
                limits: limits(),
            },
            dependencies: deps,
        }
    }
    fn store() -> (tempfile::TempDir, JobsStore) {
        let d = tempfile::tempdir().unwrap();
        let s = JobsStore::open(d.path().join("jobs.sqlite3")).unwrap();
        (d, s)
    }
    #[test]
    fn wal_initialization_fails_bounded_when_mode_cannot_become_wal() {
        let connection = Connection::open_in_memory().unwrap();
        let error = ensure_wal_journal_mode_with_timeout(&connection, Duration::from_millis(30))
            .unwrap_err();
        assert!(matches!(error, JobsError::Initialization(_)));
        assert!(error.to_string().contains("did not converge"));
    }

    #[test]
    fn simultaneous_first_open_migrates_fresh_database_once() {
        use std::sync::{Arc, Barrier};
        use std::thread;

        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("simultaneous-first-open.sqlite3");
        let start = Arc::new(Barrier::new(3));
        let openers: Vec<_> = (0..2)
            .map(|_| {
                let path = path.clone();
                let start = Arc::clone(&start);
                thread::spawn(move || -> Result<JobsStore, String> {
                    start.wait();
                    JobsStore::open(path).map_err(|error| error.to_string())
                })
            })
            .collect();

        start.wait();
        let stores: Vec<_> = openers
            .into_iter()
            .map(|opener| opener.join().expect("opener thread panicked").unwrap())
            .collect();
        for store in &stores {
            let connection = store.connection().unwrap();
            let version: i64 = connection
                .pragma_query_value(None, "user_version", |row| row.get(0))
                .unwrap();
            let tables: i64 = connection
                .query_row(
                    "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name IN ('jobs','job_checkpoints')",
                    [],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(version, SCHEMA_VERSION);
            assert_eq!(tables, 2);
        }
    }

    #[test]
    fn concurrent_store_open_and_fenced_checkpoint_writes_preserve_all_updates() {
        use std::sync::{Arc, Barrier};
        use std::thread;

        const ITERATIONS: i64 = 160;
        let (_dir, store) = store();
        let job = store
            .submit(request("FAB-L009", "concurrent-status", vec![]), 1)
            .unwrap();
        let claim = store
            .claim("checkpoint-writer", 2, 10_000)
            .unwrap()
            .unwrap();
        let path = store.path().to_path_buf();
        let start = Arc::new(Barrier::new(3));

        let writer_path = path.clone();
        let writer_start = Arc::clone(&start);
        let writer_id = claim.worker_id.clone();
        let writer_fence = claim.fencing_token;
        let writer = thread::spawn(move || -> Result<(), String> {
            writer_start.wait();
            let writer_store = JobsStore::open(writer_path).map_err(|error| error.to_string())?;
            for sequence in 0..ITERATIONS {
                writer_store
                    .record_checkpoint(
                        job.id,
                        &writer_id,
                        writer_fence,
                        "runtime_output_fixture",
                        &serde_json::json!({"sequence": sequence}),
                        sequence + 3,
                    )
                    .map_err(|error| error.to_string())?;
                thread::yield_now();
            }
            Ok(())
        });

        let reader_path = path.clone();
        let reader_start = Arc::clone(&start);
        let reader = thread::spawn(move || -> Result<(), String> {
            reader_start.wait();
            for _ in 0..ITERATIONS {
                // Mirrors status polling: opening the store followed by bounded checkpoint reads.
                let status_store =
                    JobsStore::open(&reader_path).map_err(|error| error.to_string())?;
                let _ = status_store
                    .get_checkpoint(job.id, "runtime_output_fixture")
                    .map_err(|error| error.to_string())?;
                thread::yield_now();
            }
            Ok(())
        });

        start.wait();
        writer
            .join()
            .expect("checkpoint writer thread panicked")
            .unwrap();
        reader
            .join()
            .expect("status reader thread panicked")
            .unwrap();

        assert_eq!(
            store
                .get_checkpoint(job.id, "runtime_output_fixture")
                .unwrap()
                .unwrap()["sequence"],
            ITERATIONS - 1
        );
        let connection = store.connection().unwrap();
        let writes: i64 = connection
            .query_row(
                "SELECT COUNT(*) FROM job_events WHERE job_id=?1 AND kind='checkpoint_saved' AND payload_json LIKE '%runtime_output_fixture%'",
                [job.id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(writes, ITERATIONS);
    }

    #[test]
    fn checkpoint_write_reserves_writer_before_reading_fenced_lease() {
        use std::sync::{mpsc, Arc, Barrier};
        use std::thread;

        let (_dir, store) = store();
        let job = store
            .submit(request("FAB-L009", "immediate-checkpoint", vec![]), 1)
            .unwrap();
        let claim = store
            .claim("checkpoint-writer", 2, 10_000)
            .unwrap()
            .unwrap();
        let path = store.path().to_path_buf();

        // Reproduce SQLite's stale-snapshot upgrade failure deterministically:
        // the deferred reader establishes its snapshot before the other connection commits.
        let reader = Connection::open(&path).unwrap();
        reader.busy_timeout(Duration::from_secs(2)).unwrap();
        let tx = reader.unchecked_transaction().unwrap();
        let _: i64 = tx
            .query_row("SELECT id FROM jobs WHERE id=?1", [job.id], |row| {
                row.get(0)
            })
            .unwrap();
        let mut competing = Connection::open(&path).unwrap();
        competing.busy_timeout(Duration::from_secs(2)).unwrap();
        let competing_tx = competing
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .unwrap();
        competing_tx
            .execute(
                "UPDATE jobs SET updated_at_ms=updated_at_ms+1 WHERE id=?1",
                [job.id],
            )
            .unwrap();
        competing_tx.commit().unwrap();
        let upgrade_error = tx
            .execute(
                "UPDATE jobs SET updated_at_ms=updated_at_ms+1 WHERE id=?1",
                [job.id],
            )
            .expect_err("a stale deferred snapshot must not upgrade to a writer");
        let extended_code = match &upgrade_error {
            rusqlite::Error::SqliteFailure(failure, _) => failure.extended_code,
            _ => -1,
        };
        assert_eq!(
            extended_code, 517,
            "expected SQLITE_BUSY_SNAPSHOT (517), got {upgrade_error}"
        );
        let diagnostic = JobsError::from(upgrade_error).to_string();
        assert!(diagnostic.contains("DatabaseBusy"));
        assert!(diagnostic.contains("extended code 517"));
        drop(tx);

        // Hold an uncommitted competing writer, then call the public mutation on another
        // connection. An IMMEDIATE transaction waits before reading the lease; a DEFERRED
        // one would read the old snapshot and fail its write upgrade.
        let worker_store = JobsStore::open(&path).unwrap();
        let mut blocker = Connection::open(&path).unwrap();
        blocker.busy_timeout(Duration::from_secs(2)).unwrap();
        let blocker_tx = blocker
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .unwrap();
        blocker_tx
            .execute(
                "UPDATE jobs SET updated_at_ms=updated_at_ms+1 WHERE id=?1",
                [job.id],
            )
            .unwrap();

        let entered = Arc::new(Barrier::new(2));
        let (finished_tx, finished_rx) = mpsc::channel();
        let worker_id = claim.worker_id.clone();
        let worker_fence = claim.fencing_token;
        let worker_entered = Arc::clone(&entered);
        let mutation = thread::spawn(move || {
            worker_entered.wait();
            let result = worker_store.record_checkpoint(
                job.id,
                &worker_id,
                worker_fence,
                "competing_writer_checkpoint",
                &serde_json::json!({"saved": true}),
                3,
            );
            finished_tx
                .send(result.map_err(|error| error.to_string()))
                .unwrap();
        });
        entered.wait();
        let finished_while_blocked = finished_rx.recv_timeout(Duration::from_millis(100));
        let writer_lock_blocked_mutation = matches!(
            &finished_while_blocked,
            Err(mpsc::RecvTimeoutError::Timeout)
        );
        blocker_tx.commit().unwrap();
        let mutation_result = match finished_while_blocked {
            Ok(result) => result,
            Err(mpsc::RecvTimeoutError::Timeout) => finished_rx
                .recv_timeout(Duration::from_secs(2))
                .expect("checkpoint mutation did not finish after releasing writer lock"),
            Err(error) => panic!("checkpoint mutation channel failed: {error}"),
        };
        mutation
            .join()
            .expect("checkpoint mutation thread panicked");
        assert!(
            writer_lock_blocked_mutation,
            "IMMEDIATE checkpoint transaction should wait for the competing writer"
        );
        mutation_result.expect("checkpoint write should succeed after writer lock is released");
        assert_eq!(
            store
                .get_checkpoint(job.id, "competing_writer_checkpoint")
                .unwrap()
                .unwrap(),
            serde_json::json!({"saved": true})
        );
    }

    #[test]
    fn latest_attempt_summary_projects_only_latest_summary_field() {
        let (_d, s) = store();
        let job = s.submit(request("FAB-L001", "summary", vec![]), 1).unwrap();
        let claim = s.claim("summary-worker", 2, 100).unwrap().unwrap();
        s.record_attempt_outcome(
            job.id,
            "summary-worker",
            claim.fencing_token,
            AttemptOutcome::NeedsInput {
                summary: "first reason".into(),
            },
            3,
        )
        .unwrap();
        let c = s.connection().unwrap();
        c.execute(
            "INSERT INTO job_attempts(job_id,attempt_no,state,worker_id,fencing_token,started_at_ms,result_json) \
             VALUES(?1,2,'needs_input','summary-worker',2,4,?2)",
            params![job.id, r#"{"summary":"latest reason","private":"must not escape"}"#],
        )
        .unwrap();
        drop(c);
        assert_eq!(
            s.latest_attempt_summary(job.id).unwrap().as_deref(),
            Some("latest reason")
        );

        let c = s.connection().unwrap();
        c.execute(
            "UPDATE job_attempts SET result_json='not-json' WHERE job_id=?1 AND attempt_no=2",
            [job.id],
        )
        .unwrap();
        drop(c);
        assert_eq!(s.latest_attempt_summary(job.id).unwrap(), None);
    }
    #[test]
    fn submit_is_idempotent_and_conflicting_key_rejected() {
        let (_d, s) = store();
        let r = request("FAB-L001", "once", vec![]);
        let a = s.submit(r.clone(), 1).unwrap();
        let b = s.submit(r, 2).unwrap();
        assert_eq!(a.id, b.id);
        let mut bad = request("FAB-L002", "once", vec![]);
        assert!(matches!(
            s.submit(bad.clone(), 3),
            Err(JobsError::Conflict(_))
        ));
        bad.idempotency_key = "other".into();
    }
    #[test]
    fn claims_are_fenced_and_expiry_requires_reconciliation() {
        let (_d, s) = store();
        let j = s.submit(request("FAB-L001", "k", vec![]), 1).unwrap();
        let c = s.claim("worker-a", 2, 10).unwrap().unwrap();
        assert_eq!(c.job.id, j.id);
        s.start_attempt(j.id, "worker-a", c.fencing_token, 3)
            .unwrap();
        s.record_process_reference(j.id, "worker-a", c.fencing_token, "owned-pid:42", 4)
            .unwrap();
        assert!(matches!(
            s.heartbeat(j.id, "worker-a", c.fencing_token, 20, 10),
            Err(JobsError::LeaseLost)
        ));
        let recovery = s.reconcile_expired(20).unwrap();
        assert_eq!(recovery.len(), 1);
        assert_eq!(recovery[0].job_id, j.id);
        assert_eq!(recovery[0].process_ref.as_deref(), Some("owned-pid:42"));
        assert_eq!(s.get(j.id).unwrap().state, JobState::ReconciliationRequired);
        assert_eq!(s.claim("worker-b", 21, 10).unwrap(), None);
        assert!(matches!(
            s.request_cancel(j.id, 21),
            Err(JobsError::Conflict(_))
        ));
        assert!(matches!(
            s.resolve_recovery(
                j.id,
                RecoveryDecision::SafeToRetry {
                    evidence: "incomplete reconciliation".into(),
                    process_confirmed_stopped: true,
                    effects_reconciled: false
                },
                22
            ),
            Err(JobsError::Invalid(_))
        ));
        s.resolve_recovery(
            j.id,
            RecoveryDecision::SafeToRetry {
                evidence: "process absent; effects reviewed".into(),
                process_confirmed_stopped: true,
                effects_reconciled: true,
            },
            22,
        )
        .unwrap();
        assert!(s.claim("worker-b", 23, 10).unwrap().is_some());
    }
    #[test]
    fn scoped_claim_cannot_steal_jobs_from_another_batch() {
        let (_d, s) = store();
        let batch_a = s.submit(request("FAB-A", "batch-a", vec![]), 1).unwrap();
        let batch_b = s.submit(request("FAB-B", "batch-b", vec![]), 1).unwrap();
        let claimed = s
            .claim_ids("batch-worker", &[batch_b.id], 2, 100)
            .unwrap()
            .unwrap();
        assert_eq!(claimed.job.id, batch_b.id);
        assert_eq!(s.get(batch_a.id).unwrap().state, JobState::Queued);
        assert!(s.claim_ids("empty", &[], 3, 100).unwrap().is_none());
        assert!(matches!(
            s.claim_ids("invalid", &[batch_a.id, batch_a.id], 3, 100),
            Err(JobsError::Invalid(_))
        ));
        assert!(matches!(
            s.claim_ids("stale", &[i64::MAX], 3, 100),
            Err(JobsError::NotFound(_))
        ));
        assert!(s
            .claim_ids("spent", &[batch_b.id], 3, 100)
            .unwrap()
            .is_none());
        assert_eq!(s.get(batch_a.id).unwrap().state, JobState::Queued);
    }
    #[test]
    fn scoped_expired_reconciliation_leaves_other_batches_untouched() {
        let (_d, s) = store();
        let batch_a = s
            .submit(request("FAB-A", "reconcile-a", vec![]), 1)
            .unwrap();
        let batch_b = s
            .submit(request("FAB-B", "reconcile-b", vec![]), 1)
            .unwrap();
        let outside = s
            .submit(request("FAB-C", "reconcile-c", vec![]), 1)
            .unwrap();
        let claim_a = s.claim("worker-a", 2, 10).unwrap().unwrap();
        s.start_attempt(batch_a.id, "worker-a", claim_a.fencing_token, 3)
            .unwrap();
        let claim_b = s.claim("worker-b", 4, 1000).unwrap().unwrap();
        s.start_attempt(batch_b.id, "worker-b", claim_b.fencing_token, 5)
            .unwrap();
        let claim_outside = s.claim("worker-c", 6, 10).unwrap().unwrap();
        s.start_attempt(outside.id, "worker-c", claim_outside.fencing_token, 7)
            .unwrap();

        let reconciled = s
            .reconcile_expired_ids(&[batch_a.id, batch_b.id], 20)
            .unwrap();
        assert_eq!(reconciled.len(), 1);
        assert_eq!(reconciled[0].job_id, batch_a.id);
        assert_eq!(
            s.get(batch_a.id).unwrap().state,
            JobState::ReconciliationRequired
        );
        assert_eq!(s.get(batch_b.id).unwrap().state, JobState::Running);
        assert_eq!(s.get(outside.id).unwrap().state, JobState::Running);
        assert!(s.reconcile_expired_ids(&[], 20).unwrap().is_empty());
    }
    #[test]
    fn batch_orders_jobs_and_blocks_until_prerequisite_verified() {
        let (_d, s) = store();
        let dep = DependencySpec {
            issue_id: "FAB-L001".into(),
            resolution: DependencyResolution::Job {
                prerequisite_issue_id: "FAB-L001".into(),
                required_fingerprint: "fp-FAB-L001".into(),
            },
        };
        let jobs = s
            .submit_batch(
                &[
                    request("FAB-L002", "b", vec![dep]),
                    request("FAB-L001", "a", vec![]),
                ],
                1,
            )
            .unwrap();
        let one = jobs.iter().find(|j| j.issue_id == "FAB-L001").unwrap();
        let two = jobs.iter().find(|j| j.issue_id == "FAB-L002").unwrap();
        assert_eq!(two.state, JobState::Blocked);
        let c = s.claim("w", 2, 100).unwrap().unwrap();
        assert_eq!(c.job.id, one.id);
        s.start_attempt(one.id, "w", c.fencing_token, 3).unwrap();
        s.record_attempt_outcome(
            one.id,
            "w",
            c.fencing_token,
            AttemptOutcome::ProposedComplete {
                summary: "model proposes done".into(),
            },
            4,
        )
        .unwrap();
        assert!(matches!(s.claim("w", 5, 100), Ok(None)));
        s.verify_gate(
            one.id,
            "fp-FAB-L001",
            &[VerificationEvidence {
                check_id: "fixture".into(),
                source_fingerprint: "fp-FAB-L001".into(),
                passed: true,
                details: "synthetic pass".into(),
            }],
            6,
        )
        .unwrap();
        assert!(s.claim("w", 7, 100).unwrap().is_some());
    }
    #[test]
    fn unresolved_and_external_dependencies_are_distinct() {
        let (_d, s) = store();
        let r = request(
            "GP-109",
            "gp",
            vec![DependencySpec {
                issue_id: "GP-108".into(),
                resolution: DependencyResolution::Unresolved,
            }],
        );
        let j = s.submit(r, 1).unwrap();
        assert_eq!(j.state, JobState::Blocked);
        assert_eq!(
            s.record_external_closed_evidence(
                j.id,
                "GP-108",
                "fixture",
                "GP-108",
                "test",
                "explicit closed prerequisite fixture",
                2
            )
            .unwrap()
            .state,
            JobState::Queued
        );
    }
    #[test]
    fn source_change_invalidates_and_plan_rejects_escape_or_recursion() {
        let (_d, s) = store();
        let j = s.submit(request("FAB-L001", "k", vec![]), 1).unwrap();
        assert!(s
            .check_source_fingerprint(j.id, "new-fingerprint", 2)
            .unwrap());
        assert_eq!(s.get(j.id).unwrap().state, JobState::Stale);
        let mut r = request("FAB-L002", "x", vec![]);
        r.plan.tasks[0].files[0] = "../secret".into();
        assert!(matches!(s.submit(r, 1), Err(JobsError::Invalid(_))));
    }
    #[test]
    fn stale_source_can_only_close_exact_unstarted_owned_lease() {
        let (_d, s) = store();
        let j = s
            .submit(request("FAB-L003", "stale-pre-dispatch", vec![]), 1)
            .unwrap();
        let claim = s.claim("worker-a", 2, 100).unwrap().unwrap();
        assert!(s
            .mark_stale_before_dispatch(j.id, "worker-a", claim.fencing_token, "changed", 3)
            .unwrap());
        assert_eq!(s.get(j.id).unwrap().state, JobState::Stale);

        let j2 = s
            .submit(request("FAB-L004", "stale-after-start", vec![]), 4)
            .unwrap();
        let claim2 = s.claim("worker-b", 5, 100).unwrap().unwrap();
        s.start_attempt(j2.id, "worker-b", claim2.fencing_token, 6)
            .unwrap();
        assert!(matches!(
            s.mark_stale_before_dispatch(j2.id, "worker-b", claim2.fencing_token, "changed", 7),
            Err(JobsError::Conflict(_))
        ));
        assert_eq!(s.get(j2.id).unwrap().state, JobState::Running);

        assert!(matches!(
            s.mark_stale_before_dispatch(j2.id, "worker-a", claim2.fencing_token, "changed", 7),
            Err(JobsError::LeaseLost)
        ));
    }
    #[test]
    fn aggregate_subtask_budgets_must_fit_parent() {
        let mut r = request("FAB-L001", "budget", vec![]);
        r.plan.limits.max_concurrency = 2;
        r.plan.tasks[0].limits.max_concurrency = 2;
        let mut child_limits = limits();
        child_limits.max_tokens = Some(600);
        child_limits.max_seconds = Some(100);
        child_limits.max_concurrency = 1;
        r.plan.tasks.push(PlannedTask {
            task_id: "worker-a".into(),
            parent_task_id: Some("implement".into()),
            kind: TaskKind::Worker,
            files: vec!["src/worker.rs".into()],
            acceptance: vec!["reviewable result".into()],
            limits: child_limits.clone(),
            provider_id: None,
            model_id: None,
            route_requirements: None,
        });
        r.plan.tasks.push(PlannedTask {
            task_id: "worker-b".into(),
            parent_task_id: Some("implement".into()),
            kind: TaskKind::Reviewer,
            files: vec!["src/review.rs".into()],
            acceptance: vec!["review result".into()],
            limits: child_limits,
            provider_id: None,
            model_id: None,
            route_requirements: None,
        });
        assert!(matches!(validate_plan(&r.plan), Err(JobsError::Invalid(_))));
        r.plan.tasks[1].limits.max_tokens = Some(400);
        r.plan.tasks[2].limits.max_tokens = Some(400);
        assert!(validate_plan(&r.plan).is_ok());
        r.plan.tasks[1].limits.max_tokens = None;
        assert!(matches!(validate_plan(&r.plan), Err(JobsError::Invalid(_))));
    }
    #[test]
    fn epic_is_aggregation_only_and_requires_verified_children() {
        let (_d, store) = store();
        let mut epic = request("FAB-E", "epic", vec![]);
        epic.issue_type = "epic".into();
        epic.plan.tasks[0].kind = TaskKind::EpicAggregation;
        epic.plan.tasks[0].files.clear();
        epic.plan.tasks[0].acceptance = vec!["all child gates verified".into()];
        let mut child = request("FAB-P1", "child", vec![]);
        child.parent_issue_ids = vec!["FAB-E".into()];
        let submitted = store.submit_batch(&[epic, child], 1).unwrap();
        assert_eq!(store.epic_progress("FAB-E").unwrap().complete, false);
        let claim = store.claim("worker", 2, 100).unwrap().unwrap();
        assert_eq!(claim.job.issue_id, "FAB-P1");
        assert!(submitted.iter().any(|j| j.issue_id == "FAB-E"));
        store
            .start_attempt(claim.job.id, "worker", claim.fencing_token, 3)
            .unwrap();
        store
            .record_attempt_outcome(
                claim.job.id,
                "worker",
                claim.fencing_token,
                AttemptOutcome::ProposedComplete {
                    summary: "child proposed done".into(),
                },
                4,
            )
            .unwrap();
        store
            .verify_gate(
                claim.job.id,
                "fp-FAB-P1",
                &[VerificationEvidence {
                    check_id: "fixture".into(),
                    source_fingerprint: "fp-FAB-P1".into(),
                    passed: true,
                    details: "child acceptance verified".into(),
                }],
                5,
            )
            .unwrap();
        let progress = store.epic_progress("FAB-E").unwrap();
        assert_eq!(progress.child_count, 1);
        assert_eq!(progress.verified_children, 1);
        assert!(progress.complete);
        assert_eq!(store.claim("worker", 6, 100).unwrap(), None);
    }
    #[test]
    fn cycle_and_schema_version_are_rejected() {
        let (_d, s) = store();
        let a = DependencySpec {
            issue_id: "B".into(),
            resolution: DependencyResolution::Job {
                prerequisite_issue_id: "B".into(),
                required_fingerprint: "fp-B".into(),
            },
        };
        let b = DependencySpec {
            issue_id: "A".into(),
            resolution: DependencyResolution::Job {
                prerequisite_issue_id: "A".into(),
                required_fingerprint: "fp-A".into(),
            },
        };
        assert!(matches!(
            s.submit_batch(&[request("A", "a", vec![a]), request("B", "b", vec![b])], 1),
            Err(JobsError::Invalid(_))
        ));
    }
    #[test]
    fn dependency_declared_id_must_match_job_target() {
        let (_d, s) = store();
        let wrong = DependencySpec {
            issue_id: "FAB-L009".into(),
            resolution: DependencyResolution::Job {
                prerequisite_issue_id: "FAB-L001".into(),
                required_fingerprint: "fp-FAB-L001".into(),
            },
        };
        assert!(matches!(
            s.submit(request("FAB-L002", "wrong-target", vec![wrong]), 1),
            Err(JobsError::Invalid(_))
        ));
    }
    #[test]
    fn gp109_to_gp112_requires_verified_predecessors_and_closed_gp108_evidence() {
        let (_d, s) = store();
        let dep = |id: &str| DependencySpec {
            issue_id: id.into(),
            resolution: DependencyResolution::Job {
                prerequisite_issue_id: id.into(),
                required_fingerprint: format!("fp-{id}"),
            },
        };
        let jobs = s
            .submit_batch(
                &[
                    request("GP-112", "112", vec![dep("GP-111")]),
                    request("GP-110", "110", vec![dep("GP-109")]),
                    request(
                        "GP-109",
                        "109",
                        vec![DependencySpec {
                            issue_id: "GP-108".into(),
                            resolution: DependencyResolution::Unresolved,
                        }],
                    ),
                    request("GP-111", "111", vec![dep("GP-110")]),
                ],
                1,
            )
            .unwrap();
        let ids: HashMap<_, _> = jobs.iter().map(|j| (j.issue_id.clone(), j.id)).collect();
        for id in ids.values() {
            assert_eq!(s.get(*id).unwrap().state, JobState::Blocked)
        }
        s.record_external_closed_evidence(
            ids["GP-109"],
            "GP-108",
            "synthetic-fixture",
            "GP-108",
            "2026-10-16",
            "closed external prerequisite fixture",
            2,
        )
        .unwrap();
        for issue in ["GP-109", "GP-110", "GP-111", "GP-112"] {
            let claimed = s
                .claim("fixture-worker", 10, 100)
                .unwrap()
                .expect("next topological issue must become eligible");
            assert_eq!(claimed.job.issue_id, issue);
            s.start_attempt(claimed.job.id, "fixture-worker", claimed.fencing_token, 11)
                .unwrap();
            s.record_attempt_outcome(
                claimed.job.id,
                "fixture-worker",
                claimed.fencing_token,
                AttemptOutcome::ProposedComplete {
                    summary: "synthetic proposal".into(),
                },
                12,
            )
            .unwrap();
            s.verify_gate(
                claimed.job.id,
                &format!("fp-{issue}"),
                &[VerificationEvidence {
                    check_id: "fixture".into(),
                    source_fingerprint: format!("fp-{issue}"),
                    passed: true,
                    details: "synthetic verified evidence".into(),
                }],
                13,
            )
            .unwrap();
        }
        assert_eq!(s.claim("fixture-worker", 20, 100).unwrap(), None)
    }
    #[test]
    fn parallel_claims_across_store_connections_have_one_winner() {
        let (d, s) = store();
        s.submit(request("FAB-L001", "once", vec![]), 1).unwrap();
        let path = d.path().join("jobs.sqlite3");
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(3));
        let mut workers = Vec::new();
        for name in ["a", "b"] {
            let path = path.clone();
            let barrier = barrier.clone();
            workers.push(std::thread::spawn(move || {
                let s = JobsStore::open(path).unwrap();
                barrier.wait();
                s.claim(name, 2, 100).unwrap().is_some()
            }))
        }
        barrier.wait();
        let won = workers
            .into_iter()
            .map(|w| w.join().unwrap())
            .filter(|won| *won)
            .count();
        assert_eq!(won, 1)
    }
    #[test]
    fn future_schema_is_rejected() {
        let (d, _) = store();
        let path = d.path().join("future.sqlite3");
        let c = Connection::open(&path).unwrap();
        c.pragma_update(None, "user_version", SCHEMA_VERSION + 1)
            .unwrap();
        drop(c);
        assert!(
            matches!(JobsStore::open(path),Err(JobsError::UnsupportedSchema(v)) if v==SCHEMA_VERSION+1)
        )
    }
}
