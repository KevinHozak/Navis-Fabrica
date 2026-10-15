use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tauri::{AppHandle, Emitter, Manager};

const HISTORY_FILE: &str = "workflow-runs.sqlite3";
const MAX_RUNS: usize = 100;
const MAX_EVENTS: usize = 2_000;
const MAX_OUTPUT_BYTES: usize = 512 * 1024;
const MAX_OUTPUT_CHUNK: usize = 4 * 1024;
const MAX_COMMAND_BYTES: usize = 64 * 1024;
const MAX_CONTROL_EVENT_BYTES: usize = 32 * 1024;
const MAX_PENDING_CODEX_REQUESTS: usize = 16;
const MAX_TIMEOUT_SECS: u64 = 24 * 60 * 60;
const POLL_INTERVAL: Duration = Duration::from_millis(100);

static RUNS: OnceLock<Mutex<RunStore>> = OnceLock::new();
static NEXT_ID: AtomicU64 = AtomicU64::new(1);

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct StartRequest {
    pub executable: String,
    pub arguments: Vec<String>,
    pub cwd: String,
    pub timeout_seconds: u64,
    pub mutating: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RunEvent {
    pub sequence: u64,
    pub timestamp_ms: u128,
    pub kind: String,
    pub text: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RunRecord {
    pub id: String,
    pub executable: String,
    #[serde(default)]
    pub provider: Option<String>,
    #[serde(default)]
    pub model_id: Option<String>,
    pub cwd: String,
    #[serde(default)]
    pub thread_id: Option<String>,
    #[serde(default)]
    pub process_id: Option<u32>,
    pub mutating: bool,
    #[serde(default)]
    pub timeout_seconds: u64,
    pub state: String,
    pub started_at_ms: u128,
    pub finished_at_ms: Option<u128>,
    #[serde(default)]
    pub latency_ms: Option<u128>,
    pub exit_code: Option<i32>,
    pub next_sequence: u64,
    pub events: Vec<RunEvent>,
    #[serde(default)]
    pub workflow: Option<WorkflowRunMetadata>,
    #[serde(default)]
    pub handoff: Option<ProviderHandoff>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct WorkflowRunMetadata {
    pub action: String,
    pub operation_key: String,
    #[serde(default)]
    pub intent: String,
    #[serde(default)]
    pub build_profile: Option<String>,
    pub issue_url: Option<String>,
    pub scope_paths: Vec<String>,
    pub requested_effects: Vec<String>,
    #[serde(default)]
    pub side_effects: Option<WorkflowSideEffects>,
    #[serde(default)]
    pub build_artifact: Option<BuildArtifact>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BuildArtifact {
    pub profile: String,
    pub path: String,
    pub file_version: String,
    pub product_version: String,
    pub verified: bool,
    pub verification_message: String,
}

#[derive(Debug, PartialEq, Eq)]
struct ArtifactVersionInfo {
    file_version: String,
    product_version: String,
}

fn read_embedded_artifact_version(path: &Path) -> Option<ArtifactVersionInfo> {
    #[cfg(windows)]
    {
        let output = Command::new("powershell.exe")
            .args([
                "-NoLogo",
                "-NoProfile",
                "-NonInteractive",
                "-Command",
                "$v = (Get-Item -LiteralPath $env:NAVIS_FABRICA_ARTIFACT).VersionInfo; @{ fileVersion = $v.FileVersion; productVersion = $v.ProductVersion } | ConvertTo-Json -Compress",
            ])
            .env("NAVIS_FABRICA_ARTIFACT", path)
            .output()
            .ok()?;
        if !output.status.success() {
            return None;
        }
        let value = serde_json::from_slice::<serde_json::Value>(&output.stdout).ok()?;
        return Some(ArtifactVersionInfo {
            file_version: value.get("fileVersion")?.as_str()?.to_owned(),
            product_version: value.get("productVersion")?.as_str()?.to_owned(),
        });
    }
    #[cfg(not(windows))]
    {
        let _ = path;
        None
    }
}

fn artifact_is_in_profile_target(root: &Path, artifact: &Path, profile: &str) -> bool {
    let Ok(relative) = artifact.strip_prefix(root.join("src-tauri/target")) else {
        return false;
    };
    let components = relative.components().collect::<Vec<_>>();
    let profile_matches = components
        .first()
        .is_some_and(|component| component.as_os_str() == profile)
        || components
            .get(1)
            .is_some_and(|component| component.as_os_str() == profile);
    profile_matches && components.len() >= 2
}

fn verify_build_artifact(
    cwd: &Path,
    workflow: &WorkflowRunMetadata,
    text: &str,
) -> Option<BuildArtifact> {
    verify_build_artifact_with(cwd, workflow, text, read_embedded_artifact_version)
}

fn verify_build_artifact_with(
    cwd: &Path,
    workflow: &WorkflowRunMetadata,
    text: &str,
    read_version: impl FnOnce(&Path) -> Option<ArtifactVersionInfo>,
) -> Option<BuildArtifact> {
    let json = text
        .lines()
        .find_map(|line| line.trim().strip_prefix("NAVIS_BUILD_ARTIFACT:"))?;
    let value = serde_json::from_str::<serde_json::Value>(json.trim()).ok()?;
    let profile = value.get("profile")?.as_str()?.to_owned();
    let path = value.get("path")?.as_str()?;
    let file_version = value.get("fileVersion")?.as_str()?.to_owned();
    let product_version = value.get("productVersion")?.as_str()?.to_owned();
    let expected_profile = workflow.build_profile.as_deref()?;
    let resolved = Path::new(path).canonicalize().ok()?;
    let root = cwd.canonicalize().ok()?;
    let declared_version = serde_json::from_slice::<serde_json::Value>(
        &fs::read(root.join("src-tauri/tauri.conf.json")).ok()?,
    )
    .ok()?
    .get("version")?
    .as_str()?
    .to_owned();
    let file = fs::metadata(&resolved).ok()?;
    let embedded_version = read_version(&resolved);
    let verified = profile == expected_profile
        && resolved.starts_with(&root)
        && artifact_is_in_profile_target(&root, &resolved, expected_profile)
        && file.is_file()
        && file.len() > 0
        && embedded_version.as_ref().is_some_and(|embedded| {
            embedded.file_version == declared_version
                && embedded.product_version == declared_version
                && file_version == embedded.file_version
                && product_version == embedded.product_version
        });
    Some(BuildArtifact {
        profile,
        path: resolved.to_string_lossy().into_owned(),
        file_version,
        product_version,
        verified,
        verification_message: if verified {
            "Artifact is under the selected profile's Tauri target directory; FileVersion and ProductVersion were read from the artifact and match both the receipt and tauri.conf.json.".into()
        } else {
            "Artifact receipt did not match the selected profile target path, project path, file, or embedded version metadata.".into()
        },
    })
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct WorkflowSideEffects {
    pub commit_sha: Option<String>,
    pub remote_branch: Option<String>,
    pub pull_request_url: Option<String>,
    #[serde(default)]
    pub verified: bool,
    #[serde(default)]
    pub verification_message: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ProviderHandoff {
    pub version: u32,
    pub source_run_id: String,
    pub target_provider_id: String,
    #[serde(default)]
    pub working_tree_root: String,
    #[serde(default)]
    pub working_tree_paths: Vec<String>,
    #[serde(default)]
    pub working_tree_fingerprint: Option<String>,
    pub goal: String,
    pub scope: String,
    pub diff: String,
    pub checks: String,
    pub pending_work: String,
    pub completed_side_effects: Option<WorkflowSideEffects>,
    pub created_at_ms: u128,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct HandoffInput {
    pub source_run_id: String,
    pub target_provider_id: String,
    pub goal: String,
    pub scope: String,
    pub diff: String,
    pub checks: String,
    pub pending_work: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct OllamaStartRequest {
    pub cwd: String,
    pub prompt: String,
    pub test_command: Option<String>,
    pub handoff_source_run_id: Option<String>,
}

fn parse_side_effect_marker(text: &str) -> Option<WorkflowSideEffects> {
    let json = text
        .lines()
        .find_map(|line| line.trim().strip_prefix("NAVIS_SIDE_EFFECTS:"))?;
    let value = serde_json::from_str::<WorkflowSideEffects>(json.trim()).ok()?;
    let commit_sha = value.commit_sha.filter(|sha| {
        (7..=64).contains(&sha.len()) && sha.bytes().all(|byte| byte.is_ascii_hexdigit())
    });
    let remote_branch = value.remote_branch.filter(|branch| {
        !branch.is_empty() && branch.len() <= 255 && !branch.chars().any(char::is_control)
    });
    let pull_request_url = value.pull_request_url.filter(|url| {
        url.starts_with("https://github.com/")
            && url.contains("/pull/")
            && !url.chars().any(char::is_control)
    });
    (commit_sha.is_some() || remote_branch.is_some() || pull_request_url.is_some()).then_some(
        WorkflowSideEffects {
            commit_sha,
            remote_branch,
            pull_request_url,
            verified: false,
            verification_message: None,
        },
    )
}

#[derive(Default)]
struct RunStore {
    records: Vec<RunRecord>,
    active: HashMap<String, Arc<Mutex<Child>>>,
    mutating_paths: HashSet<String>,
    output_bytes: HashMap<String, usize>,
    cancelled: HashSet<String>,
    redaction_values: HashMap<String, Vec<String>>,
    codex_writers: HashMap<String, Arc<Mutex<ChildStdin>>>,
    codex_thread_ids: HashMap<String, String>,
    codex_requests: HashMap<String, HashMap<String, PendingCodexRequest>>,
    adapter_terminal: HashSet<String>,
}

#[derive(Debug, Clone)]
struct PendingCodexRequest {
    request_id: serde_json::Value,
    method: String,
    params: serde_json::Value,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CodexStartRequest {
    pub cwd: String,
    pub prompt: String,
    pub skill_id: Option<String>,
    pub timeout_seconds: u64,
    pub workflow: Option<WorkflowRunMetadata>,
    pub handoff_source_run_id: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AntigravityStartRequest {
    pub cwd: String,
    pub prompt: String,
    pub timeout_seconds: u64,
    pub handoff_source_run_id: Option<String>,
}

#[derive(Debug, Clone)]
pub struct CodexRunConfig {
    pub executable: PathBuf,
    pub cwd: PathBuf,
    pub model_id: String,
    pub reasoning_effort: Option<String>,
    pub prompt: String,
    pub skill_id: Option<String>,
    pub skill_path: Option<PathBuf>,
    pub timeout_seconds: u64,
    pub workflow: Option<WorkflowRunMetadata>,
    pub handoff: Option<ProviderHandoff>,
}

#[derive(Debug, Clone)]
pub struct AntigravityRunConfig {
    pub executable: PathBuf,
    pub cwd: PathBuf,
    pub model_id: String,
    pub prompt: String,
    pub timeout_seconds: u64,
}

#[derive(Debug, Clone)]
pub struct OllamaRunConfig {
    pub executable: PathBuf,
    pub cwd: PathBuf,
    pub endpoint: String,
    pub model_id: String,
    pub prompt: String,
    pub test_command: Option<String>,
    pub timeout_seconds: u64,
}

fn store() -> &'static Mutex<RunStore> {
    RUNS.get_or_init(|| Mutex::new(RunStore::default()))
}

fn now_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
}

fn process_is_running(process_id: u32) -> bool {
    #[cfg(windows)]
    {
        let query = format!("(Get-Process -Id {process_id} -ErrorAction SilentlyContinue).Id");
        let output = Command::new("powershell.exe")
            .args(["-NoLogo", "-NoProfile", "-NonInteractive", "-Command"])
            .arg(query)
            .output();
        return output.is_ok_and(|output| {
            output.status.success()
                && String::from_utf8_lossy(&output.stdout)
                    .trim()
                    .parse::<u32>()
                    .is_ok_and(|found| found == process_id)
        });
    }
    #[cfg(not(windows))]
    {
        Command::new("kill")
            .args(["-0", &process_id.to_string()])
            .status()
            .is_ok_and(|status| status.success())
    }
}

fn validate_request(request: &StartRequest) -> Result<(PathBuf, PathBuf, String), String> {
    if request.executable.is_empty()
        || request.executable.len() > 32_767
        || request.executable.chars().any(char::is_control)
        || request.executable.contains('\0')
    {
        return Err("Choose a valid executable path or command name.".into());
    }
    if request.arguments.len() > 256
        || request
            .arguments
            .iter()
            .any(|arg| arg.contains('\0') || arg.len() > 32_767)
        || request.arguments.iter().map(String::len).sum::<usize>() > MAX_COMMAND_BYTES
    {
        return Err("The argument array is invalid or exceeds its size limit.".into());
    }
    if request.timeout_seconds == 0 || request.timeout_seconds > MAX_TIMEOUT_SECS {
        return Err("Timeout must be between 1 second and 24 hours.".into());
    }
    let cwd = Path::new(request.cwd.trim())
        .canonicalize()
        .map_err(|_| "The working directory could not be resolved.".to_string())?;
    if !cwd.is_dir() {
        return Err("The working directory must be an existing folder.".into());
    }

    let entered = PathBuf::from(request.executable.trim());
    let executable = if entered.components().count() > 1 || entered.is_absolute() {
        let absolute = if entered.is_absolute() {
            entered
        } else {
            cwd.join(entered)
        };
        absolute
            .canonicalize()
            .map_err(|_| "The executable could not be resolved.".to_string())?
    } else {
        PathBuf::from(request.executable.trim())
    };
    if executable.components().count() > 1 && !executable.is_file() {
        return Err("The executable must be an existing file.".into());
    }
    #[cfg(windows)]
    {
        let command_units = executable
            .as_os_str()
            .to_string_lossy()
            .encode_utf16()
            .count()
            + request
                .arguments
                .iter()
                .map(|argument| argument.encode_utf16().count() + 2)
                .sum::<usize>()
            + 32;
        if command_units > 32_767 {
            return Err(
                "The executable and arguments exceed the Windows command-line limit.".into(),
            );
        }
    }

    let suffix = executable
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or("");
    #[cfg(windows)]
    if suffix.eq_ignore_ascii_case("ps1") {
        return Ok((executable, cwd, "powershell".into()));
    }
    #[cfg(windows)]
    if suffix.eq_ignore_ascii_case("cmd") || suffix.eq_ignore_ascii_case("bat") {
        if executable
            .to_string_lossy()
            .chars()
            .any(|c| "&|<>^%!\"\r\n".contains(c))
            || request
                .arguments
                .iter()
                .any(|arg| arg.chars().any(|c| "&|<>^%!\"\r\n".contains(c)))
        {
            return Err("CMD arguments containing shell metacharacters are not supported.".into());
        }
        return Ok((executable, cwd, "cmd".into()));
    }
    let _ = suffix;
    Ok((executable, cwd, "native".into()))
}

fn mutation_scope(cwd: &Path) -> String {
    cwd.ancestors()
        .find(|folder| folder.join(".git").exists())
        .unwrap_or(cwd)
        .to_string_lossy()
        .to_lowercase()
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct GitWorktreeSnapshot {
    root: PathBuf,
    changed_paths: Vec<String>,
    fingerprint: String,
}

fn git_output(root: &Path, args: &[&str]) -> Result<std::process::Output, String> {
    Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .output()
        .map_err(|_| "Could not verify the Git worktree for this handoff.".to_string())
}

fn git_worktree_snapshot(cwd: &Path) -> Result<Option<GitWorktreeSnapshot>, String> {
    let Some(root) = cwd.ancestors().find(|folder| folder.join(".git").exists()) else {
        return Ok(None);
    };
    let canonical_root = root
        .canonicalize()
        .map_err(|_| "The Git worktree root could not be resolved.".to_string())?;
    let status = git_output(
        root,
        &["status", "--porcelain=v1", "-z", "--untracked-files=all"],
    )?;
    if !status.status.success() {
        return Err("Could not verify Git status for this handoff.".into());
    }
    let mut changed_paths = Vec::new();
    for entry in status
        .stdout
        .split(|byte| *byte == 0)
        .filter(|entry| !entry.is_empty())
    {
        if entry.len() < 4 {
            return Err("Git returned an unreadable status for this handoff.".into());
        }
        if entry[..2].contains(&b'R') || entry[..2].contains(&b'C') {
            return Err(
                "Renamed or copied files need a clean worktree before a provider handoff.".into(),
            );
        }
        let path = std::str::from_utf8(&entry[3..])
            .map_err(|_| "Git returned a path that cannot be safely scoped.".to_string())?
            .replace('\\', "/");
        changed_paths.push(path);
    }
    changed_paths.sort_by_key(|path| path.to_lowercase());

    let cached_diff = git_output(root, &["diff", "--binary", "--cached"])?;
    let working_diff = git_output(root, &["diff", "--binary"])?;
    if !cached_diff.status.success() || !working_diff.status.success() {
        return Err("Could not capture the tracked Git diff for this handoff.".into());
    }
    let untracked = git_output(root, &["ls-files", "--others", "--exclude-standard", "-z"])?;
    if !untracked.status.success() {
        return Err("Could not capture untracked files for this handoff.".into());
    }

    let mut snapshot = status.stdout;
    snapshot.extend_from_slice(&cached_diff.stdout);
    snapshot.extend_from_slice(&working_diff.stdout);
    for path_bytes in untracked
        .stdout
        .split(|byte| *byte == 0)
        .filter(|path| !path.is_empty())
    {
        let path = std::str::from_utf8(path_bytes)
            .map_err(|_| "Git returned a path that cannot be safely scoped.".to_string())?;
        let full_path = root.join(path);
        let canonical = full_path
            .canonicalize()
            .map_err(|_| "An untracked handoff file could not be resolved.".to_string())?;
        if !canonical.starts_with(&canonical_root) || !canonical.is_file() {
            return Err(
                "Untracked handoff files must be regular files inside the selected worktree."
                    .into(),
            );
        }
        let blob = git_output(root, &["hash-object", "--", path])?;
        if !blob.status.success() {
            return Err("Could not fingerprint an untracked handoff file.".into());
        }
        snapshot.extend_from_slice(path.as_bytes());
        snapshot.push(0);
        snapshot.extend_from_slice(&blob.stdout);
    }

    let mut hash = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["hash-object", "--stdin"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .map_err(|_| "Could not fingerprint the Git worktree for this handoff.".to_string())?;
    hash.stdin
        .take()
        .ok_or("Could not fingerprint the Git worktree for this handoff.")?
        .write_all(&snapshot)
        .map_err(|_| "Could not fingerprint the Git worktree for this handoff.".to_string())?;
    let fingerprint = hash
        .wait_with_output()
        .map_err(|_| "Could not fingerprint the Git worktree for this handoff.".to_string())?;
    if !fingerprint.status.success() {
        return Err("Could not fingerprint the Git worktree for this handoff.".into());
    }
    let fingerprint = String::from_utf8_lossy(&fingerprint.stdout)
        .trim()
        .to_owned();
    if fingerprint.is_empty() {
        return Err("Could not fingerprint the Git worktree for this handoff.".into());
    }
    Ok(Some(GitWorktreeSnapshot {
        root: canonical_root,
        changed_paths,
        fingerprint,
    }))
}

fn ensure_handoff_worktree(cwd: &Path, handoff: &ProviderHandoff) -> Result<(), String> {
    if handoff.working_tree_root.is_empty() {
        return ensure_clean_git_worktree(cwd);
    }
    let expected_root = handoff.working_tree_root.to_lowercase();
    let current = git_worktree_snapshot(cwd)?;
    let current_root = current
        .as_ref()
        .map(|snapshot| snapshot.root.to_string_lossy().to_lowercase())
        .unwrap_or_else(|| cwd.to_string_lossy().to_lowercase());
    if current_root != expected_root {
        return Err("This handoff belongs to a different project worktree.".into());
    }
    match current {
        Some(snapshot)
            if snapshot.changed_paths == handoff.working_tree_paths
                && handoff.working_tree_fingerprint.as_deref()
                    == Some(snapshot.fingerprint.as_str()) =>
        {
            Ok(())
        }
        None if handoff.working_tree_paths.is_empty()
            && handoff.working_tree_fingerprint.is_none() =>
        {
            Ok(())
        }
        _ => Err("The project worktree changed after this handoff was saved. Review the changes and prepare a new handoff.".into()),
    }
}

fn interrupted_process_owns_scope(records: &[RunRecord], canonical_key: &str) -> bool {
    records.iter().any(|record| {
        record.mutating
            && record.state == "interrupted"
            && record.process_id.is_some_and(process_is_running)
            && mutation_scope(Path::new(&record.cwd)) == canonical_key
    })
}

fn build_result_is_successful(state: &str, exit_code: Option<i32>) -> bool {
    state == "completed" && exit_code.is_none_or(|code| code == 0)
}

fn release_failed_checkpoint(state: &mut RunStore, id: &str, canonical_key: &str) {
    state.mutating_paths.remove(canonical_key);
    state.redaction_values.remove(id);
}

pub fn ensure_clean_git_worktree(cwd: &Path) -> Result<(), String> {
    ensure_git_scope(cwd, &[])
}

fn ensure_git_scope(cwd: &Path, allowed_dirty_paths: &[String]) -> Result<(), String> {
    let Some(repo_root) = cwd.ancestors().find(|folder| folder.join(".git").exists()) else {
        return Ok(());
    };
    let status = Command::new("git")
        .arg("-C")
        .arg(repo_root)
        .args(["status", "--porcelain=v1", "-z", "--untracked-files=all"])
        .output()
        .map_err(|_| "Could not verify Git status; Codex was not started.".to_string())?;
    if !status.status.success() {
        return Err("Could not verify Git status; Codex was not started.".into());
    }
    if status.stdout.is_empty() {
        return Ok(());
    }
    if allowed_dirty_paths.is_empty() {
        return Err("Codex requires a clean Git worktree to protect existing changes. Commit or stash your changes, or select a clean worktree.".into());
    }
    let selected = allowed_dirty_paths
        .iter()
        .map(|path| path.replace('\\', "/").to_lowercase())
        .collect::<HashSet<_>>();
    if selected.len() != allowed_dirty_paths.len() {
        return Err("Select each changed file only once.".into());
    }
    let mut actual = HashSet::new();
    for entry in status
        .stdout
        .split(|byte| *byte == 0)
        .filter(|entry| !entry.is_empty())
    {
        if entry.len() < 4 {
            return Err("Git returned an unreadable status; Codex was not started.".into());
        }
        if entry[..2].contains(&b'R') || entry[..2].contains(&b'C') {
            return Err(
                "Renamed or copied files need a clean worktree before this workflow can start."
                    .into(),
            );
        }
        let path = std::str::from_utf8(&entry[3..])
            .map_err(|_| "Git returned a path that cannot be safely scoped.".to_string())?
            .replace('\\', "/");
        actual.insert(path.to_lowercase());
    }
    if actual != selected {
        return Err("The selected paths must exactly match every changed file in the repository. Refresh the list and review your selection.".into());
    }
    Ok(())
}

fn validate_workflow_metadata(workflow: &WorkflowRunMetadata) -> Result<(), String> {
    if !matches!(
        workflow.action.as_str(),
        "dev-work" | "dev-push" | "dev-build"
    ) || workflow.operation_key.len() != 64
        || !workflow
            .operation_key
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit())
        || workflow.scope_paths.len() > 200
        || workflow.requested_effects.len() > 3
    {
        return Err("The workflow action or operation identity is invalid.".into());
    }
    if workflow.action == "dev-work" && workflow.issue_url.as_deref().is_none_or(str::is_empty) {
        return Err("Work on issue requires a selected issue URL.".into());
    }
    if workflow.action == "dev-build"
        && (!workflow.requested_effects.is_empty()
            || !matches!(workflow.build_profile.as_deref(), Some("release" | "debug")))
    {
        return Err("Build actions require a release or debug profile and cannot request repository side effects.".into());
    }
    if workflow.action != "dev-build" && workflow.build_profile.is_some() {
        return Err("Build profiles are valid only for dev-build actions.".into());
    }
    if workflow.intent.trim().is_empty()
        || workflow.intent.len() > 8_000
        || workflow.intent.contains('\0')
    {
        return Err("Describe an intentional workflow scope of at most 8,000 characters.".into());
    }
    if workflow
        .issue_url
        .as_ref()
        .is_some_and(|url| url.len() > 2048 || !url.starts_with("https://github.com/"))
    {
        return Err("Issue context must use a GitHub.com HTTPS issue URL.".into());
    }
    if workflow.action == "dev-push" && workflow.scope_paths.is_empty() {
        return Err("Push changes requires at least one selected changed file.".into());
    }
    let mut paths = HashSet::new();
    let mut scope_bytes = 0usize;
    for path in &workflow.scope_paths {
        let parsed = Path::new(path);
        scope_bytes = scope_bytes.saturating_add(path.len());
        if path.trim().is_empty()
            || path.len() > 500
            || scope_bytes > 10_000
            || path.chars().any(char::is_control)
            || parsed.is_absolute()
            || parsed.components().any(|part| {
                matches!(
                    part,
                    std::path::Component::ParentDir
                        | std::path::Component::CurDir
                        | std::path::Component::Prefix(_)
                        | std::path::Component::RootDir
                )
            })
            || !paths.insert(path.replace('\\', "/").to_lowercase())
        {
            return Err("Change scope must use unique repository-relative file paths.".into());
        }
    }
    let mut effects = HashSet::new();
    for effect in &workflow.requested_effects {
        if !matches!(effect.as_str(), "commit" | "push" | "createPullRequest")
            || !effects.insert(effect.as_str())
        {
            return Err("The requested workflow effects are invalid.".into());
        }
    }
    if effects.contains("createPullRequest") && !effects.contains("push") {
        return Err("Creating a pull request requires explicit permission to push.".into());
    }
    Ok(())
}

fn workflow_key_recorded(records: &[RunRecord], operation_key: &str) -> bool {
    records.iter().any(|record| {
        record
            .workflow
            .as_ref()
            .is_some_and(|workflow| workflow.operation_key == operation_key)
    })
}

fn github_repository_from_remote(remote: &str) -> Option<String> {
    let trimmed = remote.trim().trim_end_matches(".git");
    let path = trimmed
        .strip_prefix("https://github.com/")
        .or_else(|| trimmed.strip_prefix("ssh://git@github.com/"))
        .or_else(|| trimmed.strip_prefix("git@github.com:"))?;
    let parts = path.trim_matches('/').split('/').collect::<Vec<_>>();
    (parts.len() == 2
        && parts
            .iter()
            .all(|part| !part.is_empty() && !part.chars().any(char::is_control)))
    .then(|| format!("{}/{}", parts[0], parts[1]).to_ascii_lowercase())
}

fn verify_workflow_repository(cwd: &Path, workflow: &WorkflowRunMetadata) -> Result<(), String> {
    if workflow.action == "dev-build" {
        return Ok(());
    }
    let root = cwd
        .ancestors()
        .find(|folder| folder.join(".git").exists())
        .ok_or("Agent-backed repository workflows require a Git checkout.")?;
    let branch = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["branch", "--show-current"])
        .output()
        .map_err(|_| "Could not verify the current Git branch.".to_string())?;
    if !branch.status.success() || String::from_utf8_lossy(&branch.stdout).trim().is_empty() {
        return Err(
            "A named Git branch is required; detached HEAD workflows are unsupported.".into(),
        );
    }
    let remotes = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["remote", "-v"])
        .output()
        .map_err(|_| "Could not verify configured Git remotes.".to_string())?;
    if !remotes.status.success() {
        return Err("Could not verify configured Git remotes.".into());
    }
    let repositories = String::from_utf8_lossy(&remotes.stdout)
        .lines()
        .filter_map(|line| line.split_whitespace().nth(1))
        .filter_map(github_repository_from_remote)
        .collect::<HashSet<_>>();
    if repositories.is_empty() {
        return Err(
            "Agent-backed issue and push workflows require a configured GitHub remote.".into(),
        );
    }
    if let Some(url) = &workflow.issue_url {
        let path = url.strip_prefix("https://github.com/").unwrap_or_default();
        let parts = path.split('/').collect::<Vec<_>>();
        if parts.len() != 4
            || parts[2] != "issues"
            || parts[3].parse::<u64>().is_err()
            || !repositories.contains(&format!("{}/{}", parts[0], parts[1]).to_ascii_lowercase())
        {
            return Err(
                "The selected issue URL does not match a GitHub remote in this repository.".into(),
            );
        }
    }
    Ok(())
}

fn command_for(
    executable: &Path,
    cwd: &Path,
    host: &str,
    args: &[String],
) -> Result<Command, String> {
    let mut command = match host {
        "powershell" => {
            let mut command = Command::new("powershell.exe");
            command.args([
                "-NoLogo",
                "-NoProfile",
                "-NonInteractive",
                "-ExecutionPolicy",
                "Bypass",
                "-File",
            ]);
            command.arg(executable);
            command.args(args);
            command
        }
        "cmd" => {
            let mut command = Command::new("cmd.exe");
            command.args(["/d", "/v:off", "/s", "/c"]);
            let mut line = format!("\"\"{}\"", executable.display());
            for arg in args {
                line.push(' ');
                line.push('"');
                line.push_str(arg);
                line.push('"');
            }
            line.push('"');
            #[cfg(windows)]
            {
                use std::os::windows::process::CommandExt;
                command.raw_arg(&line);
            }
            command
        }
        _ => {
            let mut command = Command::new(executable);
            command.args(args);
            command
        }
    };
    command
        .current_dir(cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    Ok(command)
}

fn run_readonly_command(
    executable: &Path,
    cwd: &Path,
    args: &[String],
    failure_message: &str,
) -> Result<(std::process::ExitStatus, Vec<u8>), String> {
    let request = StartRequest {
        executable: executable.to_string_lossy().into_owned(),
        arguments: args.to_vec(),
        cwd: cwd.to_string_lossy().into_owned(),
        timeout_seconds: 10,
        mutating: false,
    };
    let (executable, cwd, host) = validate_request(&request)?;
    let mut command = command_for(&executable, &cwd, &host, args)?;
    command.stdin(Stdio::null()).stderr(Stdio::null());
    command.stdout(Stdio::piped());
    let mut child = command.spawn().map_err(|_| failure_message.to_owned())?;
    let mut stdout = child
        .stdout
        .take()
        .ok_or_else(|| failure_message.to_owned())?;
    let reader = thread::spawn(move || {
        let mut output = Vec::new();
        let _ = stdout.by_ref().take(64 * 1024).read_to_end(&mut output);
        output
    });
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                let output = reader.join().map_err(|_| failure_message.to_owned())?;
                return Ok((status, output));
            }
            Ok(None) if Instant::now() < deadline => {
                thread::sleep(Duration::from_millis(50));
            }
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                let _ = reader.join();
                return Err(failure_message.to_owned());
            }
            Err(_) => {
                let _ = child.kill();
                let _ = child.wait();
                let _ = reader.join();
                return Err(failure_message.to_owned());
            }
        }
    }
}

fn preflight_command(
    executable: &Path,
    cwd: &Path,
    args: &[String],
    failure_message: &str,
) -> Result<(), String> {
    let (status, _) = run_readonly_command(executable, cwd, args, failure_message)?;
    status
        .success()
        .then_some(())
        .ok_or_else(|| failure_message.to_owned())
}

pub fn preflight_agent_workflow(
    codex_executable: &Path,
    cwd: &Path,
    workflow: &WorkflowRunMetadata,
) -> Result<(), String> {
    preflight_command(
        codex_executable,
        cwd,
        &["login".into(), "status".into()],
        "Codex authentication is unavailable or expired; the workflow was not started.",
    )?;

    if workflow
        .requested_effects
        .iter()
        .any(|effect| effect == "push")
    {
        let gh = crate::providers::find_executable("gh", std::env::var_os("PATH").as_deref())
            .ok_or("GitHub CLI is required for push or pull-request permission.")?;
        preflight_command(
            &gh,
            cwd,
            &[
                "auth".into(),
                "status".into(),
                "--hostname".into(),
                "github.com".into(),
            ],
            "GitHub CLI authentication is unavailable; the workflow was not started.",
        )?;
    }
    Ok(())
}

fn command_output(cwd: &Path, program: &str, args: &[String]) -> Result<String, String> {
    let (status, output) = run_readonly_command(
        Path::new(program),
        cwd,
        args,
        "Could not verify the workflow's reported repository side effects.",
    )?;
    if !status.success() {
        return Err("Could not verify the workflow's reported repository side effects.".into());
    }
    String::from_utf8(output).map_err(|_| "Verification returned unreadable command output.".into())
}

fn verify_reported_side_effects(
    cwd: &Path,
    workflow: &WorkflowRunMetadata,
    effects: &WorkflowSideEffects,
) -> Result<(), String> {
    let permits = |effect: &str| workflow.requested_effects.iter().any(|item| item == effect);
    if effects.commit_sha.is_some() && !permits("commit") {
        return Err("The run reported a commit that was not explicitly permitted.".into());
    }
    if effects.remote_branch.is_some() && !permits("push") {
        return Err("The run reported a push that was not explicitly permitted.".into());
    }
    if effects.pull_request_url.is_some() && !permits("createPullRequest") {
        return Err("The run reported a pull request that was not explicitly permitted.".into());
    }
    if permits("commit") && effects.commit_sha.is_none() {
        return Err("The run requested a commit but did not report its commit SHA.".into());
    }

    if let Some(commit_sha) = effects.commit_sha.as_deref() {
        command_output(
            cwd,
            "git",
            &[
                "-C".into(),
                cwd.to_string_lossy().into_owned(),
                "cat-file".into(),
                "-e".into(),
                format!("{commit_sha}^{{commit}}"),
            ],
        )?;
    }

    let mut remote_sha = None;
    let mut repository = None;
    if permits("push") || permits("createPullRequest") || effects.remote_branch.is_some() {
        let remote = command_output(
            cwd,
            "git",
            &[
                "-C".into(),
                cwd.to_string_lossy().into_owned(),
                "remote".into(),
                "get-url".into(),
                "origin".into(),
            ],
        )?;
        repository = github_repository_from_remote(remote.trim());
        if repository.is_none() {
            return Err("The configured origin is not a supported GitHub remote.".into());
        }
    }

    if permits("push") {
        let branch = effects
            .remote_branch
            .as_deref()
            .ok_or("The run requested a push but did not report its remote branch.")?;
        let commit_sha = effects
            .commit_sha
            .as_deref()
            .ok_or("The run requested a push but did not report its commit SHA.")?;
        command_output(
            cwd,
            "git",
            &[
                "-C".into(),
                cwd.to_string_lossy().into_owned(),
                "check-ref-format".into(),
                "--branch".into(),
                branch.into(),
            ],
        )?;
        let listing = command_output(
            cwd,
            "git",
            &[
                "-C".into(),
                cwd.to_string_lossy().into_owned(),
                "ls-remote".into(),
                "--exit-code".into(),
                "origin".into(),
                format!("refs/heads/{branch}"),
            ],
        )?;
        let (sha, reference) = listing
            .split_whitespace()
            .collect::<Vec<_>>()
            .split_first()
            .and_then(|(sha, rest)| rest.first().map(|reference| (*sha, *reference)))
            .ok_or("The reported remote branch was not found.")?;
        if reference != format!("refs/heads/{branch}") || !sha.eq_ignore_ascii_case(commit_sha) {
            return Err("The remote branch does not point to the reported commit.".into());
        }
        remote_sha = Some(sha.to_owned());
    }

    if permits("createPullRequest") {
        let url = effects
            .pull_request_url
            .as_deref()
            .ok_or("The run requested a pull request but did not report its URL.")?;
        let parts = url
            .strip_prefix("https://github.com/")
            .unwrap_or_default()
            .split('/')
            .collect::<Vec<_>>();
        let expected_repository = repository
            .as_deref()
            .ok_or("The GitHub origin could not be verified.")?;
        if parts.len() != 4
            || format!("{}/{}", parts[0], parts[1]).to_ascii_lowercase() != expected_repository
            || parts[2] != "pull"
            || parts[3].parse::<u64>().is_err()
        {
            return Err("The reported pull request URL does not match the GitHub origin.".into());
        }
        let gh = crate::providers::find_executable("gh", std::env::var_os("PATH").as_deref())
            .ok_or("GitHub CLI is required to verify the pull request URL.")?;
        let (status, output) = run_readonly_command(
            &gh,
            cwd,
            &[
                "pr".into(),
                "view".into(),
                url.into(),
                "--json".into(),
                "url,headRefName,headRefOid".into(),
            ],
            "GitHub could not verify the reported pull request URL.",
        )?;
        if !status.success() {
            return Err("GitHub could not verify the reported pull request URL.".into());
        }
        let pull_request: serde_json::Value = serde_json::from_slice(&output).map_err(|_| {
            "GitHub returned unreadable pull request verification data.".to_string()
        })?;
        let branch = effects.remote_branch.as_deref().unwrap_or_default();
        let commit_sha = effects.commit_sha.as_deref().unwrap_or_default();
        let head_matches = pull_request
            .get("headRefName")
            .and_then(serde_json::Value::as_str)
            == Some(branch)
            && pull_request
                .get("headRefOid")
                .and_then(serde_json::Value::as_str)
                .is_some_and(|sha| {
                    remote_sha
                        .as_deref()
                        .unwrap_or(commit_sha)
                        .eq_ignore_ascii_case(sha)
                });
        if pull_request.get("url").and_then(serde_json::Value::as_str) != Some(url) || !head_matches
        {
            return Err(
                "The pull request does not point to the reported remote branch and commit.".into(),
            );
        }
    }

    Ok(())
}

fn verify_run_side_effects(app: &AppHandle, path: &Path, run_id: &str) {
    let (cwd, workflow, mut effects) = match store().lock().ok().and_then(|state| {
        state
            .records
            .iter()
            .find(|record| record.id == run_id)
            .and_then(|record| {
                let workflow = record.workflow.clone()?;
                let effects = workflow.side_effects.clone()?;
                Some((PathBuf::from(&record.cwd), workflow, effects))
            })
    }) {
        Some(value) => value,
        None => return,
    };
    if effects.verified || effects.verification_message.is_some() {
        return;
    }
    match verify_reported_side_effects(&cwd, &workflow, &effects) {
        Ok(()) => {
            effects.verified = true;
            effects.verification_message =
                Some("All reported side effects were independently verified.".into());
        }
        Err(error) => effects.verification_message = Some(error),
    }
    if let Ok(mut state) = store().lock() {
        if let Some(record) = state.records.iter_mut().find(|record| record.id == run_id) {
            if let Some(workflow) = record.workflow.as_mut() {
                workflow.side_effects = Some(effects);
            }
            let snapshot = record.clone();
            let _ = persist_records(path, &state.records);
            let _ = app.emit("workflow-run-updated", snapshot);
        }
    }
}

pub fn history_path(app: &AppHandle) -> Result<PathBuf, String> {
    let directory = app
        .path()
        .app_data_dir()
        .map_err(|_| "Could not locate run history storage.".to_string())?;
    fs::create_dir_all(&directory)
        .map_err(|_| "Could not create run history storage.".to_string())?;
    Ok(directory.join(HISTORY_FILE))
}

fn persist_records(path: &Path, records: &[RunRecord]) -> Result<(), String> {
    let mut connection =
        Connection::open(path).map_err(|_| "Could not open run history.".to_string())?;
    connection
        .busy_timeout(Duration::from_secs(2))
        .map_err(|_| "Could not configure run history.".to_string())?;
    connection
        .execute_batch(
            "CREATE TABLE IF NOT EXISTS workflow_runs (
            id TEXT PRIMARY KEY NOT NULL,
            started_at_ms INTEGER NOT NULL,
            record_json TEXT NOT NULL
        );",
        )
        .map_err(|_| "Could not initialize run history.".to_string())?;
    let transaction = connection
        .transaction()
        .map_err(|_| "Could not begin run history update.".to_string())?;
    for record in records {
        let json = serde_json::to_string(record)
            .map_err(|_| "Could not encode run history.".to_string())?;
        transaction.execute(
            "INSERT OR REPLACE INTO workflow_runs (id, started_at_ms, record_json) VALUES (?1, ?2, ?3)",
            params![record.id, record.started_at_ms as i64, json],
        ).map_err(|_| "Could not save run history.".to_string())?;
    }
    transaction.execute(
        "DELETE FROM workflow_runs WHERE id NOT IN (SELECT id FROM workflow_runs ORDER BY started_at_ms DESC LIMIT ?1)",
        [MAX_RUNS as i64],
    ).map_err(|_| "Could not trim run history.".to_string())?;
    transaction
        .commit()
        .map_err(|_| "Could not commit run history.".to_string())?;
    Ok(())
}

fn load_records(path: &Path) -> Result<Vec<RunRecord>, String> {
    if !path.exists() {
        return Ok(Vec::new());
    }
    let connection =
        Connection::open(path).map_err(|_| "Could not open run history.".to_string())?;
    connection
        .busy_timeout(Duration::from_secs(2))
        .map_err(|_| "Could not configure run history.".to_string())?;
    let mut statement = connection
        .prepare("SELECT record_json FROM workflow_runs ORDER BY started_at_ms DESC LIMIT ?1")
        .map_err(|_| "Could not read run history.".to_string())?;
    let rows = statement
        .query_map([MAX_RUNS as i64], |row| row.get::<_, String>(0))
        .map_err(|_| "Could not read run history.".to_string())?;
    let mut records = Vec::new();
    for row in rows {
        let json = row.map_err(|_| "Could not read run history row.".to_string())?;
        records.push(
            serde_json::from_str::<RunRecord>(&json)
                .map_err(|_| "Run history contains a malformed record.".to_string())?,
        );
    }
    let mut recovered = false;
    for record in &mut records {
        if matches!(
            record.state.as_str(),
            "queued" | "running" | "needsInput" | "cancelling"
        ) {
            record.state = "interrupted".into();
            record.finished_at_ms = Some(now_ms());
            record.latency_ms = None;
            recovered = true;
        }
        if record
            .process_id
            .is_some_and(|process_id| !process_is_running(process_id))
        {
            record.process_id = None;
            recovered = true;
        }
    }
    records.truncate(MAX_RUNS);
    if recovered {
        persist_records(path, &records)?;
    }
    Ok(records)
}

fn redact(line: &str, secrets: &[String]) -> String {
    let mut value = line.to_owned();
    for secret in secrets.iter().filter(|secret| secret.len() >= 4) {
        value = value.replace(secret, "[REDACTED]");
    }
    for marker in ["ghp_", "gho_", "ghu_", "ghs_", "github_pat_"] {
        let mut offset = 0;
        while let Some(index) = value[offset..].find(marker) {
            let start = offset + index;
            let end = value[start..]
                .find(char::is_whitespace)
                .map(|n| start + n)
                .unwrap_or(value.len());
            value.replace_range(start..end, "[REDACTED]");
            offset = start + 10;
            if offset >= value.len() {
                break;
            }
        }
    }
    for label in [
        "bearer ",
        "token=",
        "token:",
        "password=",
        "password:",
        "secret=",
        "secret:",
        "api_key=",
        "api_key:",
        "apikey=",
        "authorization:",
    ] {
        let mut offset = 0;
        while let Some(found) = value[offset..].to_ascii_lowercase().find(label) {
            let start = offset + found + label.len();
            let end = value[start..]
                .find(char::is_whitespace)
                .map(|index| start + index)
                .unwrap_or(value.len());
            if start < end {
                value.replace_range(start..end, "[REDACTED]");
            }
            offset = start.saturating_add(10);
            if offset >= value.len() {
                break;
            }
        }
    }
    value
}

fn emit_event(app: &AppHandle, path: &Path, id: &str, kind: &str, text: &str) {
    let mut store = match store().lock() {
        Ok(value) => value,
        Err(_) => return,
    };
    if !store.records.iter().any(|record| record.id == id) {
        return;
    }
    let uses_output_budget = output_budget_applies(kind);
    if uses_output_budget && *store.output_bytes.get(id).unwrap_or(&0) >= MAX_OUTPUT_BYTES {
        return;
    }
    let secrets = store.redaction_values.get(id).cloned().unwrap_or_default();
    let safe_text = redact(text, &secrets);
    let output_bytes = store.output_bytes.entry(id.to_owned()).or_default();
    let accepted = if uses_output_budget {
        safe_text
            .len()
            .min(MAX_OUTPUT_BYTES.saturating_sub(*output_bytes))
    } else {
        safe_text.len()
    };
    let truncated = accepted < safe_text.len();
    let mut end = accepted;
    while !safe_text.is_char_boundary(end) {
        end -= 1;
    }
    let text = safe_text[..end].to_owned();
    if uses_output_budget {
        *output_bytes += accepted;
    }
    let run_id = id.to_owned();
    let Some(record) = store.records.iter_mut().find(|record| record.id == id) else {
        return;
    };
    let event = RunEvent {
        sequence: record.next_sequence,
        timestamp_ms: now_ms(),
        kind: kind.to_owned(),
        text,
    };
    record.events.push(event.clone());
    let artifact = record
        .workflow
        .as_ref()
        .and_then(|workflow| verify_build_artifact(Path::new(&record.cwd), workflow, &event.text));
    if let (Some(workflow), Some(artifact)) = (record.workflow.as_mut(), artifact) {
        workflow.build_artifact = Some(artifact);
    }
    let combined_output = record
        .events
        .iter()
        .filter(|entry| matches!(entry.kind.as_str(), "output" | "finalOutput"))
        .map(|entry| entry.text.as_str())
        .collect::<String>();
    if let (Some(effects), Some(workflow)) = (
        parse_side_effect_marker(&combined_output),
        record.workflow.as_mut(),
    ) {
        workflow.side_effects = Some(effects);
    }
    record.next_sequence += 1;
    if truncated
        && record
            .events
            .last()
            .is_some_and(|entry| entry.kind != "outputLimit")
    {
        record.events.push(RunEvent {
            sequence: record.next_sequence,
            timestamp_ms: now_ms(),
            kind: "outputLimit".into(),
            text: "Output limit reached; additional output was discarded.".into(),
        });
        record.next_sequence += 1;
    }
    if record.events.len() > MAX_EVENTS {
        let remove = record.events.len() - MAX_EVENTS;
        record.events.drain(0..remove);
    }
    let _ = persist_records(path, &store.records);
    let _ = app.emit("workflow-run-event", (run_id, event));
}

fn emit_control_event(app: &AppHandle, path: &Path, id: &str, kind: &str, text: &str) {
    debug_assert!(matches!(kind, "approvalRequest" | "userInputRequest"));
    emit_event(app, path, id, kind, text);
}

fn output_budget_applies(kind: &str) -> bool {
    !matches!(kind, "approvalRequest" | "userInputRequest")
}

fn set_state(app: &AppHandle, path: &Path, id: &str, state: &str, exit_code: Option<i32>) {
    let mut store = match store().lock() {
        Ok(value) => value,
        Err(_) => return,
    };
    if let Some(record) = store.records.iter_mut().find(|record| record.id == id) {
        record.state = state.into();
        record.exit_code = exit_code;
        if !build_result_is_successful(state, exit_code) {
            if let Some(artifact) = record
                .workflow
                .as_mut()
                .and_then(|workflow| workflow.build_artifact.as_mut())
            {
                artifact.verified = false;
                artifact.verification_message =
                    "The build run did not complete successfully; this receipt is not a verified result.".into();
            }
        }
        if matches!(
            state,
            "completed" | "failed" | "cancelled" | "timedOut" | "interrupted"
        ) {
            record.process_id = None;
            if record.finished_at_ms.is_none() {
                record.finished_at_ms = Some(now_ms());
                record.latency_ms = record
                    .finished_at_ms
                    .map(|finished| crate::usage::latency_ms(record.started_at_ms, finished));
            }
        }
        let snapshot = record.clone();
        let _ = persist_records(path, &store.records);
        let _ = app.emit("workflow-run-updated", snapshot);
    }
}

fn pump_output(
    app: AppHandle,
    path: PathBuf,
    id: String,
    kind: &'static str,
    reader: impl std::io::Read + Send + 'static,
) {
    thread::spawn(move || {
        let mut reader = BufReader::new(reader);
        let mut buffer = [0u8; MAX_OUTPUT_CHUNK];
        let mut pending = Vec::with_capacity(MAX_OUTPUT_CHUNK);
        loop {
            match reader.read(&mut buffer) {
                Ok(0) => {
                    if !pending.is_empty() {
                        emit_event(&app, &path, &id, kind, &String::from_utf8_lossy(&pending));
                    }
                    break;
                }
                Ok(length) => {
                    for byte in &buffer[..length] {
                        if *byte == b'\n' {
                            if pending.last() == Some(&b'\r') {
                                pending.pop();
                            }
                            emit_event(&app, &path, &id, kind, &String::from_utf8_lossy(&pending));
                            pending.clear();
                        } else {
                            pending.push(*byte);
                            if pending.len() == MAX_OUTPUT_CHUNK {
                                emit_event(
                                    &app,
                                    &path,
                                    &id,
                                    kind,
                                    &String::from_utf8_lossy(&pending),
                                );
                                pending.clear();
                            }
                        }
                    }
                }
                Err(_) => {
                    emit_event(&app, &path, &id, "system", "Could not read process output.");
                    break;
                }
            }
        }
    });
}

fn pump_codex_stderr(
    app: AppHandle,
    path: PathBuf,
    id: String,
    child: Arc<Mutex<Child>>,
    reader: impl Read + Send + 'static,
) {
    thread::spawn(move || {
        let mut reader = BufReader::new(reader);
        let mut line = String::new();
        loop {
            line.clear();
            match reader.read_line(&mut line) {
                Ok(0) => break,
                Ok(_) => {
                    if let Some(event) = crate::providers::normalize_codex_stderr(&line) {
                        finish_codex_run(&app, &path, &id, "failed", &event.kind, &event.text);
                        if let Ok(mut child) = child.lock() {
                            terminate_process_tree(&mut child);
                        }
                    } else {
                        emit_event(&app, &path, &id, "stderr", line.trim_end());
                    }
                }
                Err(_) => {
                    emit_event(
                        &app,
                        &path,
                        &id,
                        "system",
                        "Could not read Codex app-server diagnostics.",
                    );
                    break;
                }
            }
        }
    });
}

fn start_codex_protocol(
    app: AppHandle,
    path: PathBuf,
    id: String,
    child: Arc<Mutex<Child>>,
    writer: Arc<Mutex<ChildStdin>>,
    stdout: ChildStdout,
    config: CodexRunConfig,
) {
    let (sender, receiver) = mpsc::channel::<Result<String, String>>();
    thread::spawn(move || {
        let mut reader = BufReader::new(stdout);
        loop {
            let mut line = String::new();
            match reader.read_line(&mut line) {
                Ok(0) => break,
                Ok(_) if line.len() > 256 * 1024 => {
                    let _ = sender.send(Err(
                        "Codex app-server message exceeded the size limit.".into()
                    ));
                }
                Ok(_) => {
                    if sender.send(Ok(line)).is_err() {
                        break;
                    }
                }
                Err(_) => {
                    let _ = sender.send(Err("Could not read Codex app-server output.".into()));
                    break;
                }
            }
        }
    });
    thread::spawn(move || {
        drive_codex_protocol(app, path, id, child, writer, receiver, config);
    });
}

fn send_codex_message(
    writer: &Arc<Mutex<ChildStdin>>,
    message: &serde_json::Value,
) -> Result<(), String> {
    let mut writer = writer
        .lock()
        .map_err(|_| "Could not access the Codex app-server input.".to_string())?;
    serde_json::to_writer(&mut *writer, message)
        .map_err(|_| "Could not encode a Codex app-server message.".to_string())?;
    writer
        .write_all(b"\n")
        .and_then(|_| writer.flush())
        .map_err(|_| "Could not send a message to the Codex app-server.".to_string())
}

fn drive_codex_protocol(
    app: AppHandle,
    path: PathBuf,
    run_id: String,
    child: Arc<Mutex<Child>>,
    writer: Arc<Mutex<ChildStdin>>,
    receiver: Receiver<Result<String, String>>,
    config: CodexRunConfig,
) {
    let initialize = crate::providers::app_server_initialize(1, env!("CARGO_PKG_VERSION"));
    if let Err(error) = send_codex_message(&writer, &initialize) {
        finish_codex_run(&app, &path, &run_id, "failed", "providerError", &error);
        return;
    }
    let Some(initialize_result) =
        wait_for_codex_response(&app, &path, &run_id, &receiver, &child, 1)
    else {
        return;
    };
    if let Err(error) = initialize_result {
        let message = codex_rpc_error_message(error);
        finish_codex_run(&app, &path, &run_id, "failed", "providerError", &message);
        return;
    }
    if send_codex_message(&writer, &serde_json::json!({ "method": "initialized" })).is_err() {
        finish_codex_run(
            &app,
            &path,
            &run_id,
            "failed",
            "providerError",
            "Could not initialize the Codex app-server connection.",
        );
        return;
    }
    let provider_model = store()
        .lock()
        .ok()
        .and_then(|state| {
            state
                .records
                .iter()
                .find(|record| record.id == run_id)
                .map(|record| {
                    (
                        record.provider.clone().unwrap_or_else(|| "codex".into()),
                        record
                            .model_id
                            .clone()
                            .unwrap_or_else(|| config.model_id.clone()),
                    )
                })
        })
        .unwrap_or_else(|| ("codex".into(), config.model_id.clone()));
    let rate_limits_read =
        serde_json::json!({ "id": 4, "method": "account/rateLimits/read", "params": {} });
    let rate_limit_signals = if send_codex_message(&writer, &rate_limits_read).is_ok() {
        match wait_for_codex_response(&app, &path, &run_id, &receiver, &child, 4) {
            Some(Ok(result)) => crate::usage::signals_from_codex_rate_limits(
                &serde_json::json!({ "result": result }),
                &provider_model.0,
                &provider_model.1,
                now_ms(),
            ),
            Some(Err(error)) => crate::usage::signals_from_codex_rate_limits(
                &serde_json::json!({ "error": error }),
                &provider_model.0,
                &provider_model.1,
                now_ms(),
            ),
            None => return,
        }
    } else {
        crate::usage::signals_from_codex_rate_limits(
            &serde_json::json!({ "error": { "message": "app-server request unavailable" } }),
            &provider_model.0,
            &provider_model.1,
            now_ms(),
        )
    };
    let _ = crate::usage::record_signals(&path, &rate_limit_signals);
    let cwd = config.cwd.to_string_lossy().into_owned();
    let thread_start = crate::providers::app_server_thread_start(2, &cwd, &config.model_id);
    if send_codex_message(&writer, &thread_start).is_err() {
        finish_codex_run(
            &app,
            &path,
            &run_id,
            "failed",
            "providerError",
            "Could not start a scoped Codex thread.",
        );
        return;
    }
    let Some(thread_result) = wait_for_codex_response(&app, &path, &run_id, &receiver, &child, 2)
    else {
        return;
    };
    let thread_id = match thread_result {
        Ok(value) => value
            .get("thread")
            .and_then(|thread| thread.get("id"))
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned),
        Err(error) => {
            let message = codex_rpc_error_message(error);
            finish_codex_run(&app, &path, &run_id, "failed", "providerError", &message);
            return;
        }
    };
    let Some(thread_id) = thread_id else {
        finish_codex_run(
            &app,
            &path,
            &run_id,
            "failed",
            "providerError",
            "Codex did not return a thread identifier.",
        );
        return;
    };
    set_codex_thread_id(&app, &path, &run_id, &thread_id);
    let skill = config
        .skill_id
        .as_deref()
        .zip(config.skill_path.as_deref())
        .map(|(name, path)| (name, path.to_string_lossy()));
    let skill = skill.as_ref().map(|(name, path)| (*name, path.as_ref()));
    let turn_start = crate::providers::app_server_turn_start(
        3,
        &thread_id,
        &cwd,
        &config.model_id,
        config.reasoning_effort.as_deref(),
        &config.prompt,
        skill,
    );
    if send_codex_message(&writer, &turn_start).is_err() {
        finish_codex_run(
            &app,
            &path,
            &run_id,
            "failed",
            "providerError",
            "Could not start the Codex turn.",
        );
        return;
    }
    if let Some(turn_result) = wait_for_codex_response(&app, &path, &run_id, &receiver, &child, 3) {
        if let Err(error) = turn_result {
            let message = codex_rpc_error_message(error);
            finish_codex_run(&app, &path, &run_id, "failed", "providerError", &message);
            return;
        }
    } else {
        return;
    }
    loop {
        match receiver.recv_timeout(POLL_INTERVAL) {
            Ok(Ok(line)) => match crate::providers::parse_app_server_message(&line) {
                Ok(message) => handle_codex_message(&app, &path, &run_id, &child, &message),
                Err(error) => emit_event(&app, &path, &run_id, "providerError", &error),
            },
            Ok(Err(error)) => emit_event(&app, &path, &run_id, "providerError", &error),
            Err(RecvTimeoutError::Timeout) => {
                let exited = child
                    .lock()
                    .ok()
                    .and_then(|mut child| child.try_wait().ok())
                    .flatten()
                    .is_some();
                if exited {
                    break;
                }
                let terminal = store()
                    .lock()
                    .map(|state| state.adapter_terminal.contains(&run_id))
                    .unwrap_or(true);
                if terminal {
                    break;
                }
            }
            Err(RecvTimeoutError::Disconnected) => break,
        }
        let terminal = store()
            .lock()
            .map(|state| state.adapter_terminal.contains(&run_id))
            .unwrap_or(true);
        if terminal {
            break;
        }
    }
    let terminal = store()
        .lock()
        .map(|state| state.adapter_terminal.contains(&run_id))
        .unwrap_or(false);
    if !terminal {
        let current_state = store().lock().ok().and_then(|state| {
            state
                .records
                .iter()
                .find(|record| record.id == run_id)
                .map(|record| record.state.clone())
        });
        if current_state.as_deref() != Some("cancelling")
            && current_state.as_deref() != Some("timedOut")
        {
            finish_codex_run(
                &app,
                &path,
                &run_id,
                "failed",
                "providerError",
                "Codex app-server ended before the turn completed.",
            );
        }
    }
}

fn wait_for_codex_response(
    app: &AppHandle,
    path: &Path,
    run_id: &str,
    receiver: &Receiver<Result<String, String>>,
    child: &Arc<Mutex<Child>>,
    request_id: u64,
) -> Option<Result<serde_json::Value, serde_json::Value>> {
    loop {
        match receiver.recv_timeout(POLL_INTERVAL) {
            Ok(Ok(line)) => match crate::providers::parse_app_server_message(&line) {
                Ok(message)
                    if message.get("id").and_then(serde_json::Value::as_u64)
                        == Some(request_id) =>
                {
                    if let Some(error) = message.get("error") {
                        return Some(Err(error.clone()));
                    }
                    return Some(Ok(message
                        .get("result")
                        .cloned()
                        .unwrap_or(serde_json::Value::Null)));
                }
                Ok(message) => handle_codex_message(app, path, run_id, child, &message),
                Err(error) => emit_event(app, path, run_id, "providerError", &error),
            },
            Ok(Err(error)) => emit_event(app, path, run_id, "providerError", &error),
            Err(RecvTimeoutError::Timeout) => {
                let exited = child
                    .lock()
                    .ok()
                    .and_then(|mut child| child.try_wait().ok())
                    .flatten()
                    .is_some();
                if exited {
                    return None;
                }
                let terminal = store()
                    .lock()
                    .map(|state| state.adapter_terminal.contains(run_id))
                    .unwrap_or(true);
                if terminal {
                    return None;
                }
            }
            Err(RecvTimeoutError::Disconnected) => return None,
        }
    }
}

fn codex_rpc_error_message(error: serde_json::Value) -> String {
    let detail = error
        .get("message")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("");
    if detail.to_ascii_lowercase().contains("auth") || detail.to_ascii_lowercase().contains("401") {
        "Codex authentication is required or expired.".into()
    } else {
        "Codex app-server rejected the request.".into()
    }
}

fn set_codex_thread_id(app: &AppHandle, path: &Path, run_id: &str, thread_id: &str) {
    if let Ok(mut state) = store().lock() {
        state
            .codex_thread_ids
            .insert(run_id.to_owned(), thread_id.to_owned());
        if let Some(record) = state.records.iter_mut().find(|record| record.id == run_id) {
            record.thread_id = Some(thread_id.to_owned());
            let snapshot = record.clone();
            let _ = persist_records(path, &state.records);
            let _ = app.emit("workflow-run-updated", snapshot);
        }
    }
}

fn handle_codex_message(
    app: &AppHandle,
    path: &Path,
    run_id: &str,
    child: &Arc<Mutex<Child>>,
    message: &serde_json::Value,
) {
    if message.get("method").is_none() {
        return;
    }
    if message.get("method").and_then(serde_json::Value::as_str)
        == Some("thread/tokenUsage/updated")
    {
        let metadata = store().lock().ok().and_then(|state| {
            state
                .records
                .iter()
                .find(|record| record.id == run_id)
                .and_then(|record| Some((record.provider.clone()?, record.model_id.clone()?)))
        });
        if let Some((provider_id, model_id)) = metadata {
            let signals = crate::usage::signals_from_codex_token_usage(
                message,
                &provider_id,
                &model_id,
                now_ms(),
            );
            let _ = crate::usage::record_signals(path, &signals);
        }
    }
    if let Some(method) = message.get("method").and_then(serde_json::Value::as_str) {
        if matches!(
            method,
            "item/commandExecution/requestApproval"
                | "item/fileChange/requestApproval"
                | "item/permissions/requestApproval"
                | "item/tool/requestUserInput"
        ) {
            let Some(event) = crate::providers::normalize_app_server_message(message) else {
                finish_codex_run(
                    app,
                    path,
                    run_id,
                    "failed",
                    "providerError",
                    "Codex sent an invalid approval or input request.",
                );
                if let Ok(mut child) = child.lock() {
                    terminate_process_tree(&mut child);
                }
                return;
            };
            if event.text.len() > MAX_CONTROL_EVENT_BYTES {
                finish_codex_run(
                    app,
                    path,
                    run_id,
                    "failed",
                    "providerError",
                    "Codex sent an approval or input request that exceeds the size limit.",
                );
                if let Ok(mut child) = child.lock() {
                    terminate_process_tree(&mut child);
                }
                return;
            }
            let Some(request_id) = message.get("id").cloned() else {
                finish_codex_run(
                    app,
                    path,
                    run_id,
                    "failed",
                    "providerError",
                    "Codex sent an approval or input request without an identifier.",
                );
                if let Ok(mut child) = child.lock() {
                    terminate_process_tree(&mut child);
                }
                return;
            };
            let Some(key) = codex_request_key(&request_id) else {
                finish_codex_run(
                    app,
                    path,
                    run_id,
                    "failed",
                    "providerError",
                    "Codex sent an unsupported approval or input request identifier.",
                );
                if let Ok(mut child) = child.lock() {
                    terminate_process_tree(&mut child);
                }
                return;
            };
            let pending_count = store()
                .lock()
                .ok()
                .and_then(|state| state.codex_requests.get(run_id).map(HashMap::len))
                .unwrap_or(0);
            if pending_count >= MAX_PENDING_CODEX_REQUESTS {
                finish_codex_run(
                    app,
                    path,
                    run_id,
                    "failed",
                    "providerError",
                    "Codex has too many unresolved approval or input requests.",
                );
                if let Ok(mut child) = child.lock() {
                    terminate_process_tree(&mut child);
                }
                return;
            }
            let params = message
                .get("params")
                .cloned()
                .unwrap_or(serde_json::Value::Null);
            if let Ok(mut state) = store().lock() {
                state
                    .codex_requests
                    .entry(run_id.to_owned())
                    .or_default()
                    .insert(
                        key,
                        PendingCodexRequest {
                            request_id,
                            method: method.to_owned(),
                            params,
                        },
                    );
            }
            set_state(app, path, run_id, "needsInput", None);
            emit_control_event(app, path, run_id, &event.kind, &event.text);
            return;
        }
    }
    let Some(event) = crate::providers::normalize_app_server_message(message) else {
        if message.get("method").and_then(serde_json::Value::as_str) == Some("turn/started") {
            emit_event(app, path, run_id, "state", "Codex turn started.");
        }
        return;
    };
    if !event.text.is_empty() {
        emit_event(app, path, run_id, &event.kind, &event.text);
    }
    if let Some(terminal_state) = event.terminal_state.as_deref() {
        finish_codex_run(app, path, run_id, terminal_state, "state", &event.text);
        if let Ok(mut child) = child.lock() {
            terminate_process_tree(&mut child);
        }
    }
}

fn finish_codex_run(
    app: &AppHandle,
    path: &Path,
    run_id: &str,
    state_name: &str,
    event_kind: &str,
    text: &str,
) {
    if let Ok(mut state) = store().lock() {
        state.adapter_terminal.insert(run_id.to_owned());
    }
    if !text.is_empty() && event_kind != "state" {
        emit_event(app, path, run_id, event_kind, text);
    }
    verify_run_side_effects(app, path, run_id);
    set_state(app, path, run_id, state_name, None);
}

fn codex_request_key(request_id: &serde_json::Value) -> Option<String> {
    request_id
        .as_str()
        .map(str::to_owned)
        .or_else(|| request_id.as_i64().map(|value| value.to_string()))
}

#[tauri::command]
pub fn list_workflow_runs(app: AppHandle) -> Result<Vec<RunRecord>, String> {
    let path = history_path(&app)?;
    let mut state = store()
        .lock()
        .map_err(|_| "Could not access workflow runs.".to_string())?;
    if state.records.is_empty() {
        state.records = load_records(&path)?;
    }
    let mut reconciled = false;
    for record in &mut state.records {
        if record.state == "interrupted"
            && record
                .process_id
                .is_some_and(|process_id| !process_is_running(process_id))
        {
            record.process_id = None;
            reconciled = true;
        }
    }
    if reconciled {
        persist_records(&path, &state.records)?;
    }
    Ok(state.records.clone())
}

#[tauri::command]
pub fn start_workflow_run(app: AppHandle, request: StartRequest) -> Result<RunRecord, String> {
    start_run(app, request, None, None, None, None, None)
}

pub fn start_codex_workflow_run(
    app: AppHandle,
    config: CodexRunConfig,
) -> Result<RunRecord, String> {
    let prompt = config
        .handoff
        .as_ref()
        .map(|packet| format!("{}{}", config.prompt, format_handoff_prompt(packet)))
        .unwrap_or_else(|| config.prompt.clone());
    if prompt.trim().is_empty() || prompt.len() > 60_000 || prompt.contains('\0') {
        return Err("Enter a prompt of 1 to 60,000 characters for Codex.".into());
    }
    if config.model_id.trim().is_empty()
        || config.model_id.len() > 160
        || config.model_id.chars().any(char::is_control)
    {
        return Err("Select a valid configured Codex model.".into());
    }
    let cwd = config
        .cwd
        .canonicalize()
        .map_err(|_| "The selected project folder could not be resolved.".to_string())?;
    if !cwd.is_dir() {
        return Err("The selected project must be an existing folder.".into());
    }
    if let Some(workflow) = &config.workflow {
        validate_workflow_metadata(workflow)?;
        if workflow.action == "dev-work" && !workflow.scope_paths.is_empty() {
            return Err("Work on issue uses a clean isolated worktree; do not pass pre-existing changes as scope.".into());
        }
        ensure_git_scope(
            &cwd,
            if workflow.action == "dev-push" {
                &workflow.scope_paths
            } else {
                &[]
            },
        )?;
        verify_workflow_repository(&cwd, workflow)?;
        let missing = crate::scanner::unavailable_workflow_prerequisites(&cwd)?;
        if !missing.is_empty() {
            return Err(format!(
                "Required project tools are unavailable: {}.",
                missing.join(", ")
            ));
        }
        preflight_agent_workflow(&config.executable, &cwd, workflow)?;
    } else if let Some(packet) = config.handoff.as_ref() {
        ensure_handoff_worktree(&cwd, packet)?;
    } else {
        ensure_clean_git_worktree(&cwd)?;
    }
    let mut config = config;
    config.prompt = prompt;
    let timeout_seconds = config.timeout_seconds;
    let request = StartRequest {
        executable: config.executable.to_string_lossy().into_owned(),
        arguments: vec!["app-server".into(), "--stdio".into()],
        cwd: cwd.to_string_lossy().into_owned(),
        timeout_seconds,
        mutating: true,
    };
    let model_id = config.model_id.clone();
    let handoff = config.handoff.clone();
    start_run(
        app,
        request,
        Some(config),
        Some("codex"),
        Some(model_id),
        handoff,
        None,
    )
}

pub fn start_antigravity_workflow_run(
    app: AppHandle,
    config: AntigravityRunConfig,
    handoff: Option<ProviderHandoff>,
) -> Result<RunRecord, String> {
    let prompt = handoff
        .as_ref()
        .map(|packet| format!("{}{}", config.prompt, format_handoff_prompt(packet)))
        .unwrap_or_else(|| config.prompt.clone());
    if prompt.trim().is_empty() || prompt.len() > 60_000 || prompt.contains('\0') {
        return Err("Enter a prompt of 1 to 60,000 characters for Antigravity.".into());
    }
    if config.model_id.trim().is_empty()
        || config.model_id.len() > 160
        || config.model_id.chars().any(char::is_control)
    {
        return Err("Select a valid configured Antigravity model.".into());
    }
    let cwd = config
        .cwd
        .canonicalize()
        .map_err(|_| "The selected project folder could not be resolved.".to_string())?;
    if !cwd.is_dir() {
        return Err("The selected project must be an existing folder.".into());
    }
    if let Some(packet) = handoff.as_ref() {
        ensure_handoff_worktree(&cwd, packet)?;
    } else {
        ensure_clean_git_worktree(&cwd)?;
    }
    let request = StartRequest {
        executable: config.executable.to_string_lossy().into_owned(),
        arguments: crate::providers::antigravity_arguments(
            &config.model_id,
            &prompt,
            config.timeout_seconds,
        ),
        cwd: cwd.to_string_lossy().into_owned(),
        timeout_seconds: config.timeout_seconds,
        mutating: true,
    };
    let model_id = config.model_id.clone();
    start_run(
        app,
        request,
        None,
        Some("antigravity"),
        Some(model_id),
        handoff,
        None,
    )
}

pub fn create_provider_handoff(
    app: AppHandle,
    input: HandoffInput,
) -> Result<ProviderHandoff, String> {
    let validate = |name: &str, value: &str| -> Result<String, String> {
        let value = value.trim();
        if value.is_empty() || value.len() > 8_000 || value.contains('\0') {
            return Err(format!(
                "Handoff {name} must contain 1 to 8,000 characters."
            ));
        }
        Ok(redact(value, &[]))
    };
    if input.source_run_id.is_empty()
        || input.source_run_id.len() > 100
        || input.target_provider_id.is_empty()
        || input.target_provider_id.len() > 80
    {
        return Err("Choose a valid source run and target provider for the handoff.".into());
    }
    let path = history_path(&app)?;
    let mut state = store()
        .lock()
        .map_err(|_| "Could not access workflow runs.".to_string())?;
    if state.records.is_empty() {
        state.records = load_records(&path)?;
    }
    let source = state
        .records
        .iter()
        .find(|record| record.id == input.source_run_id)
        .ok_or("The source run is no longer in saved workflow history.")?;
    if !matches!(
        source.state.as_str(),
        "completed" | "failed" | "cancelled" | "timedOut" | "interrupted"
    ) {
        return Err("Finish or cancel the source run before preparing a manual handoff.".into());
    }
    let source_cwd = Path::new(&source.cwd)
        .canonicalize()
        .map_err(|_| "The source run's project folder could not be resolved.".to_string())?;
    let source_root = source_cwd
        .ancestors()
        .find(|folder| folder.join(".git").exists())
        .unwrap_or(&source_cwd)
        .canonicalize()
        .map_err(|_| "The source run's project folder could not be resolved.".to_string())?;
    let snapshot = git_worktree_snapshot(&source_cwd)?;
    let source = state
        .records
        .iter_mut()
        .find(|record| record.id == input.source_run_id)
        .ok_or("The source run is no longer in saved workflow history.")?;
    let packet = ProviderHandoff {
        version: 2,
        source_run_id: source.id.clone(),
        target_provider_id: input.target_provider_id,
        working_tree_root: source_root.to_string_lossy().into_owned(),
        working_tree_paths: snapshot
            .as_ref()
            .map(|snapshot| snapshot.changed_paths.clone())
            .unwrap_or_default(),
        working_tree_fingerprint: snapshot.map(|snapshot| snapshot.fingerprint),
        goal: validate("goal", &input.goal)?,
        scope: validate("scope", &input.scope)?,
        diff: validate("diff", &input.diff)?,
        checks: validate("checks", &input.checks)?,
        pending_work: validate("pending work", &input.pending_work)?,
        completed_side_effects: source
            .workflow
            .as_ref()
            .and_then(|workflow| workflow.side_effects.clone()),
        created_at_ms: now_ms(),
    };
    source.handoff = Some(packet.clone());
    persist_records(&path, &state.records)?;
    Ok(packet)
}

pub fn saved_provider_handoff(
    app: &AppHandle,
    source_run_id: &str,
    target_provider_id: &str,
) -> Result<ProviderHandoff, String> {
    let path = history_path(app)?;
    let mut state = store()
        .lock()
        .map_err(|_| "Could not access workflow runs.".to_string())?;
    if state.records.is_empty() {
        state.records = load_records(&path)?;
    }
    state
        .records
        .iter()
        .find(|record| record.id == source_run_id)
        .and_then(|record| record.handoff.clone())
        .filter(|packet| {
            packet.target_provider_id == target_provider_id && packet.source_run_id == source_run_id
        })
        .ok_or_else(|| {
            "Create a manual handoff for this source run and selected target provider first.".into()
        })
}

pub fn format_handoff_prompt(packet: &ProviderHandoff) -> String {
    let effects = packet
        .completed_side_effects
        .as_ref()
        .map(|value| {
            format!(
                "commit={}, branch={}, pull_request={}",
                value.commit_sha.as_deref().unwrap_or("none"),
                value.remote_branch.as_deref().unwrap_or("none"),
                value.pull_request_url.as_deref().unwrap_or("none"),
            )
        })
        .unwrap_or_else(|| "none recorded".into());
    format!("\n\nMANUAL PROVIDER HANDOFF (source run: {})\nDo not automatically repeat completed commits, pushes, pull requests, or other remote side effects. Completed side effects: {}\nGoal:\n{}\nScope:\n{}\nCurrent diff:\n{}\nChecks already run and results:\n{}\nPending work:\n{}\nContinue only with pending work and report any side effect for user review.\n", packet.source_run_id, effects, packet.goal, packet.scope, packet.diff, packet.checks, packet.pending_work)
}

pub fn start_ollama_workflow_run(
    app: AppHandle,
    config: OllamaRunConfig,
    handoff: Option<ProviderHandoff>,
) -> Result<RunRecord, String> {
    if config.prompt.trim().is_empty()
        || config.prompt.len() > 60_000
        || config.prompt.contains('\0')
    {
        return Err("Enter a prompt of 1 to 60,000 characters for the local Ollama run.".into());
    }
    if config.model_id.trim().is_empty()
        || config.model_id.len() > 160
        || config.model_id.chars().any(char::is_control)
    {
        return Err("Select a valid tool-capable Ollama model.".into());
    }
    if config.timeout_seconds == 0 || config.timeout_seconds > 3_600 {
        return Err(
            "The hardware-aware Ollama timeout must be between 1 and 3,600 seconds.".into(),
        );
    }
    if config
        .test_command
        .as_ref()
        .is_some_and(|command| command.len() > 2_048 || command.chars().any(char::is_control))
    {
        return Err("The optional test command must be 2,048 characters or fewer without control characters.".into());
    }
    let cwd = config
        .cwd
        .canonicalize()
        .map_err(|_| "The selected project folder could not be resolved.".to_string())?;
    if !cwd.is_dir() {
        return Err("The selected project must be an existing folder.".into());
    }
    if let Some(packet) = handoff.as_ref() {
        ensure_handoff_worktree(&cwd, packet)?;
    } else {
        ensure_clean_git_worktree(&cwd)?;
    }
    if handoff
        .as_ref()
        .is_some_and(|packet| ![1, 2].contains(&packet.version))
    {
        return Err("This handoff packet version is unsupported.".into());
    }
    let prompt = handoff
        .as_ref()
        .map(|packet| format!("{}{}", config.prompt, format_handoff_prompt(packet)))
        .unwrap_or_else(|| config.prompt.clone());
    if prompt.len() > 60_000 {
        return Err("The prompt and handoff packet exceed 60,000 characters.".into());
    }
    let env_file = if cfg!(windows) { "NUL" } else { "/dev/null" };
    let request = StartRequest {
        executable: config.executable.to_string_lossy().into_owned(),
        arguments: crate::providers::aider_ollama_arguments(
            &config.model_id,
            &prompt,
            config.test_command.as_deref(),
            env_file,
        ),
        cwd: cwd.to_string_lossy().into_owned(),
        timeout_seconds: config.timeout_seconds,
        mutating: true,
    };
    let model_id = config.model_id.clone();
    start_run(
        app,
        request,
        None,
        Some("ollama"),
        Some(model_id),
        handoff,
        Some(config.endpoint),
    )
}

fn start_run(
    app: AppHandle,
    request: StartRequest,
    codex: Option<CodexRunConfig>,
    provider: Option<&'static str>,
    model_id: Option<String>,
    handoff: Option<ProviderHandoff>,
    ollama_endpoint: Option<String>,
) -> Result<RunRecord, String> {
    let codex_run = codex.is_some();
    let (executable, cwd, host) = validate_request(&request)?;
    let canonical_key = mutation_scope(&cwd);
    let path = history_path(&app)?;
    let id = format!(
        "run-{}-{}",
        now_ms(),
        NEXT_ID.fetch_add(1, Ordering::Relaxed)
    );
    let redaction_values = std::env::vars_os()
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
        .chain(
            request
                .arguments
                .iter()
                .filter(|arg| {
                    arg.starts_with("ghp_")
                        || arg.starts_with("gho_")
                        || arg.starts_with("github_pat_")
                        || arg.starts_with("sk-")
                })
                .cloned(),
        )
        .collect::<Vec<_>>();
    let record = RunRecord {
        id: id.clone(),
        executable: executable.to_string_lossy().into_owned(),
        provider: provider.map(str::to_owned),
        model_id: model_id.map(|value| crate::usage::safe_identity(&value)),
        cwd: cwd.to_string_lossy().into_owned(),
        thread_id: None,
        process_id: None,
        mutating: request.mutating,
        timeout_seconds: request.timeout_seconds,
        state: "queued".into(),
        started_at_ms: now_ms(),
        finished_at_ms: None,
        latency_ms: None,
        exit_code: None,
        next_sequence: 1,
        events: Vec::new(),
        workflow: codex.as_ref().and_then(|config| config.workflow.clone()),
        handoff,
    };
    {
        let mut state = store()
            .lock()
            .map_err(|_| "Could not access workflow runs.".to_string())?;
        if state.records.is_empty() {
            state.records = load_records(&path)?;
        }
        if let Some(workflow) = &record.workflow {
            if workflow_key_recorded(&state.records, &workflow.operation_key) {
                return Err("This exact workflow scope already has a saved run. Review its persisted output and side-effect record before starting another run.".into());
            }
        }
        if request.mutating && interrupted_process_owns_scope(&state.records, &canonical_key) {
            return Err(
                "A prior interrupted workflow process is still running for this repository or worktree.".into(),
            );
        }
        if request.mutating && !state.mutating_paths.insert(canonical_key.clone()) {
            return Err(
                "A mutating workflow is already active for this repository or worktree.".into(),
            );
        }
        state.records.insert(0, record.clone());
        state.records.truncate(MAX_RUNS);
        state.redaction_values.insert(id.clone(), redaction_values);
        if let Err(error) = persist_records(&path, &state.records) {
            state.records.retain(|entry| entry.id != id);
            state.redaction_values.remove(&id);
            if request.mutating {
                state.mutating_paths.remove(&canonical_key);
            }
            return Err(error);
        }
    }
    let mut command = match command_for(&executable, &cwd, &host, &request.arguments) {
        Ok(command) => command,
        Err(error) => {
            set_state(&app, &path, &id, "failed", None);
            return Err(error);
        }
    };
    if let Some(endpoint) = ollama_endpoint {
        command.env("OLLAMA_API_BASE", endpoint);
        for key in [
            "OLLAMA_HOST",
            "OLLAMA_API_KEY",
            "OPENAI_API_KEY",
            "OPENAI_API_BASE",
            "ANTHROPIC_API_KEY",
            "GEMINI_API_KEY",
            "HTTP_PROXY",
            "HTTPS_PROXY",
            "ALL_PROXY",
            "http_proxy",
            "https_proxy",
            "all_proxy",
        ] {
            command.env_remove(key);
        }
        command.env("NO_PROXY", "*").env("no_proxy", "*");
    }
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) => {
            set_state(&app, &path, &id, "failed", None);
            if request.mutating {
                if let Ok(mut state) = store().lock() {
                    state.mutating_paths.remove(&canonical_key);
                }
            }
            return Err(format!("Could not start configured executable: {error}"));
        }
    };
    let process_id = child.id();
    let process_saved = store().lock().ok().is_some_and(|mut state| {
        if let Some(record) = state.records.iter_mut().find(|record| record.id == id) {
            record.process_id = Some(process_id);
            persist_records(&path, &state.records).is_ok()
        } else {
            false
        }
    });
    if !process_saved {
        terminate_process_tree(&mut child);
        if let Ok(mut state) = store().lock() {
            release_failed_checkpoint(&mut state, &id, &canonical_key);
        }
        set_state(&app, &path, &id, "failed", None);
        return Err(
            "Could not persist the run process checkpoint; the process was stopped.".into(),
        );
    }
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let codex_stdin = if codex.is_some() {
        child.stdin.take()
    } else {
        None
    };
    let child = Arc::new(Mutex::new(child));
    if let Ok(mut state) = store().lock() {
        state.active.insert(id.clone(), child.clone());
        if let Some(stdin) = codex_stdin {
            state
                .codex_writers
                .insert(id.clone(), Arc::new(Mutex::new(stdin)));
            state.codex_requests.insert(id.clone(), HashMap::new());
        }
    }
    set_state(&app, &path, &id, "running", None);
    if let Some(codex) = codex {
        if let (Some(stdout), Some(writer)) = (
            stdout,
            store()
                .lock()
                .ok()
                .and_then(|state| state.codex_writers.get(&id).cloned()),
        ) {
            start_codex_protocol(
                app.clone(),
                path.clone(),
                id.clone(),
                child.clone(),
                writer,
                stdout,
                codex,
            );
        }
    } else if let Some(stdout) = stdout {
        pump_output(app.clone(), path.clone(), id.clone(), "stdout", stdout);
    }
    if let Some(stderr) = stderr {
        if codex_run {
            pump_codex_stderr(app.clone(), path.clone(), id.clone(), child.clone(), stderr);
        } else {
            pump_output(app.clone(), path.clone(), id.clone(), "stderr", stderr);
        }
    }
    let worker_app = app.clone();
    let worker_id = id.clone();
    thread::spawn(move || {
        let started = Instant::now();
        let outcome = loop {
            if started.elapsed() >= Duration::from_secs(request.timeout_seconds) {
                if let Ok(mut child) = child.lock() {
                    terminate_process_tree(&mut child);
                }
                break "timedOut";
            }
            let result = child
                .lock()
                .map_err(|_| ())
                .and_then(|mut process| process.try_wait().map_err(|_| ()));
            match result {
                Ok(Some(status)) => {
                    let finished_by_adapter = store()
                        .lock()
                        .map(|mut state| state.adapter_terminal.remove(&worker_id))
                        .unwrap_or(false);
                    if finished_by_adapter {
                        break "adapterFinished";
                    }
                    let was_cancelled = store()
                        .lock()
                        .map(|mut state| state.cancelled.remove(&worker_id))
                        .unwrap_or(false);
                    if was_cancelled {
                        set_state(&worker_app, &path, &worker_id, "cancelled", status.code());
                        break "cancelled";
                    }
                    if status.success() {
                        set_state(&worker_app, &path, &worker_id, "completed", status.code());
                        break "completed";
                    }
                    set_state(&worker_app, &path, &worker_id, "failed", status.code());
                    break "failed";
                }
                Ok(None) => thread::sleep(POLL_INTERVAL),
                Err(()) => {
                    set_state(&worker_app, &path, &worker_id, "failed", None);
                    break "failed";
                }
            }
        };
        if outcome == "timedOut" {
            set_state(&worker_app, &path, &worker_id, outcome, None);
        }
        if let Ok(mut state) = store().lock() {
            state.active.remove(&worker_id);
            state.codex_writers.remove(&worker_id);
            state.codex_requests.remove(&worker_id);
            state.codex_thread_ids.remove(&worker_id);
            state.adapter_terminal.remove(&worker_id);
            state.output_bytes.remove(&worker_id);
            state.cancelled.remove(&worker_id);
            state.redaction_values.remove(&worker_id);
            if request.mutating {
                state.mutating_paths.remove(&canonical_key);
            }
        }
    });
    Ok(record)
}

