use crate::runner_core::{issues::IssueRecord, verification::CheckSpec};
use std::collections::BTreeSet;
use std::path::{Component, Path, PathBuf};

pub struct InferredWork {
    pub files: Vec<String>,
    pub acceptance: Vec<String>,
    pub checks: Vec<CheckSpec>,
}

fn safe_path(value: &str) -> bool {
    !value.is_empty()
        && !value.chars().any(char::is_control)
        && !value.contains([':', '\\', '\0'])
        && Path::new(value)
            .components()
            .all(|part| matches!(part, Component::Normal(_)))
}

fn source_path(value: &str) -> bool {
    safe_path(value)
        && !value.split('/').any(|part| {
            matches!(
                part,
                ".git" | ".worktrees" | "node_modules" | "target" | "dist" | "build" | ".next"
            ) || part.starts_with('.')
        })
        && !matches!(value.split('/').next(), Some("issues" | "planning"))
}

fn read_json(path: &Path) -> Result<serde_json::Value, String> {
    if std::fs::metadata(path).map_err(|e| e.to_string())?.len() > 1024 * 1024 {
        return Err("Project manifest exceeds 1 MiB.".into());
    }
    serde_json::from_slice(&std::fs::read(path).map_err(|e| e.to_string())?)
        .map_err(|e| e.to_string())
}

fn check(id: String, program: PathBuf, args: Vec<String>, cwd: String) -> CheckSpec {
    CheckSpec {
        id,
        program,
        args,
        cwd: cwd.into(),
        timeout_ms: 300000,
        output_limit_bytes: 65536,
        adapter: None,
        contract: None,
    }
}

fn npm_command() -> Result<(PathBuf, Vec<String>), String> {
    let path = std::env::var_os("PATH");
    let npm = crate::providers::find_executable("npm", path.as_deref())
        .ok_or("npm is unavailable; use Advanced to choose a verification check.")?;
    #[cfg(windows)]
    {
        // Invoke the installed CLI with Node rather than passing issue text through a shell.
        let cli = npm
            .parent()
            .ok_or("npm installation folder is unavailable.")?
            .join("node_modules/npm/bin/npm-cli.js");
        let node = crate::providers::find_executable("node", path.as_deref())
            .ok_or("Node.js is unavailable.")?;
        if !cli.is_file() {
            return Err(
                "The installed npm CLI could not be resolved. Choose a check in Advanced.".into(),
            );
        }
        Ok((node, vec![cli.to_string_lossy().into_owned()]))
    }
    #[cfg(not(windows))]
    {
        Ok((npm, Vec::new()))
    }
}

