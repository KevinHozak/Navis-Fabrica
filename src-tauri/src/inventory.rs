use crate::scanner::ProjectEntry;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::fs;
use std::io::{Read, Write};
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};
use tempfile::NamedTempFile;

const SCHEMA_VERSION: u32 = 1;
const MAX_CACHE_BYTES: u64 = 32 * 1024 * 1024;
const MAX_PROJECTS: usize = 10_000;

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SavedInventory {
    schema_version: u32,
    project_root: String,
    saved_at_ms: u64,
    projects: Vec<ProjectEntry>,
}

pub fn load(path: &Path, expected_root: &str) -> Vec<ProjectEntry> {
    let Ok(metadata) = fs::metadata(path) else {
        return Vec::new();
    };
    if !metadata.is_file() || metadata.len() > MAX_CACHE_BYTES {
        return Vec::new();
    }
    let Ok(file) = fs::File::open(path) else {
        return Vec::new();
    };
    let mut contents = Vec::with_capacity(metadata.len().min(MAX_CACHE_BYTES) as usize);
    if file
        .take(MAX_CACHE_BYTES + 1)
        .read_to_end(&mut contents)
        .is_err()
        || contents.len() as u64 > MAX_CACHE_BYTES
    {
        return Vec::new();
    }
    let Ok(saved) = serde_json::from_slice::<SavedInventory>(&contents) else {
        return Vec::new();
    };
    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(u64::MAX as u128) as u64;
    if saved.schema_version != SCHEMA_VERSION
        || normalize_root(&saved.project_root) != normalize_root(expected_root)
        || saved.projects.len() > MAX_PROJECTS
        || saved.saved_at_ms > now_ms.saturating_add(5 * 60 * 1_000)
    {
        return Vec::new();
    }
    let projects = deduplicate(saved.projects);
    if projects.iter().any(|project| !is_valid_project(project)) {
        return Vec::new();
    }
    projects
}

pub fn save(path: &Path, project_root: &str, projects: &[ProjectEntry]) -> Result<(), String> {
    if projects.len() > MAX_PROJECTS || project_root.is_empty() || project_root.len() > 32_768 {
        return Err("Project inventory exceeds the supported cache size.".into());
    }
    let projects = sanitize_projects(deduplicate(projects.to_vec()));
    if projects.iter().any(|project| !is_valid_project(project)) {
        return Err("Project inventory contains invalid project data.".into());
    }
    let saved = SavedInventory {
        schema_version: SCHEMA_VERSION,
        project_root: project_root.to_owned(),
        saved_at_ms: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis()
            .min(u64::MAX as u128) as u64,
        projects,
    };
    let contents = serde_json::to_vec(&saved)
        .map_err(|error| format!("Could not encode project inventory: {error}"))?;
    if contents.len() as u64 > MAX_CACHE_BYTES {
        return Err("Project inventory exceeds the supported cache size.".into());
    }
    let parent = path
        .parent()
        .ok_or_else(|| "Project inventory location has no parent folder.".to_string())?;
    fs::create_dir_all(parent)
        .map_err(|error| format!("Could not create project inventory folder: {error}"))?;
    let mut temporary = NamedTempFile::new_in(parent)
        .map_err(|error| format!("Could not prepare project inventory file: {error}"))?;
    temporary
        .write_all(&contents)
        .and_then(|()| temporary.as_file().sync_all())
        .map_err(|error| format!("Could not write project inventory: {error}"))?;
    replace_file(temporary, path)
}

fn deduplicate(projects: Vec<ProjectEntry>) -> Vec<ProjectEntry> {
    let mut ids = HashSet::new();
    let mut paths = HashSet::new();
    projects
        .into_iter()
        .filter(|project| {
            !project.id.is_empty()
                && ids.insert(project.id.clone())
                && paths.insert(normalize_root(&project.path))
        })
        .take(MAX_PROJECTS)
        .collect()
}

fn is_valid_project(project: &ProjectEntry) -> bool {
    !project.id.is_empty()
        && project.id.len() <= 512
        && !project.name.is_empty()
        && project.name.len() <= 1_024
        && !project.path.is_empty()
        && project.path.len() <= 32_768
        && project.detected_stacks.len() <= 32
        && project.workflow_prerequisites.len() <= 64
        && project.git.remotes.len() <= 64
        && project
            .git
            .branch
            .as_ref()
            .is_none_or(|branch| branch.len() <= 1_024)
        && project
            .git
            .latest_commit
            .as_ref()
            .is_none_or(|commit| commit.short_hash.len() <= 64 && commit.subject.len() <= 1_024)
        && project.git.remotes.iter().all(|remote| {
            remote.name.len() <= 128
                && remote.url.as_ref().is_none_or(|url| {
                    url.len() <= 4_096 && crate::scanner::sanitize_remote(url) == *url
                })
                && remote.github.as_ref().is_none_or(|repository| {
                    repository.owner.len() <= 256
                        && repository.repository.len() <= 256
                        && repository.canonical_url.len() <= 1_024
                })
        })
}

fn sanitize_projects(mut projects: Vec<ProjectEntry>) -> Vec<ProjectEntry> {
    for project in &mut projects {
        for remote in &mut project.git.remotes {
            remote.url = remote.url.as_deref().map(crate::scanner::sanitize_remote);
        }
    }
    projects
}

fn normalize_root(root: &str) -> String {
    let root = root.replace('\\', "/");
    let root = root.strip_prefix("//?/").unwrap_or(&root);
    let root = root.trim_end_matches('/');
    if cfg!(windows) {
        root.to_lowercase()
    } else {
        root.to_owned()
    }
}

