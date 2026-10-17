//! Local Git worktree isolation and ownership tracking for runner jobs.
//!
//! This module does not switch or clean the selected primary checkout, contact a
//! remote, or infer that a worktree is safe to delete from its directory name.

use serde::{Deserialize, Serialize};
use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};
use std::process::{Child, ExitStatus, Output, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const RECORD_VERSION: u32 = 1;

#[cfg(test)]
thread_local! {
    static METADATA_LOCK_WAIT_OBSERVER: std::cell::RefCell<Option<Box<dyn FnMut()>>> =
        const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
pub(crate) fn set_metadata_lock_wait_observer(observer: impl FnMut() + 'static) {
    METADATA_LOCK_WAIT_OBSERVER.with(|slot| *slot.borrow_mut() = Some(Box::new(observer)));
}

#[cfg(test)]
fn notify_metadata_lock_wait_observer() {
    METADATA_LOCK_WAIT_OBSERVER.with(|slot| {
        if let Some(observer) = slot.borrow_mut().as_mut() {
            observer();
        }
    });
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IsolationError {
    Invalid(String),
    Conflict(String),
    PermissionDenied(String),
    Git(String),
    NotFound(String),
    Io(String),
    RecoveryRequired(String),
}

impl std::fmt::Display for IsolationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Invalid(s) => write!(f, "invalid isolation request: {s}"),
            Self::Conflict(s) => write!(f, "isolation conflict: {s}"),
            Self::PermissionDenied(s) => write!(f, "permission denied: {s}"),
            Self::Git(s) => write!(f, "Git operation failed: {s}"),
            Self::NotFound(s) => write!(f, "isolation record not found: {s}"),
            Self::Io(s) => write!(f, "isolation I/O error: {s}"),
            Self::RecoveryRequired(s) => write!(f, "worktree recovery required: {s}"),
        }
    }
}
impl std::error::Error for IsolationError {}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct WorktreeRecord {
    pub schema_version: u32,
    pub issue_id: String,
    pub issue_snapshot_fingerprint: String,
    pub repository_root: PathBuf,
    pub worktree_path: PathBuf,
    pub branch: String,
    pub base_commit: String,
    pub predecessor_commits: Vec<String>,
    pub owned_commits: Vec<String>,
    pub owned_changes: Vec<String>,
    pub source_fingerprint: Option<String>,
    pub artifact_paths: Vec<PathBuf>,
    pub build_output_path: PathBuf,
    pub created_at_ms: u128,
    pub state: WorktreeState,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum WorktreeState {
    Preparing,
    Ready,
    Recoverable,
    CleanupPending,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct WorktreeInfo {
    pub path: PathBuf,
    pub branch: Option<String>,
    pub head: Option<String>,
    pub detached: bool,
    pub primary: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct RepositoryInspection {
    pub repository_root: PathBuf,
    pub primary_path: PathBuf,
    pub primary_dirty: bool,
    pub worktrees: Vec<WorktreeInfo>,
    pub has_remote: bool,
    pub verified_head: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PermissionClass {
    MetadataDenied,
    WorktreePathDenied,
    BuildOutputDenied,
    LockContended,
    Other(String),
}

#[derive(Clone, Debug)]
pub struct PrepareRequest {
    pub issue_id: String,
    pub issue_snapshot_fingerprint: String,
    /// Must resolve to this exact commit object. Pass a commit SHA, not a moving branch name.
    pub verified_base_commit: String,
    pub predecessor_commits: Vec<VerifiedPredecessor>,
    pub build_root: PathBuf,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct VerifiedPredecessor {
    pub issue_id: String,
    pub commit: String,
    pub source_fingerprint: String,
    pub verification_evidence: String,
}

/// Owns an active worktree writer lock. Dropping the lease releases only the lock;
/// cleanup is always an explicit ownership-checked operation.
pub struct WorkspaceLease {
    pub record: WorktreeRecord,
    _writer_lock: FileLock,
}

pub struct RepositoryMetadataLease {
    _lock: FileLock,
}

pub struct RepoIsolation {
    repo_root: PathBuf,
    primary_path: PathBuf,
    managed_root: PathBuf,
    git_common_dir: PathBuf,
}

impl RepoIsolation {
    pub fn new(
        selected_project_path: impl AsRef<Path>,
        managed_root: impl Into<PathBuf>,
        _state_root: impl Into<PathBuf>,
    ) -> Result<Self, IsolationError> {
        let selected = fs::canonicalize(selected_project_path.as_ref())
            .map_err(|e| classify_io("selected project", e))?;
        let repo = git(&selected, ["rev-parse", "--show-toplevel"])?;
        let repo_root = PathBuf::from(String::from_utf8_lossy(&repo.stdout).trim());
        let repo_root =
            fs::canonicalize(repo_root).map_err(|e| classify_io("repository root", e))?;
        let common = git(
            &repo_root,
            ["rev-parse", "--path-format=absolute", "--git-common-dir"],
        )?;
        let git_common_dir = fs::canonicalize(String::from_utf8_lossy(&common.stdout).trim())
            .map_err(|e| classify_io("Git common directory", e))?;
        let managed_root = absolute_path(managed_root.into())?;
        Ok(Self {
            repo_root,
            primary_path: selected,
            managed_root,
            git_common_dir,
        })
    }

    pub fn repository_root(&self) -> &Path {
        &self.repo_root
    }

    /// Serialize shared-repository integration, version, and release mutations.
    pub fn lock_repository_metadata(&self) -> Result<RepositoryMetadataLease, IsolationError> {
        FileLock::acquire(
            self.git_common_dir
                .join("fabrica-runner-locks/repository-metadata.lock"),
        )
        .map(|_lock| RepositoryMetadataLease { _lock })
    }

    fn lock_repository_metadata_until(
        &self,
        deadline: Instant,
        cancelled: &AtomicBool,
    ) -> Result<RepositoryMetadataLease, IsolationError> {
        let deadline = deadline.min(Instant::now() + Duration::from_secs(30));
        loop {
            if cancelled.load(Ordering::SeqCst) {
                return Err(IsolationError::Conflict(
                    "cancelled while waiting for repository metadata lock".into(),
                ));
            }
            if Instant::now() >= deadline {
                return Err(IsolationError::Conflict(
                    "repository metadata lock wait reached its bounded deadline".into(),
                ));
            }
            match self.lock_repository_metadata() {
                Ok(lease) => {
                    if cancelled.load(Ordering::SeqCst) {
                        drop(lease);
                        return Err(IsolationError::Conflict(
                            "cancelled while acquiring repository metadata lock".into(),
                        ));
                    }
                    if Instant::now() >= deadline {
                        drop(lease);
                        return Err(IsolationError::Conflict(
                            "repository metadata lock wait reached its bounded deadline".into(),
                        ));
                    }
                    return Ok(lease);
                }
                Err(IsolationError::Conflict(message)) if message.starts_with("lock is held:") => {
                    #[cfg(test)]
                    notify_metadata_lock_wait_observer();
                    let remaining = deadline.saturating_duration_since(Instant::now());
                    if remaining.is_zero() {
                        return Err(IsolationError::Conflict(
                            "repository metadata lock wait reached its bounded deadline".into(),
                        ));
                    }
                    thread::sleep(remaining.min(Duration::from_millis(25)));
                }
                Err(error) => return Err(error),
            }
        }
    }

    fn repository_metadata_lease(
        &self,
        lock_wait: Option<(Instant, &AtomicBool)>,
    ) -> Result<RepositoryMetadataLease, IsolationError> {
        match lock_wait {
            Some((deadline, cancelled)) => self.lock_repository_metadata_until(deadline, cancelled),
            None => self.lock_repository_metadata(),
        }
    }

    /// Read-only inspection. Local-only repositories are valid; remotes are reported,
    /// never required or contacted.
    pub fn inspect(&self) -> Result<RepositoryInspection, IsolationError> {
        let head = git(&self.repo_root, ["rev-parse", "--verify", "HEAD^{commit}"])?;
        let verified_head = String::from_utf8_lossy(&head.stdout).trim().to_string();
        let status = git(
            &self.primary_path,
            ["status", "--porcelain=v1", "-z", "--untracked-files=all"],
        )?;
        let listing = git(&self.repo_root, ["worktree", "list", "--porcelain"])?;
        let remotes = git(&self.repo_root, ["remote"])?;
        Ok(RepositoryInspection {
            repository_root: self.repo_root.clone(),
            primary_path: self.primary_path.clone(),
            primary_dirty: !status.stdout.is_empty(),
            worktrees: parse_worktrees(
                &String::from_utf8_lossy(&listing.stdout),
                &self.primary_path,
            ),
            has_remote: !remotes.stdout.iter().all(u8::is_ascii_whitespace),
            verified_head,
        })
    }

    /// Create or reuse exactly one owned worktree for this issue and verified base.
    /// Git metadata operations are serialized per repository; each returned lease
    /// holds a per-worktree writer lock until dropped.
    pub fn prepare_workspace(
        &self,
        request: PrepareRequest,
    ) -> Result<WorkspaceLease, IsolationError> {
        self.prepare_workspace_inner(request, None)
    }

    /// Prepare a worker workspace while waiting for ordinary repository metadata
    /// contention only until cancellation or the caller's absolute batch deadline.
    pub fn prepare_workspace_with_deadline(
        &self,
        request: PrepareRequest,
        deadline: Instant,
        cancelled: &AtomicBool,
    ) -> Result<WorkspaceLease, IsolationError> {
        self.prepare_workspace_inner(request, Some((deadline, cancelled)))
    }

    fn prepare_workspace_inner(
        &self,
        request: PrepareRequest,
        lock_wait: Option<(Instant, &AtomicBool)>,
    ) -> Result<WorkspaceLease, IsolationError> {
        validate_issue_id(&request.issue_id)?;
        if request.issue_snapshot_fingerprint.trim().is_empty()
            || request.issue_snapshot_fingerprint.len() > 256
        {
            return Err(IsolationError::Invalid(
                "issue snapshot fingerprint is required and bounded".into(),
            ));
        }
        validate_hex(&request.verified_base_commit, "base commit")?;
        if request.predecessor_commits.len() > 32 {
            return Err(IsolationError::Invalid(
                "at most 32 verified predecessor commits are allowed".into(),
            ));
        }
        let metadata = if let Some((deadline, cancelled)) = lock_wait {
            self.lock_repository_metadata_until(deadline, cancelled)?
        } else {
            self.lock_repository_metadata()?
        };
        let base = resolve_commit(&self.repo_root, &request.verified_base_commit)?;
        if base != request.verified_base_commit.to_ascii_lowercase() {
            return Err(IsolationError::Conflict(
                "verified base does not resolve to the exact requested commit".into(),
            ));
        }
        for predecessor in &request.predecessor_commits {
            validate_issue_id(&predecessor.issue_id)?;
            validate_hex(&predecessor.commit, "predecessor commit")?;
            if predecessor.source_fingerprint.trim().is_empty()
                || predecessor.verification_evidence.trim().is_empty()
            {
                return Err(IsolationError::Invalid("predecessor integration requires source fingerprint and explicit verification evidence".into()));
            }
            if resolve_commit(&self.repo_root, &predecessor.commit)?
                != predecessor.commit.to_ascii_lowercase()
            {
                return Err(IsolationError::Conflict(
                    "predecessor commit did not resolve exactly".into(),
                ));
            }
        }
        let key = safe_key(&request.issue_id);
        let record_path = self.record_path(&request.issue_id)?;
        let mut worktree_path = self.managed_root.join(&key);
        let build_root = absolute_path(request.build_root.clone())?;
        fs::create_dir_all(&build_root).map_err(|e| classify_io("build output", e))?;
        let build_root =
            fs::canonicalize(build_root).map_err(|e| classify_io("build output", e))?;
        if build_root.starts_with(&self.repo_root) || self.repo_root.starts_with(&build_root) {
            return Err(IsolationError::Invalid(
                "build output root must be separate from the repository tree".into(),
            ));
        }
        let build_output_path = build_root.join(&key).join(&base[..12.min(base.len())]);
        if build_output_path.starts_with(&worktree_path)
            || worktree_path.starts_with(&build_output_path)
        {
            return Err(IsolationError::Invalid(
                "build output must be separate from the source worktree".into(),
            ));
        }
        fs::create_dir_all(&self.managed_root)
            .map_err(|e| classify_io("managed worktree root", e))?;
        let managed_root = fs::canonicalize(&self.managed_root)
            .map_err(|e| classify_io("managed worktree root", e))?;
        worktree_path = managed_root.join(&key);
        fs::create_dir_all(&build_output_path).map_err(|e| classify_io("build output root", e))?;

        if record_path.exists() {
            let record = self.read_record(&request.issue_id)?;
            if record.repository_root != self.repo_root
                || record.base_commit != base
                || record.issue_snapshot_fingerprint != request.issue_snapshot_fingerprint
                || record.build_output_path != build_output_path
            {
                return Err(IsolationError::Conflict("existing issue checkout has a different repository or base; use recovery/integration explicitly".into()));
            }
            if record.state != WorktreeState::Ready {
                return Err(IsolationError::RecoveryRequired(
                    "owned worktree is not ready; reconcile it explicitly".into(),
                ));
            }
            validate_owned_path(&record, &managed_root, &self.repo_root)?;
            if !self.is_registered_worktree(&record.worktree_path)? {
                return Err(IsolationError::RecoveryRequired("ownership record exists but Git no longer registers its worktree; inspect before recreating".into()));
            }
            verify_existing_workspace(&record)?;
            let lock =
                FileLock::acquire(writer_lock_path(&self.git_common_dir, &request.issue_id))?;
            if record.predecessor_commits
                != request
                    .predecessor_commits
                    .iter()
                    .map(|p| p.commit.clone())
                    .collect::<Vec<_>>()
            {
                return Err(IsolationError::Conflict(
                    "reused worktree has a different predecessor integration set".into(),
                ));
            }
            if dirty_paths(&record.worktree_path)? != record.owned_changes {
                return Err(IsolationError::Conflict(
                    "worktree contains unrecorded edits; preserve it for review".into(),
                ));
            }
            self.write_record(&record)?;
            drop(metadata);
            return Ok(WorkspaceLease {
                record,
                _writer_lock: lock,
            });
        }
        if worktree_path.exists() {
            return Err(IsolationError::Conflict("target path exists without a matching ownership record; it will not be reused or removed".into()));
        }

        let branch = format!("codex/runner/{key}");
        let branch_ref = format!("refs/heads/{branch}");
        let branch_exists = git_raw(
            &self.repo_root,
            ["show-ref", "--verify", "--quiet", branch_ref.as_str()],
        )?
        .status
        .success();
        if branch_exists {
            return Err(IsolationError::RecoveryRequired("runner branch already exists without a matching ownership record; inspect it before retrying".into()));
        }
        let mut record = WorktreeRecord {
            schema_version: RECORD_VERSION,
            issue_id: request.issue_id.clone(),
            issue_snapshot_fingerprint: request.issue_snapshot_fingerprint.clone(),
            repository_root: self.repo_root.clone(),
            worktree_path: worktree_path.clone(),
            branch: branch.clone(),
            base_commit: base.clone(),
            predecessor_commits: request
                .predecessor_commits
                .iter()
                .map(|p| p.commit.clone())
                .collect(),
            owned_commits: Vec::new(),
            owned_changes: Vec::new(),
            source_fingerprint: None,
            artifact_paths: Vec::new(),
            build_output_path,
            created_at_ms: now_ms(),
            state: WorktreeState::Preparing,
        };
        self.write_record(&record)?;
        let lock = FileLock::acquire(writer_lock_path(&self.git_common_dir, &request.issue_id))?;
        let add = git_output(
            &self.repo_root,
            [
                "worktree",
                "add",
                "-b",
                branch.as_str(),
                path_for_git(&worktree_path).as_str(),
                base.as_str(),
            ],
        );
        if let Err(error) = add {
            record.state = WorktreeState::Recoverable;
            let _ = self.write_record(&record);
            return Err(error);
        }
        for predecessor in &request.predecessor_commits {
            if is_ancestor(&self.repo_root, &predecessor.commit, &base)? {
                continue;
            }
            let pick = git_output(
                &worktree_path,
                [
                    "cherry-pick",
                    "-x",
                    "--no-edit",
                    predecessor.commit.as_str(),
                ],
            );
            if let Err(error) = pick {
                record.state = WorktreeState::Recoverable;
                let _ = self.write_record(&record);
                return Err(error);
            }
        }
        record.state = WorktreeState::Ready;
        self.write_record(&record)?;
        drop(metadata);
        Ok(WorkspaceLease {
            record,
            _writer_lock: lock,
        })
    }

    /// Read an ownership record for explicit crash recovery. This never creates a
    /// checkout and does not break stale locks automatically.
    pub fn recover_workspace(&self, issue_id: &str) -> Result<WorktreeRecord, IsolationError> {
        validate_issue_id(issue_id)?;
        let _metadata = self.lock_repository_metadata()?;
        self.recover_workspace_under_metadata_lock(issue_id)
    }

    /// Recover an ownership record while waiting for the repository metadata lock
    /// only until the caller's batch deadline or cancellation. The metadata lease
    /// remains held through all record/path validation.
    pub(crate) fn recover_workspace_with_control(
        &self,
        issue_id: &str,
        deadline: Instant,
        cancelled: &AtomicBool,
    ) -> Result<WorktreeRecord, IsolationError> {
        validate_issue_id(issue_id)?;
        let _metadata = self.repository_metadata_lease(Some((deadline, cancelled)))?;
        self.recover_workspace_under_metadata_lock(issue_id)
    }

    /// True only for bounded metadata-lock wait stops produced by
    /// lock_repository_metadata_until; ownership and record conflicts remain false.
    pub(crate) fn is_metadata_lock_wait_stop(error: &IsolationError) -> bool {
        matches!(error, IsolationError::Conflict(message)
            if message == "cancelled while waiting for repository metadata lock"
                || message == "cancelled while acquiring repository metadata lock"
                || message == "repository metadata lock wait reached its bounded deadline")
    }

    fn recover_workspace_under_metadata_lock(
        &self,
        issue_id: &str,
    ) -> Result<WorktreeRecord, IsolationError> {
        let record = self.read_record(issue_id)?;
        if record.repository_root != self.repo_root {
            return Err(IsolationError::Conflict(
                "ownership record belongs to another repository".into(),
            ));
        }
        let managed = fs::canonicalize(&self.managed_root)
            .map_err(|e| classify_io("managed worktree root", e))?;
        validate_owned_path(&record, &managed, &self.repo_root)?;
        if !record.worktree_path.exists() || !self.is_registered_worktree(&record.worktree_path)? {
            return Err(IsolationError::RecoveryRequired(
                "owned path is absent or unregistered; no replacement checkout was created".into(),
            ));
        }
        Ok(record)
    }

    /// Remove only a matching, clean, registered worktree owned by this ledger.
    pub fn cleanup_owned(&self, issue_id: &str) -> Result<(), IsolationError> {
        validate_issue_id(issue_id)?;
        let _metadata = self.lock_repository_metadata()?;
        let record = self.read_record(issue_id)?;
        let managed = fs::canonicalize(&self.managed_root)
            .map_err(|e| classify_io("managed worktree root", e))?;
        validate_owned_path(&record, &managed, &self.repo_root)?;
        let lock = FileLock::acquire(writer_lock_path(&self.git_common_dir, issue_id))?;
        if record.repository_root != self.repo_root
            || !self.is_registered_worktree(&record.worktree_path)?
        {
            return Err(IsolationError::Conflict(
                "cleanup target is not a registered worktree owned by this repository".into(),
            ));
        }
        let status = git(
            &record.worktree_path,
            ["status", "--porcelain=v1", "-z", "--untracked-files=all"],
        )?;
        if !status.stdout.is_empty() {
            return Err(IsolationError::Conflict(
                "owned worktree has uncommitted changes; preserve it for recovery".into(),
            ));
        }
        self.mark_state(issue_id, WorktreeState::CleanupPending)?;
        git_output(
            &self.repo_root,
            [
                "worktree",
                "remove",
                path_for_git(&record.worktree_path).as_str(),
            ],
        )?;
        fs::remove_file(self.record_path(issue_id)?)
            .map_err(|e| classify_io("ownership record", e))?;
        drop(lock);
        Ok(())
    }

    /// Observe changed paths in the exact runner-owned worktree without recording them.
    /// The ownership record and canonical managed-root boundary are checked before Git status is read.
    pub fn observed_changed_paths(&self, issue_id: &str) -> Result<Vec<String>, IsolationError> {
        self.observed_changed_paths_with_wait(issue_id, None)
    }

    /// Observe changed paths while waiting for ordinary repository-metadata
    /// contention, bounded by the caller's absolute deadline and cancellation.
    pub fn observed_changed_paths_until(
        &self,
        issue_id: &str,
        deadline: Instant,
        cancelled: &AtomicBool,
    ) -> Result<Vec<String>, IsolationError> {
        self.observed_changed_paths_with_wait(issue_id, Some((deadline, cancelled)))
    }

    fn observed_changed_paths_with_wait(
        &self,
        issue_id: &str,
        lock_wait: Option<(Instant, &AtomicBool)>,
    ) -> Result<Vec<String>, IsolationError> {
        validate_issue_id(issue_id)?;
        let _metadata = self.repository_metadata_lease(lock_wait)?;
        let record = self.read_record(issue_id)?;
        if record.state != WorktreeState::Ready {
            return Err(IsolationError::RecoveryRequired(
                "worktree is not ready to observe changed paths".into(),
            ));
        }
        let managed = fs::canonicalize(&self.managed_root)
            .map_err(|e| classify_io("managed worktree root", e))?;
        validate_owned_path(&record, &managed, &self.repo_root)?;
        let paths = dirty_paths(&record.worktree_path)?;
        for path in &paths {
            let relative = Path::new(path);
            if path.trim().is_empty()
                || relative.is_absolute()
                || relative.components().any(|component| {
                    matches!(component, Component::ParentDir | Component::Prefix(_))
                })
            {
                return Err(IsolationError::Invalid(
                    "Git reported a changed path that is not safe and relative".into(),
                ));
            }
        }
        Ok(paths)
    }

    /// Persist the exact dirty path set authorized as this job's owned changes.
    /// A later reuse must observe the same set; unexpected edits remain recoverable.
    pub fn record_owned_changes(
        &self,
        issue_id: &str,
        source_fingerprint: &str,
        allowed_paths: &[String],
    ) -> Result<WorktreeRecord, IsolationError> {
        self.record_owned_changes_with_wait(issue_id, source_fingerprint, allowed_paths, None)
    }

    /// Record the exact dirty path set while waiting for ordinary repository
    /// metadata contention, bounded by the caller's absolute deadline/cancel.
    pub fn record_owned_changes_until(
        &self,
        issue_id: &str,
        source_fingerprint: &str,
        allowed_paths: &[String],
        deadline: Instant,
        cancelled: &AtomicBool,
    ) -> Result<WorktreeRecord, IsolationError> {
        self.record_owned_changes_with_wait(
            issue_id,
            source_fingerprint,
            allowed_paths,
            Some((deadline, cancelled)),
        )
    }

    fn record_owned_changes_with_wait(
        &self,
        issue_id: &str,
        source_fingerprint: &str,
        allowed_paths: &[String],
        lock_wait: Option<(Instant, &AtomicBool)>,
    ) -> Result<WorktreeRecord, IsolationError> {
        validate_issue_id(issue_id)?;
        if source_fingerprint.trim().is_empty() || source_fingerprint.len() > 256 {
            return Err(IsolationError::Invalid(
                "source fingerprint is required and bounded".into(),
            ));
        }
        let _metadata = self.repository_metadata_lease(lock_wait)?;
        let mut record = self.read_record(issue_id)?;
        if record.state != WorktreeState::Ready {
            return Err(IsolationError::RecoveryRequired(
                "worktree is not ready to record owned changes".into(),
            ));
        }
        let managed = fs::canonicalize(&self.managed_root)
            .map_err(|e| classify_io("managed worktree root", e))?;
        validate_owned_path(&record, &managed, &self.repo_root)?;
        let mut expected = Vec::new();
        for path in allowed_paths {
            if path.trim().is_empty()
                || Path::new(path).is_absolute()
                || Path::new(path)
                    .components()
                    .any(|c| matches!(c, Component::ParentDir | Component::Prefix(_)))
            {
                return Err(IsolationError::Invalid(
                    "owned change path must be safe and relative".into(),
                ));
            }
            expected.push(path.replace('\\', "/"));
        }
        expected.sort_by_key(|s| s.to_ascii_lowercase());
        expected.dedup_by(|a, b| a.eq_ignore_ascii_case(b));
        let observed = dirty_paths(&record.worktree_path)?;
        if observed != expected {
            return Err(IsolationError::Conflict(
                "authorized changed paths must exactly match Git status".into(),
            ));
        }
        record.owned_changes = expected;
        record.source_fingerprint = Some(source_fingerprint.into());
        self.write_record(&record)?;
        Ok(record)
    }

    pub fn record_owned_commit(
        &self,
        issue_id: &str,
        commit: &str,
    ) -> Result<WorktreeRecord, IsolationError> {
        validate_hex(commit, "owned commit")?;
        let _metadata = self.lock_repository_metadata()?;
        let mut record = self.read_record(issue_id)?;
        if record.state != WorktreeState::Ready {
            return Err(IsolationError::RecoveryRequired(
                "worktree is not ready to record an owned commit".into(),
            ));
        }
        let managed = fs::canonicalize(&self.managed_root)
            .map_err(|e| classify_io("managed worktree root", e))?;
        validate_owned_path(&record, &managed, &self.repo_root)?;
        let resolved = resolve_commit(&self.repo_root, commit)?;
        let status = git_raw(
            &record.worktree_path,
            ["merge-base", "--is-ancestor", resolved.as_str(), "HEAD"],
        )?;
        if !status.status.success() {
            return Err(IsolationError::Conflict(
                "owned commit is not reachable from the worktree head".into(),
            ));
        }
        if !record.owned_commits.contains(&resolved) {
            record.owned_commits.push(resolved);
        }
        self.write_record(&record)?;
        Ok(record)
    }

    pub fn record_artifact(
        &self,
        issue_id: &str,
        artifact: impl AsRef<Path>,
    ) -> Result<WorktreeRecord, IsolationError> {
        let _metadata = self.lock_repository_metadata()?;
        let mut record = self.read_record(issue_id)?;
        if record.state != WorktreeState::Ready {
            return Err(IsolationError::RecoveryRequired(
                "worktree is not ready to record an artifact".into(),
            ));
        }
        let managed = fs::canonicalize(&self.managed_root)
            .map_err(|e| classify_io("managed worktree root", e))?;
        validate_owned_path(&record, &managed, &self.repo_root)?;
        let artifact =
            fs::canonicalize(artifact.as_ref()).map_err(|e| classify_io("build artifact", e))?;
        let build_root = fs::canonicalize(&record.build_output_path)
            .map_err(|e| classify_io("build output", e))?;
        if !artifact.starts_with(&build_root) {
            return Err(IsolationError::Invalid(
                "artifact must be inside this job's isolated build-output directory".into(),
            ));
        }
        if !artifact.is_file() {
            return Err(IsolationError::Invalid(
                "artifact must be an existing file inside the isolated build output".into(),
            ));
        }
        if !record.artifact_paths.contains(&artifact) {
            record.artifact_paths.push(artifact);
        }
        self.write_record(&record)?;
        Ok(record)
    }

    pub fn classify_permission_error(error: &IsolationError) -> Option<PermissionClass> {
        match error {
            IsolationError::PermissionDenied(message) if message.contains("managed worktree") => {
                Some(PermissionClass::WorktreePathDenied)
            }
            IsolationError::PermissionDenied(message) if message.contains("build output") => {
                Some(PermissionClass::BuildOutputDenied)
            }
            IsolationError::PermissionDenied(_message) => Some(PermissionClass::MetadataDenied),
            IsolationError::Conflict(message) if message.contains("lock is held") => {
                Some(PermissionClass::LockContended)
            }
            IsolationError::Git(message) => Some(PermissionClass::Other(message.clone())),
            _ => None,
        }
    }

    fn record_path(&self, issue_id: &str) -> Result<PathBuf, IsolationError> {
        validate_issue_id(issue_id)?;
        Ok(self
            .git_common_dir
            .join("fabrica-runner-ownership")
            .join(format!("{}.json", safe_key(issue_id))))
    }
    fn read_record(&self, issue_id: &str) -> Result<WorktreeRecord, IsolationError> {
        let path = self.record_path(issue_id)?;
        let bytes = fs::read(&path).map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                IsolationError::NotFound(issue_id.into())
            } else {
                classify_io("ownership record", e)
            }
        })?;
        let record: WorktreeRecord = serde_json::from_slice(&bytes).map_err(|e| {
            IsolationError::RecoveryRequired(format!("ownership record is unreadable: {e}"))
        })?;
        if record.schema_version != RECORD_VERSION || record.issue_id != issue_id {
            return Err(IsolationError::RecoveryRequired(
                "ownership record version or issue identity does not match".into(),
            ));
        }
        Ok(record)
    }
    fn write_record(&self, record: &WorktreeRecord) -> Result<(), IsolationError> {
        let path = self.record_path(&record.issue_id)?;
        let parent = path
            .parent()
            .ok_or_else(|| IsolationError::Invalid("ownership directory is missing".into()))?;
        fs::create_dir_all(parent).map_err(|e| classify_io("ownership metadata", e))?;
        let bytes =
            serde_json::to_vec_pretty(record).map_err(|e| IsolationError::Io(e.to_string()))?;
        let temp = path.with_extension(format!("json.{}.tmp", now_ms()));
        let mut file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&temp)
            .map_err(|e| classify_io("ownership metadata", e))?;
        file.write_all(&bytes)
            .map_err(|e| classify_io("ownership metadata", e))?;
        file.sync_all()
            .map_err(|e| classify_io("ownership metadata", e))?;
        fs::rename(&temp, &path).map_err(|e| classify_io("ownership metadata", e))?;
        Ok(())
    }
    fn mark_state(&self, issue_id: &str, state: WorktreeState) -> Result<(), IsolationError> {
        let mut record = self.read_record(issue_id)?;
        record.state = state;
        self.write_record(&record)
    }
    fn is_registered_worktree(&self, path: &Path) -> Result<bool, IsolationError> {
        Ok(self
            .inspect()?
            .worktrees
            .iter()
            .any(|worktree| same_path(&worktree.path, path)))
    }
}

struct FileLock {
    #[cfg(windows)]
    file: fs::File,
    #[cfg(windows)]
    overlapped: Box<windows_sys::Win32::System::IO::OVERLAPPED>,
    #[cfg(not(windows))]
    path: PathBuf,
}
impl FileLock {
    fn acquire(path: PathBuf) -> Result<Self, IsolationError> {
        let parent = path
            .parent()
            .ok_or_else(|| IsolationError::Invalid("lock directory is missing".into()))?;
        fs::create_dir_all(parent).map_err(|e| classify_io("lock metadata", e))?;
        #[cfg(windows)]
        {
            use std::os::windows::io::AsRawHandle;
            use windows_sys::Win32::Storage::FileSystem::{
                LockFileEx, LOCKFILE_EXCLUSIVE_LOCK, LOCKFILE_FAIL_IMMEDIATELY,
            };
            let file = OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false)
                .open(&path)
                .map_err(|e| classify_io("lock metadata", e))?;
            let mut overlapped = Box::new(windows_sys::Win32::System::IO::OVERLAPPED::default());
            let locked = unsafe {
                LockFileEx(
                    file.as_raw_handle() as _,
                    LOCKFILE_EXCLUSIVE_LOCK | LOCKFILE_FAIL_IMMEDIATELY,
                    0,
                    1,
                    0,
                    overlapped.as_mut(),
                )
            };
            if locked == 0 {
                let error = std::io::Error::last_os_error();
                if error.raw_os_error() == Some(33) {
                    return Err(IsolationError::Conflict(format!(
                        "lock is held: {}",
                        path.display()
                    )));
                }
                return Err(classify_io("lock metadata", error));
            }
            // Older runner versions used a nonempty PID/timestamp sentinel and
            // did not hold an OS lock. Never assume that marker is stale: an old
            // worker may still be mutating the repository.
            if file
                .metadata()
                .map_err(|e| classify_io("lock metadata", e))?
                .len()
                != 0
            {
                return Err(IsolationError::RecoveryRequired(format!(
                    "legacy nonempty lock marker requires explicit owner recovery: {}",
                    path.display()
                )));
            }
            Ok(Self { file, overlapped })
        }
        #[cfg(not(windows))]
        match OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(mut file) => {
                writeln!(file, "pid={} acquired_ms={}", std::process::id(), now_ms())
                    .map_err(|e| classify_io("lock metadata", e))?;
                file.sync_all()
                    .map_err(|e| classify_io("lock metadata", e))?;
                Ok(Self { path })
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Err(
                IsolationError::Conflict(format!("lock is held: {}", path.display())),
            ),
            Err(e) => Err(classify_io("lock metadata", e)),
        }
    }
}
impl Drop for FileLock {
    fn drop(&mut self) {
        #[cfg(windows)]
        {
            use std::os::windows::io::AsRawHandle;
            use windows_sys::Win32::Storage::FileSystem::UnlockFileEx;
            let _ = unsafe {
                UnlockFileEx(
                    self.file.as_raw_handle() as _,
                    0,
                    1,
                    0,
                    self.overlapped.as_mut(),
                )
            };
        }
        #[cfg(not(windows))]
        let _ = fs::remove_file(&self.path);
    }
}