fn terminate_process_tree(child: &mut Child) {
    #[cfg(windows)]
    {
        let id = child.id().to_string();
        let _ = Command::new("taskkill.exe")
            .args(["/pid", id.as_str(), "/t", "/f"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
    let _ = child.kill();
}

#[tauri::command]
pub fn cancel_workflow_run(app: AppHandle, run_id: String) -> Result<bool, String> {
    let child = store()
        .lock()
        .map_err(|_| "Could not access workflow runs.".to_string())?
        .active
        .get(&run_id)
        .cloned();
    let Some(child) = child else { return Ok(false) };
    let mut process = child
        .lock()
        .map_err(|_| "Could not access the running process.".to_string())?;
    if process
        .try_wait()
        .map_err(|_| "Could not inspect the running process.".to_string())?
        .is_some()
    {
        return Ok(false);
    }
    {
        let mut state = store()
            .lock()
            .map_err(|_| "Could not access workflow runs.".to_string())?;
        if !state.active.contains_key(&run_id) {
            return Ok(false);
        }
        state.cancelled.insert(run_id.clone());
        let snapshot = state
            .records
            .iter_mut()
            .find(|record| record.id == run_id)
            .map(|record| {
                record.state = "cancelling".into();
                record.clone()
            });
        persist_records(&history_path(&app)?, &state.records)?;
        if let Some(snapshot) = snapshot {
            let _ = app.emit("workflow-run-updated", snapshot);
        }
    }
    terminate_process_tree(&mut process);
    Ok(true)
}

#[tauri::command]
pub fn mark_workflow_run_needs_input(app: AppHandle, run_id: String) -> Result<bool, String> {
    let child = store()
        .lock()
        .map_err(|_| "Could not access workflow runs.".to_string())?
        .active
        .get(&run_id)
        .cloned();
    let Some(child) = child else { return Ok(false) };
    let mut process = child
        .lock()
        .map_err(|_| "Could not access the running process.".to_string())?;
    if process
        .try_wait()
        .map_err(|_| "Could not inspect the running process.".to_string())?
        .is_some()
    {
        return Ok(false);
    }
    set_state(&app, &history_path(&app)?, &run_id, "needsInput", None);
    Ok(true)
}

#[tauri::command]
pub fn write_workflow_run_input(
    app: AppHandle,
    run_id: String,
    input: String,
) -> Result<bool, String> {
    if input.len() > 64 * 1024 || input.contains('\0') {
        return Err("The input is invalid or too large.".into());
    }
    let state = store()
        .lock()
        .map_err(|_| "Could not access workflow runs.".to_string())?;
    if state.codex_writers.contains_key(&run_id) {
        return Err(
            "Respond to the pending Codex approval or input request in the run details.".into(),
        );
    }
    let child = state.active.get(&run_id).cloned();
    drop(state);
    let Some(child) = child else { return Ok(false) };
    let needs_input = store()
        .lock()
        .map_err(|_| "Could not access workflow runs.".to_string())?
        .records
        .iter()
        .find(|record| record.id == run_id)
        .is_some_and(|record| record.state == "needsInput");
    if !needs_input {
        return Err("Mark the running process as waiting for input first.".into());
    }
    let mut process = child
        .lock()
        .map_err(|_| "Could not access the running process.".to_string())?;
    let Some(stdin) = process.stdin.as_mut() else {
        return Ok(false);
    };
    stdin
        .write_all(input.as_bytes())
        .and_then(|_| stdin.flush())
        .map_err(|_| "Could not send input to the process.".to_string())?;
    let path = history_path(&app)?;
    set_state(&app, &path, &run_id, "running", None);
    emit_event(&app, &path, &run_id, "inputSent", "Input sent.");
    Ok(true)
}

#[tauri::command]
pub fn respond_codex_approval(
    app: AppHandle,
    run_id: String,
    request_id: String,
    decision: String,
) -> Result<bool, String> {
    if request_id.is_empty() || request_id.len() > 160 || request_id.chars().any(char::is_control) {
        return Err("The Codex approval request identifier is invalid.".into());
    }
    let (writer, pending) = get_pending_codex_request(&run_id, &request_id)?;
    if !matches!(
        pending.method.as_str(),
        "item/commandExecution/requestApproval"
            | "item/fileChange/requestApproval"
            | "item/permissions/requestApproval"
    ) {
        return Err("This Codex request is not an approval request.".into());
    }
    let response = codex_approval_response(&pending, &decision)?;
    send_codex_message(
        &writer,
        &crate::providers::app_server_response(pending.request_id.clone(), response),
    )?;
    clear_pending_codex_request(&app, &run_id, &request_id)?;
    Ok(true)
}

fn codex_approval_response(
    pending: &PendingCodexRequest,
    decision: &str,
) -> Result<serde_json::Value, String> {
    if pending.method == "item/permissions/requestApproval" {
        return match decision {
            "accept" => {
                let permissions = pending
                    .params
                    .get("permissions")
                    .cloned()
                    .ok_or("Codex sent a permission request without a permission profile.")?;
                Ok(serde_json::json!({
                    "permissions": permissions,
                    "scope": "turn"
                }))
            }
            "decline" => Ok(serde_json::json!({
                "permissions": {},
                "scope": "turn"
            })),
            _ => Err("Choose whether to grant or decline the requested permissions.".into()),
        };
    }

    let allowed = pending
        .params
        .get("availableDecisions")
        .and_then(serde_json::Value::as_array);
    let allowed_by_server = allowed.is_some_and(|choices| {
        choices
            .iter()
            .any(|choice| choice.as_str() == Some(decision))
    });
    let has_renderable_choice =
        allowed.is_some_and(|choices| choices.iter().any(serde_json::Value::is_string));
    let safe_legacy_choice = !has_renderable_choice && matches!(decision, "decline" | "cancel");
    if !allowed_by_server && !safe_legacy_choice {
        return Err("Choose an approval decision offered by Codex.".into());
    }
    if !matches!(
        decision,
        "accept" | "acceptForSession" | "decline" | "cancel"
    ) {
        return Err("Choose an approval decision offered by Codex.".into());
    }
    Ok(serde_json::json!({ "decision": decision }))
}

#[tauri::command]
pub fn respond_codex_user_input(
    app: AppHandle,
    run_id: String,
    request_id: String,
    answers: HashMap<String, Vec<String>>,
) -> Result<bool, String> {
    if request_id.is_empty() || request_id.len() > 160 || request_id.chars().any(char::is_control) {
        return Err("The Codex input request identifier is invalid.".into());
    }
    let (writer, pending) = get_pending_codex_request(&run_id, &request_id)?;
    if pending.method != "item/tool/requestUserInput" {
        return Err("This Codex request is not asking for user input.".into());
    }
    let questions = pending
        .params
        .get("questions")
        .and_then(serde_json::Value::as_array)
        .ok_or("Codex sent an invalid input request.")?;
    let allowed = questions
        .iter()
        .filter_map(|question| question.get("id").and_then(serde_json::Value::as_str))
        .collect::<HashSet<_>>();
    let response = codex_user_input_response(questions, &allowed, &answers)?;
    send_codex_message(
        &writer,
        &crate::providers::app_server_response(pending.request_id.clone(), response),
    )?;
    clear_pending_codex_request(&app, &run_id, &request_id)?;
    Ok(true)
}

fn codex_user_input_response(
    questions: &[serde_json::Value],
    allowed: &HashSet<&str>,
    answers: &HashMap<String, Vec<String>>,
) -> Result<serde_json::Value, String> {
    if allowed.len() != questions.len()
        || allowed.contains("")
        || answers.keys().any(|key| !allowed.contains(key.as_str()))
        || answers.values().any(|values| {
            values.len() > 10
                || values
                    .iter()
                    .any(|value| value.len() > 10_000 || value.contains('\0'))
        })
    {
        return Err(
            "The Codex answers do not match the requested fields or exceed their limits.".into(),
        );
    }
    let response_answers = allowed
        .iter()
        .map(|key| {
            let values = answers.get(*key).cloned().unwrap_or_default();
            ((*key).to_owned(), serde_json::json!({ "answers": values }))
        })
        .collect::<HashMap<_, _>>();
    Ok(serde_json::json!({ "answers": response_answers }))
}

fn get_pending_codex_request(
    run_id: &str,
    request_id: &str,
) -> Result<(Arc<Mutex<ChildStdin>>, PendingCodexRequest), String> {
    let state = store()
        .lock()
        .map_err(|_| "Could not access workflow runs.".to_string())?;
    let writer = state
        .codex_writers
        .get(run_id)
        .cloned()
        .ok_or("This Codex run is no longer accepting requests.")?;
    let pending = state
        .codex_requests
        .get(run_id)
        .and_then(|requests| requests.get(request_id))
        .cloned()
        .ok_or("This Codex request is no longer pending.")?;
    Ok((writer, pending))
}

fn clear_pending_codex_request(
    app: &AppHandle,
    run_id: &str,
    request_id: &str,
) -> Result<(), String> {
    let (no_pending, path) = {
        let mut state = store()
            .lock()
            .map_err(|_| "Could not access workflow runs.".to_string())?;
        let requests = state
            .codex_requests
            .get_mut(run_id)
            .ok_or("This Codex run is no longer accepting requests.")?;
        requests.remove(request_id);
        (requests.is_empty(), history_path(app)?)
    };
    if no_pending {
        let waiting = store()
            .lock()
            .map_err(|_| "Could not access workflow runs.".to_string())?
            .records
            .iter()
            .find(|record| record.id == run_id)
            .is_some_and(|record| record.state == "needsInput");
        if waiting {
            set_state(app, &path, run_id, "running", None);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(executable: &str, cwd: &str, args: Vec<&str>) -> StartRequest {
        StartRequest {
            executable: executable.into(),
            cwd: cwd.into(),
            arguments: args.into_iter().map(str::to_owned).collect(),
            timeout_seconds: 30,
            mutating: false,
        }
    }

    fn git_fixture(path: &Path) {
        for args in [
            vec!["init", "-q"],
            vec!["config", "user.name", "Navis Fixture"],
            vec!["config", "user.email", "fixture@example.invalid"],
        ] {
            let output = Command::new("git")
                .current_dir(path)
                .args(&args)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "git {args:?} failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
        fs::write(path.join("tracked.txt"), "baseline\n").unwrap();
        assert!(Command::new("git")
            .current_dir(path)
            .args(["add", "tracked.txt"])
            .status()
            .unwrap()
            .success());
        assert!(Command::new("git")
            .current_dir(path)
            .args(["commit", "-qm", "baseline"])
            .status()
            .unwrap()
            .success());
    }

    #[test]
    fn handoff_accepts_only_the_exact_saved_worktree_snapshot() {
        let directory = tempfile::tempdir_in(env!("CARGO_MANIFEST_DIR")).unwrap();
        git_fixture(directory.path());
        fs::write(directory.path().join("tracked.txt"), "recorded edit\n").unwrap();
        fs::write(directory.path().join("new.txt"), "recorded new file\n").unwrap();
        let snapshot = git_worktree_snapshot(directory.path()).unwrap().unwrap();
        assert_eq!(snapshot.changed_paths, vec!["new.txt", "tracked.txt"]);
        let handoff = ProviderHandoff {
            version: 2,
            source_run_id: "source".into(),
            target_provider_id: "ollama".into(),
            working_tree_root: snapshot.root.to_string_lossy().into_owned(),
            working_tree_paths: snapshot.changed_paths,
            working_tree_fingerprint: Some(snapshot.fingerprint),
            goal: "continue".into(),
            scope: "tracked.txt and new.txt".into(),
            diff: "saved diff".into(),
            checks: "none".into(),
            pending_work: "finish".into(),
            completed_side_effects: None,
            created_at_ms: 1,
        };
        assert!(ensure_handoff_worktree(directory.path(), &handoff).is_ok());

        fs::write(
            directory.path().join("unrelated.txt"),
            "keep this user change\n",
        )
        .unwrap();
        let error = ensure_handoff_worktree(directory.path(), &handoff).unwrap_err();
        assert!(error.contains("worktree changed after this handoff"));
        fs::remove_file(directory.path().join("unrelated.txt")).unwrap();

        fs::write(
            directory.path().join("new.txt"),
            "changed after checkpoint\n",
        )
        .unwrap();
        let error = ensure_handoff_worktree(directory.path(), &handoff).unwrap_err();
        assert!(error.contains("worktree changed after this handoff"));
    }

    #[test]
    fn rejects_invalid_cwd_timeout_and_argument_arrays() {
        assert!(validate_request(&request("echo", "relative", vec![])).is_err());
        let mut invalid = request("echo", env!("CARGO_MANIFEST_DIR"), vec![]);
        invalid.timeout_seconds = 0;
        assert!(validate_request(&invalid).is_err());
        let invalid = request("echo", env!("CARGO_MANIFEST_DIR"), vec!["bad\0argument"]);
        assert!(validate_request(&invalid).is_err());
        let mut invalid = request("echo", env!("CARGO_MANIFEST_DIR"), vec![]);
        invalid.arguments = vec!["x".repeat(MAX_COMMAND_BYTES + 1)];
        assert!(validate_request(&invalid).is_err());
    }

    #[test]
    fn history_survives_restart_and_orphans_become_interrupted() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join(HISTORY_FILE);
        let mut record = RunRecord {
            id: "fixture".into(),
            executable: "test".into(),
            provider: None,
            model_id: None,
            cwd: directory.path().display().to_string(),
            thread_id: None,
            process_id: Some(std::process::id()),
            mutating: true,
            timeout_seconds: 1,
            state: "running".into(),
            started_at_ms: 1,
            finished_at_ms: None,
            latency_ms: None,
            exit_code: None,
            next_sequence: 1,
            events: vec![],
            workflow: Some(WorkflowRunMetadata {
                action: "dev-push".into(),
                operation_key: "a".repeat(64),
                intent: "Review the selected change.".into(),
                build_profile: None,
                issue_url: None,
                scope_paths: vec!["src/main.rs".into()],
                requested_effects: vec!["commit".into()],
                side_effects: Some(WorkflowSideEffects {
                    commit_sha: Some("0123456789abcdef0123456789abcdef01234567".into()),
                    remote_branch: Some("feat/example".into()),
                    pull_request_url: None,
                    verified: false,
                    verification_message: None,
                }),
                build_artifact: None,
            }),
            handoff: Some(ProviderHandoff {
                version: 1,
                source_run_id: "fixture".into(),
                target_provider_id: "ollama-local".into(),
                working_tree_root: directory
                    .path()
                    .canonicalize()
                    .unwrap()
                    .to_string_lossy()
                    .into_owned(),
                working_tree_paths: vec![],
                working_tree_fingerprint: None,
                goal: "Finish the requested change".into(),
                scope: "src/main.rs".into(),
                diff: "One file changed".into(),
                checks: "cargo test passed".into(),
                pending_work: "Review the remaining edge case".into(),
                completed_side_effects: Some(WorkflowSideEffects {
                    commit_sha: Some("0123456789abcdef0123456789abcdef01234567".into()),
                    remote_branch: Some("feat/example".into()),
                    pull_request_url: None,
                    verified: true,
                    verification_message: None,
                }),
                created_at_ms: 2,
            }),
        };
        persist_records(&path, &[record.clone()]).unwrap();
        record.state = "interrupted".into();
        let restored = load_records(&path).unwrap();
        assert_eq!(restored[0].state, record.state);
        assert_eq!(restored[0].handoff, record.handoff);
        assert_eq!(restored[0].process_id, Some(std::process::id()));
        assert!(interrupted_process_owns_scope(
            &restored,
            &mutation_scope(directory.path())
        ));
        assert!(restored[0].finished_at_ms.is_some());
        assert!(workflow_key_recorded(&restored, &"a".repeat(64)));
        let handoff_prompt = format_handoff_prompt(restored[0].handoff.as_ref().unwrap());
        assert!(handoff_prompt.contains("Do not automatically repeat completed commits"));
        assert!(handoff_prompt.contains("0123456789abcdef0123456789abcdef01234567"));
        assert!(handoff_prompt.contains("Review the remaining edge case"));
        assert_eq!(
            restored[0]
                .workflow
                .as_ref()
                .unwrap()
                .side_effects
                .as_ref()
                .unwrap()
                .remote_branch
                .as_deref(),
            Some("feat/example")
        );
    }

    #[test]
    fn process_checkpoint_detects_live_processes_and_cleared_pids() {
        assert!(process_is_running(std::process::id()));
        assert!(!process_is_running(u32::MAX));
    }

    #[test]
    fn redacts_github_token_shapes_from_output() {
        let value = redact("token=ghp_0123456789abcdefghijklmnop", &[]);
        assert!(!value.contains("ghp_"));
        assert!(value.contains("[REDACTED]"));
    }

    #[test]
    fn codex_preflight_allows_clean_repositories_and_rejects_dirty_ones() {
        let fixture = tempfile::tempdir_in(env!("CARGO_MANIFEST_DIR")).unwrap();
        let init = Command::new("git")
            .arg("init")
            .arg("--quiet")
            .current_dir(fixture.path())
            .output()
            .unwrap();
        assert!(init.status.success());
        ensure_clean_git_worktree(fixture.path()).unwrap();

        fs::write(fixture.path().join("unrelated.txt"), "local work").unwrap();
        let error = ensure_clean_git_worktree(fixture.path()).unwrap_err();
        assert!(error.contains("clean Git worktree"));
    }

    #[test]
    fn push_preflight_requires_an_exact_changed_file_scope() {
        let fixture = tempfile::tempdir_in(env!("CARGO_MANIFEST_DIR")).unwrap();
        let init = Command::new("git")
            .arg("init")
            .arg("--quiet")
            .current_dir(fixture.path())
            .output()
            .unwrap();
        assert!(
            init.status.success(),
            "{}",
            String::from_utf8_lossy(&init.stderr)
        );
        fs::write(fixture.path().join("one.txt"), "one").unwrap();
        fs::write(fixture.path().join("two.txt"), "two").unwrap();
        assert!(ensure_git_scope(fixture.path(), &["one.txt".into()]).is_err());
        assert!(ensure_git_scope(fixture.path(), &["one.txt".into(), "two.txt".into()]).is_ok());
    }

    #[test]
    fn workflow_scope_validation_and_side_effect_markers_are_bounded() {
        let mut workflow = WorkflowRunMetadata {
            action: "dev-push".into(),
            operation_key: "a".repeat(64),
            intent: "Review the selected change.".into(),
            build_profile: None,
            issue_url: None,
            scope_paths: vec!["src/main.rs".into()],
            requested_effects: vec!["commit".into()],
            side_effects: None,
            build_artifact: None,
        };
        assert!(validate_workflow_metadata(&workflow).is_ok());
        workflow.requested_effects = vec!["createPullRequest".into()];
        assert!(validate_workflow_metadata(&workflow).is_err());
        workflow.requested_effects = vec!["commit".into()];
        workflow.scope_paths = vec!["../secret.txt".into()];
        assert!(validate_workflow_metadata(&workflow).is_err());
        let marker = parse_side_effect_marker("NAVIS_SIDE_EFFECTS:{\"commitSha\":\"0123456789abcdef0123456789abcdef01234567\",\"remoteBranch\":\"feat/example\",\"pullRequestUrl\":\"https://github.com/owner/repo/pull/12\"}").unwrap();
        assert_eq!(marker.remote_branch.as_deref(), Some("feat/example"));
        assert_eq!(
            marker.pull_request_url.as_deref(),
            Some("https://github.com/owner/repo/pull/12")
        );
        assert!(parse_side_effect_marker(
            "NAVIS_SIDE_EFFECTS:{\"pullRequestUrl\":\"file:///secret\"}"
        )
        .is_none());
        assert_eq!(
            github_repository_from_remote("git@github.com:KevinHozak/Navis-Fabrica.git").as_deref(),
            Some("kevinhozak/navis-fabrica")
        );
    }

    #[test]
    fn build_artifact_receipt_checks_profile_project_path_and_declared_version() {
        let fixture = tempfile::tempdir().unwrap();
        fs::create_dir_all(fixture.path().join("src-tauri")).unwrap();
        fs::write(
            fixture.path().join("src-tauri/tauri.conf.json"),
            r#"{"version":"1.2.3"}"#,
        )
        .unwrap();
        let artifact_path = fixture.path().join("src-tauri/target/release/app.exe");
        fs::create_dir_all(artifact_path.parent().unwrap()).unwrap();
        fs::write(&artifact_path, b"fixture artifact").unwrap();
        let workflow = WorkflowRunMetadata {
            action: "dev-build".into(),
            operation_key: "d".repeat(64),
            intent: "Build release artifacts.".into(),
            build_profile: Some("release".into()),
            issue_url: None,
            scope_paths: vec![],
            requested_effects: vec![],
            side_effects: None,
            build_artifact: None,
        };
        let receipt_data = serde_json::json!({
            "profile": "release",
            "path": artifact_path,
            "fileVersion": "1.2.3",
            "productVersion": "1.2.3"
        });
        let marker = format!(
            "NAVIS_BUILD_ARTIFACT:{}",
            serde_json::to_string(&receipt_data).unwrap()
        );
        assert!(
            serde_json::from_str::<serde_json::Value>(
                marker.trim_start_matches("NAVIS_BUILD_ARTIFACT:")
            )
            .is_ok(),
            "{marker}"
        );
        assert!(artifact_path.canonicalize().is_ok());
        assert!(fixture.path().join("src-tauri/tauri.conf.json").exists());
        let receipt = verify_build_artifact_with(fixture.path(), &workflow, &marker, |_| {
            Some(ArtifactVersionInfo {
                file_version: "1.2.3".into(),
                product_version: "1.2.3".into(),
            })
        })
        .unwrap();
        assert!(receipt.verified);
        assert!(receipt
            .verification_message
            .contains("read from the artifact"));
        let mismatch = marker.replace("1.2.3", "9.9.9");
        assert!(
            !verify_build_artifact_with(fixture.path(), &workflow, &mismatch, |_| {
                Some(ArtifactVersionInfo {
                    file_version: "1.2.3".into(),
                    product_version: "1.2.3".into(),
                })
            })
            .unwrap()
            .verified
        );
        assert!(
            !verify_build_artifact_with(fixture.path(), &workflow, &marker, |_| {
                Some(ArtifactVersionInfo {
                    file_version: "9.9.9".into(),
                    product_version: "9.9.9".into(),
                })
            })
            .unwrap()
            .verified
        );
        assert!(!artifact_is_in_profile_target(
            fixture.path(),
            &artifact_path.canonicalize().unwrap(),
            "debug"
        ));
    }

    #[test]
    fn completed_codex_builds_without_process_exit_code_remain_successful() {
        assert!(build_result_is_successful("completed", None));
        assert!(build_result_is_successful("completed", Some(0)));
        assert!(!build_result_is_successful("completed", Some(1)));
        assert!(!build_result_is_successful("failed", None));
    }

    #[test]
    fn failed_checkpoint_releases_mutation_scope_and_run_redaction_state() {
        let mut state = RunStore::default();
        state.mutating_paths.insert("project".into());
        state
            .redaction_values
            .insert("run-1".into(), vec!["secret".into()]);

        release_failed_checkpoint(&mut state, "run-1", "project");

        assert!(!state.mutating_paths.contains("project"));
        assert!(!state.redaction_values.contains_key("run-1"));
    }

    #[cfg(windows)]
    #[test]
    fn embedded_artifact_version_reader_reads_windows_executable_metadata() {
        let output = Command::new("powershell.exe")
            .args([
                "-NoLogo",
                "-NoProfile",
                "-NonInteractive",
                "-Command",
                "(Get-Process -Id $PID).Path",
            ])
            .output()
            .unwrap();
        assert!(output.status.success());
        let powershell = PathBuf::from(String::from_utf8_lossy(&output.stdout).trim());
        let version = read_embedded_artifact_version(&powershell).unwrap();
        assert!(!version.file_version.is_empty());
        assert!(!version.product_version.is_empty());
    }

    #[test]
    fn side_effect_verification_rejects_effects_without_user_permission() {
        let fixture = tempfile::tempdir().unwrap();
        let workflow = WorkflowRunMetadata {
            action: "dev-work".into(),
            operation_key: "b".repeat(64),
            intent: "Inspect the selected issue.".into(),
            build_profile: None,
            issue_url: Some("https://github.com/owner/repo/issues/1".into()),
            scope_paths: vec![],
            requested_effects: vec![],
            side_effects: None,
            build_artifact: None,
        };
        let effects = WorkflowSideEffects {
            commit_sha: Some("0123456789abcdef0123456789abcdef01234567".into()),
            remote_branch: None,
            pull_request_url: None,
            verified: false,
            verification_message: None,
        };

        assert!(
            verify_reported_side_effects(fixture.path(), &workflow, &effects)
                .unwrap_err()
                .contains("not explicitly permitted")
        );
    }

    #[test]
    fn side_effect_verification_confirms_a_local_commit_object() {
        let fixture = tempfile::tempdir_in(env!("CARGO_MANIFEST_DIR")).unwrap();
        let git = |args: &[&str]| {
            Command::new("git")
                .args(args)
                .current_dir(fixture.path())
                .output()
                .unwrap()
        };
        assert!(git(&["init", "--quiet"]).status.success());
        assert!(git(&["config", "user.name", "Fixture User"])
            .status
            .success());
        assert!(git(&["config", "user.email", "fixture@example.invalid"])
            .status
            .success());
        fs::write(fixture.path().join("tracked.txt"), "fixture").unwrap();
        assert!(git(&["add", "tracked.txt"]).status.success());
        assert!(git(&["commit", "--quiet", "-m", "fixture"])
            .status
            .success());
        let sha = String::from_utf8(git(&["rev-parse", "HEAD"]).stdout)
            .unwrap()
            .trim()
            .to_owned();
        let workflow = WorkflowRunMetadata {
            action: "dev-work".into(),
            operation_key: "c".repeat(64),
            intent: "Commit the selected changes.".into(),
            build_profile: None,
            issue_url: Some("https://github.com/owner/repo/issues/1".into()),
            scope_paths: vec![],
            requested_effects: vec!["commit".into()],
            side_effects: None,
            build_artifact: None,
        };
        let effects = WorkflowSideEffects {
            commit_sha: Some(sha),
            remote_branch: None,
            pull_request_url: None,
            verified: false,
            verification_message: None,
        };

        assert!(verify_reported_side_effects(fixture.path(), &workflow, &effects).is_ok());
    }

    #[test]
    fn codex_preflight_allows_non_git_project_directories() {
        let fixture = tempfile::tempdir().unwrap();
        ensure_clean_git_worktree(fixture.path()).unwrap();
    }

    #[test]
    fn codex_permission_approval_uses_permission_profile_response() {
        let requested = serde_json::json!({
            "fileSystem": { "write": ["/workspace", "/shared"] },
            "network": { "enabled": true }
        });
        let pending = PendingCodexRequest {
            request_id: serde_json::json!(61),
            method: "item/permissions/requestApproval".into(),
            params: serde_json::json!({ "permissions": requested }),
        };
        assert_eq!(
            codex_approval_response(&pending, "accept").unwrap(),
            serde_json::json!({ "permissions": requested, "scope": "turn" })
        );
        assert_eq!(
            codex_approval_response(&pending, "decline").unwrap(),
            serde_json::json!({ "permissions": {}, "scope": "turn" })
        );
        assert!(codex_approval_response(&pending, "acceptForSession").is_err());
    }

    #[test]
    fn codex_command_approval_allows_safe_decline_when_choices_are_omitted() {
        let pending = PendingCodexRequest {
            request_id: serde_json::json!(62),
            method: "item/commandExecution/requestApproval".into(),
            params: serde_json::json!({}),
        };
        assert_eq!(
            codex_approval_response(&pending, "decline").unwrap(),
            serde_json::json!({ "decision": "decline" })
        );
        assert!(codex_approval_response(&pending, "accept").is_err());
    }

    #[test]
    fn approval_and_input_events_bypass_the_output_budget() {
        assert!(!output_budget_applies("approvalRequest"));
        assert!(!output_budget_applies("userInputRequest"));
        assert!(output_budget_applies("output"));
        assert!(output_budget_applies("providerError"));
    }

    #[test]
    fn codex_user_input_allows_skips_and_returns_each_requested_question() {
        let questions = vec![
            serde_json::json!({ "id": "choice" }),
            serde_json::json!({ "id": "freeform" }),
        ];
        let allowed = ["choice", "freeform"].into_iter().collect::<HashSet<_>>();
        let answers = HashMap::from([("choice".to_owned(), vec!["Option A".to_owned()])]);
        let response = codex_user_input_response(&questions, &allowed, &answers).unwrap();
        assert_eq!(response["answers"]["choice"]["answers"][0], "Option A");
        assert_eq!(
            response["answers"]["freeform"]["answers"],
            serde_json::json!([])
        );
        assert!(codex_user_input_response(
            &questions,
            &allowed,
            &HashMap::from([("unknown".to_owned(), vec!["value".to_owned()])])
        )
        .is_err());
    }

    #[cfg(windows)]
    #[test]
    fn script_launchers_preserve_paths_and_arguments_with_spaces() {
        let fixture = tempfile::tempdir().unwrap();
        let folder = fixture.path().join("Fixture Folder");
        fs::create_dir_all(&folder).unwrap();
        let cmd_file = folder.join("run file.cmd");
        fs::write(&cmd_file, "@echo off\necho %~1\necho %~2\n").unwrap();
        let mut cmd = command_for(
            &cmd_file,
            &folder,
            "cmd",
            &["first arg".into(), "second arg".into()],
        )
        .unwrap();
        let output = cmd.output().unwrap();
        assert!(
            output.status.success(),
            "{cmd:?}\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let text = String::from_utf8_lossy(&output.stdout);
        assert!(text.contains("first arg"), "{text}");
        assert!(text.contains("second arg"), "{text}");
        let ps_file = folder.join("run file.ps1");
        fs::write(
            &ps_file,
            "param($One, $Two)\nWrite-Output $One\nWrite-Output $Two\n",
        )
        .unwrap();
        let mut powershell = command_for(
            &ps_file,
            &folder,
            "powershell",
            &["first arg".into(), "second arg".into()],
        )
        .unwrap();
        let output = powershell.output().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let text = String::from_utf8_lossy(&output.stdout);
        assert!(text.contains("first arg"), "{text}");
        assert!(text.contains("second arg"), "{text}");
    }
}
