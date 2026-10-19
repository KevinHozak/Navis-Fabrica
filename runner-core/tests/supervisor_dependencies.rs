//! Synthetic selected-issue dependency admission; no live issue source or provider calls.
mod common;
use navis_fabrica_runner_core::{
    budgets::BudgetPolicy,
    issues,
    jobs::{
        DependencyResolution, DependencySpec, JobPlan, JobState, JobsStore, PlannedTask,
        ResourceLimits, SubmitJob, TaskKind,
    },
    providers::{ModelProfile, ProviderKind, ProviderProfile},
    settings::Settings,
    supervisor::{
        preview_batch_request, BatchPolicy, BatchRequest, PermittedEffect, Supervisor,
        VerificationPolicy,
    },
    verification::{CheckSpec, ReviewerIdentity},
};
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
};

fn limits() -> ResourceLimits {
    ResourceLimits {
        max_attempts: 1,
        max_tokens: Some(200),
        max_cost_micros: Some(0),
        max_seconds: Some(30),
        max_concurrency: 1,
    }
}

#[test]
fn selected_gp_chain_waits_for_verified_predecessors_and_independent_issue_can_dispatch() {
    let temp = tempfile::tempdir().unwrap();
    let project = temp.path().join("synthetic-project");
    let issue_dir = project.join("issues");
    fs::create_dir_all(&issue_dir).unwrap();
    fs::write(project.join("README.md"), "synthetic\n").unwrap();

    let issue_ids = ["GP-109", "GP-110", "GP-111", "GP-112", "GP-199"];
    let mut jobs = Vec::new();
    let mut verification = Vec::new();
    let mut issue_sources = BTreeMap::new();
    let mut fingerprints = BTreeMap::new();
    let limits = limits();
    for id in issue_ids {
        let raw = format!("---\nid: {id}\ntitle: Synthetic {id}\n---\n\n## Acceptance criteria\n- [ ] synthetic result exists\n");
        let path = issue_dir.join(format!("{id}.md"));
        fs::write(&path, raw.as_bytes()).unwrap();
        issue_sources.insert(id.to_owned(), path.canonicalize().unwrap());
        let fingerprint = issues::revision(&raw);
        fingerprints.insert(id.to_owned(), fingerprint.clone());
        let acceptance = vec!["synthetic result exists".to_owned()];
        let task = |task_id: &str, kind: TaskKind, parent: Option<&str>| PlannedTask {
            task_id: task_id.into(),
            parent_task_id: parent.map(str::to_owned),
            kind,
            files: vec!["result.txt".into()],
            acceptance: acceptance.clone(),
            limits: limits.clone(),
            provider_id: Some("fixture-codex".into()),
            model_id: Some("fixture-model".into()),
            route_requirements: None,
        };
        let mut dependencies = Vec::new();
        if id == "GP-109" {
            dependencies.push(DependencySpec {
                issue_id: "GP-108".into(),
                resolution: DependencyResolution::ExternalClosed {
                    source: "synthetic-fixture".into(),
                    reference: "GP-108".into(),
                    observed_at: "2026-10-16T00:00:00Z".into(),
                    evidence: "Fixture records GP-108 as closed before selection.".into(),
                },
            });
        } else if let Some(previous) = match id {
            "GP-110" => Some("GP-109"),
            "GP-111" => Some("GP-110"),
            "GP-112" => Some("GP-111"),
            _ => None,
        } {
            dependencies.push(DependencySpec {
                issue_id: previous.into(),
                resolution: DependencyResolution::Job {
                    prerequisite_issue_id: previous.into(),
                    required_fingerprint: fingerprints[previous].clone(),
                },
            });
        }
        jobs.push(SubmitJob {
            idempotency_key: format!("selected-{id}"),
            issue_id: id.into(),
            issue_type: "task".into(),
            parent_issue_ids: vec!["FAB-L010".into()],
            source_fingerprint: fingerprint,
            issue_snapshot: raw,
            dependencies,
            plan: JobPlan {
                tasks: vec![
                    task("implement", TaskKind::Implementation, None),
                    task("review", TaskKind::Reviewer, Some("implement")),
                ],
                limits: ResourceLimits {
                    max_attempts: 1,
                    max_tokens: Some(400),
                    max_cost_micros: Some(0),
                    max_seconds: Some(60),
                    max_concurrency: 1,
                },
            },
        });
        verification.push(VerificationPolicy {
            issue_id: id.into(),
            checks: vec![CheckSpec {
                id: "fixture-check".into(),
                program: PathBuf::from("fixture-check"),
                args: vec![],
                cwd: PathBuf::from("."),
                timeout_ms: 1000,
                output_limit_bytes: 1024,
                adapter: None,
                contract: None,
            }],
            reviewer: ReviewerIdentity {
                task_id: "review".into(),
                provider_id: "fixture-codex".into(),
                model_id: "fixture-model".into(),
                reasoning_effort: None,
            },
            acceptance_criteria: acceptance,
        });
    }

    let mut epic_raw = "---\nid: FAB-L010\ntitle: Synthetic parent epic\n---\n".to_owned();
    let epic_path = issue_dir.join("FAB-L010.md");
    fs::write(&epic_path, &epic_raw).unwrap();
    issue_sources.insert("FAB-L010".into(), epic_path.canonicalize().unwrap());
    let epic_fingerprint = issues::revision(&epic_raw);
    jobs.push(SubmitJob {
        idempotency_key: "selected-FAB-L010".into(),
        issue_id: "FAB-L010".into(),
        issue_type: "epic".into(),
        parent_issue_ids: vec![],
        source_fingerprint: epic_fingerprint,
        issue_snapshot: std::mem::take(&mut epic_raw),
        dependencies: vec![],
        plan: JobPlan {
            tasks: vec![PlannedTask {
                task_id: "aggregate".into(),
                parent_task_id: None,
                kind: TaskKind::EpicAggregation,
                files: vec![],
                acceptance: vec!["all selected children verified".into()],
                limits: limits.clone(),
                provider_id: None,
                model_id: None,
                route_requirements: None,
            }],
            limits: limits.clone(),
        },
    });

    let observation_app_server = temp.path().join("fake-observation-app-server.ps1");
    common::write_observation_app_server(&observation_app_server, "fixture-model");
    let mut settings = Settings::default();
    settings.providers = vec![ProviderProfile {
        id: "fixture-codex".into(),
        kind: ProviderKind::Codex,
        name: "Synthetic Codex fixture".into(),
        executable: observation_app_server.to_string_lossy().into_owned(),
        endpoint: None,
        secret_ref: None,
        models: vec![ModelProfile {
            id: "fixture-model".into(),
            name: "Synthetic model".into(),
            reasoning_efforts: vec![],
            context_window_tokens: Some(16_384),
            supports_tools: Some(true),
            quality_rating: None,
        }],
    }];
    let settings_path = temp.path().join("settings.json");
    fs::write(&settings_path, serde_json::to_vec(&settings).unwrap()).unwrap();
    let request = BatchRequest {
        batch_id: "synthetic-gp-chain".into(),
        project_path: project.canonicalize().unwrap(),
        managed_worktree_root: temp.path().join("worktrees"),
        build_root: temp.path().join("builds"),
        settings_path,
        settings_fingerprint: None,
        verified_base_commit: "synthetic-base".into(),
        jobs,
        verification,
        issue_sources,
        paid_policies: BTreeMap::new(),
        resolved_routes: Default::default(),
        provider_observation_fingerprint: None,
        dispatch_refresh_consent:
            navis_fabrica_runner_core::provider_observations::ModelFreeRefreshConsent::Disabled,
        route_commitments: Default::default(),
        policy: BatchPolicy {
            window_seconds: 60,
            max_concurrency: 2,
            max_auth_retries: 0,
            max_network_retries: 0,
            budget: BudgetPolicy {
                max_tokens: 10_000,
                max_cost_micros: 0,
                max_seconds: 60,
                max_concurrency: 2,
            },
            permitted_effects: vec![
                PermittedEffect::ProviderExecution,
                PermittedEffect::WorkspaceWrites,
                PermittedEffect::LocalChecks,
            ],
        },
    };

    let supervisor = Supervisor::open(temp.path().join("runner-state")).unwrap();
    common::refresh_provider_observations(
        supervisor.paths().root.as_path(),
        &request.settings_path,
    );
    let worker = Path::new(env!("CARGO_BIN_EXE_fabrica-worker"));
    let preview =
        preview_batch_request(supervisor.paths().root.as_path(), &request, worker).unwrap();
    assert_eq!(preview.plan.issues.len(), 6);
    assert_eq!(
        preview.plan.issues[0].eligibility,
        navis_fabrica_runner_core::supervisor::PreviewEligibility::Eligible
    );
    assert_eq!(
        preview.plan.issues[1].eligibility,
        navis_fabrica_runner_core::supervisor::PreviewEligibility::WaitingForDependency
    );
    assert!(preview
        .request
        .resolved_routes
        .contains_key("GP-109/implement"));
    let started = supervisor.start(&preview.request, 1).unwrap();
    let store = JobsStore::open(supervisor.paths().jobs.clone()).unwrap();
    let by_issue = |issue: &str| {
        started
            .job_ids
            .iter()
            .copied()
            .find(|id| store.get(*id).unwrap().issue_id == issue)
            .unwrap()
    };
    let gp109 = by_issue("GP-109");
    let gp110 = by_issue("GP-110");
    let gp111 = by_issue("GP-111");
    let gp112 = by_issue("GP-112");
    let independent = by_issue("GP-199");
    let epic = by_issue("FAB-L010");
    assert_eq!(store.get(epic).unwrap().issue_id, "FAB-L010");
    assert_eq!(store.get(gp112).unwrap().state, JobState::Blocked);
    assert!(store
        .claim_ids("fixture", &[gp110, gp111, gp112], 2, 1000)
        .unwrap()
        .is_none());
    assert_eq!(
        store
            .claim_ids("fixture", &[gp109], 3, 1000)
            .unwrap()
            .unwrap()
            .job
            .id,
        gp109
    );
    assert_eq!(
        store
            .claim_ids("independent", &[independent], 3, 1000)
            .unwrap()
            .unwrap()
            .job
            .id,
        independent
    );
    assert!(store
        .claim_ids("fixture", &[gp110, gp111, gp112], 4, 1000)
        .unwrap()
        .is_none());
}