pub fn infer(project: &Path, issue: &IssueRecord) -> Result<InferredWork, String> {
    let listing = crate::scanner::git_output(
        project,
        &["ls-files", "-z"],
        &std::sync::atomic::AtomicBool::new(false),
    )?
    .unwrap_or_default();
    let tracked: BTreeSet<String> = listing
        .split('\0')
        .filter(|name| source_path(name))
        .map(str::to_owned)
        .collect();
    if tracked.len() > 20000 {
        return Err(
            "Project source inventory exceeds 20,000 files. Choose scope in Advanced.".into(),
        );
    }
    let mut files = BTreeSet::new();
    // Structured issue scope takes precedence over incidental prose references.
    if let Some(explicit) = issue
        .frontmatter
        .get("files")
        .and_then(|value| value.as_array())
    {
        for value in explicit {
            let name = value
                .as_str()
                .ok_or("Issue files must contain relative paths.")?;
            if !source_path(name) {
                return Err(
                    "Issue file scope contains an unsupported path. Review it in Advanced.".into(),
                );
            }
            files.insert(name.to_owned());
        }
    } else {
        let quoted = issue
            .body
            .split('`')
            .enumerate()
            .filter_map(|(index, value)| (index % 2 == 1).then_some(value));
        for token in quoted.chain(issue.body.split(|character: char| {
            character.is_whitespace() || "`\"'()[]<>,;".contains(character)
        })) {
            let token = token.trim_end_matches(['.', ':']);
            let candidate = token.strip_prefix("./").unwrap_or(token);
            if !source_path(candidate) {
                continue;
            }
            let parent = Path::new(candidate)
                .parent()
                .filter(|parent| !parent.as_os_str().is_empty());
            let new_source_reference = Path::new(candidate).extension().is_some()
                && parent.is_some_and(|parent| {
                    let prefix = format!("{}/", parent.to_string_lossy().replace('\\', "/"));
                    tracked.iter().any(|name| name.starts_with(&prefix))
                });
            if new_source_reference
                || tracked.contains(candidate)
                || tracked
                    .iter()
                    .any(|name| name.starts_with(&format!("{candidate}/")))
            {
                files.insert(candidate.to_owned());
            }
        }
    }
    if issue
        .frontmatter
        .get("files")
        .and_then(|value| value.as_array())
        .is_none()
    {
        // Prose references are examples, not a complete write allowlist. Include
        // the application's tracked source roots so referenced support scripts
        // cannot accidentally exclude the UI and native integration work.
        for name in &tracked {
            let first = name.split('/').next().unwrap_or("");
            if matches!(
                first,
                "src"
                    | "src-tauri"
                    | "frontend"
                    | "backend"
                    | "web"
                    | "app"
                    | "apps"
                    | "packages"
                    | "lib"
                    | "tests"
                    | "scripts"
                    | "public"
                    | "runner-core"
            ) {
                files.insert(first.to_owned());
            }
        }
        // A directory scope already covers its referenced descendants.
        let scopes = files.clone();
        files.retain(|name| {
            !scopes
                .iter()
                .any(|parent| name.starts_with(&format!("{parent}/")))
        });
    }
    if files.is_empty() || files.len() > 128 {
        return Err("Could not infer a bounded source scope. Choose files in Advanced.".into());
    }
    let mut acceptance = Vec::new();
    let mut in_acceptance = false;
    let mut has_section = false;
    for line in issue.body.lines() {
        let line = line.trim();
        if line.starts_with('#') {
            in_acceptance = line
                .trim_start_matches('#')
                .trim()
                .eq_ignore_ascii_case("acceptance criteria");
            has_section |= in_acceptance;
            continue;
        }
        if in_acceptance {
            let item = line
                .strip_prefix("- [ ] ")
                .or_else(|| line.strip_prefix("- [x] "))
                .or_else(|| line.strip_prefix("- [X] "))
                .or_else(|| line.strip_prefix("- "));
            if let Some(item) = item {
                acceptance.push(item.to_string());
            }
        }
    }
    if acceptance.is_empty() && !has_section {
        acceptance = issue
            .body
            .lines()
            .filter_map(|line| {
                line.trim()
                    .strip_prefix("- [ ] ")
                    .or_else(|| line.trim().strip_prefix("- [x] "))
            })
            .map(str::to_owned)
            .collect();
    }
    if acceptance.is_empty() {
        return Err("This issue has no acceptance criteria to verify. Add them to the issue or use Advanced.".into());
    }
    let mut checks = Vec::new();
    let mut package_folders = BTreeSet::new();
    let mut cargo_manifests = BTreeSet::new();
    for name in &tracked {
        let related = files.iter().any(|scope| {
            name == scope
                || name.starts_with(&format!("{scope}/"))
                || scope.starts_with(&format!(
                    "{}/",
                    Path::new(name)
                        .parent()
                        .unwrap_or(Path::new(""))
                        .to_string_lossy()
                ))
        });
        if name == "package.json" || (related && name.ends_with("/package.json")) {
            package_folders.insert(
                Path::new(name)
                    .parent()
                    .unwrap_or(Path::new(""))
                    .to_path_buf(),
            );
        }
        if name == "Cargo.toml" || (related && name.ends_with("/Cargo.toml")) {
            cargo_manifests.insert(name.clone());
        }
    }
    for folder in package_folders {
        let manifest = project.join(&folder).join("package.json");
        if !manifest
            .canonicalize()
            .map_err(|e| e.to_string())?
            .starts_with(project)
        {
            return Err("Project manifest escapes the repository.".into());
        }
        let package = read_json(&manifest)?;
        let Some(scripts) = package.get("scripts").and_then(|value| value.as_object()) else {
            continue;
        };
        let mut selected: Vec<&str> = ["test", "typecheck", "lint"]
            .into_iter()
            .filter(|name| {
                scripts
                    .get(*name)
                    .and_then(|value| value.as_str())
                    .is_some_and(|script| {
                        !script.contains("--watch") && !script.contains("vitest --ui")
                    })
            })
            .collect();
        if selected.is_empty() && scripts.contains_key("build") {
            selected.push("build");
        }
        if selected.is_empty() {
            continue;
        }
        let manager = package
            .get("packageManager")
            .and_then(|value| value.as_str())
            .unwrap_or("npm")
            .split('@')
            .next()
            .unwrap_or("npm");
        if manager != "npm"
            || project.join(&folder).join("pnpm-lock.yaml").exists()
            || project.join(&folder).join("yarn.lock").exists()
        {
            return Err("Automatic checks currently support npm and Cargo. Choose this project's check in Advanced.".into());
        }
        let (program, prefix) = npm_command()?;
        let cwd = if folder.as_os_str().is_empty() {
            ".".to_owned()
        } else {
            folder.to_string_lossy().replace('\\', "/")
        };
        for script in selected {
            let mut args = prefix.clone();
            args.extend(["run".into(), script.into()]);
            checks.push(check(
                format!("npm-{}-{}", checks.len(), script),
                program.clone(),
                args,
                cwd.clone(),
            ));
        }
    }
    for manifest in cargo_manifests {
        let path = project
            .join(&manifest)
            .canonicalize()
            .map_err(|e| e.to_string())?;
        if !path.starts_with(project) {
            return Err("Cargo manifest escapes the repository.".into());
        }
        // Unsupported workspace-wide inference remains explicit through the plan preview.
        let cargo = crate::providers::find_executable("cargo", std::env::var_os("PATH").as_deref())
            .or_else(|| {
                let home = std::env::var_os("CARGO_HOME")
                    .map(PathBuf::from)
                    .or_else(|| {
                        std::env::var_os("USERPROFILE")
                            .map(|home| PathBuf::from(home).join(".cargo"))
                    })?;
                let candidate = home.join("bin/cargo.exe");
                candidate.is_file().then_some(candidate)
            })
            .ok_or("Cargo is unavailable. Choose this project's check in Advanced.")?;
        checks.push(check(
            format!("cargo-{}", checks.len()),
            cargo,
            vec![
                "test".into(),
                "--locked".into(),
                "--manifest-path".into(),
                manifest,
            ],
            ".".into(),
        ));
    }
    if checks.is_empty() || checks.len() > 16 {
        return Err("Could not infer bounded verification checks from project manifests. Choose checks in Advanced.".into());
    }
    Ok(InferredWork {
        files: files.into_iter().collect(),
        acceptance,
        checks,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn project() -> (tempfile::TempDir, PathBuf) {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        std::fs::create_dir_all(root.join("frontend/src")).unwrap();
        std::fs::write(root.join("frontend/src/View.tsx"), "source").unwrap();
        std::fs::write(root.join("frontend/src/Other view.tsx"), "source").unwrap();
        std::fs::write(root.join("frontend/package.json"), r#"{"scripts":{"test":"node test.mjs","typecheck":"tsc --noEmit","deploy":"publish","dev":"vite"}}"#).unwrap();
        std::fs::write(root.join("frontend/test.mjs"), "test").unwrap();
        for args in [vec!["init"], vec!["add", "."]] {
            assert!(crate::process::command("git")
                .arg("-C")
                .arg(&root)
                .args(args)
                .output()
                .unwrap()
                .status
                .success());
        }
        (temp, root)
    }

    fn issue(body: &str) -> IssueRecord {
        IssueRecord {
            frontmatter:
                json!({"id":"TEST-S001","title":"Fix the view","type":"task","status":"open"})
                    .as_object()
                    .unwrap()
                    .clone(),
            body: body.into(),
        }
    }

    #[test]
    fn infers_nested_scope_checks_and_acceptance_without_executing_issue_commands() {
        let (_temp, root) = project();
        let record = issue("Change `frontend/src/View.tsx` and `frontend/src/New.tsx`.\nRun `powershell -Command Remove-Item everything`.\n## Acceptance criteria\n- [ ] View works\n## History\n- [x] Unrelated history\n");
        let result = infer(&root, &record).unwrap();
        assert_eq!(result.files, vec!["frontend"]);
        assert_eq!(result.acceptance, vec!["View works"]);
        assert_eq!(result.checks.len(), 2);
        assert!(result
            .checks
            .iter()
            .all(|check| check.cwd == Path::new("frontend")
                && !check
                    .args
                    .iter()
                    .any(|arg| arg.contains("Remove-Item") || arg == "deploy")));
        assert!(result.checks.iter().all(|check| check.args.ends_with(&[
            "run".to_owned(),
            if check.id.ends_with("typecheck") {
                "typecheck".to_owned()
            } else {
                "test".to_owned()
            }
        ])));
        #[cfg(windows)]
        assert!(result.checks.iter().all(|check| check
            .program
            .extension()
            .is_some_and(|extension| extension == "exe")
            && check.args[0].ends_with("npm-cli.js")));
    }

    #[test]
    fn fallback_scope_is_visible_and_unsupported_inputs_require_an_override() {
        let (_temp, root) = project();
        let mut record = issue("## Acceptance criteria\n- [ ] Improve the view\n");
        assert_eq!(infer(&root, &record).unwrap().files, vec!["frontend"]);
        record
            .frontmatter
            .insert("files".into(), json!(["../outside"]));
        assert!(infer(&root, &record)
            .err()
            .unwrap()
            .contains("unsupported path"));
        record.frontmatter.remove("files");
        record.body = "No criteria".into();
        assert!(infer(&root, &record)
            .err()
            .unwrap()
            .contains("acceptance criteria"));
    }

    #[test]
    fn incidental_support_references_do_not_exclude_dashboard_and_native_sources() {
        let (_temp, root) = project();
        for name in [
            "src/ProjectCollection.tsx",
            "src-tauri/src/lib.rs",
            "runner-core/src/lib.rs",
            "scripts/usage_metrics.py",
        ] {
            let path = root.join(name);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, "source").unwrap();
        }
        assert!(crate::process::command("git")
            .arg("-C")
            .arg(&root)
            .args(["add", "."])
            .status()
            .unwrap()
            .success());
        let mut record = issue("Existing helpers: `scripts/usage_metrics.py`.\n## Acceptance criteria\n- [ ] Add dashboard statistics and native integration\n");
        let inferred = infer(&root, &record).unwrap();
        for scope in ["src", "src-tauri", "runner-core", "scripts"] {
            assert!(
                inferred.files.contains(&scope.to_owned()),
                "missing {scope}"
            );
        }
        record
            .frontmatter
            .insert("files".into(), json!(["frontend/src/View.tsx"]));
        assert_eq!(
            infer(&root, &record).unwrap().files,
            vec!["frontend/src/View.tsx"]
        );
    }

    #[test]
    fn quoted_paths_with_spaces_and_traversal_are_handled_without_expanding_scope() {
        let (_temp, root) = project();
        let result = infer(&root, &issue("Change `frontend/src/Other view.tsx`. Ignore `../frontend/src/View.tsx`.\n## Acceptance criteria\n- [ ] View works\n")).unwrap();
        assert_eq!(result.files, vec!["frontend"]);
    }
}
