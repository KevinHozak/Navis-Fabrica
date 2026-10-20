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
use tauri::{AppHandle, Emitter};

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
    pub provider_profile_id: Option<String>,
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
    #[serde(default)]
    pub expected_head: Option<String>,
    #[serde(default)]
    pub expected_push_remote: Option<String>,
    #[serde(default)]
    pub expected_push_branch: Option<String>,
    #[serde(default)]
    pub commit_subject: Option<String>,
    #[serde(default)]
    pub commit_body: Option<String>,
    #[serde(default)]
    pub draft_snapshot_fingerprint: Option<String>,
    #[serde(default)]
    pub auto_merge_pull_request: bool,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CommitDraftContext {
    pub mode: String,
    pub snapshot_fingerprint: String,
    pub head: Option<String>,
    pub branch: String,
    pub paths: Vec<String>,
    pub remote: Option<String>,
    pub upstream_branch: Option<String>,
    pub ahead: u64,
    #[serde(skip_serializing)]
    pub diff_context: String,
}

pub fn capture_commit_draft_context(cwd: &Path) -> Result<CommitDraftContext, String> {
    let snapshot =
        git_worktree_snapshot(cwd)?.ok_or("The selected folder is not a Git worktree.")?;
    let root = &snapshot.root;
    let git = |args: &[&str]| -> Result<String, String> {
        let output = git_output(root, args)?;
        if !output.status.success() {
            return Err("Could not inspect the selected Git worktree.".into());
        }
        String::from_utf8(output.stdout)
            .map_err(|_| "Git returned unreadable repository details.".into())
    };
    for operation in [
        "rebase-merge",
        "rebase-apply",
        "MERGE_HEAD",
        "CHERRY_PICK_HEAD",
        "REVERT_HEAD",
    ] {
        let path = git(&["rev-parse", "--git-path", operation])?;
        if root.join(path.trim()).exists() {
            return Err("A Git operation is already in progress. Resolve or abort it before reviewing changes.".into());
        }
    }
    let branch = git(&["branch", "--show-current"])?;
    let branch = branch.trim().to_owned();
    if branch.is_empty() {
        return Err("Check out an attached branch before reviewing changes.".into());
    }
    let status = git(&["status", "--porcelain=v1", "-z", "--untracked-files=all"])?;
    let dirty = !status.is_empty();
    let head = git(&["rev-parse", "--verify", "HEAD"])
        .ok()
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty());
    let mut remote = None;
    let mut upstream_branch = None;
    let mut ahead = 0;
    let mode = if dirty {
        if snapshot.changed_paths.is_empty() {
            return Err("Git reports changes, but the changed-file list is unavailable.".into());
        }
        let remotes = git(&["remote"])?;
        if remotes.trim().is_empty() {
            "commit"
        } else {
            let origin = git(&["remote", "get-url", "origin"]).ok();
            if origin
                .as_deref()
                .is_none_or(|url| github_repository_from_remote(url.trim()).is_none())
            {
                return Err("A configured remote is not supported for Commit & Push; review it separately or remove it before a local commit.".into());
            }
            remote = Some("origin".to_owned());
            upstream_branch = Some(branch.clone());
            "commitPush"
        }
    } else {
        let (current_head, upstream_remote, branch_name) = capture_push_target(root, true)?;
        let counts = git(&["rev-list", "--left-right", "--count", "HEAD...@{upstream}"])?;
        let mut counts = counts.split_whitespace();
        ahead = counts
            .next()
            .and_then(|value| value.parse().ok())
            .unwrap_or(0);
        if ahead == 0 {
            return Err("There are no changes to commit or commits to push.".into());
        }
        remote = Some(upstream_remote);
        upstream_branch = Some(branch_name);
        let fingerprint = commit_draft_fingerprint(
            root,
            &snapshot.fingerprint,
            &head,
            &branch,
            remote.as_deref(),
            upstream_branch.as_deref(),
        )?;
        return Ok(CommitDraftContext {
            mode: "pushOnly".into(),
            snapshot_fingerprint: fingerprint,
            head: Some(current_head),
            branch,
            paths: Vec::new(),
            remote,
            upstream_branch,
            ahead,
            diff_context: git(&["log", "--format=%s", "@{upstream}..HEAD"])?,
        });
    };

    let untracked_output = git_output(root, &["ls-files", "--others", "--exclude-standard", "-z"])?;
    if !untracked_output.status.success() {
        return Err("Could not inspect untracked changes.".into());
    }
    let untracked = untracked_output
        .stdout
        .split(|byte| *byte == 0)
        .filter_map(|bytes| std::str::from_utf8(bytes).ok())
        .filter(|path| !path.is_empty())
        .map(|path| path.replace('\\', "/"))
        .collect::<HashSet<_>>();
    let mut context = String::new();
    for path in snapshot.changed_paths.iter().take(40) {
        if !safe_commit_context_path(path) {
            context.push_str(&format!(
                "\n[content omitted for sensitive or non-source path: {}]\n",
                safe_path_label(path)
            ));
            continue;
        }
        let file = root.join(path);
        let metadata_len = fs::metadata(&file).map(|meta| meta.len()).unwrap_or(0);
        if metadata_len > 32 * 1024 {
            context.push_str(&format!("\n[content omitted for large file: {path}]\n"));
            continue;
        }
        if untracked.contains(path) {
            let bytes = fs::read(&file)
                .map_err(|_| "Could not read a changed text file for its draft.".to_string())?;
            if bytes.contains(&0) || bytes.len() > 32 * 1024 {
                context.push_str(&format!(
                    "\n[content omitted for binary or large file: {path}]\n"
                ));
                continue;
            }
            let text = String::from_utf8(bytes)
                .map_err(|_| "Changed files must be UTF-8 text for AI drafting.".to_string())?;
            append_bounded_diff(
                &mut context,
                path,
                &format!("+++ {path} (new file) +++\n{text}"),
            );
        } else {
            let staged = git_output(
                root,
                &[
                    "diff",
                    "--cached",
                    "--no-ext-diff",
                    "--no-textconv",
                    "--no-color",
                    "--unified=2",
                    "--",
                    path,
                ],
            )?;
            let unstaged = git_output(
                root,
                &[
                    "diff",
                    "--no-ext-diff",
                    "--no-textconv",
                    "--no-color",
                    "--unified=2",
                    "--",
                    path,
                ],
            )?;
            if !staged.status.success() || !unstaged.status.success() {
                return Err("Could not read the selected text changes.".into());
            }
            let mut text = String::from_utf8_lossy(&staged.stdout).into_owned();
            text.push_str(&String::from_utf8_lossy(&unstaged.stdout));
            append_bounded_diff(&mut context, path, &text);
        }
        if context.len() >= 48 * 1024 {
            break;
        }
    }
    let fingerprint = commit_draft_fingerprint(
        root,
        &snapshot.fingerprint,
        &head,
        &branch,
        remote.as_deref(),
        upstream_branch.as_deref(),
    )?;
    Ok(CommitDraftContext {
        mode: mode.into(),
        snapshot_fingerprint: fingerprint,
        head,
        branch,
        paths: snapshot.changed_paths,
        remote,
        upstream_branch,
        ahead,
        diff_context: context,
    })
}