#[cfg(windows)]
fn replace_file(temporary: NamedTempFile, destination: &Path) -> Result<(), String> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::ReplaceFileW;

    let (file, temporary_path) = temporary.keep().map_err(|error| {
        format!(
            "Could not prepare project inventory replacement: {}",
            error.error
        )
    })?;
    drop(file);
    let source: Vec<u16> = temporary_path
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect();
    let target: Vec<u16> = destination
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect();
    let result = match fs::rename(&temporary_path, destination) {
        Ok(()) => Ok(()),
        Err(_rename_error) if destination.exists() => {
            let replaced = unsafe {
                ReplaceFileW(
                    target.as_ptr(),
                    source.as_ptr(),
                    std::ptr::null(),
                    0,
                    std::ptr::null(),
                    std::ptr::null_mut(),
                )
            };
            if replaced != 0 {
                Ok(())
            } else {
                Err(std::io::Error::last_os_error())
            }
        }
        Err(error) => Err(error),
    };
    if let Err(error) = result {
        let _ = fs::remove_file(&temporary_path);
        return Err(format!("Could not replace project inventory: {error}"));
    }
    Ok(())
}

#[cfg(not(windows))]
fn replace_file(temporary: NamedTempFile, destination: &Path) -> Result<(), String> {
    temporary
        .persist(destination)
        .map_err(|error| format!("Could not replace project inventory: {}", error.error))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scanner::{GitFacts, UpstreamDivergence};

    fn project(id: &str, name: &str, path: &str) -> ProjectEntry {
        ProjectEntry {
            id: id.into(),
            name: name.into(),
            path: path.into(),
            is_git_repository: true,
            is_worktree: false,
            detected_stacks: vec!["Rust".into()],
            git: GitFacts {
                status: "available".into(),
                branch: Some("main".into()),
                dirty: Some(false),
                latest_commit: None,
                remotes: Vec::new(),
                upstream: UpstreamDivergence {
                    status: "noUpstream".into(),
                    ahead: None,
                    behind: None,
                },
            },
            workflow_prerequisites: Vec::new(),
        }
    }

    #[test]
    fn restores_inventory_and_atomically_replaces_prior_snapshot() {
        let folder = tempfile::tempdir_in(env!("CARGO_MANIFEST_DIR")).unwrap();
        let path = folder.path().join("project-inventory.json");
        let mut original = project("one", "Before", r"C:\Projects\before");
        original.git.remotes.push(crate::scanner::GitRemote {
            name: "origin".into(),
            url: Some("https://user:secret@github.com/owner/repo?access_token=secret".into()),
            github: None,
        });
        save(&path, r"C:\Projects", &[original]).unwrap();
        let mut updated = project("one", "After", r"C:\Projects\after");
        updated.git.remotes.push(crate::scanner::GitRemote {
            name: "origin".into(),
            url: Some("https://user:secret@github.com/owner/repo?access_token=secret".into()),
            github: None,
        });
        save(&path, r"C:\Projects", &[updated]).unwrap();

        let restored = load(&path, "c:/projects/");
        assert_eq!(restored.len(), 1);
        assert_eq!(restored[0].name, "After");
        assert_eq!(restored[0].path, r"C:\Projects\after");
        assert_eq!(
            restored[0].git.remotes[0].url.as_deref(),
            Some("https://github.com/owner/repo")
        );
        let saved_contents = fs::read_to_string(path).unwrap();
        assert!(!saved_contents.contains("secret"));
    }

    #[test]
    fn complete_refresh_reconciles_removed_moved_and_added_projects() {
        let folder = tempfile::tempdir_in(env!("CARGO_MANIFEST_DIR")).unwrap();
        let path = folder.path().join("project-inventory.json");
        save(
            &path,
            r"C:\Projects",
            &[
                project("stable-id", "Old name", r"C:\Projects\old-path"),
                project("removed", "Removed", r"C:\Projects\removed"),
            ],
        )
        .unwrap();
        let refreshed = vec![
            project("stable-id", "Renamed", r"C:\Projects\new-path"),
            project("added", "Added", r"C:\Projects\added"),
        ];
        save(&path, r"C:\Projects", &refreshed).unwrap();

        let restored = load(&path, r"C:\Projects");
        assert_eq!(restored.len(), 2);
        assert_eq!(restored[0].name, "Renamed");
        assert!(!restored.iter().any(|project| project.id == "removed"));
        assert_eq!(restored[1].id, "added");
    }

    #[test]
    fn invalid_incompatible_oversized_and_wrong_root_cache_data_falls_back_empty() {
        let folder = tempfile::tempdir_in(env!("CARGO_MANIFEST_DIR")).unwrap();
        let path = folder.path().join("project-inventory.json");
        fs::write(&path, "not json").unwrap();
        assert!(load(&path, r"C:\Projects").is_empty());

        fs::write(
            &path,
            r#"{"schemaVersion":2,"projectRoot":"C:\\Projects","savedAtMs":0,"projects":[]}"#,
        )
        .unwrap();
        assert!(load(&path, r"C:\Projects").is_empty());

        save(&path, r"D:\Work", &[project("one", "One", r"D:\Work\one")]).unwrap();
        assert!(load(&path, r"C:\Projects").is_empty());

        let file = fs::OpenOptions::new().write(true).open(&path).unwrap();
        file.set_len(MAX_CACHE_BYTES + 1).unwrap();
        assert!(load(&path, r"D:\Work").is_empty());
    }
}