fn git_raw<const N: usize>(cwd: &Path, args: [&str; N]) -> Result<Output, IsolationError> {
    let mut command = crate::process::command("git");
    command
        .arg("-C")
        .arg(path_for_git(cwd))
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command
        .spawn()
        .map_err(|e| classify_io("Git executable", e))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| IsolationError::Git("could not capture Git stdout".into()))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| IsolationError::Git("could not capture Git stderr".into()))?;
    let out_reader = thread::spawn(move || read_bounded(stdout));
    let err_reader = thread::spawn(move || read_bounded(stderr));
    let status = wait_bounded(&mut child, Duration::from_secs(30))?;
    let (stdout, out_truncated) = out_reader
        .join()
        .map_err(|_| IsolationError::Git("Git output reader failed".into()))?;
    let (stderr, err_truncated) = err_reader
        .join()
        .map_err(|_| IsolationError::Git("Git error reader failed".into()))?;
    if out_truncated || err_truncated {
        return Err(IsolationError::Git(
            "Git output exceeded the 1 MiB safety limit".into(),
        ));
    }
    Ok(Output {
        status,
        stdout,
        stderr,
    })
}
fn git<const N: usize>(cwd: &Path, args: [&str; N]) -> Result<Output, IsolationError> {
    let output = git_raw(cwd, args)?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        let lower = stderr.to_ascii_lowercase();
        if lower.contains("permission denied")
            || lower.contains("access is denied")
            || lower.contains("operation not permitted")
        {
            return Err(IsolationError::PermissionDenied(stderr));
        }
        return Err(IsolationError::Git(if stderr.is_empty() {
            "command failed".into()
        } else {
            stderr
        }));
    }
    Ok(output)
}
fn git_output<const N: usize>(cwd: &Path, args: [&str; N]) -> Result<(), IsolationError> {
    git(cwd, args).map(|_| ())
}
fn resolve_commit(repo: &Path, commit: &str) -> Result<String, IsolationError> {
    validate_hex(commit, "commit")?;
    let revision = format!("{commit}^{{commit}}");
    let output = git(repo, ["rev-parse", "--verify", revision.as_str()])?;
    Ok(String::from_utf8_lossy(&output.stdout)
        .trim()
        .to_ascii_lowercase())
}
fn parse_worktrees(list: &str, primary: &Path) -> Vec<WorktreeInfo> {
    let mut result = Vec::new();
    let mut path: Option<PathBuf> = None;
    let mut branch = None;
    let mut head = None;
    let mut detached = false;
    let flush = |result: &mut Vec<WorktreeInfo>,
                 path: &mut Option<PathBuf>,
                 branch: &mut Option<String>,
                 head: &mut Option<String>,
                 detached: &mut bool| {
        if let Some(path) = path.take() {
            result.push(WorktreeInfo {
                primary: same_path(&path, primary),
                path,
                branch: branch.take(),
                head: head.take(),
                detached: *detached,
            });
        }
        *detached = false;
    };
    for line in list.lines() {
        if let Some(value) = line.strip_prefix("worktree ") {
            flush(
                &mut result,
                &mut path,
                &mut branch,
                &mut head,
                &mut detached,
            );
            path = Some(PathBuf::from(value));
        } else if let Some(value) = line.strip_prefix("HEAD ") {
            head = Some(value.into());
        } else if let Some(value) = line.strip_prefix("branch refs/heads/") {
            branch = Some(value.into());
        } else if line == "detached" {
            detached = true;
        }
    }
    flush(
        &mut result,
        &mut path,
        &mut branch,
        &mut head,
        &mut detached,
    );
    result
}
fn absolute_path(path: PathBuf) -> Result<PathBuf, IsolationError> {
    if path.is_absolute() {
        Ok(path)
    } else {
        std::env::current_dir()
            .map(|cwd| cwd.join(path))
            .map_err(|e| classify_io("current directory", e))
    }
}
fn validate_issue_id(issue: &str) -> Result<(), IsolationError> {
    if issue.is_empty()
        || issue.len() > 128
        || !issue
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-')
    {
        return Err(IsolationError::Invalid(
            "issue ID must contain only letters, digits or '-'".into(),
        ));
    }
    Ok(())
}
fn validate_hex(value: &str, name: &str) -> Result<(), IsolationError> {
    if !(40..=64).contains(&value.len()) || !value.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(IsolationError::Invalid(format!(
            "{name} must be a full Git object ID"
        )));
    }
    Ok(())
}
fn safe_key(issue: &str) -> String {
    issue
        .bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() || b == b'-' {
                b as char
            } else {
                '-'
            }
        })
        .collect()
}
fn writer_lock_path(common_dir: &Path, issue: &str) -> PathBuf {
    common_dir
        .join("fabrica-runner-locks")
        .join(format!("{}.writer.lock", safe_key(issue)))
}
fn validate_owned_path(
    record: &WorktreeRecord,
    managed_root: &Path,
    primary_root: &Path,
) -> Result<(), IsolationError> {
    let path =
        fs::canonicalize(&record.worktree_path).map_err(|e| classify_io("owned worktree", e))?;
    let managed =
        fs::canonicalize(managed_root).map_err(|e| classify_io("managed worktree root", e))?;
    let primary = fs::canonicalize(primary_root).unwrap_or_else(|_| normalize_path(primary_root));
    if !path.starts_with(&managed) || same_path(&path, &primary) {
        return Err(IsolationError::Conflict(
            "ownership record points outside the managed root or into the primary checkout".into(),
        ));
    }
    Ok(())
}
fn dirty_paths(worktree: &Path) -> Result<Vec<String>, IsolationError> {
    let output = git(
        worktree,
        ["status", "--porcelain=v1", "-z", "--untracked-files=all"],
    )?;
    let mut paths = Vec::new();
    for entry in output
        .stdout
        .split(|b| *b == 0)
        .filter(|entry| !entry.is_empty())
    {
        if entry.len() < 4 {
            return Err(IsolationError::Git(
                "Git returned malformed status output".into(),
            ));
        }
        if entry[..2].contains(&b'R') || entry[..2].contains(&b'C') {
            return Err(IsolationError::Conflict(
                "renamed/copied files require explicit recovery".into(),
            ));
        }
        let path = std::str::from_utf8(&entry[3..])
            .map_err(|_| IsolationError::Invalid("Git path is not valid UTF-8".into()))?
            .replace('\\', "/");
        paths.push(path);
    }
    paths.sort_by_key(|p| p.to_ascii_lowercase());
    Ok(paths)
}
fn is_ancestor(repo: &Path, commit: &str, base: &str) -> Result<bool, IsolationError> {
    Ok(
        git_raw(repo, ["merge-base", "--is-ancestor", commit, base])?
            .status
            .success(),
    )
}
fn verify_existing_workspace(record: &WorktreeRecord) -> Result<(), IsolationError> {
    let listing = git(&record.repository_root, ["worktree", "list", "--porcelain"])?;
    let worktrees = parse_worktrees(
        &String::from_utf8_lossy(&listing.stdout),
        &record.repository_root,
    );
    let current = worktrees
        .iter()
        .find(|w| same_path(&w.path, &record.worktree_path))
        .ok_or_else(|| {
            IsolationError::RecoveryRequired("owned worktree is no longer registered".into())
        })?;
    if current.branch.as_deref() != Some(record.branch.trim_start_matches("refs/heads/")) {
        return Err(IsolationError::Conflict(
            "owned worktree branch changed; preserve it for recovery".into(),
        ));
    }
    let head = git(
        &record.worktree_path,
        ["rev-parse", "--verify", "HEAD^{commit}"],
    )?;
    let head = String::from_utf8_lossy(&head.stdout).trim().to_string();
    if !is_ancestor(&record.repository_root, &record.base_commit, &head)? {
        return Err(IsolationError::Conflict(
            "owned worktree head no longer descends from its verified base".into(),
        ));
    }
    let log = git(
        &record.worktree_path,
        [
            "log",
            "--format=%B",
            &format!("{}..HEAD", record.base_commit),
        ],
    )?;
    let messages = String::from_utf8_lossy(&log.stdout);
    for predecessor in &record.predecessor_commits {
        if is_ancestor(&record.repository_root, predecessor, &record.base_commit)? {
            continue;
        }
        let marker = format!("cherry picked from commit {predecessor}");
        if !messages
            .to_ascii_lowercase()
            .contains(&marker.to_ascii_lowercase())
        {
            return Err(IsolationError::RecoveryRequired(format!(
                "predecessor {predecessor} is not verified in the worktree history"
            )));
        }
    }
    Ok(())
}
fn same_path(a: &Path, b: &Path) -> bool {
    let left = fs::canonicalize(a).unwrap_or_else(|_| normalize_path(a));
    let right = fs::canonicalize(b).unwrap_or_else(|_| normalize_path(b));
    left.to_string_lossy()
        .eq_ignore_ascii_case(&right.to_string_lossy())
}
fn path_for_git(path: &Path) -> String {
    let value = path.to_string_lossy();
    if let Some(rest) = value.strip_prefix("\\\\?\\UNC\\") {
        format!("\\\\{rest}")
    } else if let Some(rest) = value.strip_prefix("\\\\?\\") {
        rest.to_string()
    } else {
        value.to_string()
    }
}
fn normalize_path(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}
fn classify_io(context: &str, error: std::io::Error) -> IsolationError {
    if matches!(error.kind(), std::io::ErrorKind::PermissionDenied)
        || error.raw_os_error() == Some(5)
    {
        IsolationError::PermissionDenied(format!("{context}: {error}"))
    } else {
        IsolationError::Io(format!("{context}: {error}"))
    }
}
fn wait_bounded(child: &mut Child, timeout: Duration) -> Result<ExitStatus, IsolationError> {
    let start = Instant::now();
    loop {
        if let Some(status) = child
            .try_wait()
            .map_err(|e| classify_io("Git process", e))?
        {
            return Ok(status);
        }
        if start.elapsed() >= timeout {
            let _ = child.kill();
            let _ = child.wait();
            return Err(IsolationError::Git(
                "Git command timed out after 30 seconds; inspect repository state before retrying"
                    .into(),
            ));
        }
        thread::sleep(Duration::from_millis(40));
    }
}
fn read_bounded<R: Read>(mut reader: R) -> (Vec<u8>, bool) {
    const LIMIT: usize = 1024 * 1024;
    let mut bytes = Vec::new();
    let _ = reader
        .by_ref()
        .take((LIMIT + 1) as u64)
        .read_to_end(&mut bytes);
    let truncated = bytes.len() > LIMIT;
    bytes.truncate(LIMIT);
    (bytes, truncated)
}
fn now_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::ZERO)
        .as_millis()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn git_test(root: &Path, args: &[&str]) -> String {
        let out = std::process::Command::new("git")
            .arg("-C")
            .arg(root)
            .args(args)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git {:?}: {}",
            args,
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }
    fn fixture() -> (TempDir, PathBuf, RepoIsolation, String) {
        let temp = tempfile::tempdir().unwrap();
        let repo = temp.path().join("local-project");
        fs::create_dir_all(&repo).unwrap();
        let init = std::process::Command::new("git")
            .args(["init"])
            .arg(&repo)
            .output()
            .unwrap();
        assert!(
            init.status.success(),
            "git init: {}",
            String::from_utf8_lossy(&init.stderr)
        );
        git_test(&repo, &["config", "user.name", "Runner Test"]);
        git_test(
            &repo,
            &["config", "user.email", "runner-test@example.invalid"],
        );
        fs::write(repo.join("README.md"), "base\n").unwrap();
        git_test(&repo, &["add", "README.md"]);
        git_test(&repo, &["commit", "-m", "base"]);
        let base = git_test(&repo, &["rev-parse", "HEAD"]);
        let iso =
            RepoIsolation::new(&repo, repo.join(".worktrees"), temp.path().join("state")).unwrap();
        (temp, repo, iso, base)
    }
    fn request(issue: &str, base: &str, build: &Path) -> PrepareRequest {
        PrepareRequest {
            issue_id: issue.into(),
            issue_snapshot_fingerprint: format!("snapshot-{issue}"),
            verified_base_commit: base.into(),
            predecessor_commits: vec![],
            build_root: build.to_path_buf(),
        }
    }

    #[test]
    fn preserves_dirty_primary_and_isolates_build_output() {
        let (temp, repo, iso, base) = fixture();
        fs::write(repo.join("keeper.txt"), "unrelated local change\n").unwrap();
        let inspection = iso.inspect().unwrap();
        assert!(inspection.primary_dirty);
        assert!(!inspection.has_remote);
        let lease = iso
            .prepare_workspace(request("FAB-L007", &base, &temp.path().join("build")))
            .unwrap();
        assert_eq!(
            fs::read_to_string(repo.join("keeper.txt")).unwrap(),
            "unrelated local change\n"
        );
        assert_ne!(lease.record.worktree_path, repo);
        assert!(lease
            .record
            .build_output_path
            .starts_with(fs::canonicalize(temp.path().join("build")).unwrap()));
        assert!(!lease.record.build_output_path.starts_with(&repo));
        assert!(lease.record.branch.starts_with("codex/"));
        assert_eq!(iso.inspect().unwrap().worktrees.len(), 2);
    }

    #[test]
    fn verified_predecessor_is_cherry_picked_and_recorded_before_worker_claim() {
        let (temp, repo, iso, base) = fixture();
        fs::write(repo.join("predecessor.txt"), "verified result\n").unwrap();
        git_test(&repo, &["add", "predecessor.txt"]);
        git_test(&repo, &["commit", "-m", "verified predecessor"]);
        let predecessor = git_test(&repo, &["rev-parse", "HEAD"]);
        let mut req = request("FAB-L008", &base, &temp.path().join("build"));
        req.predecessor_commits.push(VerifiedPredecessor {
            issue_id: "FAB-L007".into(),
            commit: predecessor.clone(),
            source_fingerprint: "source-fp".into(),
            verification_evidence: "parent gate passed".into(),
        });
        let lease = iso.prepare_workspace(req).unwrap();
        assert!(lease.record.predecessor_commits.contains(&predecessor));
        assert_eq!(
            fs::read_to_string(lease.record.worktree_path.join("predecessor.txt"))
                .unwrap()
                .replace("\r\n", "\n"),
            "verified result\n"
        );
        assert!(verify_existing_workspace(&lease.record).is_ok());
        let head = git_test(&lease.record.worktree_path, &["rev-parse", "HEAD"]);
        let recorded = iso.record_owned_commit("FAB-L008", &head).unwrap();
        assert!(recorded.owned_commits.contains(&head));
    }

    #[test]
    fn conflicting_predecessor_is_recoverable_without_duplicate_checkout() {
        let (temp, repo, iso, base) = fixture();
        git_test(&repo, &["checkout", "-b", "pred-branch", &base]);
        fs::write(repo.join("README.md"), "predecessor side\n").unwrap();
        git_test(&repo, &["add", "README.md"]);
        git_test(&repo, &["commit", "-m", "predecessor edit"]);
        let pred = git_test(&repo, &["rev-parse", "HEAD"]);
        git_test(&repo, &["checkout", "-b", "base-side", &base]);
        fs::write(repo.join("README.md"), "base side\n").unwrap();
        git_test(&repo, &["add", "README.md"]);
        git_test(&repo, &["commit", "-m", "base edit"]);
        let verified_base = git_test(&repo, &["rev-parse", "HEAD"]);
        let mut req = request("FAB-L012", &verified_base, &temp.path().join("build"));
        req.predecessor_commits.push(VerifiedPredecessor {
            issue_id: "FAB-L011".into(),
            commit: pred,
            source_fingerprint: "source-fp".into(),
            verification_evidence: "verified predecessor".into(),
        });
        assert!(matches!(
            iso.prepare_workspace(req.clone()),
            Err(IsolationError::Git(_))
        ));
        let recovered = iso.recover_workspace("FAB-L012").unwrap();
        assert_eq!(recovered.state, WorktreeState::Recoverable);
        assert!(matches!(
            iso.prepare_workspace(req),
            Err(IsolationError::RecoveryRequired(_))
        ));
        assert_eq!(iso.inspect().unwrap().worktrees.len(), 2);
    }

    #[test]
    fn observed_changed_paths_are_read_only_and_exact_recording_rejects_mismatch() {
        let (temp, _repo, iso, base) = fixture();
        let lease = iso
            .prepare_workspace(request("FAB-L013", &base, &temp.path().join("build")))
            .unwrap();
        let worktree = lease.record.worktree_path.clone();
        fs::write(worktree.join("allowed-one.rs"), "synthetic change\n").unwrap();
        let before = iso.recover_workspace("FAB-L013").unwrap();
        let observed = iso.observed_changed_paths("FAB-L013").unwrap();
        assert_eq!(observed, vec!["allowed-one.rs"]);
        assert_eq!(iso.recover_workspace("FAB-L013").unwrap(), before);
        assert!(iso
            .record_owned_changes("FAB-L013", "source-fp", &observed)
            .is_ok());
        drop(lease);
    }

    #[test]
    fn exact_owned_change_recording_preserves_unexpected_paths_for_recovery() {
        let (temp, _repo, iso, base) = fixture();
        let lease = iso
            .prepare_workspace(request("FAB-L014", &base, &temp.path().join("build")))
            .unwrap();
        let worktree = lease.record.worktree_path.clone();
        fs::write(worktree.join("unexpected.txt"), "preserve this\n").unwrap();
        let before = iso.recover_workspace("FAB-L014").unwrap();
        let observed = iso.observed_changed_paths("FAB-L014").unwrap();
        assert_eq!(observed, vec!["unexpected.txt"]);
        assert!(matches!(
            iso.record_owned_changes("FAB-L014", "source-fp", &[]),
            Err(IsolationError::Conflict(_))
        ));
        assert_eq!(iso.recover_workspace("FAB-L014").unwrap(), before);
        assert_eq!(
            fs::read_to_string(worktree.join("unexpected.txt")).unwrap(),
            "preserve this\n"
        );
        drop(lease);
    }

    #[test]
    fn conflicting_edits_and_dirty_cleanup_are_preserved_for_recovery() {
        let (temp, _repo, iso, base) = fixture();
        let lease = iso
            .prepare_workspace(request("FAB-L009", &base, &temp.path().join("build")))
            .unwrap();
        let worktree = lease.record.worktree_path.clone();
        fs::write(worktree.join("owned.txt"), "expected\n").unwrap();
        assert!(iso
            .record_owned_changes("FAB-L009", "source-fp", &["owned.txt".into()])
            .is_ok());
        drop(lease);
        let active = iso
            .prepare_workspace(request("FAB-L009", &base, &temp.path().join("build")))
            .unwrap();
        assert!(matches!(
            iso.prepare_workspace(request("FAB-L009", &base, &temp.path().join("build"))),
            Err(IsolationError::Conflict(_))
        ));
        drop(active);
        fs::write(worktree.join("unexpected.txt"), "unowned\n").unwrap();
        assert!(matches!(
            iso.cleanup_owned("FAB-L009"),
            Err(IsolationError::Conflict(_))
        ));
        assert!(worktree.exists());
    }

    #[test]
    fn shared_metadata_lock_and_tampered_owner_path_fail_closed() {
        let (temp, repo, iso, base) = fixture();
        let other = RepoIsolation::new(
            &repo,
            repo.join(".worktrees"),
            temp.path().join("other-state"),
        )
        .unwrap();
        let metadata = iso.lock_repository_metadata().unwrap();
        let before = iso.inspect().unwrap().worktrees.len();
        assert!(matches!(
            other.prepare_workspace(request("FAB-L010", &base, &temp.path().join("build"))),
            Err(IsolationError::Conflict(_))
        ));
        assert_eq!(iso.inspect().unwrap().worktrees.len(), before);
        drop(metadata);
        let lease = iso
            .prepare_workspace(request("FAB-L010", &base, &temp.path().join("build")))
            .unwrap();
        let first_worktree = lease.record.worktree_path.clone();
        drop(lease);
        let reused = other
            .prepare_workspace(request("FAB-L010", &base, &temp.path().join("build")))
            .unwrap();
        assert_eq!(reused.record.worktree_path, first_worktree);
        drop(reused);
        let path = iso.record_path("FAB-L010").unwrap();
        let mut record = iso.read_record("FAB-L010").unwrap();
        record.worktree_path = repo.clone();
        fs::write(path, serde_json::to_vec(&record).unwrap()).unwrap();
        assert!(matches!(
            iso.recover_workspace("FAB-L010"),
            Err(IsolationError::Conflict(_))
        ));
        assert!(matches!(
            iso.cleanup_owned("FAB-L010"),
            Err(IsolationError::Conflict(_))
        ));
    }

    #[test]
    fn bounded_metadata_lock_rejects_expired_deadline_before_acquiring() {
        let (_temp, _repo, iso, _base) = fixture();
        let cancelled = AtomicBool::new(false);
        assert!(matches!(
            iso.lock_repository_metadata_until(Instant::now(), &cancelled),
            Err(IsolationError::Conflict(message)) if message.contains("deadline")
        ));
        // The expired call must not leave a lock behind.
        drop(iso.lock_repository_metadata().unwrap());
    }

    #[test]
    fn bounded_metadata_lock_waits_for_ordinary_contention() {
        let (temp, repo, iso, _base) = fixture();
        let other = RepoIsolation::new(
            &repo,
            repo.join(".worktrees"),
            temp.path().join("other-state"),
        )
        .unwrap();
        let held = iso.lock_repository_metadata().unwrap();
        let waiter = thread::spawn(move || {
            let result = other.lock_repository_metadata_until(
                Instant::now() + Duration::from_secs(2),
                &AtomicBool::new(false),
            );
            result.is_ok()
        });
        thread::sleep(Duration::from_millis(75));
        drop(held);
        assert!(waiter.join().unwrap());
    }

    #[test]
    fn controlled_workspace_recovery_waits_under_lock_then_returns_same_record() {
        let (temp, repo, iso, base) = fixture();
        let lease = iso
            .prepare_workspace(request("FAB-L018", &base, &temp.path().join("build")))
            .unwrap();
        let expected = lease.record.clone();
        let other = RepoIsolation::new(
            &repo,
            repo.join(".worktrees"),
            temp.path().join("other-state"),
        )
        .unwrap();
        let held = iso.lock_repository_metadata().unwrap();
        let (wait_tx, wait_rx) = std::sync::mpsc::sync_channel(1);
        let waiter = thread::spawn(move || {
            set_metadata_lock_wait_observer(move || {
                let _ = wait_tx.try_send(());
            });
            other.recover_workspace_with_control(
                "FAB-L018",
                Instant::now() + Duration::from_secs(2),
                &AtomicBool::new(false),
            )
        });
        wait_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        drop(held);
        assert_eq!(waiter.join().unwrap().unwrap(), expected);
        drop(lease);
    }

    #[test]
    fn controlled_workspace_recovery_stops_on_deadline_and_cancellation() {
        let (temp, repo, iso, base) = fixture();
        let lease = iso
            .prepare_workspace(request("FAB-L019", &base, &temp.path().join("build")))
            .unwrap();
        let other = RepoIsolation::new(
            &repo,
            repo.join(".worktrees"),
            temp.path().join("other-state"),
        )
        .unwrap();
        let held = iso.lock_repository_metadata().unwrap();
        let cancelled = AtomicBool::new(false);
        assert!(matches!(
            other.recover_workspace_with_control(
                "FAB-L019", Instant::now() + Duration::from_millis(40), &cancelled,
            ),
            Err(IsolationError::Conflict(message)) if message == "repository metadata lock wait reached its bounded deadline"
        ));
        let cancelled = AtomicBool::new(true);
        assert!(matches!(
            other.recover_workspace_with_control(
                "FAB-L019", Instant::now() + Duration::from_secs(2), &cancelled,
            ),
            Err(IsolationError::Conflict(message)) if message == "cancelled while waiting for repository metadata lock"
        ));
        assert!(RepoIsolation::is_metadata_lock_wait_stop(
            &IsolationError::Conflict(
                "repository metadata lock wait reached its bounded deadline".into()
            )
        ));
        assert!(!RepoIsolation::is_metadata_lock_wait_stop(
            &IsolationError::Conflict("ownership record belongs to another repository".into())
        ));
        drop(held);
        assert_eq!(other.recover_workspace("FAB-L019").unwrap(), lease.record);
        drop(lease);
    }

    #[test]
    fn capture_observation_and_record_wait_for_ordinary_metadata_contention() {
        let (temp, repo, iso, base) = fixture();
        let lease = iso
            .prepare_workspace(request("FAB-L016", &base, &temp.path().join("build")))
            .unwrap();
        fs::write(lease.record.worktree_path.join("allowed.rs"), "change\n").unwrap();
        let other = RepoIsolation::new(
            &repo,
            repo.join(".worktrees"),
            temp.path().join("other-state"),
        )
        .unwrap();
        let held = iso.lock_repository_metadata().unwrap();
        let (observe_wait_tx, observe_wait_rx) = std::sync::mpsc::sync_channel(1);
        let (record_wait_tx, record_wait_rx) = std::sync::mpsc::sync_channel(1);
        let (observed_tx, observed_rx) = std::sync::mpsc::channel();
        let (continue_tx, continue_rx) = std::sync::mpsc::channel();
        let waiter = thread::spawn(move || {
            set_metadata_lock_wait_observer(move || {
                let _ = observe_wait_tx.try_send(());
            });
            let cancelled = AtomicBool::new(false);
            let deadline = Instant::now() + Duration::from_secs(2);
            let observed = other
                .observed_changed_paths_until("FAB-L016", deadline, &cancelled)
                .unwrap();
            observed_tx.send(()).unwrap();
            continue_rx.recv().unwrap();
            set_metadata_lock_wait_observer(move || {
                let _ = record_wait_tx.try_send(());
            });
            let recorded = other
                .record_owned_changes_until(
                    "FAB-L016",
                    "source-fp",
                    &observed,
                    deadline,
                    &cancelled,
                )
                .unwrap();
            (observed, recorded.owned_changes)
        });
        observe_wait_rx
            .recv_timeout(Duration::from_secs(2))
            .unwrap();
        drop(held);
        observed_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        let held_for_record = iso.lock_repository_metadata().unwrap();
        continue_tx.send(()).unwrap();
        record_wait_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        drop(held_for_record);
        let (observed, recorded) = waiter.join().unwrap();
        assert_eq!(observed, vec!["allowed.rs"]);
        assert_eq!(recorded, observed);
        drop(lease);
    }

    #[test]
    fn capture_observation_and_record_honor_cancel_and_deadline_without_mutation() {
        let (temp, _repo, iso, base) = fixture();
        let lease = iso
            .prepare_workspace(request("FAB-L017", &base, &temp.path().join("build")))
            .unwrap();
        let worktree = lease.record.worktree_path.clone();
        fs::write(worktree.join("allowed.rs"), "change\n").unwrap();
        let before = iso.recover_workspace("FAB-L017").unwrap();
        let held = iso.lock_repository_metadata().unwrap();

        let cancelled = AtomicBool::new(true);
        assert!(matches!(
            iso.observed_changed_paths_until(
                "FAB-L017",
                Instant::now() + Duration::from_secs(2),
                &cancelled,
            ),
            Err(IsolationError::Conflict(message)) if message.contains("cancelled")
        ));
        assert!(matches!(
            iso.record_owned_changes_until(
                "FAB-L017",
                "source-fp",
                &["allowed.rs".into()],
                Instant::now() + Duration::from_secs(2),
                &cancelled,
            ),
            Err(IsolationError::Conflict(message)) if message.contains("cancelled")
        ));

        let not_cancelled = AtomicBool::new(false);
        let deadline = Instant::now() + Duration::from_millis(40);
        assert!(matches!(
            iso.observed_changed_paths_until("FAB-L017", deadline, &not_cancelled),
            Err(IsolationError::Conflict(message)) if message.contains("deadline")
        ));
        let deadline = Instant::now() + Duration::from_millis(40);
        assert!(matches!(
            iso.record_owned_changes_until(
                "FAB-L017",
                "source-fp",
                &["allowed.rs".into()],
                deadline,
                &not_cancelled,
            ),
            Err(IsolationError::Conflict(message)) if message.contains("deadline")
        ));
        drop(held);
        assert_eq!(iso.recover_workspace("FAB-L017").unwrap(), before);
        assert!(iso
            .observed_changed_paths("FAB-L017")
            .unwrap()
            .contains(&"allowed.rs".into()));
        drop(lease);
    }

    #[cfg(windows)]
    #[test]
    fn legacy_nonempty_lock_marker_requires_explicit_recovery() {
        let (_temp, _repo, iso, _base) = fixture();
        let path = iso
            .git_common_dir
            .join("fabrica-runner-locks/repository-metadata.lock");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, "pid=123 acquired_ms=456\n").unwrap();
        assert!(matches!(
            iso.lock_repository_metadata(),
            Err(IsolationError::RecoveryRequired(message)) if message.contains("legacy nonempty")
        ));
    }

    #[test]
    fn denies_build_output_inside_source_and_classifies_permissions() {
        let (_temp, repo, iso, base) = fixture();
        assert!(matches!(
            iso.prepare_workspace(request("FAB-L011", &base, &repo.join("build"))),
            Err(IsolationError::Invalid(_))
        ));
        assert_eq!(
            RepoIsolation::classify_permission_error(&IsolationError::PermissionDenied(
                "ownership metadata denied".into()
            )),
            Some(PermissionClass::MetadataDenied)
        );
        assert_eq!(
            RepoIsolation::classify_permission_error(&IsolationError::Conflict(
                "lock is held".into()
            )),
            Some(PermissionClass::LockContended)
        );
    }
}
