use serde::Serialize;
use std::collections::{HashMap, HashSet, VecDeque};
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

const MAX_DIRECTORIES: usize = 20_000;
const MAX_PROJECTS: usize = 10_000;
const MAX_ERRORS: usize = 500;
const GIT_COMMAND_TIMEOUT: Duration = Duration::from_secs(30);
const GIT_POLL_INTERVAL: Duration = Duration::from_millis(50);

#[derive(Debug, Clone)]
pub struct ScanOptions {
    pub depth: u8,
    pub include_hidden: bool,
    pub exclusions: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectEntry {
    pub id: String,
    pub name: String,
    pub path: String,
    pub is_git_repository: bool,
    pub is_worktree: bool,
    pub detected_stacks: Vec<String>,
    pub git: GitFacts,
    pub workflow_prerequisites: Vec<WorkflowPrerequisite>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GitFacts {
    /// available, detached, unborn, unsafeOwnership, gitUnavailable, notRepository, or error
    pub status: String,
    pub branch: Option<String>,
    pub dirty: Option<bool>,
    pub latest_commit: Option<LatestCommit>,
    pub remotes: Vec<GitRemote>,
    pub upstream: UpstreamDivergence,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LatestCommit {
    pub short_hash: String,
    pub subject: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GitRemote {
    pub name: String,
    pub url: Option<String>,
    pub github: Option<GitHubRepository>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GitHubRepository {
    pub owner: String,
    pub repository: String,
    pub canonical_url: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UpstreamDivergence {
    /// tracking, noUpstream, or unavailable
    pub status: String,
    pub ahead: Option<u64>,
    pub behind: Option<u64>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkflowPrerequisite {
    pub name: String,
    pub required_for: Vec<String>,
    pub available: bool,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ScanProgress {
    pub scan_id: String,
    pub directories_scanned: usize,
    pub projects_found: usize,
    pub current_path: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ScanFinished {
    pub scan_id: String,
    pub projects: Vec<ProjectEntry>,
    pub errors: Vec<String>,
    pub directories_scanned: usize,
    pub cancelled: bool,
    pub fatal_error: Option<String>,
}

pub fn scan(
    scan_id: &str,
    root: &Path,
    options: &ScanOptions,
    cancelled: &AtomicBool,
    cached_projects: &HashMap<String, ProjectEntry>,
    mut progress: impl FnMut(ScanProgress),
) -> ScanFinished {
    let root = match fs::canonicalize(root) {
        Ok(root) if root.is_dir() => root,
        Ok(_) => {
            return failed(scan_id, "The configured project root is not a folder.");
        }
        Err(error) => {
            return failed(
                scan_id,
                &format!("Could not open the configured project root: {error}"),
            );
        }
    };

    let mut queue = VecDeque::from([(root.clone(), 0u8)]);
    let mut visited = HashSet::new();
    let mut project_ids = HashSet::new();
    let mut projects = Vec::new();
    let mut errors = Vec::new();
    let mut directories_scanned = 0;
    let mut was_cancelled = false;
    let mut root_error = None;
    let mut tool_availability = HashMap::new();

    while let Some((path, depth)) = queue.pop_front() {
        if cancelled.load(Ordering::Relaxed) {
            was_cancelled = true;
            break;
        }

        let canonical = match fs::canonicalize(&path) {
            Ok(canonical) => canonical,
            Err(error) => {
                push_error(&mut errors, format!("Could not open a folder: {error}"));
                continue;
            }
        };
        if !canonical.starts_with(&root) {
            continue;
        }
        if !visited.insert(path_key(&canonical)) {
            continue;
        }
        if directories_scanned >= MAX_DIRECTORIES {
            push_error(
                &mut errors,
                format!("Scan stopped at the {MAX_DIRECTORIES} folder safety limit."),
            );
            break;
        }
        directories_scanned += 1;

        let (is_git_repository, is_worktree) = git_marker(&canonical, &mut errors);
        let mut children = Vec::new();
        let mut names = HashSet::new();
        match fs::read_dir(&canonical) {
            Ok(entries) => {
                for entry in entries {
                    if cancelled.load(Ordering::Relaxed) {
                        was_cancelled = true;
                        break;
                    }
                    let entry = match entry {
                        Ok(entry) => entry,
                        Err(error) => {
                            push_error(
                                &mut errors,
                                format!("Could not inspect a folder entry: {error}"),
                            );
                            continue;
                        }
                    };
                    let name = entry.file_name().to_string_lossy().into_owned();
                    if !options.include_hidden && name.starts_with('.') {
                        continue;
                    }
                    let child_path = entry.path();
                    let relative = match child_path.strip_prefix(&root) {
                        Ok(relative) => relative,
                        Err(_) => continue,
                    };
                    if excluded(relative, &options.exclusions) {
                        continue;
                    }
                    match entry.metadata() {
                        Ok(metadata) if metadata.is_dir() => {
                            if depth < options.depth {
                                children.push(child_path);
                            }
                        }
                        Ok(metadata) if metadata.is_file() => {
                            names.insert(name.to_lowercase());
                        }
                        Ok(_) => {}
                        Err(error) => {
                            push_error(&mut errors, format!("Could not inspect {name}: {error}"))
                        }
                    }
                }
            }
            Err(error) => {
                let message = format!("Could not read a folder: {error}");
                push_error(&mut errors, message.clone());
                if depth == 0 {
                    root_error = Some(message);
                }
            }
        }
        let detected_stacks = detect_stacks(&canonical, &names);
        if is_git_repository || !detected_stacks.is_empty() {
            let key = project_identity(&canonical);
            if project_ids.contains(&key) {
                // Canonical aliases have already contributed this project.
            } else if projects.len() >= MAX_PROJECTS {
                push_error(
                    &mut errors,
                    format!("Scan stopped at the {MAX_PROJECTS} project safety limit."),
                );
                break;
            } else {
                project_ids.insert(key.clone());
                let git = cached_projects
                    .get(&key)
                    .map(|cached| cached.git.clone())
                    .unwrap_or_else(|| inspect_git(&canonical, is_git_repository, cancelled));
                if cancelled.load(Ordering::Relaxed) {
                    was_cancelled = true;
                    break;
                }
                projects.push(ProjectEntry {
                    id: key,
                    name: canonical
                        .file_name()
                        .map(|name| name.to_string_lossy().into_owned())
                        .unwrap_or_else(|| canonical.to_string_lossy().into_owned()),
                    path: canonical.to_string_lossy().into_owned(),
                    is_git_repository,
                    is_worktree,
                    detected_stacks,
                    git,
                    workflow_prerequisites: workflow_prerequisites(
                        &canonical,
                        &names,
                        is_git_repository,
                        &mut tool_availability,
                    ),
                });
            }
        }

        progress(ScanProgress {
            scan_id: scan_id.to_owned(),
            directories_scanned,
            projects_found: projects.len(),
            current_path: canonical.to_string_lossy().into_owned(),
        });

        if depth >= options.depth {
            continue;
        }
        children.sort_by_key(|child| {
            child
                .file_name()
                .map(|name| name.to_string_lossy().to_lowercase())
        });
        queue.extend(children.into_iter().map(|child| (child, depth + 1)));
    }

    ScanFinished {
        scan_id: scan_id.to_owned(),
        projects,
        errors,
        directories_scanned,
        cancelled: was_cancelled || cancelled.load(Ordering::Relaxed),
        fatal_error: root_error,
    }
}

fn failed(scan_id: &str, message: &str) -> ScanFinished {
    ScanFinished {
        scan_id: scan_id.to_owned(),
        projects: Vec::new(),
        errors: Vec::new(),
        directories_scanned: 0,
        cancelled: false,
        fatal_error: Some(message.to_owned()),
    }
}

fn path_key(path: &Path) -> String {
    let value = path.to_string_lossy().replace('\\', "/");
    if cfg!(windows) {
        value.to_lowercase()
    } else {
        value
    }
}

fn project_identity(path: &Path) -> String {
    if let Ok(metadata) = fs::metadata(path) {
        #[cfg(windows)]
        {
            let _ = metadata;
            if let Some(identity) = windows_file_identity(path) {
                return identity;
            }
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            return format!("unix:{}:{}", metadata.dev(), metadata.ino());
        }
    }
    format!("path:{}", path_key(path))
}

#[cfg(windows)]
fn windows_file_identity(path: &Path) -> Option<String> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::Storage::FileSystem::{
        CreateFileW, GetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION,
        FILE_FLAG_BACKUP_SEMANTICS, FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE, FILE_SHARE_READ,
        FILE_SHARE_WRITE, OPEN_EXISTING,
    };

    let mut wide: Vec<u16> = path.as_os_str().encode_wide().collect();
    wide.push(0);
    // Metadata handles are opened only to read the stable volume/file ID for reconciliation.
    let handle = unsafe {
        CreateFileW(
            wide.as_ptr(),
            FILE_READ_ATTRIBUTES,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            std::ptr::null(),
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS,
            std::ptr::null_mut(),
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        return None;
    }
    let mut information = BY_HANDLE_FILE_INFORMATION::default();
    let succeeded = unsafe { GetFileInformationByHandle(handle, &mut information) } != 0;
    unsafe { CloseHandle(handle) };
    succeeded.then(|| {
        let index =
            (u64::from(information.nFileIndexHigh) << 32) | u64::from(information.nFileIndexLow);
        format!("windows:{}:{index}", information.dwVolumeSerialNumber)
    })
}

fn excluded(relative: &Path, exclusions: &[String]) -> bool {
    let candidate = relative.to_string_lossy().replace('\\', "/");
    let candidate = candidate.trim_matches('/');
    exclusions.iter().any(|excluded| {
        let excluded = excluded.replace('\\', "/").trim_matches('/').to_lowercase();
        if excluded.is_empty() {
            return false;
        }
        let candidate = candidate.to_lowercase();
        candidate == excluded
            || candidate
                .strip_prefix(&excluded)
                .is_some_and(|suffix| suffix.starts_with('/'))
            || candidate.split('/').any(|component| component == excluded)
    })
}

fn git_marker(path: &Path, errors: &mut Vec<String>) -> (bool, bool) {
    let marker = path.join(".git");
    let metadata = match fs::metadata(&marker) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return (false, false),
        Err(error) => {
            push_error(
                errors,
                format!(
                    "Could not inspect the Git marker in {}: {error}",
                    path.display()
                ),
            );
            return (false, false);
        }
    };
    if metadata.is_dir() {
        return (true, false);
    }
    if !metadata.is_file() {
        return (false, false);
    }
    match fs::read_to_string(&marker) {
        Ok(contents) => {
            let Some(target) = contents.trim().strip_prefix("gitdir:") else {
                return (false, false);
            };
            let target = target.trim();
            let target = PathBuf::from(target);
            let target = if target.is_absolute() {
                target
            } else {
                path.join(target)
            };
            if target.is_dir() {
                (true, true)
            } else {
                push_error(
                    errors,
                    format!(
                        "A Git worktree marker in {} points to a missing location.",
                        path.display()
                    ),
                );
                (false, true)
            }
        }
        Err(error) => {
            push_error(
                errors,
                format!(
                    "Could not read the Git marker in {}: {error}",
                    path.display()
                ),
            );
            (false, false)
        }
    }
}

fn detect_stacks(path: &Path, names: &HashSet<String>) -> Vec<String> {
    let markers = [
        ("package.json", "Node.js"),
        ("cargo.toml", "Rust"),
        ("go.mod", "Go"),
        ("pyproject.toml", "Python"),
        ("requirements.txt", "Python"),
        ("setup.py", "Python"),
        ("composer.json", "PHP"),
        ("gemfile", "Ruby"),
        ("pom.xml", "Java"),
        ("build.gradle", "Java"),
        ("build.gradle.kts", "Kotlin/Gradle"),
        ("*.sln", ".NET"),
        ("*.csproj", ".NET"),
    ];
    let mut stacks = Vec::new();
    for (marker, stack) in markers {
        let found = if marker.starts_with("*.") {
            names.iter().any(|name| name.ends_with(&marker[1..]))
        } else {
            names.contains(marker)
        };
        if found && !stacks.iter().any(|existing| existing == stack.trim()) {
            stacks.push(stack.trim().to_owned());
        }
    }
    if names.contains("tauri.conf.json")
        || path.join("src-tauri/tauri.conf.json").is_file()
        || path.join("src-tauri/tauri.conf.json5").is_file()
    {
        stacks.push("Tauri".to_owned());
    }
    if path.join("src-tauri/Cargo.toml").is_file() && !stacks.iter().any(|stack| stack == "Rust") {
        stacks.push("Rust".to_owned());
    }
    if names.contains("pubspec.yaml") {
        stacks.push("Flutter".to_owned());
    }
    if path.join("android/build.gradle").is_file()
        || path.join("android/build.gradle.kts").is_file()
    {
        stacks.push("Android".to_owned());
    }
    let ios_project = path.join("ios").read_dir().is_ok_and(|entries| {
        entries.filter_map(Result::ok).any(|entry| {
            entry
                .file_name()
                .to_string_lossy()
                .to_lowercase()
                .ends_with(".xcodeproj")
        })
    });
    if path.join("ios/Podfile").is_file() || ios_project {
        stacks.push("iOS".to_owned());
    }
    if package_uses(path, "expo") {
        stacks.push("Expo".to_owned());
    } else if package_uses(path, "react-native") {
        stacks.push("React Native".to_owned());
    }
    stacks
}

fn package_uses(path: &Path, package: &str) -> bool {
    let manifest = path.join("package.json");
    let Ok(metadata) = fs::metadata(&manifest) else {
        return false;
    };
    if metadata.len() > 1_048_576 {
        return false;
    }
    let Ok(contents) = fs::read(&manifest) else {
        return false;
    };
    let Ok(value) = serde_json::from_slice::<serde_json::Value>(&contents) else {
        return false;
    };
    ["dependencies", "devDependencies", "peerDependencies"]
        .iter()
        .any(|section| {
            value
                .get(section)
                .and_then(serde_json::Value::as_object)
                .is_some_and(|deps| deps.contains_key(package))
        })
}

fn workflow_prerequisites(
    path: &Path,
    names: &HashSet<String>,
    is_git_repository: bool,
    tool_availability: &mut HashMap<String, bool>,
) -> Vec<WorkflowPrerequisite> {
    let mut required: Vec<(&str, Vec<&str>)> = Vec::new();
    if is_git_repository {
        required.push(("git", vec!["Git workflows"]));
    }
    if names.contains("package.json") {
        required.push(("node", vec!["Node.js", "Tauri", "Expo", "React Native"]));
        if names.contains("pnpm-lock.yaml") {
            required.push(("pnpm", vec!["Node.js", "Tauri", "Expo", "React Native"]));
        } else if names.contains("yarn.lock") {
            required.push(("yarn", vec!["Node.js", "Tauri", "Expo", "React Native"]));
        } else {
            required.push(("npm", vec!["Node.js", "Tauri", "Expo", "React Native"]));
        }
    }
    if names.contains("cargo.toml") || path.join("src-tauri/Cargo.toml").is_file() {
        required.push(("cargo", vec!["Rust", "Tauri"]));
        required.push(("rustc", vec!["Rust", "Tauri"]));
    }
    if names.contains("go.mod") {
        required.push(("go", vec!["Go"]));
    }
    if names.contains("pyproject.toml")
        || names.contains("requirements.txt")
        || names.contains("setup.py")
    {
        required.push(("python", vec!["Python"]));
    }
    if names.contains("pubspec.yaml") {
        required.push(("flutter", vec!["Flutter"]));
    }
    if path.join("android/build.gradle").is_file()
        || path.join("android/build.gradle.kts").is_file()
    {
        required.push(("gradle", vec!["Android"]));
        required.push(("java", vec!["Android"]));
    }
    if names.contains("pom.xml")
        || names.contains("build.gradle")
        || names.contains("build.gradle.kts")
    {
        required.push(("java", vec!["Java", "Kotlin/Gradle"]));
        if names.contains("build.gradle") || names.contains("build.gradle.kts") {
            required.push(("gradle", vec!["Kotlin/Gradle"]));
        }
    }
    if path.join("ios/Podfile").is_file() {
        required.push(("xcodebuild", vec!["iOS"]));
        required.push(("pod", vec!["iOS"]));
    }
    let mut merged: Vec<(&str, Vec<&str>)> = Vec::new();
    for (name, required_for) in required {
        if let Some((_, existing)) = merged.iter_mut().find(|(existing, _)| *existing == name) {
            for stack in required_for {
                if !existing.contains(&stack) {
                    existing.push(stack);
                }
            }
        } else {
            merged.push((name, required_for));
        }
    }
    merged
        .into_iter()
        .map(|(name, required_for)| WorkflowPrerequisite {
            name: name.to_owned(),
            required_for: required_for.into_iter().map(str::to_owned).collect(),
            available: match name {
                "gradle" => {
                    executable_available(name, tool_availability)
                        || path.join("gradlew").is_file()
                        || path.join("gradlew.bat").is_file()
                        || path.join("android/gradlew.bat").is_file()
                }
                _ => executable_available(name, tool_availability),
            },
        })
        .collect()
}

pub fn unavailable_workflow_prerequisites(path: &Path) -> Result<Vec<String>, String> {
    let entries = fs::read_dir(path).map_err(|_| {
        "Could not inspect project dependencies before starting the workflow.".to_string()
    })?;
    let mut names = HashSet::new();
    for entry in entries {
        let entry = entry.map_err(|_| {
            "Could not inspect project dependencies before starting the workflow.".to_string()
        })?;
        if entry
            .metadata()
            .map_err(|_| {
                "Could not inspect project dependencies before starting the workflow.".to_string()
            })?
            .is_file()
        {
            names.insert(entry.file_name().to_string_lossy().to_lowercase());
        }
    }
    let mut tool_availability = HashMap::new();
    Ok(
        workflow_prerequisites(path, &names, true, &mut tool_availability)
            .into_iter()
            .filter(|item| !item.available)
            .map(|item| item.name)
            .collect(),
    )
}

fn executable_available(name: &str, cache: &mut HashMap<String, bool>) -> bool {
    if let Some(available) = cache.get(name) {
        return *available;
    }
    let Some(path) = std::env::var_os("PATH") else {
        cache.insert(name.to_owned(), false);
        return false;
    };
    let available = std::env::split_paths(&path).any(|folder| {
        #[cfg(windows)]
        let candidates = [
            format!("{name}.exe"),
            format!("{name}.cmd"),
            format!("{name}.bat"),
            name.to_owned(),
        ];
        #[cfg(not(windows))]
        let candidates = [name.to_owned()];
        candidates
            .iter()
            .any(|candidate| folder.join(candidate).is_file())
    });
    cache.insert(name.to_owned(), available);
    available
}

fn inspect_git(path: &Path, is_repository: bool, cancelled: &AtomicBool) -> GitFacts {
    if !is_repository {
        return GitFacts {
            status: "notRepository".to_owned(),
            branch: None,
            dirty: None,
            latest_commit: None,
            remotes: Vec::new(),
            upstream: UpstreamDivergence {
                status: "unavailable".to_owned(),
                ahead: None,
                behind: None,
            },
        };
    }
    let (status, branch) = match git_output(
        path,
        &["symbolic-ref", "--quiet", "--short", "HEAD"],
        cancelled,
    ) {
        Ok(Some(value)) => match git_output(
            path,
            &["rev-parse", "--verify", "--quiet", "HEAD"],
            cancelled,
        ) {
            Ok(Some(_)) => ("available", Some(value)),
            Ok(None) => ("unborn", Some(value)),
            Err(status) => return empty_git_facts(&status),
        },
        Ok(None) => match git_output(
            path,
            &["rev-parse", "--verify", "--quiet", "HEAD"],
            cancelled,
        ) {
            Ok(Some(_)) => ("detached", None),
            Ok(None) => ("unborn", None),
            Err(status) => return empty_git_facts(&status),
        },
        Err(status) => return empty_git_facts(&status),
    };
    let dirty = match git_output(
        path,
        &[
            "--no-optional-locks",
            "status",
            "--porcelain=v1",
            "--untracked-files=normal",
        ],
        cancelled,
    ) {
        Ok(Some(value)) => Some(!value.is_empty()),
        Ok(None) => Some(false),
        Err(status) => return empty_git_facts(&status),
    };
    let latest_commit = match git_output(path, &["log", "-1", "--format=%H%x00%s"], cancelled) {
        Ok(Some(value)) => {
            let (hash, subject) = value.split_once('\0').unwrap_or((&value, ""));
            Some(LatestCommit {
                short_hash: hash.chars().take(12).collect(),
                subject: subject.chars().take(240).collect(),
            })
        }
        Ok(None) => None,
        Err(status) => return empty_git_facts(&status),
    };
    let remotes = match git_output(path, &["remote"], cancelled) {
        Ok(Some(value)) => value
            .lines()
            .filter(|line| !line.is_empty())
            .map(|name| {
                let raw_url = git_output(path, &["remote", "get-url", name], cancelled)
                    .ok()
                    .flatten();
                let github = raw_url.as_deref().and_then(parse_github_remote);
                let url = raw_url.as_deref().map(sanitize_remote);
                GitRemote {
                    name: name.to_owned(),
                    url,
                    github,
                }
            })
            .collect(),
        Ok(None) => Vec::new(),
        Err(status) => return empty_git_facts(&status),
    };
    let upstream = match git_output(
        path,
        &[
            "rev-parse",
            "--abbrev-ref",
            "--symbolic-full-name",
            "@{upstream}",
        ],
        cancelled,
    ) {
        Ok(Some(_)) => match git_output(
            path,
            &["rev-list", "--left-right", "--count", "HEAD...@{upstream}"],
            cancelled,
        ) {
            Ok(Some(counts)) => {
                let mut parts = counts
                    .split_whitespace()
                    .filter_map(|part| part.parse::<u64>().ok());
                match (parts.next(), parts.next()) {
                    (Some(ahead), Some(behind)) => UpstreamDivergence {
                        status: "tracking".to_owned(),
                        ahead: Some(ahead),
                        behind: Some(behind),
                    },
                    _ => UpstreamDivergence {
                        status: "unavailable".to_owned(),
                        ahead: None,
                        behind: None,
                    },
                }
            }
            _ => UpstreamDivergence {
                status: "unavailable".to_owned(),
                ahead: None,
                behind: None,
            },
        },
        Ok(None) => UpstreamDivergence {
            status: "noUpstream".to_owned(),
            ahead: None,
            behind: None,
        },
        Err(status) if status == "unsafeOwnership" => return empty_git_facts(&status),
        Err(_) => UpstreamDivergence {
            status: "unavailable".to_owned(),
            ahead: None,
            behind: None,
        },
    };
    GitFacts {
        status: status.to_owned(),
        branch,
        dirty,
        latest_commit,
        remotes,
        upstream,
    }
}

fn empty_git_facts(status: &str) -> GitFacts {
    GitFacts {
        status: status.to_owned(),
        branch: None,
        dirty: None,
        latest_commit: None,
        remotes: Vec::new(),
        upstream: UpstreamDivergence {
            status: "unavailable".to_owned(),
            ahead: None,
            behind: None,
        },
    }
}

fn git_output(
    path: &Path,
    args: &[&str],
    cancelled: &AtomicBool,
) -> Result<Option<String>, String> {
    let mut command = crate::process::command("git");
    command
        .arg("-C")
        .arg(path)
        .args(["-c", "core.fsmonitor=false"])
        .args(args)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("LC_ALL", "C")
        .env("LANG", "C")
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .env_remove("GIT_COMMON_DIR")
        .env_remove("GIT_OBJECT_DIRECTORY")
        .env_remove("GIT_ALTERNATE_OBJECT_DIRECTORIES");
    let output = run_git_command(&mut command, cancelled)?;
    if output.status.success() {
        let value = String::from_utf8_lossy(&output.stdout).trim().to_owned();
        Ok((!value.is_empty()).then_some(value))
    } else {
        let error = String::from_utf8_lossy(&output.stderr).to_lowercase();
        if error.contains("dubious ownership") || error.contains("safe.directory") {
            Err("unsafeOwnership".to_owned())
        } else if error.trim().is_empty()
            || error.contains("does not have any commits yet")
            || error.contains("no upstream")
            || error.contains("upstream branch")
            || error.contains("needed a single revision")
        {
            Ok(None)
        } else if error.contains("not a git repository") {
            Err("notRepository".to_owned())
        } else {
            Err("error".to_owned())
        }
    }
}

fn run_git_command(command: &mut Command, cancelled: &AtomicBool) -> Result<Output, String> {
    if cancelled.load(Ordering::Relaxed) {
        return Err("cancelled".to_owned());
    }
    command.stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = command.spawn().map_err(|_| "gitUnavailable".to_owned())?;
    let stdout = child.stdout.take().expect("stdout is piped");
    let stderr = child.stderr.take().expect("stderr is piped");
    let stdout_reader = thread::spawn(move || {
        let mut bytes = Vec::new();
        let _ = stdout.take(u64::MAX).read_to_end(&mut bytes);
        bytes
    });
    let stderr_reader = thread::spawn(move || {
        let mut bytes = Vec::new();
        let _ = stderr.take(u64::MAX).read_to_end(&mut bytes);
        bytes
    });
    let started = Instant::now();
    let status = loop {
        if cancelled.load(Ordering::Relaxed) {
            terminate_child(&mut child);
            let _ = stdout_reader.join();
            let _ = stderr_reader.join();
            return Err("cancelled".to_owned());
        }
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if started.elapsed() < GIT_COMMAND_TIMEOUT => {
                thread::sleep(GIT_POLL_INTERVAL);
            }
            Ok(None) => {
                terminate_child(&mut child);
                let _ = stdout_reader.join();
                let _ = stderr_reader.join();
                return Err("timeout".to_owned());
            }
            Err(_) => {
                terminate_child(&mut child);
                let _ = stdout_reader.join();
                let _ = stderr_reader.join();
                return Err("error".to_owned());
            }
        }
    };
    Ok(Output {
        status,
        stdout: stdout_reader.join().unwrap_or_default(),
        stderr: stderr_reader.join().unwrap_or_default(),
    })
}

fn terminate_child(child: &mut std::process::Child) {
    let _ = child.kill();
    let _ = child.wait();
}

fn sanitize_remote(value: &str) -> String {
    let without_credentials = if let Some((scheme, rest)) = value.split_once("://") {
        let authority_end = rest.find('/').unwrap_or(rest.len());
        let (authority, suffix) = rest.split_at(authority_end);
        let host = authority.rsplit('@').next().unwrap_or(authority);
        format!("{scheme}://{host}{suffix}")
    } else if let Some((user, host)) = value.split_once('@') {
        if host.contains(':') {
            let _ = user;
            host.to_owned()
        } else {
            value.to_owned()
        }
    } else {
        value.to_owned()
    };
    without_credentials
        .split(['?', '#'])
        .next()
        .unwrap_or("")
        .to_owned()
}

fn parse_github_remote(value: &str) -> Option<GitHubRepository> {
    let trimmed = value.trim();
    let path = if let Some(rest) = trimmed.strip_prefix("https://github.com/") {
        rest
    } else if let Some(rest) = trimmed.strip_prefix("ssh://git@github.com/") {
        rest
    } else if let Some(rest) = trimmed.strip_prefix("git@github.com:") {
        rest
    } else {
        return None;
    };
    let path = path
        .split(['?', '#'])
        .next()
        .unwrap_or("")
        .trim_end_matches('/');
    let mut parts = path.split('/');
    let owner = parts.next()?.trim();
    let repository = parts.next()?.trim_end_matches(".git").trim();
    if parts.next().is_some()
        || owner.is_empty()
        || repository.is_empty()
        || !owner
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-')
        || !repository
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b))
    {
        return None;
    }
    Some(GitHubRepository {
        owner: owner.to_owned(),
        repository: repository.to_owned(),
        canonical_url: format!("https://github.com/{owner}/{repository}"),
    })
}

fn push_error(errors: &mut Vec<String>, message: String) {
    if errors.len() < MAX_ERRORS {
        errors.push(message);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicBool;

    fn inspect_git_fixture(path: &Path) -> GitFacts {
        inspect_git(path, true, &AtomicBool::new(false))
    }

    fn run_git(path: &Path, args: &[&str]) {
        let output = Command::new("git")
            .arg("-C")
            .arg(path)
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git fixture command failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn run_git_value(path: &Path, args: &[&str]) -> String {
        let output = Command::new("git")
            .arg("-C")
            .arg(path)
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git fixture command failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).trim().to_owned()
    }

    #[test]
    fn git_command_child_waits_for_cancellation() {
        if std::env::var_os("NAVIS_GIT_CANCEL_CHILD").is_some() {
            thread::sleep(Duration::from_secs(60));
            return;
        }

        let mut command = Command::new(std::env::current_exe().unwrap());
        command
            .args([
                "--exact",
                "scanner::tests::git_command_child_waits_for_cancellation",
                "--nocapture",
            ])
            .env("NAVIS_GIT_CANCEL_CHILD", "1");
        let cancelled = std::sync::Arc::new(AtomicBool::new(false));
        let cancel_signal = cancelled.clone();
        let cancel_thread = thread::spawn(move || {
            thread::sleep(Duration::from_millis(200));
            cancel_signal.store(true, Ordering::Relaxed);
        });
        let started = Instant::now();
        let result = run_git_command(&mut command, &cancelled);
        cancel_thread.join().unwrap();

        assert!(matches!(result, Err(ref reason) if reason == "cancelled"));
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn reports_unborn_dirty_remotes_and_read_only_commit_facts() {
        if Command::new("git").arg("--version").output().is_err() {
            return;
        }
        let root = tempfile::tempdir_in(env!("CARGO_MANIFEST_DIR")).unwrap();
        run_git(root.path(), &["init", "-q"]);
        let unborn = inspect_git_fixture(root.path());
        assert_eq!(unborn.status, "unborn");
        assert_eq!(unborn.dirty, Some(false));

        run_git(
            root.path(),
            &[
                "-c",
                "user.name=Fixture",
                "-c",
                "user.email=fixture@example.invalid",
                "commit",
                "--allow-empty",
                "-m",
                "fixture commit",
            ],
        );
        run_git(
            root.path(),
            &[
                "remote",
                "add",
                "origin",
                "https://user:secret@example.invalid/team/repo.git?token=hidden",
            ],
        );
        fs::write(root.path().join("untracked.txt"), "fixture").unwrap();
        let facts = inspect_git_fixture(root.path());
        assert_eq!(facts.status, "available");
        assert_eq!(facts.dirty, Some(true));
        assert_eq!(
            facts.latest_commit.as_ref().unwrap().subject,
            "fixture commit"
        );
        assert_eq!(facts.upstream.status, "noUpstream");
        assert_eq!(
            facts.remotes[0].url.as_deref(),
            Some("https://example.invalid/team/repo.git")
        );
        assert!(!facts.remotes[0].url.as_deref().unwrap().contains("secret"));

        run_git(root.path(), &["checkout", "--quiet", "--detach", "HEAD"]);
        assert_eq!(inspect_git_fixture(root.path()).status, "detached");
    }

    #[test]
    fn recognizes_canonical_github_https_and_ssh_remotes_only() {
        for input in [
            "https://github.com/Owner/Repo.git",
            "ssh://git@github.com/Owner/Repo.git",
            "git@github.com:Owner/Repo.git",
        ] {
            let repository = parse_github_remote(input).unwrap();
            assert_eq!(repository.owner, "Owner");
            assert_eq!(repository.repository, "Repo");
            assert_eq!(repository.canonical_url, "https://github.com/Owner/Repo");
        }
        for input in [
            "https://example.com/Owner/Repo.git",
            "http://github.com/Owner/Repo.git",
            "https://github.com/Owner/Repo/extra",
            "https://github.com/Owner/../Repo",
        ] {
            assert!(parse_github_remote(input).is_none(), "{input}");
        }
    }

    #[test]
    fn scan_detects_github_identity_before_sanitizing_ssh_remotes() {
        if Command::new("git").arg("--version").output().is_err() {
            return;
        }
        let root = tempfile::tempdir_in(env!("CARGO_MANIFEST_DIR")).unwrap();
        run_git(root.path(), &["init", "-q"]);
        run_git(
            root.path(),
            &["remote", "add", "origin", "git@github.com:Owner/Repo.git"],
        );

        let facts = inspect_git_fixture(root.path());
        assert_eq!(
            facts.remotes[0].url.as_deref(),
            Some("github.com:Owner/Repo.git")
        );
        let github = facts.remotes[0]
            .github
            .as_ref()
            .expect("GitHub remote should be recognized");
        assert_eq!(github.owner, "Owner");
        assert_eq!(github.repository, "Repo");

        run_git(
            root.path(),
            &[
                "remote",
                "set-url",
                "origin",
                "ssh://git@github.com/Owner/Repo.git",
            ],
        );
        let facts = inspect_git_fixture(root.path());
        assert_eq!(
            facts.remotes[0].url.as_deref(),
            Some("ssh://github.com/Owner/Repo.git")
        );
        assert!(facts.remotes[0].github.is_some());
    }

    #[test]
    fn reports_ahead_and_behind_counts_from_existing_upstream_refs() {
        if Command::new("git").arg("--version").output().is_err() {
            return;
        }
        let root = tempfile::tempdir_in(env!("CARGO_MANIFEST_DIR")).unwrap();
        let local = root.path().join("local");
        fs::create_dir_all(&local).unwrap();
        run_git(&local, &["init", "-q"]);
        run_git(
            &local,
            &[
                "-c",
                "user.name=Fixture",
                "-c",
                "user.email=fixture@example.invalid",
                "commit",
                "--allow-empty",
                "-m",
                "initial",
            ],
        );
        let branch = run_git_value(&local, &["symbolic-ref", "--short", "HEAD"]);
        run_git(
            &local,
            &[
                "remote",
                "add",
                "origin",
                "https://example.invalid/repo.git",
            ],
        );
        let initial = run_git_value(&local, &["rev-parse", "HEAD"]);
        run_git(
            &local,
            &[
                "update-ref",
                &format!("refs/remotes/origin/{branch}"),
                &initial,
            ],
        );
        run_git(
            &local,
            &["config", &format!("branch.{branch}.remote"), "origin"],
        );
        run_git(
            &local,
            &[
                "config",
                &format!("branch.{branch}.merge"),
                &format!("refs/heads/{branch}"),
            ],
        );
        run_git(
            &local,
            &[
                "-c",
                "user.name=Fixture",
                "-c",
                "user.email=fixture@example.invalid",
                "commit",
                "--allow-empty",
                "-m",
                "local ahead",
            ],
        );
        let local_head = run_git_value(&local, &["rev-parse", "HEAD"]);
        let tree = run_git_value(&local, &["rev-parse", "HEAD^{tree}"]);
        let remote_head = run_git_value(
            &local,
            &[
                "-c",
                "user.name=Fixture",
                "-c",
                "user.email=fixture@example.invalid",
                "commit-tree",
                &tree,
                "-p",
                &initial,
                "-m",
                "remote ahead",
            ],
        );
        assert_ne!(local_head, remote_head);
        run_git(
            &local,
            &[
                "update-ref",
                &format!("refs/remotes/origin/{branch}"),
                &remote_head,
            ],
        );

        let facts = inspect_git_fixture(&local);
        assert_eq!(facts.upstream.status, "tracking");
        assert_eq!(facts.upstream.ahead, Some(1));
        assert_eq!(facts.upstream.behind, Some(1));
    }

    #[test]
    fn detects_tauri_mobile_stacks_and_lists_toolchain_availability() {
        let root = tempfile::tempdir().unwrap();
        fs::create_dir_all(root.path().join("src-tauri")).unwrap();
        fs::create_dir_all(root.path().join("android")).unwrap();
        fs::write(root.path().join("src-tauri/tauri.conf.json"), "{}").unwrap();
        fs::write(root.path().join("src-tauri/Cargo.toml"), "").unwrap();
        fs::write(root.path().join("android/build.gradle"), "").unwrap();
        fs::write(root.path().join("build.gradle.kts"), "").unwrap();
        fs::write(
            root.path().join("package.json"),
            r#"{"devDependencies":{"react-native":"*"}}"#,
        )
        .unwrap();
        let names = HashSet::from(["package.json".to_owned(), "build.gradle.kts".to_owned()]);
        let stacks = detect_stacks(root.path(), &names);
        assert!(stacks.contains(&"Tauri".to_owned()));
        assert!(stacks.contains(&"React Native".to_owned()));
        assert!(stacks.contains(&"Android".to_owned()));
        let prerequisites = workflow_prerequisites(root.path(), &names, false, &mut HashMap::new());
        assert!(prerequisites
            .iter()
            .any(|item| item.name == "cargo" && item.required_for.contains(&"Tauri".to_owned())));
        assert!(
            prerequisites
                .iter()
                .any(|item| item.name == "gradle"
                    && item.required_for.contains(&"Android".to_owned()))
        );
        assert!(prerequisites.iter().any(|item| {
            item.name == "gradle" && item.required_for.contains(&"Kotlin/Gradle".to_owned())
        }));
        assert_eq!(
            prerequisites
                .iter()
                .filter(|item| item.name == "gradle")
                .count(),
            1
        );
        assert_eq!(
            prerequisites
                .iter()
                .filter(|item| item.name == "java")
                .count(),
            1
        );
    }

    fn options(depth: u8) -> ScanOptions {
        ScanOptions {
            depth,
            include_hidden: false,
            exclusions: vec![".git".into(), "node_modules".into()],
        }
    }

    fn scan_fixture(root: &Path, depth: u8) -> ScanFinished {
        scan_fixture_with_cache(root, depth, &HashMap::new())
    }

    fn scan_fixture_with_cache(
        root: &Path,
        depth: u8,
        cached_projects: &HashMap<String, ProjectEntry>,
    ) -> ScanFinished {
        scan(
            "fixture",
            root,
            &options(depth),
            &AtomicBool::new(false),
            cached_projects,
            |_| {},
        )
    }

    #[test]
    fn finds_git_repositories_coding_projects_nested_and_worktree_markers() {
        let root = tempfile::tempdir().unwrap();
        let repo = root.path().join("repo with spaces");
        fs::create_dir_all(repo.join(".git")).unwrap();
        fs::create_dir_all(repo.join("nested/.git")).unwrap();
        fs::write(repo.join("nested/package.json"), "{}").unwrap();
        let linked = root.path().join("linked-worktree");
        fs::create_dir_all(&linked).unwrap();
        fs::create_dir_all(root.path().join("git-metadata")).unwrap();
        fs::write(linked.join(".git"), "gitdir: ../git-metadata\n").unwrap();

        let result = scan_fixture(root.path(), 3);
        assert_eq!(result.projects.len(), 3);
        assert!(result
            .projects
            .iter()
            .any(|project| project.name == "repo with spaces" && project.is_git_repository));
        assert!(result.projects.iter().any(|project| {
            project.name == "nested"
                && project.is_git_repository
                && project.detected_stacks == ["Node.js"]
        }));
        assert!(result
            .projects
            .iter()
            .any(|project| project.name == "linked-worktree" && project.is_worktree));
    }

    #[test]
    fn preserves_project_identity_across_rename_and_drops_removed_projects() {
        let root = tempfile::tempdir_in(env!("CARGO_MANIFEST_DIR")).unwrap();
        let original = root.path().join("before");
        fs::create_dir_all(&original).unwrap();
        fs::write(original.join("package.json"), "{}").unwrap();
        let first = scan_fixture(root.path(), 1);
        let original_project = first.projects.first().unwrap().clone();
        let cache = HashMap::from([(original_project.id.clone(), original_project.clone())]);

        let renamed = root.path().join("after");
        fs::rename(&original, &renamed).unwrap();
        let second = scan_fixture_with_cache(root.path(), 1, &cache);
        assert_eq!(second.projects.len(), 1);
        assert_eq!(second.projects[0].id, original_project.id);
        assert_eq!(second.projects[0].name, "after");
        assert_eq!(
            second.projects[0].path,
            fs::canonicalize(&renamed).unwrap().to_string_lossy()
        );

        fs::remove_dir_all(renamed).unwrap();
        let removed = scan_fixture_with_cache(root.path(), 1, &cache);
        assert!(removed.projects.is_empty());
    }

    #[test]
    fn honors_depth_hidden_and_exclusion_limits() {
        let root = tempfile::tempdir().unwrap();
        let deep = root.path().join("one/two/three");
        fs::create_dir_all(&deep).unwrap();
        fs::write(deep.join("Cargo.toml"), "").unwrap();
        fs::create_dir_all(root.path().join("node_modules/pkg")).unwrap();
        fs::write(root.path().join("node_modules/pkg/package.json"), "{}").unwrap();
        fs::create_dir_all(root.path().join(".hidden")).unwrap();
        fs::write(root.path().join(".hidden/package.json"), "{}").unwrap();

        assert!(scan_fixture(root.path(), 2).projects.is_empty());
        let included = scan_fixture(root.path(), 3);
        assert_eq!(included.projects.len(), 1);
        assert_eq!(included.projects[0].detected_stacks, ["Rust"]);
    }

    #[test]
    fn marker_named_directories_do_not_create_coding_projects() {
        let root = tempfile::tempdir().unwrap();
        fs::create_dir(root.path().join("package.json")).unwrap();

        let result = scan_fixture(root.path(), 1);

        assert!(result.projects.is_empty());
        assert!(result.errors.is_empty());
    }

    #[test]
    fn missing_roots_report_a_fatal_error() {
        let root = tempfile::tempdir().unwrap();
        let missing = root.path().join("missing");
        assert!(scan_fixture(&missing, 3).fatal_error.is_some());
        let not_a_folder = root.path().join("file");
        fs::write(&not_a_folder, "").unwrap();
        assert!(scan_fixture(&not_a_folder, 3).fatal_error.is_some());
    }

    #[cfg(unix)]
    #[test]
    fn duplicate_aliases_are_deduplicated() {
        let root = tempfile::tempdir().unwrap();
        let project = root.path().join("project");
        fs::create_dir_all(&project).unwrap();
        fs::write(project.join("package.json"), "{}").unwrap();
        std::os::unix::fs::symlink(&project, root.path().join("alias")).unwrap();
        let result = scan_fixture(root.path(), 2);
        assert_eq!(
            result
                .projects
                .iter()
                .filter(|item| item.detected_stacks == ["Node.js"])
                .count(),
            1
        );
    }

    #[cfg(unix)]
    #[test]
    fn file_symlink_is_not_scanned_as_a_directory() {
        let root = tempfile::tempdir().unwrap();
        let target = root.path().join("notes.txt");
        fs::write(&target, "not a project").unwrap();
        std::os::unix::fs::symlink(&target, root.path().join("notes-link")).unwrap();

        let result = scan_fixture(root.path(), 1);

        assert_eq!(result.directories_scanned, 1);
        assert!(result.projects.is_empty());
        assert!(result.errors.is_empty());
    }

    #[test]
    fn retains_projects_when_a_neighbor_has_broken_worktree_metadata() {
        let root = tempfile::tempdir().unwrap();
        let good = root.path().join("good-project");
        fs::create_dir_all(&good).unwrap();
        fs::write(good.join("package.json"), "{}").unwrap();
        let broken = root.path().join("broken-worktree");
        fs::create_dir_all(&broken).unwrap();
        fs::write(broken.join(".git"), "gitdir: ../missing-git-metadata\n").unwrap();

        let result = scan_fixture(root.path(), 2);
        assert!(result.fatal_error.is_none());
        assert_eq!(result.projects.len(), 1);
        assert_eq!(result.projects[0].name, "good-project");
        assert_eq!(result.errors.len(), 1);
    }

    #[cfg(unix)]
    #[test]
    fn inaccessible_subfolder_reports_an_error_and_keeps_sibling_projects() {
        use std::os::unix::fs::PermissionsExt;

        let root = tempfile::tempdir().unwrap();
        let good = root.path().join("good-project");
        fs::create_dir_all(&good).unwrap();
        fs::write(good.join("package.json"), "{}").unwrap();
        let denied = root.path().join("inaccessible");
        fs::create_dir(&denied).unwrap();
        let original = fs::metadata(&denied).unwrap().permissions();
        fs::set_permissions(&denied, fs::Permissions::from_mode(0)).unwrap();
        let can_read = fs::read_dir(&denied).is_ok();
        if can_read {
            fs::set_permissions(&denied, original).unwrap();
            eprintln!("permission-denied fixture skipped for a privileged test user");
            return;
        }

        let result = scan_fixture(root.path(), 2);
        fs::set_permissions(&denied, original).unwrap();
        assert!(result
            .errors
            .iter()
            .any(|error| error.contains("Could not read a folder")));
        assert!(result
            .projects
            .iter()
            .any(|project| project.name == "good-project"));
    }

    #[cfg(unix)]
    #[test]
    fn inaccessible_root_reports_a_fatal_error() {
        use std::os::unix::fs::PermissionsExt;

        let root = tempfile::tempdir().unwrap();
        let original = fs::metadata(root.path()).unwrap().permissions();
        fs::set_permissions(root.path(), fs::Permissions::from_mode(0)).unwrap();
        if fs::read_dir(root.path()).is_ok() {
            fs::set_permissions(root.path(), original).unwrap();
            eprintln!("permission-denied fixture skipped for a privileged test user");
            return;
        }

        let result = scan_fixture(root.path(), 2);
        fs::set_permissions(root.path(), original).unwrap();
        assert!(result.fatal_error.is_some());
    }

    #[test]
    fn cancellation_keeps_already_discovered_projects() {
        let root = tempfile::tempdir().unwrap();
        fs::write(root.path().join("package.json"), "{}").unwrap();
        let cancelled = AtomicBool::new(false);
        let result = scan(
            "fixture",
            root.path(),
            &options(3),
            &cancelled,
            &HashMap::new(),
            |_| cancelled.store(true, Ordering::Relaxed),
        );
        assert!(result.cancelled);
        assert_eq!(result.projects.len(), 1);
    }

    #[test]
    fn benchmarks_one_hundred_direct_child_projects_against_documented_targets() {
        let root = tempfile::tempdir_in(env!("CARGO_MANIFEST_DIR")).unwrap();
        for index in 0..100 {
            let project = root.path().join(format!("project-{index:03}"));
            fs::create_dir_all(&project).unwrap();
            fs::write(project.join("package.json"), "{}").unwrap();
        }
        let mut first_result = None;
        let started = Instant::now();
        let result = scan(
            "benchmark",
            root.path(),
            &options(1),
            &AtomicBool::new(false),
            &HashMap::new(),
            |progress| {
                if progress.projects_found > 0 && first_result.is_none() {
                    first_result = Some(started.elapsed());
                }
            },
        );
        let total = started.elapsed();
        let first_result = first_result.expect("fixture should discover a project");
        println!(
            "100-project fixture: first result {first_result:?} (target <1s), total {total:?} (target <5s), projects {}, folders {}",
            result.projects.len(), result.directories_scanned
        );
        assert_eq!(result.projects.len(), 100);
        assert!(first_result < Duration::from_secs(1));
        assert!(total < Duration::from_secs(5));
    }
}
