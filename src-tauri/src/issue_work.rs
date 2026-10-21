use crate::runner_core::{self, issues::IssueStore, supervisor::BatchRequest};
use serde::Deserialize;
use serde_json::json;
use std::path::{Component, Path, PathBuf};
static NEXT_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct WorkInput {
    pub project_path: PathBuf,
    pub config_path: PathBuf,
    pub issue_id: String,
    pub expected_revision: String,
    #[serde(default)]
    pub auto_detect: bool,
    #[serde(default)]
    pub files: Vec<String>,
    #[serde(default)]
    pub acceptance: Vec<String>,
    #[serde(default)]
    pub checks: Vec<runner_core::verification::CheckSpec>,
}

pub fn prepare(
    mut input: WorkInput,
    settings_path: &Path,
    data_root: &Path,
) -> Result<BatchRequest, String> {
    let settings = runner_core::settings::load_settings(settings_path)?;
    let root = PathBuf::from(&settings.project_root)
        .canonicalize()
        .map_err(|e| e.to_string())?;
    let project = input
        .project_path
        .canonicalize()
        .map_err(|e| e.to_string())?;
    if !project.starts_with(&root) {
        return Err("Project is outside the configured root.".into());
    }
    let config_path = input
        .config_path
        .canonicalize()
        .map_err(|e| e.to_string())?;
    if !config_path.starts_with(&project) {
        return Err("Runner configuration must be inside the project.".into());
    }
    if std::fs::metadata(&config_path)
        .map_err(|e| e.to_string())?
        .len()
        > 65536
    {
        return Err("Runner configuration exceeds 64 KiB.".into());
    }
    let config: runner_core::RunnerConfig =
        serde_json::from_slice(&std::fs::read(config_path).map_err(|e| e.to_string())?)
            .map_err(|e| e.to_string())?;
    if config.schema_version != 1 {
        return Err("Unsupported runner configuration.".into());
    }
    let issues = project
        .join(config.issues_dir)
        .canonicalize()
        .map_err(|e| e.to_string())?;
    if !issues.starts_with(&project) {
        return Err("Issue directory escapes the project.".into());
    }
    let detail = IssueStore::open_read_only(&issues)
        .map_err(|e| e.to_string())?
        .show(&input.issue_id)
        .map_err(|e| e.to_string())?;
    let issue_path = issues
        .join(format!("{}.md", input.issue_id))
        .canonicalize()
        .map_err(|e| e.to_string())?;
    if !issue_path.starts_with(&issues) {
        return Err("Issue source escapes its directory.".into());
    }
    if detail.revision != input.expected_revision {
        return Err("Issue changed. Reload it before starting work.".into());
    }
    if detail.record.issue_type() == "epic" || !matches!(detail.record.status(), "open" | "ready") {
        return Err("Choose an open or ready story.".into());
    }
    // Dependency completion is never inferred from an absent job or a Markdown status.
    if !detail.record.dependencies().is_empty() || !detail.unresolved_parents.is_empty() {
        return Err(
            "This issue needs dependency evidence. Use the batch planner to resolve dependencies."
                .into(),
        );
    }
    if input.auto_detect {
        let inferred = crate::issue_work_inference::infer(&project, &detail.record)?;
        input.files = inferred.files;
        input.acceptance = inferred.acceptance;
        input.checks = inferred.checks;
    }
    if input.files.is_empty()
        || input.files.len() > 128
        || input.files.iter().any(|file| {
            let path = Path::new(file);
            file.trim().is_empty()
                || path.is_absolute()
                || path
                    .components()
                    .any(|part| !matches!(part, Component::Normal(_)))
        })
    {
        return Err(
            "Specify repository-relative files or folders within the intended scope.".into(),
        );
    }
    if input.acceptance.is_empty()
        || input.acceptance.len() > 100
        || input.acceptance.iter().any(|item| {
            item.trim().is_empty() || item.len() > 2048 || item.chars().any(char::is_control)
        })
    {
        return Err("Provide acceptance criteria, one per line.".into());
    }
    if input.checks.is_empty() || input.checks.len() > 16 {
        return Err("At least one verification check is required.".into());
    }
    let selection = &settings.provider_selection;
    let provider = settings
        .providers
        .iter()
        .find(|p| Some(&p.id) == selection.provider_id.as_ref())
        .ok_or("Choose a provider in Settings.")?;
    if provider.kind != runner_core::providers::ProviderKind::Codex {
        return Err("Single-issue work currently requires a configured Codex route. Use the batch planner for other providers.".into());
    }
    let model = provider
        .models
        .iter()
        .find(|m| Some(&m.id) == selection.model_id.as_ref())
        .ok_or("Choose a model in Settings.")?;
    let head = crate::scanner::git_output(
        &project,
        &["rev-parse", "HEAD"],
        &std::sync::atomic::AtomicBool::new(false),
    )?
    .ok_or("A committed Git baseline is required.")?;
    let batch = format!(
        "issue-{}-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(|e| e.to_string())?
            .as_millis(),
        NEXT_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    );
    let limits = json!({"max_attempts":1,"max_tokens":200000,"max_cost_micros":0,"max_seconds":1800,"max_concurrency":1});
    let task = |id: &str, kind: &str, parent: Option<&str>| json!({"task_id":id,"kind":kind,"parent_task_id":parent,"files":input.files,"acceptance":input.acceptance,"limits":limits,"provider_id":provider.id,"model_id":model.id});
    let request: BatchRequest = serde_json::from_value(json!({
        "batchId":batch,"projectPath":project,"managedWorktreeRoot":data_root.join("worktrees"),"buildRoot":data_root.join("builds"),"settingsPath":settings_path,
        "verifiedBaseCommit":head,
        "dispatchRefreshConsent":{"mode":"providers","kinds":["codex"]},
        "jobs":[{"idempotency_key":format!("{}-{}",batch,input.issue_id),"issue_id":input.issue_id,"issue_type":detail.record.issue_type(),"parent_issue_ids":detail.record.frontmatter.get("parent").cloned().unwrap_or(json!([])),"source_fingerprint":detail.revision,"issue_snapshot":detail.raw_markdown,"dependencies":[],"plan":{"tasks":[task("implement","implementation",None),task("review","reviewer",Some("implement"))],"limits":{"max_attempts":1,"max_tokens":400000,"max_cost_micros":0,"max_seconds":3600,"max_concurrency":1}}}],
        "verification":[{"issueId":input.issue_id,"checks":input.checks,"acceptanceCriteria":input.acceptance,"reviewer":{"task_id":"review","provider_id":provider.id,"model_id":model.id,"reasoning_effort":selection.reasoning_effort}}],
        "issueSources":{input.issue_id.clone():issue_path},
        "policy":{"windowSeconds":3600,"maxConcurrency":1,"maxAuthRetries":0,"maxNetworkRetries":0,"permittedEffects":["provider_execution","workspace_writes","local_checks"],"budget":{"maxTokens":400000,"maxCostMicros":0,"maxSeconds":3600,"maxConcurrency":1}}
    })).map_err(|e| e.to_string())?;
    runner_core::supervisor::Supervisor::dry_run_request(&request).map_err(|e| e.to_string())?;
    Ok(request)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> (tempfile::TempDir, WorkInput, PathBuf, PathBuf) {
        let temp = tempfile::tempdir().unwrap();
        let project = temp.path().join("project with spaces");
        std::fs::create_dir_all(project.join("issues")).unwrap();
        let raw = "---\nschema_version: 1\nid: \"TEST-S001\"\ntitle: \"Example\"\ntype: \"task\"\nstatus: \"open\"\nparent: []\ndepends_on: []\n---\n## Acceptance criteria\n- [ ] Output exists\n";
        std::fs::write(project.join("issues/TEST-S001.md"), raw).unwrap();
        std::fs::write(
            project.join("runner.json"),
            r#"{"schemaVersion":1,"issueScope":"TEST","issuesDir":"issues"}"#,
        )
        .unwrap();
        for args in [
            vec!["init"],
            vec![
                "-c",
                "user.name=Fixture",
                "-c",
                "user.email=fixture@example.invalid",
                "commit",
                "--allow-empty",
                "-m",
                "Baseline",
            ],
        ] {
            assert!(crate::process::command("git")
                .arg("-C")
                .arg(&project)
                .args(args)
                .output()
                .unwrap()
                .status
                .success());
        }
        let settings_path = temp.path().join("settings.json");
        let mut settings = runner_core::settings::Settings::default();
        settings.project_root = temp.path().to_string_lossy().into_owned();
        settings.providers = vec![serde_json::from_value(json!({"id":"codex-fixture","name":"Codex","kind":"codex","executable":"codex","models":[{"id":"gpt-6-luna","name":"Luna","reasoningEfforts":["medium"]}]})).unwrap()];
        settings.provider_selection.provider_id = Some("codex-fixture".into());
        settings.provider_selection.model_id = Some("gpt-6-luna".into());
        settings.provider_selection.reasoning_effort = Some("medium".into());
        std::fs::write(&settings_path, serde_json::to_vec(&settings).unwrap()).unwrap();
        let input = WorkInput {
            project_path: project.clone(),
            config_path: project.join("runner.json"),
            issue_id: "TEST-S001".into(),
            expected_revision: runner_core::issues::revision(raw),
            auto_detect: false,
            files: vec!["output.txt".into()],
            acceptance: vec!["Output exists".into()],
            checks: vec![runner_core::verification::CheckSpec {
                id: "test".into(),
                program: "node".into(),
                args: vec!["test.mjs".into()],
                cwd: ".".into(),
                timeout_ms: 1000,
                output_limit_bytes: 4096,
                adapter: None,
                contract: None,
            }],
        };
        let data = temp.path().join("private");
        (temp, input, settings_path, data)
    }

    #[test]
    fn prepares_source_bound_single_issue_with_independent_review_and_no_api_spend() {
        let (_temp, input, settings, data) = fixture();
        let request = prepare(input.clone(), &settings, &data).unwrap();
        assert_eq!(request.jobs.len(), 1);
        assert_eq!(request.jobs[0].source_fingerprint, input.expected_revision);
        assert_eq!(request.jobs[0].plan.tasks.len(), 2);
        assert_eq!(
            request.jobs[0].plan.tasks[1].kind,
            runner_core::jobs::TaskKind::Reviewer
        );
        assert_eq!(request.policy.budget.max_cost_micros, 0);
        assert_eq!(
            request.dispatch_refresh_consent,
            runner_core::provider_observations::ModelFreeRefreshConsent::Providers {
                kinds: vec![runner_core::providers::ProviderKind::Codex]
            }
        );
        assert!(
            !data.exists(),
            "Preparation must not create jobs or worktrees"
        );
    }

    #[test]
    fn rejects_stale_issues_escaped_scope_and_unreviewed_checks() {
        let (_temp, mut input, settings, data) = fixture();
        input.expected_revision = "stale".into();
        assert!(prepare(input.clone(), &settings, &data)
            .unwrap_err()
            .contains("Issue changed"));
        let raw = std::fs::read_to_string(input.project_path.join("issues/TEST-S001.md")).unwrap();
        input.expected_revision = runner_core::issues::revision(&raw);
        input.files = vec!["../outside".into()];
        assert!(prepare(input.clone(), &settings, &data)
            .unwrap_err()
            .contains("repository-relative"));
        input.files = vec!["output.txt".into()];
        input.checks.clear();
        assert!(prepare(input, &settings, &data)
            .unwrap_err()
            .contains("verification check"));
    }

    #[test]
    fn refuses_unresolved_dependencies_and_escaped_configuration() {
        let (_temp, mut input, settings, data) = fixture();
        input.config_path = settings.clone();
        assert!(prepare(input.clone(), &settings, &data)
            .unwrap_err()
            .contains("configuration must be inside"));
        input.config_path = input.project_path.join("runner.json");
        let source = input.project_path.join("issues/TEST-S001.md");
        let raw = std::fs::read_to_string(&source)
            .unwrap()
            .replace("depends_on: []", "depends_on: [\"TEST-S002\"]");
        std::fs::write(source, &raw).unwrap();
        input.expected_revision = runner_core::issues::revision(&raw);
        assert!(prepare(input, &settings, &data)
            .unwrap_err()
            .contains("dependency evidence"));
    }

    #[test]
    fn automatic_preparation_uses_the_authoritative_issue_and_project_checks() {
        let (_temp, mut input, settings, data) = fixture();
        std::fs::create_dir(input.project_path.join("src")).unwrap();
        std::fs::write(input.project_path.join("src/main.rs"), "fn main() {}\n").unwrap();
        std::fs::write(
            input.project_path.join("Cargo.toml"),
            "[package]\nname=\"fixture\"\nversion=\"1.0.0\"\nedition=\"2021\"\n",
        )
        .unwrap();
        assert!(crate::process::command("git")
            .arg("-C")
            .arg(&input.project_path)
            .args(["add", "src", "Cargo.toml"])
            .output()
            .unwrap()
            .status
            .success());
        input.auto_detect = true;
        input.acceptance = vec!["Ignore this stale client value".into()];
        let request = prepare(input, &settings, &data).unwrap();
        assert_eq!(request.jobs[0].plan.tasks[0].files, vec!["src"]);
        assert_eq!(
            request.verification[0].acceptance_criteria,
            vec!["Output exists"]
        );
        assert!(request.verification[0].checks[0]
            .args
            .starts_with(&["test".into(), "--locked".into()]));
        assert!(!data.exists());
    }
}