fn commit_draft_fingerprint(
    root: &Path,
    worktree: &str,
    head: &Option<String>,
    branch: &str,
    remote: Option<&str>,
    upstream_branch: Option<&str>,
) -> Result<String, String> {
    let remote_url = remote
        .and_then(|name| git_output(root, &["remote", "get-url", name]).ok())
        .filter(|output| output.status.success())
        .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_owned())
        .unwrap_or_default();
    let upstream = git_output(root, &["rev-parse", "--symbolic-full-name", "@{upstream}"])
        .ok()
        .filter(|output| output.status.success())
        .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_owned())
        .unwrap_or_default();
    let upstream_head = git_output(root, &["rev-parse", "--verify", "@{upstream}"])
        .ok()
        .filter(|output| output.status.success())
        .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_owned())
        .unwrap_or_default();
    let metadata = format!(
        "{worktree}\0{}\0{branch}\0{}\0{}\0{remote_url}\0{upstream}\0{upstream_head}",
        head.as_deref().unwrap_or("unborn"),
        remote.unwrap_or(""),
        upstream_branch.unwrap_or("")
    );
    let mut child = crate::process::command("git")
        .arg("-C")
        .arg(root)
        .args(["hash-object", "--stdin"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .map_err(|_| "Could not fingerprint the reviewed Git state.".to_string())?;
    child
        .stdin
        .take()
        .ok_or("Could not fingerprint the reviewed Git state.")?
        .write_all(metadata.as_bytes())
        .map_err(|_| "Could not fingerprint the reviewed Git state.".to_string())?;
    let output = child
        .wait_with_output()
        .map_err(|_| "Could not fingerprint the reviewed Git state.".to_string())?;
    if !output.status.success() {
        return Err("Could not fingerprint the reviewed Git state.".into());
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

pub fn safe_commit_context_path(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    let file = lower.rsplit('/').next().unwrap_or(&lower);
    !file.starts_with(".env")
        && ![
            "secret",
            "credential",
            "token",
            "password",
            "id_rsa",
            "id_ed25519",
        ]
        .iter()
        .any(|part| file.contains(part))
        && ![
            ".pem",
            ".key",
            ".p12",
            ".pfx",
            ".jks",
            ".keystore",
            ".db",
            ".sqlite",
            ".png",
            ".jpg",
            ".jpeg",
            ".gif",
            ".pdf",
            ".zip",
            ".exe",
            ".dll",
        ]
        .iter()
        .any(|suffix| file.ends_with(suffix))
}

fn safe_path_label(path: &str) -> &str {
    if safe_commit_context_path(path) {
        path
    } else {
        "[sensitive path]"
    }
}

fn append_bounded_diff(context: &mut String, path: &str, diff: &str) {
    let remaining = (48 * 1024usize).saturating_sub(context.len());
    if remaining == 0 {
        return;
    }
    context.push_str(&format!("\n--- {path} ---\n"));
    let remaining = (48 * 1024usize).saturating_sub(context.len());
    let end = diff
        .char_indices()
        .map(|(index, _)| index)
        .take_while(|index| *index <= remaining)
        .last()
        .unwrap_or(0);
    context.push_str(&diff[..end]);
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
        let output = crate::process::command("powershell.exe")
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
        .rev()
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

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
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
    #[serde(default)]
    pub fallback_decisions: Vec<crate::routing::FallbackDecision>,
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

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct OpenRouterStartRequest {
    pub cwd: String,
    pub prompt: String,
    pub test_command: Option<String>,
    pub timeout_seconds: u64,
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
    pub provider_profile_id: String,
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
    pub provider_profile_id: String,
    pub prompt: String,
    pub timeout_seconds: u64,
}

#[derive(Debug, Clone)]
pub struct OllamaRunConfig {
    pub executable: PathBuf,
    pub cwd: PathBuf,
    pub endpoint: String,
    pub model_id: String,
    pub provider_profile_id: String,
    pub prompt: String,
    pub test_command: Option<String>,
    pub timeout_seconds: u64,
}

#[derive(Debug, Clone)]
pub struct OpenRouterRunConfig {
    pub executable: PathBuf,
    pub cwd: PathBuf,
    pub model_id: String,
    pub provider_profile_id: String,
    pub api_key: String,
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
        let output = crate::process::command("powershell.exe")
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
        crate::process::command("kill")
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

pub struct ExternalMutationGuard {
    canonical_key: String,
    app: AppHandle,
    history_path: PathBuf,
    run_id: String,
    finished: bool,
}

impl ExternalMutationGuard {
    pub fn finish(&mut self, success: bool, message: &str) -> Result<(), String> {
        if self.finished {
            return Ok(());
        }
        let changed = if let Ok(mut state) = store().lock() {
            state.mutating_paths.remove(&self.canonical_key);
            if let Some(record) = state
                .records
                .iter_mut()
                .find(|record| record.id == self.run_id)
            {
                record.state = if success { "completed" } else { "failed" }.into();
                record.finished_at_ms = Some(now_ms());
                record.latency_ms = Some(now_ms().saturating_sub(record.started_at_ms));
                record.exit_code = Some(if success { 0 } else { 1 });
                record.events.push(RunEvent {
                    sequence: record.next_sequence,
                    timestamp_ms: now_ms(),
                    kind: if success { "completed" } else { "failed" }.into(),
                    text: message.chars().take(2000).collect(),
                });
                record.next_sequence += 1;
                let changed = record.clone();
                persist_records(&self.history_path, &state.records)?;
                Some(changed)
            } else {
                return Err("The Git action Activity record is missing.".into());
            }
        } else {
            return Err("Could not access workflow runs to finish the Git action.".into());
        };
        self.finished = true;
        if let Some(record) = changed {
            let _ = self.app.emit("workflow-run-updated", record);
        }
        Ok(())
    }
}

impl Drop for ExternalMutationGuard {
    fn drop(&mut self) {
        let _ = self.finish(false, "Git action stopped before completion.");
    }
}

pub fn reserve_external_mutation(
    app: &AppHandle,
    cwd: &Path,
    action: &str,
) -> Result<ExternalMutationGuard, String> {
    let history = history_path(app)?;
    let canonical_key = mutation_scope(cwd);
    let mut state = store()
        .lock()
        .map_err(|_| "Could not access workflow runs.".to_string())?;
    if state.records.is_empty() {
        state.records = load_records(&history)?;
    }
    if state.records.iter().any(|record| {
        mutation_scope(Path::new(&record.cwd)) == canonical_key
            && matches!(
                record.state.as_str(),
                "queued" | "running" | "needsInput" | "cancelling"
            )
    }) {
        return Err("A workflow is active for this project.".into());
    }
    if interrupted_process_owns_scope(&state.records, &canonical_key) {
        return Err("A prior interrupted workflow process is still running for this repository or worktree.".into());
    }
    if !state.mutating_paths.insert(canonical_key.clone()) {
        return Err("A repository operation is already active for this project.".into());
    }
    let run_id = format!(
        "git-{}-{}",
        now_ms(),
        NEXT_ID.fetch_add(1, Ordering::Relaxed)
    );
    let started_at_ms = now_ms();
    let record = RunRecord {
        id: run_id.clone(),
        executable: "git".into(),
        provider: None,
        provider_profile_id: None,
        model_id: None,
        cwd: cwd.to_string_lossy().into_owned(),
        thread_id: None,
        process_id: None,
        mutating: true,
        timeout_seconds: 30,
        state: "running".into(),
        started_at_ms,
        finished_at_ms: None,
        latency_ms: None,
        exit_code: None,
        next_sequence: 2,
        events: vec![RunEvent {
            sequence: 1,
            timestamp_ms: started_at_ms,
            kind: "started".into(),
            text: format!("Starting {action}."),
        }],
        workflow: Some(WorkflowRunMetadata {
            action: action.into(),
            operation_key: run_id.clone(),
            intent: String::new(),
            build_profile: None,
            issue_url: None,
            scope_paths: Vec::new(),
            requested_effects: Vec::new(),
            side_effects: None,
            build_artifact: None,
            expected_head: None,
            expected_push_remote: None,
            expected_push_branch: None,
            commit_subject: None,
            commit_body: None,
            draft_snapshot_fingerprint: None,
            auto_merge_pull_request: false,
        }),
        handoff: None,
    };
    state.records.insert(0, record.clone());
    state.records.truncate(MAX_RUNS);
    if let Err(error) = persist_records(&history, &state.records) {
        state.records.retain(|entry| entry.id != run_id);
        state.mutating_paths.remove(&canonical_key);
        return Err(error);
    }
    drop(state);
    let _ = app.emit("workflow-run-updated", record);
    Ok(ExternalMutationGuard {
        canonical_key,
        app: app.clone(),
        history_path: history,
        run_id,
        finished: false,
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct GitWorktreeSnapshot {
    root: PathBuf,
    changed_paths: Vec<String>,
    fingerprint: String,
}

fn git_output(root: &Path, args: &[&str]) -> Result<std::process::Output, String> {
    crate::process::command("git")
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

    let mut hash = crate::process::command("git")
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
    let status = crate::process::command("git")
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
        || workflow.scope_paths.len() > 10_000
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
    if workflow.commit_subject.is_some() != workflow.commit_body.is_some()
        || workflow.commit_subject.as_ref().is_some_and(|subject| {
            subject.trim().is_empty()
                || subject.len() > 200
                || subject.chars().any(char::is_control)
        })
        || workflow
            .commit_body
            .as_ref()
            .is_some_and(|body| body.len() > 4_000 || body.contains('\0'))
        || workflow
            .draft_snapshot_fingerprint
            .as_ref()
            .is_some_and(|fingerprint| {
                !(40..=64).contains(&fingerprint.len())
                    || !fingerprint.bytes().all(|byte| byte.is_ascii_hexdigit())
            })
    {
        return Err("The approved commit message or draft snapshot is invalid.".into());
    }
    if workflow.commit_subject.is_some()
        && (workflow.action != "dev-push"
            || !workflow
                .requested_effects
                .iter()
                .any(|effect| effect == "commit"))
    {
        return Err("Approved commit messages require an explicitly permitted commit.".into());
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
    if workflow.action == "dev-push"
        && workflow.scope_paths.is_empty()
        && !is_push_only_workflow(workflow)
    {
        return Err("Push changes requires at least one selected changed file.".into());
    }
    if workflow.auto_merge_pull_request
        && (workflow.action != "dev-push"
            || !workflow
                .requested_effects
                .iter()
                .any(|effect| effect == "createPullRequest"))
    {
        return Err(
            "Automatic merging requires an explicitly permitted reviewed pull request workflow."
                .into(),
        );
    }
    let mut paths = HashSet::new();
    let mut scope_bytes = 0usize;
    for path in &workflow.scope_paths {
        let parsed = Path::new(path);
        scope_bytes = scope_bytes.saturating_add(path.len());
        if path.trim().is_empty()
            || path.len() > 500
            || scope_bytes > 2 * 1024 * 1024
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

fn is_push_only_workflow(workflow: &WorkflowRunMetadata) -> bool {
    workflow.action == "dev-push"
        && workflow
            .requested_effects
            .iter()
            .any(|effect| effect == "push")
        && !workflow
            .requested_effects
            .iter()
            .any(|effect| effect == "commit")
        && workflow.scope_paths.is_empty()
}

fn verify_push_listing(listing: &str, branch: &str, expected_head: &str) -> Result<String, String> {
    let mut parts = listing.split_whitespace();
    let sha = parts
        .next()
        .ok_or("The configured upstream branch was not found.")?;
    let reference = parts
        .next()
        .ok_or("The configured upstream branch was not found.")?;
    if reference != format!("refs/heads/{branch}") || !sha.eq_ignore_ascii_case(expected_head) {
        return Err("The remote branch does not point to the expected commit.".into());
    }
    Ok(sha.to_owned())
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
    let branch = crate::process::command("git")
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
    let remotes = crate::process::command("git")
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
    let requires_github_remote = workflow.action == "dev-work"
        || workflow
            .requested_effects
            .iter()
            .any(|effect| matches!(effect.as_str(), "push" | "createPullRequest"));
    if repositories.is_empty() && requires_github_remote {
        return Err("Issue and remote push workflows require a configured GitHub remote.".into());
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

fn capture_push_only_target(cwd: &Path) -> Result<(String, String, String), String> {
    capture_push_target(cwd, false)
}

fn capture_push_target(cwd: &Path, allow_rebase: bool) -> Result<(String, String, String), String> {
    let git = |args: &[&str]| {
        let mut command_args = vec!["-C".to_owned(), cwd.to_string_lossy().into_owned()];
        command_args.extend(args.iter().map(|arg| (*arg).to_owned()));
        command_output(cwd, "git", &command_args)
    };
    let branch = git(&["branch", "--show-current"])?;
    let branch = branch.trim();
    if branch.is_empty() {
        return Err("Push-only requires an attached branch.".into());
    }
    let head = git(&["rev-parse", "--verify", "HEAD"])?;
    let head = head.trim();
    if head.is_empty() || !head.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err("Could not verify the current commit for push-only.".into());
    }
    let upstream = git(&["rev-parse", "--symbolic-full-name", "@{upstream}"])?;
    let upstream = upstream.trim();
    let upstream = upstream
        .strip_prefix("refs/remotes/")
        .ok_or("Push-only requires a configured remote-tracking upstream.")?;
    let (remote, upstream_branch) = upstream
        .split_once('/')
        .ok_or("Could not resolve the configured upstream branch.")?;
    if remote.is_empty() || remote == "." || upstream_branch.is_empty() {
        return Err("Push-only requires the current branch's configured remote upstream.".into());
    }
    let remote_url = git(&["remote", "get-url", remote])?;
    if github_repository_from_remote(remote_url.trim()).is_none() {
        return Err("Push-only requires a supported GitHub upstream.".into());
    }
    let counts = git(&["rev-list", "--left-right", "--count", "HEAD...@{upstream}"])?;
    let mut counts = counts.split_whitespace();
    let ahead = counts
        .next()
        .and_then(|count| count.parse::<u64>().ok())
        .ok_or("Could not verify commits ahead of upstream.")?;
    let behind = counts
        .next()
        .and_then(|count| count.parse::<u64>().ok())
        .ok_or("Could not verify commits behind upstream.")?;
    if ahead == 0 || (behind > 0 && !allow_rebase) {
        return Err("Push-only requires commits ahead of an upstream with no divergence.".into());
    }
    if !git(&["status", "--porcelain", "--untracked-files=all"])?
        .trim()
        .is_empty()
    {
        return Err("Push-only requires a clean working tree.".into());
    }
    Ok((
        head.to_owned(),
        remote.to_owned(),
        upstream_branch.to_owned(),
    ))
}

fn command_for(
    executable: &Path,
    cwd: &Path,
    host: &str,
    args: &[String],
) -> Result<Command, String> {
    let mut command = match host {
        "powershell" => {
            let mut command = crate::process::command("powershell.exe");
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
            let mut command = crate::process::command("cmd.exe");
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
            let mut command = crate::process::command(executable);
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
    run_bounded_command(executable, cwd, args, failure_message, 10)
}

fn run_bounded_command(
    executable: &Path,
    cwd: &Path,
    args: &[String],
    failure_message: &str,
    timeout_seconds: u64,
) -> Result<(std::process::ExitStatus, Vec<u8>), String> {
    let request = StartRequest {
        executable: executable.to_string_lossy().into_owned(),
        arguments: args.to_vec(),
        cwd: cwd.to_string_lossy().into_owned(),
        timeout_seconds,
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
    let deadline = Instant::now() + Duration::from_secs(timeout_seconds);
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
                terminate_process_tree(&mut child);
                let _ = child.wait();
                let _ = reader.join();
                return Err(failure_message.to_owned());
            }
            Err(_) => {
                terminate_process_tree(&mut child);
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
    let push_only = is_push_only_workflow(workflow);
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

    if push_only {
        let expected_head = workflow
            .expected_head
            .as_deref()
            .ok_or("The verified push target is missing its expected commit.")?;
        let expected_branch = workflow
            .expected_push_branch
            .as_deref()
            .ok_or("The verified push target is missing its upstream branch.")?;
        let current_head = command_output(
            cwd,
            "git",
            &[
                "-C".into(),
                cwd.to_string_lossy().into_owned(),
                "rev-parse".into(),
                "--verify".into(),
                "HEAD".into(),
            ],
        )?;
        if !current_head.trim().eq_ignore_ascii_case(expected_head) {
            return Err(
                "The current commit changed while the push-only workflow was running.".into(),
            );
        }
        let status = command_output(
            cwd,
            "git",
            &[
                "-C".into(),
                cwd.to_string_lossy().into_owned(),
                "status".into(),
                "--porcelain".into(),
                "--untracked-files=all".into(),
            ],
        )?;
        if !status.trim().is_empty() {
            return Err(
                "The push-only workflow changed files; review them separately before pushing."
                    .into(),
            );
        }
        if effects.remote_branch.as_deref() != Some(expected_branch) {
            return Err(
                "The reported pushed branch does not match the configured upstream.".into(),
            );
        }
    }
    if workflow
        .expected_push_branch
        .as_deref()
        .is_some_and(|expected| effects.remote_branch.as_deref() != Some(expected))
    {
        return Err("The reported pushed branch does not match the reviewed branch.".into());
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
        if let Some(subject) = workflow.commit_subject.as_deref() {
            let actual_subject = command_output(
                cwd,
                "git",
                &[
                    "-C".into(),
                    cwd.to_string_lossy().into_owned(),
                    "show".into(),
                    "-s".into(),
                    "--format=%s".into(),
                    commit_sha.into(),
                ],
            )?;
            let actual_body = command_output(
                cwd,
                "git",
                &[
                    "-C".into(),
                    cwd.to_string_lossy().into_owned(),
                    "show".into(),
                    "-s".into(),
                    "--format=%b".into(),
                    commit_sha.into(),
                ],
            )?;
            if actual_subject.trim_end_matches(['\r', '\n']) != subject
                || actual_body.trim_end_matches(['\r', '\n'])
                    != workflow
                        .commit_body
                        .as_deref()
                        .unwrap_or("")
                        .trim_end_matches(['\r', '\n'])
            {
                return Err("The created commit message differs from the approved message.".into());
            }
        }
    }

    let mut remote_sha = None;
    let mut repository = None;
    if permits("push") || permits("createPullRequest") || effects.remote_branch.is_some() {
        let remote_name = workflow.expected_push_remote.as_deref().unwrap_or("origin");
        let remote = command_output(
            cwd,
            "git",
            &[
                "-C".into(),
                cwd.to_string_lossy().into_owned(),
                "remote".into(),
                "get-url".into(),
                remote_name.into(),
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
        let commit_sha = if push_only {
            workflow
                .expected_head
                .as_deref()
                .ok_or("The verified push target is missing its expected commit.")?
        } else {
            effects
                .commit_sha
                .as_deref()
                .ok_or("The run requested a push but did not report its commit SHA.")?
        };
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
                workflow
                    .expected_push_remote
                    .as_deref()
                    .unwrap_or("origin")
                    .into(),
                format!("refs/heads/{branch}"),
            ],
        )?;
        remote_sha = Some(verify_push_listing(&listing, branch, commit_sha)?);
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
    let directory = crate::storage::data_root(app)?;
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
    if records.is_empty() {
        transaction
            .execute("DELETE FROM workflow_runs", [])
            .map_err(|_| "Could not delete run history.".to_string())?;
    } else {
        let placeholders = (1..=records.len())
            .map(|index| format!("?{index}"))
            .collect::<Vec<_>>()
            .join(", ");
        let query = format!("DELETE FROM workflow_runs WHERE id NOT IN ({placeholders})");
        transaction
            .execute(
                &query,
                rusqlite::params_from_iter(records.iter().map(|record| record.id.as_str())),
            )
            .map_err(|_| "Could not remove deleted run history.".to_string())?;
    }
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

fn budgeted_output(text: &str, already_used: usize) -> (String, usize, bool) {
    let accepted = text
        .len()
        .min(MAX_OUTPUT_BYTES.saturating_sub(already_used));
    let mut end = accepted;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    (text[..end].to_owned(), end, end < text.len())
}

fn emit_event(app: &AppHandle, path: &Path, id: &str, kind: &str, text: &str) {
    let mut store = match store().lock() {
        Ok(value) => value,
        Err(_) => return,
    };
    if !store.records.iter().any(|record| record.id == id) {
        return;
    }
    // Keep the bounded script's phase/receipt lines even when compiler logs
    // exhaust the ordinary output allowance.
    let build_control = kind == "stdout"
        && text.len() <= MAX_OUTPUT_CHUNK
        && (text.starts_with("BUILD_STEP:") || text.starts_with("NAVIS_BUILD_ARTIFACT:"))
        && store
            .records
            .iter()
            .any(|record| record.id == id && record.provider.as_deref() == Some("build-script"));
    let git_control = kind == "stdout"
        && text.len() <= MAX_OUTPUT_CHUNK
        && (text.starts_with("GIT_STEP:")
            || text.starts_with("NAVIS_SIDE_EFFECTS:")
            || text.starts_with("NAVIS_PR_MERGE:")
            || text.starts_with("NAVIS_LOCAL_SYNC:"))
        && store
            .records
            .iter()
            .any(|record| record.id == id && record.provider.as_deref() == Some("git"));
    let uses_output_budget = output_budget_applies(kind) && !build_control && !git_control;
    if uses_output_budget && *store.output_bytes.get(id).unwrap_or(&0) >= MAX_OUTPUT_BYTES {
        return;
    }
    let secrets = store.redaction_values.get(id).cloned().unwrap_or_default();
    let safe_text = redact(text, &secrets);
    let output_bytes = store.output_bytes.entry(id.to_owned()).or_default();
    let (text, accepted, truncated) = if uses_output_budget {
        budgeted_output(&safe_text, *output_bytes)
    } else {
        (safe_text.clone(), safe_text.len(), false)
    };
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
        kind: if build_control && text.starts_with("BUILD_STEP:") {
            "buildProgress".into()
        } else if git_control && text.starts_with("GIT_STEP:") {
            "gitProgress".into()
        } else {
            kind.to_owned()
        },
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
    let effects = if record.provider.as_deref() == Some("git") && kind == "stdout" {
        parse_side_effect_marker(&event.text)
    } else {
        parse_side_effect_marker(&combined_output)
    };
    if let (Some(effects), Some(workflow)) = (effects, record.workflow.as_mut()) {
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
    trim_run_events(&mut record.events);
    let _ = persist_records(path, &store.records);
    let _ = app.emit("workflow-run-event", (run_id, event));
}

fn trim_run_events(events: &mut Vec<RunEvent>) {
    if events.len() <= MAX_EVENTS {
        return;
    }
    let phase = events
        .iter()
        .rev()
        .find(|event| matches!(event.kind.as_str(), "buildProgress" | "gitProgress"))
        .cloned();
    events.drain(0..events.len() - MAX_EVENTS);
    if let Some(phase) = phase {
        if !events.iter().any(|event| event.sequence == phase.sequence) {
            events.remove(0);
            events.insert(0, phase);
        }
    }
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
pub fn export_workflow_diagnostics(app: AppHandle) -> Result<String, String> {
    let records = list_workflow_runs(app)?;
    diagnostics_json(&records, now_ms())
}

fn diagnostics_json(records: &[RunRecord], exported_at_ms: u128) -> Result<String, String> {
    let mut states = std::collections::BTreeMap::<String, usize>::new();
    let mut providers = std::collections::BTreeMap::<String, usize>::new();
    let mut actions = std::collections::BTreeMap::<String, usize>::new();
    let mut event_kinds = std::collections::BTreeMap::<String, usize>::new();
    let mut event_count = 0usize;
    for record in records {
        let state = if matches!(
            record.state.as_str(),
            "queued"
                | "running"
                | "needsInput"
                | "cancelling"
                | "completed"
                | "failed"
                | "timedOut"
                | "cancelled"
                | "interrupted"
        ) {
            record.state.as_str()
        } else {
            "other"
        };
        *states.entry(state.to_owned()).or_default() += 1;
        if let Some(provider) = record.provider.as_deref().filter(|provider| {
            matches!(*provider, "codex" | "antigravity" | "ollama" | "openRouter")
        }) {
            *providers.entry(provider.to_owned()).or_default() += 1;
        }
        if let Some(action) = record
            .workflow
            .as_ref()
            .map(|workflow| workflow.action.as_str())
        {
            if matches!(action, "dev-work" | "dev-push" | "dev-build") {
                *actions.entry(action.to_owned()).or_default() += 1;
            }
        }
        event_count += record.events.len();
        for event in &record.events {
            let kind = if matches!(
                event.kind.as_str(),
                "output" | "state" | "providerError" | "approvalRequest" | "userInputRequest"
            ) {
                event.kind.as_str()
            } else {
                "other"
            };
            *event_kinds.entry(kind.to_owned()).or_default() += 1;
        }
    }
    serde_json::to_string_pretty(&serde_json::json!({
        "schemaVersion": 1,
        "applicationVersion": env!("CARGO_PKG_VERSION"),
        "exportedAtMs": exported_at_ms,
        "entrypointToMainPageLoadMs": crate::startup_measurement_ms(),
        "retention": { "maximumRuns": MAX_RUNS, "maximumEventsPerRun": MAX_EVENTS, "maximumOutputBytesPerRun": MAX_OUTPUT_BYTES },
        "history": { "runCount": records.len(), "eventCount": event_count, "states": states, "providers": providers, "actions": actions, "eventKinds": event_kinds }
    }))
    .map_err(|_| "Could not encode diagnostics export.".to_string())
}

#[tauri::command]
pub fn delete_finished_workflow_runs(app: AppHandle) -> Result<usize, String> {
    let path = history_path(&app)?;
    let mut state = store()
        .lock()
        .map_err(|_| "Could not access workflow runs.".to_string())?;
    if state.records.is_empty() {
        state.records = load_records(&path)?;
    }
    let deleted = retain_active_runs(&mut state.records);
    if deleted > 0 {
        persist_records(&path, &state.records)?;
    }
    Ok(deleted)
}

fn retain_active_runs(records: &mut Vec<RunRecord>) -> usize {
    let before = records.len();
    records.retain(|record| {
        matches!(
            record.state.as_str(),
            "queued" | "running" | "needsInput" | "cancelling"
        )
    });
    before - records.len()
}

#[tauri::command]
pub fn start_workflow_run(app: AppHandle, request: StartRequest) -> Result<RunRecord, String> {
    start_run(app, request, None, None, None, None, None, None, Vec::new())
}

fn script_version_state(app: &AppHandle) -> Result<PathBuf, String> {
    let root = crate::storage::data_root(app)?;
    let versions = std::env::var_os("FABRICA_VERSION_STATE_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            if root.file_name().is_some_and(|name| name == "desktop") {
                root.parent().unwrap_or(&root).join("versions")
            } else {
                root.join("version-preparation")
            }
        });
    Ok(versions)
}

fn install_version_helpers(inputs: &Path) -> Result<(), String> {
    for (name, content) in [
        (
            "prepare_version.py",
            include_str!("../../scripts/dev-version/prepare_version.py"),
        ),
        (
            "set_version.py",
            include_str!("../../scripts/dev-version/set_version.py"),
        ),
        (
            "version_workflow.py",
            include_str!("../scripts/version_workflow.py"),
        ),
    ] {
        fs::write(inputs.join(name), content)
            .map_err(|_| "Could not prepare the shared version helpers.".to_string())?;
    }
    Ok(())
}

pub fn prepare_commit_review(app: &AppHandle, cwd: &Path) -> Result<CommitDraftContext, String> {
    let context = capture_commit_draft_context(cwd)?;
    if context.mode == "pushOnly" || !cwd.join("src-tauri/tauri.conf.json").is_file() {
        return Ok(context);
    }
    let mut guard = reserve_external_mutation(app, cwd, "prepare-version")?;
    let result = (|| {
        let python =
            crate::providers::find_executable("python", std::env::var_os("PATH").as_deref())
                .ok_or("Python 3.11+ is required for Tauri version preparation.")?;
        preflight_command(
            &python,
            cwd,
            &[
                "-c".into(),
                "import sys; assert sys.version_info >= (3, 11)".into(),
            ],
            "Python 3.11+ is required for Tauri version preparation.",
        )?;
        let inputs = history_path(app)?
            .parent()
            .ok_or("The version helper folder is unavailable.")?
            .join(format!(
                "version-review-{}-{}",
                now_ms(),
                NEXT_ID.fetch_add(1, Ordering::Relaxed)
            ));
        fs::create_dir_all(&inputs)
            .map_err(|_| "Could not prepare the version helper folder.".to_string())?;
        install_version_helpers(&inputs)?;
        let (status, output) = run_bounded_command(
            &python,
            cwd,
            &[
                inputs
                    .join("version_workflow.py")
                    .to_string_lossy()
                    .into_owned(),
                "--repo".into(),
                cwd.to_string_lossy().into_owned(),
                "--state-dir".into(),
                script_version_state(app)?.to_string_lossy().into_owned(),
                "--action".into(),
                "prepare".into(),
            ],
            "Version preparation did not complete.",
            60,
        )?;
        let message = String::from_utf8_lossy(&output).trim().to_owned();
        if !status.success() {
            return Err(format!("Version preparation failed: {message}"));
        }
        let value: serde_json::Value = serde_json::from_str(&message)
            .map_err(|_| "Could not read version preparation result.".to_string())?;
        let refreshed = capture_commit_draft_context(cwd)?;
        Ok((
            refreshed,
            format!(
                "Version {}: {}.",
                value["version"].as_str().unwrap_or("unknown"),
                value["action"].as_str().unwrap_or("prepared")
            ),
        ))
    })();
    match result {
        Ok((context, message)) => {
            guard.finish(true, &message)?;
            Ok(context)
        }
        Err(error) => {
            let _ = guard.finish(false, &error);
            Err(error)
        }
    }
}

pub fn start_script_build_run(
    app: AppHandle,
    cwd: &Path,
    workflow: WorkflowRunMetadata,
) -> Result<RunRecord, String> {
    validate_workflow_metadata(&workflow)?;
    if workflow.action != "dev-build"
        || !workflow.scope_paths.is_empty()
        || workflow.issue_url.is_some()
        || workflow.draft_snapshot_fingerprint.is_some()
    {
        return Err("This command accepts only a Tauri build profile.".into());
    }
    let cwd = cwd
        .canonicalize()
        .map_err(|_| "The project folder could not be resolved.".to_string())?;
    if !cwd.join("src-tauri/tauri.conf.json").is_file() || !cwd.join("package.json").is_file() {
        return Err("The scripted build supports Tauri projects with package.json.".into());
    }
    let python = crate::providers::find_executable("python", std::env::var_os("PATH").as_deref())
        .ok_or("Python 3.11 or newer is required by the bundled version/build helper.")?;
    preflight_command(
        &python,
        &cwd,
        &[
            "-c".into(),
            "import sys; assert sys.version_info >= (3, 11)".into(),
        ],
        "Python 3.11 or newer is required by the bundled version/build helper.",
    )?;
    let versions = script_version_state(&app)?;
    let inputs = history_path(&app)?
        .parent()
        .ok_or("The build input folder is unavailable.")?
        .join(format!(
            "script-build-{}-{}",
            now_ms(),
            NEXT_ID.fetch_add(1, Ordering::Relaxed)
        ));
    fs::create_dir_all(&inputs)
        .map_err(|_| "Could not prepare the build helper folder.".to_string())?;
    install_version_helpers(&inputs)?;
    fs::write(
        inputs.join("build_project.py"),
        include_str!("../scripts/build_project.py"),
    )
    .map_err(|_| "Could not prepare the bundled build helper.".to_string())?;
    let profile = workflow
        .build_profile
        .as_deref()
        .ok_or("Choose a build profile.")?;
    let request = StartRequest {
        executable: python.to_string_lossy().into_owned(),
        arguments: vec![
            "-u".into(),
            inputs
                .join("build_project.py")
                .to_string_lossy()
                .into_owned(),
            "--repo".into(),
            cwd.to_string_lossy().into_owned(),
            "--profile".into(),
            profile.into(),
            "--state-dir".into(),
            versions.to_string_lossy().into_owned(),
        ],
        cwd: cwd.to_string_lossy().into_owned(),
        timeout_seconds: 3600,
        mutating: true,
    };
    start_run_with_workflow(
        app,
        request,
        None,
        Some("build-script"),
        None,
        None,
        None,
        None,
        Vec::new(),
        Some(workflow),
    )
}

pub fn start_reviewed_git_run(
    app: AppHandle,
    cwd: &Path,
    mut workflow: WorkflowRunMetadata,
) -> Result<RunRecord, String> {
    validate_workflow_metadata(&workflow)?;
    if workflow.action != "dev-push"
        || workflow.issue_url.is_some()
        || workflow
            .requested_effects
            .iter()
            .any(|effect| !matches!(effect.as_str(), "commit" | "push" | "createPullRequest"))
    {
        return Err(
            "This operation supports only reviewed commit, push and pull request effects.".into(),
        );
    }
    let context = capture_commit_draft_context(cwd)?;
    let mut expected_effects = match context.mode.as_str() {
        "commit" => vec!["commit"],
        "commitPush" => vec!["commit", "push"],
        "pushOnly" => vec!["push"],
        _ => return Err("The reviewed Git action is unsupported.".into()),
    };
    let allow_pull_request = workflow
        .requested_effects
        .iter()
        .any(|effect| effect == "createPullRequest");
    if allow_pull_request && context.mode != "commit" {
        expected_effects.push("createPullRequest");
    }
    if workflow.draft_snapshot_fingerprint.as_deref() != Some(context.snapshot_fingerprint.as_str())
        || workflow.scope_paths != context.paths
        || workflow.requested_effects != expected_effects
        || (context.mode != "pushOnly" && workflow.commit_subject.is_none())
    {
        return Err("The reviewed Git scope changed. Refresh the dialog.".into());
    }
    let cwd = cwd
        .canonicalize()
        .map_err(|_| "Could not resolve the repository.".to_string())?;
    verify_workflow_repository(&cwd, &workflow)?;
    let read = |args: &[&str]| -> Result<String, String> {
        let output = git_output(&cwd, args)?;
        if !output.status.success() {
            return Err("Could not capture the reviewed Git state.".into());
        }
        Ok(String::from_utf8_lossy(&output.stdout)
            .lines()
            .collect::<Vec<_>>()
            .join("\n")
            .trim_end_matches(['\r', '\n'])
            .to_string())
    };
    let status = read(&["status", "--porcelain=v1", "--untracked-files=all"])?;
    let index = read(&["ls-files", "--stage"])?;
    let mut files = Vec::new();
    for path in &context.paths {
        let file = cwd.join(path);
        if file.is_dir() {
            return Err("Directory and submodule changes require separate review.".into());
        }
        let hash = if file.is_file() {
            read(&["hash-object", "--no-filters", "--", path])?
        } else {
            String::new()
        };
        files.push(serde_json::json!({"path":path,"hash":hash}));
    }
    let remote_url = context
        .remote
        .as_deref()
        .map(|remote| read(&["remote", "get-url", remote]))
        .transpose()?;
    // Recheck after reading all per-file state; never bless a mixed snapshot.
    if capture_commit_draft_context(&cwd)?.snapshot_fingerprint != context.snapshot_fingerprint {
        return Err("Repository changes shifted during preflight. Refresh the dialog.".into());
    }
    workflow.expected_head = context.head.clone();
    workflow.expected_push_remote = context.remote.clone();
    workflow.expected_push_branch = context.upstream_branch.clone();
    let inputs = history_path(&app)?
        .parent()
        .ok_or("The run input folder is unavailable.")?
        .join(format!(
            "reviewed-git-{}-{}",
            now_ms(),
            NEXT_ID.fetch_add(1, Ordering::Relaxed)
        ));
    fs::create_dir_all(&inputs)
        .map_err(|_| "Could not create the Git operation inputs.".to_string())?;
    let script = inputs.join("commit-reviewed.ps1");
    let input = inputs.join("request.json");
    fs::write(&script, include_str!("../scripts/commit-reviewed.ps1"))
        .map_err(|_| "Could not prepare the Git script.".to_string())?;
    let version_helper = if cwd.join("src-tauri/tauri.conf.json").is_file() {
        install_version_helpers(&inputs)?;
        Some(inputs.join("version_workflow.py"))
    } else {
        None
    };
    let python = if version_helper.is_some() {
        Some(
            crate::providers::find_executable("python", std::env::var_os("PATH").as_deref())
                .ok_or("Python 3.11+ is required for Tauri version verification.")?,
        )
    } else {
        None
    };
    let version_state = script_version_state(&app)?;
    let data = serde_json::json!({
        "cwd":cwd, "head":context.head.unwrap_or_default(), "branch":context.branch,
        "python":python,"versionHelper":version_helper,"versionStateDir":version_state,
        "status":status, "index":index, "files":files,
        "remote":context.remote, "remoteUrl":remote_url, "remoteBranch":context.upstream_branch,
        "allowPullRequest":allow_pull_request,
        "autoMergePullRequest":workflow.auto_merge_pull_request,
        "mergeWaitSeconds":540,
        "githubRepository":remote_url.as_deref().and_then(github_repository_from_remote),
        "ghExecutable":crate::providers::find_executable("gh", std::env::var_os("PATH").as_deref()),
        "commit":expected_effects.contains(&"commit"), "push":expected_effects.contains(&"push"),
        "subject":workflow.commit_subject, "body":workflow.commit_body,
    });
    fs::write(
        &input,
        serde_json::to_vec(&data).map_err(|_| "Could not serialize Git inputs.".to_string())?,
    )
    .map_err(|_| "Could not save the Git inputs.".to_string())?;
    let request = StartRequest {
        executable: script.to_string_lossy().into_owned(),
        arguments: vec!["-RequestPath".into(), input.to_string_lossy().into_owned()],
        cwd: cwd.to_string_lossy().into_owned(),
        timeout_seconds: 600,
        mutating: true,
    };
    start_run_with_workflow(
        app,
        request,
        None,
        Some("git"),
        None,
        None,
        None,
        None,
        Vec::new(),
        Some(workflow),
    )
}

pub fn start_codex_workflow_run(
    app: AppHandle,
    config: CodexRunConfig,
) -> Result<RunRecord, String> {
    let mut prompt = config
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
    let mut config = config;
    if let Some(workflow) = &mut config.workflow {
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
        if let Some(expected) = workflow.draft_snapshot_fingerprint.as_deref() {
            let actual = capture_commit_draft_context(&cwd)?;
            if actual.snapshot_fingerprint != expected {
                return Err("Repository changes shifted after this draft. Refresh the draft before confirming.".into());
            }
        }
        verify_workflow_repository(&cwd, workflow)?;
        if is_push_only_workflow(workflow) {
            let (head, remote, branch) = capture_push_only_target(&cwd)?;
            workflow.expected_head = Some(head);
            workflow.expected_push_remote = Some(remote);
            workflow.expected_push_branch = Some(branch);
        } else if workflow.draft_snapshot_fingerprint.is_some()
            && workflow
                .requested_effects
                .iter()
                .any(|effect| effect == "push")
        {
            let target = capture_commit_draft_context(&cwd)?;
            if target.mode != "commitPush"
                || target.snapshot_fingerprint
                    != workflow
                        .draft_snapshot_fingerprint
                        .as_deref()
                        .unwrap_or_default()
            {
                return Err(
                    "The reviewed GitHub push target changed. Refresh the draft before confirming."
                        .into(),
                );
            }
            workflow.expected_push_remote = target.remote;
            workflow.expected_push_branch = target.upstream_branch;
        } else {
            workflow.expected_head = None;
            workflow.expected_push_remote = None;
            workflow.expected_push_branch = None;
        }
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
    if let Some(workflow) = &config.workflow {
        if let Some(subject) = workflow.commit_subject.as_deref() {
            let body = workflow.commit_body.as_deref().unwrap_or("");
            prompt.push_str(&format!(
                "\n\nUser-approved commit message (use exactly; do not edit):\nSubject: {subject}\nBody:\n{body}\n\nDo not change this message. Do not commit files outside the approved scope."
            ));
        }
    }
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
    let provider_profile_id = config.provider_profile_id.clone();
    let handoff = config.handoff.clone();
    start_run(
        app,
        request,
        Some(config),
        Some("codex"),
        Some(model_id),
        Some(provider_profile_id),
        handoff,
        None,
        Vec::new(),
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
        Some(config.provider_profile_id),
        handoff,
        None,
        Vec::new(),
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
        fallback_decisions: source
            .handoff
            .as_ref()
            .map(|handoff| handoff.fallback_decisions.clone())
            .unwrap_or_default(),
        created_at_ms: now_ms(),
    };
    source.handoff = Some(packet.clone());
    persist_records(&path, &state.records)?;
    Ok(packet)
}

fn source_provider_profile_matches(
    saved_profile_id: Option<&str>,
    candidate_profile_id: &str,
) -> bool {
    saved_profile_id == Some(candidate_profile_id)
}

fn workflow_effects_reconciled(workflow: Option<&WorkflowRunMetadata>) -> bool {
    workflow.is_none_or(|workflow| {
        if workflow.requested_effects.is_empty() && workflow.side_effects.is_none() {
            return true;
        }
        workflow
            .side_effects
            .as_ref()
            .is_some_and(|effects| effects.verified)
    })
}

pub fn evaluate_provider_fallback(
    app: &AppHandle,
    mut request: crate::routing::FallbackRequest,
) -> Result<crate::routing::FallbackDecision, String> {
    let path = history_path(app)?;
    let mut state = store()
        .lock()
        .map_err(|_| "Could not access workflow runs.".to_string())?;
    if state.records.is_empty() {
        state.records = load_records(&path)?;
    }
    let source_index = state
        .records
        .iter()
        .position(|record| record.id == request.source_run_id)
        .ok_or("The source run is no longer in saved workflow history.")?;
    let source = state.records[source_index].clone();
    let Some(mut handoff) = source.handoff.clone() else {
        return Err(
            "Prepare a worktree-verified handoff checkpoint before evaluating fallback.".into(),
        );
    };
    if !matches!(source.state.as_str(), "failed" | "timedOut" | "interrupted") {
        return Err("Fallback can be evaluated only after a failed, timed-out, or interrupted run reaches a terminal state.".into());
    }
    let current_candidate = request
        .routing
        .candidates
        .iter()
        .find(|candidate| candidate.provider_id == request.current_provider_id)
        .ok_or("The source provider must be included in the routing candidates.")?;
    if !source_provider_profile_matches(
        source.provider_profile_id.as_deref(),
        &current_candidate.provider_id,
    ) || source.provider.as_deref() != Some(current_candidate.telemetry_provider_id.as_str())
        || source.model_id.as_deref() != Some(current_candidate.model_id.as_str())
    {
        return Err("The exact source provider profile and model must match the saved run; older runs without profile identity cannot be used for fallback.".into());
    }
    if matches!(
        request.trigger,
        crate::routing::FallbackTrigger::QuotaBoundary
    ) && !crate::routing::has_confirmed_quota_boundary(
        &request.routing,
        &current_candidate.telemetry_provider_id,
    ) {
        request.trigger = crate::routing::FallbackTrigger::QuotaUnknown;
    }

    request.source_run_terminal = true;
    request.safe_checkpoint_ref = None;
    if let (Some(expected), Ok(Some(snapshot))) = (
        handoff.working_tree_fingerprint.as_deref(),
        git_worktree_snapshot(Path::new(&source.cwd)),
    ) {
        if snapshot.fingerprint == expected {
            request.safe_checkpoint_ref = Some(expected.to_owned());
        }
    }
    request.effects_reconciled = workflow_effects_reconciled(source.workflow.as_ref())
        && handoff
            .completed_side_effects
            .as_ref()
            .is_none_or(|effects| effects.verified);
    let visited = handoff
        .fallback_decisions
        .iter()
        .filter(|decision| decision.accepted)
        .flat_map(|decision| {
            decision
                .target_provider_id
                .iter()
                .chain(std::iter::once(&decision.source_provider_id))
        })
        .cloned()
        .collect::<HashSet<_>>();
    request.visited_provider_ids = visited.into_iter().collect();
    request.handoff_count = handoff
        .fallback_decisions
        .iter()
        .filter(|decision| decision.accepted)
        .count()
        .min(u8::MAX as usize) as u8;
    let decision = crate::routing::evaluate_fallback(request);
    handoff.fallback_decisions.push(decision.clone());
    if handoff.fallback_decisions.len() > 16 {
        handoff.fallback_decisions.remove(0);
    }
    state.records[source_index].handoff = Some(handoff);
    persist_records(&path, &state.records)?;
    let snapshot = state.records[source_index].clone();
    drop(state);
    let _ = app.emit("workflow-run-updated", snapshot);
    Ok(decision)
}

pub fn saved_provider_handoff(
    app: &AppHandle,
    source_run_id: &str,
    target_provider_id: &str,
    target_model_id: &str,
    target_is_local: bool,
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
        .filter(|packet| {
            approved_fallback_matches(packet, target_provider_id, target_model_id, target_is_local)
        })
        .ok_or_else(|| {
            "Create a manual handoff for this source run and selected target provider first, and keep any approved fallback on its recorded provider and model.".into()
        })
}

fn approved_fallback_matches(
    packet: &ProviderHandoff,
    target_provider_id: &str,
    target_model_id: &str,
    target_is_local: bool,
) -> bool {
    packet
        .fallback_decisions
        .iter()
        .rev()
        .next()
        .is_none_or(|decision| {
            (!decision.local_only || target_is_local)
                && (!decision.accepted
                    || (decision.target_provider_id.as_deref() == Some(target_provider_id)
                        && decision.target_model_id.as_deref() == Some(target_model_id)))
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
        Some(config.provider_profile_id),
        handoff,
        Some(config.endpoint),
        Vec::new(),
    )
}

pub fn start_openrouter_workflow_run(
    app: AppHandle,
    config: OpenRouterRunConfig,
    handoff: Option<ProviderHandoff>,
) -> Result<RunRecord, String> {
    let prompt = handoff
        .as_ref()
        .map(|packet| format!("{}{}", config.prompt, format_handoff_prompt(packet)))
        .unwrap_or(config.prompt);
    if prompt.trim().is_empty() || prompt.len() > 60_000 || prompt.contains('\0') {
        return Err("Enter a prompt of 1 to 60,000 characters for OpenRouter.".into());
    }
    if config.model_id.trim().is_empty()
        || config.model_id.len() > 160
        || config.model_id.chars().any(char::is_control)
    {
        return Err("Select a valid configured OpenRouter model.".into());
    }
    if !crate::providers::valid_openrouter_key(&config.api_key) {
        return Err("The saved OpenRouter credential is invalid.".into());
    }
    if config.timeout_seconds == 0 || config.timeout_seconds > 86_400 {
        return Err("The OpenRouter run timeout must be between 1 and 86,400 seconds.".into());
    }
    if config
        .test_command
        .as_ref()
        .is_some_and(|value| value.len() > 2_048 || value.chars().any(char::is_control))
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
    let env_file = if cfg!(windows) { "NUL" } else { "/dev/null" };
    let request = StartRequest {
        executable: config.executable.to_string_lossy().into_owned(),
        arguments: crate::providers::aider_openrouter_arguments(
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
        Some("openrouter"),
        Some(model_id),
        Some(config.provider_profile_id),
        handoff,
        None,
        vec![("OPENROUTER_API_KEY".into(), config.api_key)],
    )
}

fn start_run(
    app: AppHandle,
    request: StartRequest,
    codex: Option<CodexRunConfig>,
    provider: Option<&'static str>,
    model_id: Option<String>,
    provider_profile_id: Option<String>,
    handoff: Option<ProviderHandoff>,
    ollama_endpoint: Option<String>,
    provider_environment: Vec<(String, String)>,
) -> Result<RunRecord, String> {
    start_run_with_workflow(
        app,
        request,
        codex,
        provider,
        model_id,
        provider_profile_id,
        handoff,
        ollama_endpoint,
        provider_environment,
        None,
    )
}

fn start_run_with_workflow(
    app: AppHandle,
    request: StartRequest,
    codex: Option<CodexRunConfig>,
    provider: Option<&'static str>,
    model_id: Option<String>,
    provider_profile_id: Option<String>,
    handoff: Option<ProviderHandoff>,
    ollama_endpoint: Option<String>,
    provider_environment: Vec<(String, String)>,
    script_workflow: Option<WorkflowRunMetadata>,
) -> Result<RunRecord, String> {
    if provider != Some("git")
        && script_workflow
            .as_ref()
            .or_else(|| codex.as_ref().and_then(|config| config.workflow.as_ref()))
            .is_some_and(|value| value.auto_merge_pull_request)
    {
        return Err("Automatic PR merging is supported only by the reviewed Git script.".into());
    }

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
        .chain(provider_environment.iter().map(|(_, value)| value.clone()))
        .collect::<Vec<_>>();
    let record = RunRecord {
        id: id.clone(),
        executable: executable.to_string_lossy().into_owned(),
        provider: provider.map(str::to_owned),
        provider_profile_id,
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
        workflow: script_workflow
            .or_else(|| codex.as_ref().and_then(|config| config.workflow.clone())),
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
        if state.mutating_paths.contains(&canonical_key) {
            return Err(
                "A repository operation is already active for this project or worktree.".into(),
            );
        }
        if request.mutating && !state.mutating_paths.insert(canonical_key.clone()) {
            return Err(
                "A mutating workflow is already active for this repository or worktree.".into(),
            );
        }
        // Recheck reviewed repository state after reserving this checkout. The
        // model draft can take time, so the earlier preflight alone cannot
        // prevent another app operation from changing the approved scope.
        if let Some(expected) = record
            .workflow
            .as_ref()
            .and_then(|workflow| workflow.draft_snapshot_fingerprint.as_deref())
        {
            let current =
                capture_commit_draft_context(&cwd).map(|context| context.snapshot_fingerprint);
            if current.as_deref().ok() != Some(expected) {
                if request.mutating {
                    state.mutating_paths.remove(&canonical_key);
                }
                return Err("Repository changes shifted before the workflow started. Refresh the draft before confirming.".into());
            }
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
    for (name, value) in provider_environment {
        if name != "OPENROUTER_API_KEY" || !crate::providers::valid_openrouter_key(&value) {
            set_state(&app, &path, &id, "failed", None);
            return Err("The provider environment configuration is invalid.".into());
        }
        command
            .env_remove("OPENROUTER_API_KEY")
            .env("OPENROUTER_API_KEY", value);
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

pub(crate) fn terminate_process_tree(child: &mut Child) {
    #[cfg(windows)]
    {
        let id = child.id().to_string();
        let _ = crate::process::command("taskkill.exe")
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

    fn fixture_run(id: &str, state: &str) -> RunRecord {
        RunRecord {
            id: id.into(),
            executable: "C:\\Users\\private\\secret-tool.exe".into(),
            provider: Some("codex".into()),
            provider_profile_id: Some("private-profile".into()),
            model_id: Some("private-model".into()),
            cwd: "C:\\Users\\private\\project".into(),
            thread_id: Some("private-thread-id".into()),
            process_id: Some(1234),
            mutating: true,
            timeout_seconds: 30,
            state: state.into(),
            started_at_ms: 1,
            finished_at_ms: Some(2),
            latency_ms: Some(1),
            exit_code: Some(0),
            next_sequence: 2,
            events: vec![RunEvent {
                sequence: 1,
                timestamp_ms: 1,
                kind: "output".into(),
                text: "private prompt ghp_sensitive-token-value".into(),
            }],
            workflow: Some(WorkflowRunMetadata {
                action: "dev-work".into(),
                operation_key: "private-operation-key".into(),
                intent: "private prompt".into(),
                build_profile: None,
                issue_url: Some("https://github.com/private/project/issues/1".into()),
                scope_paths: vec!["secret.txt".into()],
                requested_effects: vec!["push".into()],
                side_effects: None,
                build_artifact: None,
                expected_head: None,
                expected_push_remote: None,
                expected_push_branch: None,
                commit_subject: None,
                commit_body: None,
                draft_snapshot_fingerprint: None,
                auto_merge_pull_request: false,
            }),
            handoff: None,
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
    fn diagnostics_export_contains_only_allowlisted_aggregate_fields() {
        let record = fixture_run("run-1", "running");
        let export = diagnostics_json(&[record], 99).unwrap();
        let value: serde_json::Value = serde_json::from_str(&export).unwrap();
        assert_eq!(value["schemaVersion"], 1);
        assert_eq!(value["exportedAtMs"], 99);
        assert_eq!(value["history"]["runCount"], 1);
        assert_eq!(value["history"]["eventCount"], 1);
        assert_eq!(value["history"]["states"]["running"], 1);
        assert_eq!(value["history"]["providers"]["codex"], 1);
        assert_eq!(value["history"]["actions"]["dev-work"], 1);
        for private_value in [
            "secret-tool.exe",
            "C:\\Users\\private",
            "private-thread-id",
            "private prompt",
            "ghp_sensitive-token-value",
            "private-operation-key",
            "private-profile",
            "private-model",
            "private/project/issues",
        ] {
            assert!(
                !export.contains(private_value),
                "export leaked {private_value}"
            );
        }
        assert_eq!(value["retention"]["maximumRuns"], MAX_RUNS);
        assert_eq!(value["retention"]["maximumEventsPerRun"], MAX_EVENTS);
        assert_eq!(
            value["retention"]["maximumOutputBytesPerRun"],
            MAX_OUTPUT_BYTES
        );
    }

    #[test]
    fn deleting_completed_history_keeps_active_runs_and_removes_database_rows() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join(HISTORY_FILE);
        let mut records = vec![
            fixture_run("finished", "completed"),
            fixture_run("active", "running"),
        ];
        persist_records(&path, &records).unwrap();
        let deleted = retain_active_runs(&mut records);
        assert_eq!(deleted, 1);
        assert_eq!(
            records
                .iter()
                .map(|record| record.id.as_str())
                .collect::<Vec<_>>(),
            ["active"]
        );
        persist_records(&path, &records).unwrap();
        let restored = load_records(&path).unwrap();
        assert_eq!(restored.len(), 1);
        assert_eq!(restored[0].id, "active");
    }

    #[test]
    fn output_flood_is_bounded_and_utf8_truncation_keeps_valid_boundaries() {
        let chunk = "x".repeat(16 * 1024);
        let mut accepted_total = 0;
        let mut reached_limit = false;
        for _ in 0..200 {
            let (_, accepted, truncated) = budgeted_output(&chunk, accepted_total);
            accepted_total += accepted;
            reached_limit |= truncated;
        }
        assert_eq!(accepted_total, MAX_OUTPUT_BYTES);
        assert!(reached_limit);

        let (value, accepted, truncated) = budgeted_output("ÃƒÆ’Ã‚Â©", MAX_OUTPUT_BYTES - 1);
        assert!(value.is_empty());
        assert_eq!(accepted, 0);
        assert!(truncated);
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
            fallback_decisions: vec![],
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

    #[cfg(windows)]
    #[test]
    fn rejects_cmd_shell_metacharacters_in_script_paths_and_arguments() {
        let folder = tempfile::tempdir().unwrap();
        let safe_script = folder.path().join("safe.cmd");
        fs::write(&safe_script, "@echo off\n").unwrap();
        let safe = request(
            safe_script.to_str().unwrap(),
            folder.path().to_str().unwrap(),
            vec!["literal argument"],
        );
        assert!(validate_request(&safe).is_ok());

        let injected_argument = request(
            safe_script.to_str().unwrap(),
            folder.path().to_str().unwrap(),
            vec!["safe & whoami"],
        );
        assert!(validate_request(&injected_argument)
            .unwrap_err()
            .contains("shell metacharacters"));

        let unsafe_script = folder.path().join("unsafe & script.cmd");
        fs::write(&unsafe_script, "@echo off\n").unwrap();
        let injected_path = request(
            unsafe_script.to_str().unwrap(),
            folder.path().to_str().unwrap(),
            vec![],
        );
        assert!(validate_request(&injected_path)
            .unwrap_err()
            .contains("shell metacharacters"));
    }

    #[test]
    fn history_survives_restart_and_orphans_become_interrupted() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join(HISTORY_FILE);
        let mut record = RunRecord {
            id: "fixture".into(),
            executable: "test".into(),
            provider: None,
            provider_profile_id: None,
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
                expected_head: None,
                expected_push_remote: None,
                expected_push_branch: None,
                commit_subject: None,
                commit_body: None,
                draft_snapshot_fingerprint: None,
                auto_merge_pull_request: false,
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
                fallback_decisions: vec![crate::routing::FallbackDecision {
                    policy_version: crate::routing::FALLBACK_POLICY_VERSION,
                    accepted: true,
                    local_only: false,
                    source_run_id: "fixture".into(),
                    trigger: crate::routing::FallbackTrigger::ProviderFailure,
                    source_provider_id: "codex-profile".into(),
                    target_provider_id: Some("ollama-local".into()),
                    target_model_id: Some("model".into()),
                    checkpoint_ref: Some("fixture-checkpoint".into()),
                    handoff_count: 1,
                    reason: "fixture decision".into(),
                    routing: None,
                }],
                created_at_ms: 2,
            }),
        };
        persist_records(&path, &[record.clone()]).unwrap();
        record.state = "interrupted".into();
        let restored = load_records(&path).unwrap();
        assert_eq!(restored[0].state, record.state);
        assert_eq!(restored[0].handoff, record.handoff);
        assert_eq!(
            restored[0].handoff.as_ref().unwrap().fallback_decisions[0].policy_version,
            crate::routing::FALLBACK_POLICY_VERSION
        );
        let mut legacy_json = serde_json::to_value(&record).unwrap();
        legacy_json
            .as_object_mut()
            .unwrap()
            .remove("providerProfileId");
        let legacy_record: RunRecord = serde_json::from_value(legacy_json).unwrap();
        assert_eq!(legacy_record.provider_profile_id, None);
        let checkpoint = restored[0].handoff.as_ref().unwrap();
        assert!(approved_fallback_matches(
            checkpoint,
            "ollama-local",
            "model",
            true
        ));
        assert!(!approved_fallback_matches(
            checkpoint,
            "ollama-local",
            "different-model",
            true
        ));
        let mut privacy_checkpoint = checkpoint.clone();
        privacy_checkpoint.fallback_decisions[0].local_only = true;
        assert!(!approved_fallback_matches(
            &privacy_checkpoint,
            "openrouter-profile",
            "model",
            false
        ));
        assert!(approved_fallback_matches(
            &privacy_checkpoint,
            "ollama-local",
            "model",
            true
        ));
        assert_eq!(restored[0].process_id, Some(std::process::id()));
        assert!(interrupted_process_owns_scope(
            &restored,
            &mutation_scope(directory.path())
        ));
        assert!(restored[0].finished_at_ms.is_some());
        assert!(workflow_key_recorded(&restored, &"a".repeat(64)));
        assert!(!workflow_key_recorded(&restored, &"b".repeat(64)));
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
    fn redacts_openrouter_credential_from_child_process_output() {
        let key = "sk-or-v1-fixture-secret".to_string();
        let value = redact(&format!("request failed with key {key}"), &[key]);
        assert!(!value.contains("sk-or-v1-fixture-secret"));
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
            expected_head: None,
            expected_push_remote: None,
            expected_push_branch: None,
            commit_subject: None,
            commit_body: None,
            draft_snapshot_fingerprint: None,
            auto_merge_pull_request: false,
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
    fn push_only_requires_a_clean_ahead_upstream_and_pins_its_target() {
        let fixture = tempfile::tempdir_in(env!("CARGO_MANIFEST_DIR")).unwrap();
        let git = |args: &[&str]| {
            let output = Command::new("git")
                .args(args)
                .current_dir(fixture.path())
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "git {args:?}: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            String::from_utf8_lossy(&output.stdout).trim().to_owned()
        };
        git(&["init", "--quiet", "--initial-branch=main"]);
        git(&["config", "user.name", "Fixture User"]);
        git(&["config", "user.email", "fixture@example.invalid"]);
        fs::write(fixture.path().join("tracked.txt"), "base").unwrap();
        git(&["add", "tracked.txt"]);
        git(&["commit", "--quiet", "-m", "base"]);
        let base = git(&["rev-parse", "HEAD"]);
        git(&["remote", "add", "origin", "git@github.com:owner/repo.git"]);
        git(&["update-ref", "refs/remotes/origin/main", &base]);
        git(&["config", "branch.main.remote", "origin"]);
        git(&["config", "branch.main.merge", "refs/heads/main"]);
        fs::write(fixture.path().join("tracked.txt"), "ahead").unwrap();
        git(&["commit", "--quiet", "-am", "ahead"]);

        let mut workflow = WorkflowRunMetadata {
            action: "dev-push".into(),
            operation_key: "e".repeat(64),
            intent: "Push the committed branch to its upstream.".into(),
            build_profile: None,
            issue_url: None,
            scope_paths: vec![],
            requested_effects: vec!["push".into()],
            side_effects: None,
            build_artifact: None,
            expected_head: None,
            expected_push_remote: None,
            expected_push_branch: None,
            commit_subject: None,
            commit_body: None,
            draft_snapshot_fingerprint: None,
            auto_merge_pull_request: false,
        };
        validate_workflow_metadata(&workflow).unwrap();
        workflow.requested_effects.push("createPullRequest".into());
        workflow.auto_merge_pull_request = true;
        validate_workflow_metadata(&workflow).unwrap();
        workflow.requested_effects.pop();
        assert!(validate_workflow_metadata(&workflow).is_err());
        workflow.auto_merge_pull_request = false;
        ensure_git_scope(fixture.path(), &[]).unwrap();
        let (head, remote, branch) = capture_push_only_target(fixture.path()).unwrap();
        assert_eq!(remote, "origin");
        assert_eq!(branch, "main");
        assert_eq!(
            verify_push_listing(&format!("{head}\trefs/heads/{branch}"), &branch, &head).unwrap(),
            head
        );
        assert!(verify_push_listing(
            &format!("{}\trefs/heads/{branch}", "f".repeat(40)),
            &branch,
            &head
        )
        .is_err());

        git(&["update-ref", "refs/remotes/origin/main", &head]);
        assert!(capture_push_only_target(fixture.path())
            .unwrap_err()
            .contains("commits ahead"));
        let tree = git(&["rev-parse", "HEAD^{tree}"]);
        let diverged = git(&[
            "-c",
            "user.name=Fixture User",
            "-c",
            "user.email=fixture@example.invalid",
            "commit-tree",
            &tree,
            "-p",
            &base,
            "-m",
            "remote divergence",
        ]);
        git(&["update-ref", "refs/remotes/origin/main", &diverged]);
        assert!(capture_push_only_target(fixture.path())
            .unwrap_err()
            .contains("no divergence"));
        assert!(capture_push_target(fixture.path(), true).is_ok());
        git(&["update-ref", "refs/remotes/origin/main", &base]);

        workflow.requested_effects = vec!["commit".into(), "push".into()];
        assert!(validate_workflow_metadata(&workflow).is_err());
        workflow.requested_effects = vec!["push".into()];
        fs::write(fixture.path().join("tracked.txt"), "dirty").unwrap();
        assert!(capture_push_only_target(fixture.path())
            .unwrap_err()
            .contains("clean working tree"));
        fs::write(fixture.path().join(".git/MERGE_HEAD"), format!("{head}\n")).unwrap();
        assert!(capture_commit_draft_context(fixture.path())
            .unwrap_err()
            .contains("already in progress"));
    }

    #[test]
    fn commit_draft_context_redacts_sensitive_files_and_fingerprints_head_branch_and_remote() {
        let fixture = tempfile::tempdir_in(env!("CARGO_MANIFEST_DIR")).unwrap();
        let git = |args: &[&str]| {
            let output = Command::new("git")
                .args(args)
                .current_dir(fixture.path())
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "git {args:?}: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            String::from_utf8_lossy(&output.stdout).trim().to_owned()
        };
        git(&["init", "--quiet", "--initial-branch=main"]);
        git(&["config", "user.name", "Fixture User"]);
        git(&["config", "user.email", "fixture@example.invalid"]);
        fs::write(fixture.path().join("tracked.txt"), "base\n").unwrap();
        git(&["add", "tracked.txt"]);
        git(&["commit", "--quiet", "-m", "base"]);
        let base = git(&["rev-parse", "HEAD"]);
        git(&["remote", "add", "origin", "git@github.com:owner/repo.git"]);
        git(&["update-ref", "refs/remotes/origin/main", &base]);
        git(&["config", "branch.main.remote", "origin"]);
        git(&["config", "branch.main.merge", "refs/heads/main"]);
        fs::write(fixture.path().join("tracked.txt"), "ahead\n").unwrap();
        git(&["commit", "--quiet", "-am", "ahead change"]);

        let first = capture_commit_draft_context(fixture.path()).unwrap();
        assert_eq!(first.mode, "pushOnly");
        let original_fingerprint = first.snapshot_fingerprint.clone();
        let head = git(&["rev-parse", "HEAD"]);
        git(&["switch", "--quiet", "-c", "other"]);
        git(&["update-ref", "refs/remotes/origin/other", &base]);
        git(&["config", "branch.other.remote", "origin"]);
        git(&["config", "branch.other.merge", "refs/heads/other"]);
        let second = capture_commit_draft_context(fixture.path()).unwrap();
        assert_eq!(second.head.as_deref(), Some(head.as_str()));
        assert_ne!(
            second.snapshot_fingerprint, original_fingerprint,
            "branch changes invalidate push-only drafts"
        );
        git(&[
            "remote",
            "set-url",
            "origin",
            "git@github.com:other/repo.git",
        ]);
        let third = capture_commit_draft_context(fixture.path()).unwrap();
        assert_ne!(
            third.snapshot_fingerprint, second.snapshot_fingerprint,
            "remote changes invalidate push-only drafts"
        );

        fs::write(fixture.path().join(".env"), "TOKEN=fixture-secret-value\n").unwrap();
        fs::write(fixture.path().join("safe.txt"), "ordinary change\n").unwrap();
        let dirty = capture_commit_draft_context(fixture.path()).unwrap();
        assert_eq!(dirty.mode, "commitPush");
        assert!(dirty.diff_context.contains("ordinary change"));
        assert!(!dirty.diff_context.contains("fixture-secret-value"));
        assert!(!dirty.diff_context.contains(".env"));
    }

    #[test]
    fn approved_commit_receipt_must_match_subject_and_body() {
        let fixture = tempfile::tempdir_in(env!("CARGO_MANIFEST_DIR")).unwrap();
        let git = |args: &[&str]| {
            let output = Command::new("git")
                .args(args)
                .current_dir(fixture.path())
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "git {args:?}: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            String::from_utf8_lossy(&output.stdout).trim().to_owned()
        };
        git(&["init", "--quiet", "--initial-branch=main"]);
        git(&["config", "user.name", "Fixture User"]);
        git(&["config", "user.email", "fixture@example.invalid"]);
        fs::write(fixture.path().join("file.txt"), "content\n").unwrap();
        git(&["add", "file.txt"]);
        git(&[
            "commit",
            "--quiet",
            "-m",
            "approved subject",
            "-m",
            "approved body",
        ]);
        let sha = git(&["rev-parse", "HEAD"]);
        let mut workflow = WorkflowRunMetadata {
            action: "dev-push".into(),
            operation_key: "f".repeat(64),
            intent: "Approved local commit".into(),
            build_profile: None,
            issue_url: None,
            scope_paths: vec!["file.txt".into()],
            requested_effects: vec!["commit".into()],
            side_effects: None,
            build_artifact: None,
            expected_head: None,
            expected_push_remote: None,
            expected_push_branch: None,
            commit_subject: Some("approved subject".into()),
            commit_body: Some("approved body".into()),
            draft_snapshot_fingerprint: None,
            auto_merge_pull_request: false,
        };
        let receipt = WorkflowSideEffects {
            commit_sha: Some(sha),
            remote_branch: None,
            pull_request_url: None,
            verified: false,
            verification_message: None,
        };
        assert!(verify_reported_side_effects(fixture.path(), &workflow, &receipt).is_ok());
        workflow.commit_subject = Some("different subject".into());
        assert!(
            verify_reported_side_effects(fixture.path(), &workflow, &receipt)
                .unwrap_err()
                .contains("differs from the approved")
        );
    }

    #[test]
    fn fallback_requires_a_verified_receipt_for_requested_side_effects() {
        let mut workflow = WorkflowRunMetadata {
            action: "dev-push".into(),
            operation_key: "b".repeat(64),
            intent: "Complete the selected delivery.".into(),
            build_profile: None,
            issue_url: None,
            scope_paths: vec!["src/main.rs".into()],
            requested_effects: vec!["push".into()],
            side_effects: None,
            build_artifact: None,
            expected_head: None,
            expected_push_remote: None,
            expected_push_branch: None,
            commit_subject: None,
            commit_body: None,
            draft_snapshot_fingerprint: None,
            auto_merge_pull_request: false,
        };
        assert!(!workflow_effects_reconciled(Some(&workflow)));

        workflow.side_effects = Some(WorkflowSideEffects {
            commit_sha: None,
            remote_branch: Some("feat/example".into()),
            pull_request_url: None,
            verified: false,
            verification_message: None,
        });
        assert!(!workflow_effects_reconciled(Some(&workflow)));

        workflow.side_effects.as_mut().unwrap().verified = true;
        assert!(workflow_effects_reconciled(Some(&workflow)));

        workflow.requested_effects.clear();
        workflow.side_effects = None;
        assert!(workflow_effects_reconciled(Some(&workflow)));
        assert!(workflow_effects_reconciled(None));
    }

    #[test]
    fn fallback_source_identity_requires_the_exact_provider_profile() {
        assert!(source_provider_profile_matches(
            Some("codex-profile-a"),
            "codex-profile-a"
        ));
        assert!(!source_provider_profile_matches(
            Some("codex-profile-a"),
            "codex-profile-b"
        ));
        assert!(!source_provider_profile_matches(None, "codex-profile-a"));
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
            expected_head: None,
            expected_push_remote: None,
            expected_push_branch: None,
            commit_subject: None,
            commit_body: None,
            draft_snapshot_fingerprint: None,
            auto_merge_pull_request: false,
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
            expected_head: None,
            expected_push_remote: None,
            expected_push_branch: None,
            commit_subject: None,
            commit_body: None,
            draft_snapshot_fingerprint: None,
            auto_merge_pull_request: false,
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
            expected_head: None,
            expected_push_remote: None,
            expected_push_branch: None,
            commit_subject: None,
            commit_body: None,
            draft_snapshot_fingerprint: None,
            auto_merge_pull_request: false,
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
    fn local_only_commit_workflow_allows_unborn_branch_and_verifies_local_receipt() {
        let fixture = tempfile::tempdir_in(env!("CARGO_MANIFEST_DIR")).unwrap();
        let git = |args: &[&str]| {
            Command::new("git")
                .args(args)
                .current_dir(fixture.path())
                .output()
                .unwrap()
        };
        let init = git(&["init", "--quiet", "--initial-branch=main"]);
        assert!(
            init.status.success(),
            "{}",
            String::from_utf8_lossy(&init.stderr)
        );
        assert!(git(&["config", "user.name", "Fixture User"])
            .status
            .success());
        assert!(git(&["config", "user.email", "fixture@example.invalid"])
            .status
            .success());
        fs::write(fixture.path().join("first.txt"), "initial content").unwrap();

        let commit_only = WorkflowRunMetadata {
            action: "dev-push".into(),
            operation_key: "d".repeat(64),
            intent: "Review and commit the selected initial file.".into(),
            build_profile: None,
            issue_url: None,
            scope_paths: vec!["first.txt".into()],
            requested_effects: vec!["commit".into()],
            side_effects: None,
            build_artifact: None,
            expected_head: None,
            expected_push_remote: None,
            expected_push_branch: None,
            commit_subject: None,
            commit_body: None,
            draft_snapshot_fingerprint: None,
            auto_merge_pull_request: false,
        };
        validate_workflow_metadata(&commit_only).unwrap();
        ensure_git_scope(fixture.path(), &commit_only.scope_paths).unwrap();
        verify_workflow_repository(fixture.path(), &commit_only).unwrap();

        assert!(git(&["add", "first.txt"]).status.success());
        let commit = git(&["commit", "--quiet", "-m", "fixture initial commit"]);
        assert!(
            commit.status.success(),
            "{}",
            String::from_utf8_lossy(&commit.stderr)
        );
        let sha = String::from_utf8(git(&["rev-parse", "HEAD"]).stdout)
            .unwrap()
            .trim()
            .to_owned();
        let effects = WorkflowSideEffects {
            commit_sha: Some(sha),
            remote_branch: None,
            pull_request_url: None,
            verified: false,
            verification_message: None,
        };
        verify_reported_side_effects(fixture.path(), &commit_only, &effects).unwrap();

        let mut push = commit_only;
        push.requested_effects.push("push".into());
        assert!(verify_workflow_repository(fixture.path(), &push)
            .unwrap_err()
            .contains("GitHub remote"));
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

#[cfg(test)]
mod build_progress_retention_tests {
    use super::*;
    #[test]
    fn long_logs_keep_the_latest_phase_within_the_event_bound() {
        let mut events = vec![
            RunEvent {
                sequence: 1,
                timestamp_ms: 0,
                kind: "buildProgress".into(),
                text: "BUILD_STEP:1/6:Preparing version".into(),
            },
            RunEvent {
                sequence: 2,
                timestamp_ms: 0,
                kind: "buildProgress".into(),
                text: "BUILD_STEP:2/6:Building release artifacts".into(),
            },
        ];
        for sequence in 3..MAX_EVENTS as u64 + 100 {
            events.push(RunEvent {
                sequence,
                timestamp_ms: 0,
                kind: "stdout".into(),
                text: "Compiling".into(),
            });
        }
        trim_run_events(&mut events);
        assert_eq!(events.len(), MAX_EVENTS);
        assert_eq!(events[0].sequence, 2);
        assert_eq!(
            events
                .iter()
                .filter(|event| event.kind == "buildProgress")
                .count(),
            1
        );
        assert!(events
            .windows(2)
            .all(|pair| pair[0].sequence < pair[1].sequence));
    }
}
