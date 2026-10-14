use serde::Serialize;
use std::collections::{HashSet, VecDeque};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

const MAX_DIRECTORIES: usize = 20_000;
const MAX_PROJECTS: usize = 10_000;
const MAX_ERRORS: usize = 500;

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
        let detected_stacks = detect_stacks(&names);
        if is_git_repository || !detected_stacks.is_empty() {
            let key = path_key(&canonical);
            if project_ids.contains(&key) {
                // Canonical aliases have already contributed this project.
            } else if projects.len() >= MAX_PROJECTS {
                push_error(
                    &mut errors,
                    format!("Scan stopped at the {MAX_PROJECTS} project safety limit."),
                );
                break;
            } else {
                project_ids.insert(key);
                projects.push(ProjectEntry {
                    id: canonical.to_string_lossy().into_owned(),
                    name: canonical
                        .file_name()
                        .map(|name| name.to_string_lossy().into_owned())
                        .unwrap_or_else(|| canonical.to_string_lossy().into_owned()),
                    path: canonical.to_string_lossy().into_owned(),
                    is_git_repository,
                    is_worktree,
                    detected_stacks,
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

fn detect_stacks(names: &HashSet<String>) -> Vec<String> {
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
        ("*.sln", " .NET"),
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
    stacks
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

    fn options(depth: u8) -> ScanOptions {
        ScanOptions {
            depth,
            include_hidden: false,
            exclusions: vec![".git".into(), "node_modules".into()],
        }
    }

    fn scan_fixture(root: &Path, depth: u8) -> ScanFinished {
        scan(
            "fixture",
            root,
            &options(depth),
            &AtomicBool::new(false),
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
        let result = scan("fixture", root.path(), &options(3), &cancelled, |_| {
            cancelled.store(true, Ordering::Relaxed);
        });
        assert!(result.cancelled);
        assert_eq!(result.projects.len(), 1);
    }
}
