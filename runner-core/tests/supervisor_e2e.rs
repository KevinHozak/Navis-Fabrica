//! Real worker lifecycle with a fake local app-server; never contacts a provider.
#![cfg(windows)]
mod common;
use navis_fabrica_runner_core::{
    budgets::BudgetPolicy,
    execute,
    jobs::{JobPlan, JobState, JobsStore, PlannedTask, ResourceLimits, SubmitJob, TaskKind},
    providers::{ModelProfile, ProviderKind, ProviderProfile},
    settings::Settings,
    supervisor::{
        preview_batch_request, BatchPolicy, BatchRequest, BatchState, PermittedEffect, Supervisor,
        VerificationPolicy,
    },
    supervisor_protocol::SupervisorOperationResult,
    verification::{CheckSpec, ReviewerIdentity},
    CoreRequest, Operation, PROTOCOL_VERSION,
};
use std::{
    fs,
    path::Path,
    process::Command,
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

fn git(root: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}
fn limits(tokens: u64, seconds: u64) -> ResourceLimits {
    ResourceLimits {
        max_attempts: 1,
        max_tokens: Some(tokens),
        max_cost_micros: Some(0),
        max_seconds: Some(seconds),
        max_concurrency: 1,
    }
}
fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64
}

struct ReleaseBarrier(std::path::PathBuf);
impl Drop for ReleaseBarrier {
    fn drop(&mut self) {
        let _ = fs::write(&self.0, "release");
    }
}
fn status_output(runner: &Path, data_root: &Path, batch_id: &str) -> std::process::Output {
    let mut child = Command::new(runner)
        .args(["status", "--data-root"])
        .arg(data_root)
        .args(["--batch-id", batch_id])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdout = child.stdout.take().unwrap();
    let stdout_reader = thread::spawn(move || {
        let mut bytes = Vec::new();
        std::io::Read::read_to_end(&mut stdout, &mut bytes).unwrap();
        bytes
    });
    let mut stderr = child.stderr.take().unwrap();
    let stderr_reader = thread::spawn(move || {
        let mut bytes = Vec::new();
        std::io::Read::read_to_end(&mut stderr, &mut bytes).unwrap();
        bytes
    });
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            return std::process::Output {
                status,
                stdout: stdout_reader.join().expect("status stdout reader panicked"),
                stderr: stderr_reader.join().expect("status stderr reader panicked"),
            };
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            let stderr = stderr_reader.join().expect("status stderr reader panicked");
            let _ = stdout_reader.join().expect("status stdout reader panicked");
            panic!(
                "status CLI timed out while worker was streaming: {}",
                String::from_utf8_lossy(&stderr)
            );
        }
        thread::sleep(Duration::from_millis(10));
    }
}
fn latest_delta_sequence(checkpoints: &[serde_json::Value]) -> Option<u64> {
    checkpoints
        .iter()
        .filter_map(|checkpoint| {
            let value = checkpoint.to_string();
            value
                .match_indices("DELTA-")
                .filter_map(|(index, _)| {
                    let digits: String = value[index + 6..]
                        .chars()
                        .take_while(|ch| ch.is_ascii_digit())
                        .collect();
                    digits.parse().ok()
                })
                .max()
        })
        .max()
}

#[test]
fn worker_e2e_verifies_detached_when_supported_and_hosted_when_required() {
    let temp = tempfile::tempdir().unwrap();
    let repo = temp.path().join("synthetic project with spaces");
    fs::create_dir(&repo).unwrap();
    git(&repo, &["init", "-q"]);
    git(&repo, &["config", "user.name", "Fixture"]);
    git(&repo, &["config", "user.email", "fixture@example.invalid"]);
    fs::write(repo.join("README.md"), "Synthetic worker fixture\n").unwrap();
    fs::create_dir(repo.join("issues")).unwrap();
    let issue_path = repo.join("issues/TEST-L001.md");
    let issue_raw = "---\nschema_version: 1\nid: \"TEST-L001\"\ntitle: \"Synthetic task\"\ntype: \"task\"\nstatus: \"ready\"\nparent: []\ndepends_on: []\n---\n\n## Acceptance criteria\n- [ ] Produce result.txt.\n";
    fs::write(&issue_path, issue_raw).unwrap();
    git(&repo, &["add", "README.md", "issues/TEST-L001.md"]);
    git(&repo, &["commit", "-q", "-m", "fixture baseline"]);
    let head = git(&repo, &["rev-parse", "HEAD"]);
    // An unrelated dirty primary file must survive untouched.
    fs::write(repo.join("README.md"), "User's unrelated local change\n").unwrap();
    let harness = temp.path().join("fake app server.ps1");
    fs::write(&harness, r#"$ErrorActionPreference='Stop'
$review=$false
while ($line=[Console]::In.ReadLine()) {
  $message=$line|ConvertFrom-Json
  switch ($message.method) {
    'initialize' { [Console]::Out.WriteLine((@{id=$message.id;result=@{}}|ConvertTo-Json -Compress -Depth 10)) }
    'account/read' { [Console]::Out.WriteLine('{"id":2,"result":{"account":{"type":"chatgpt","requiresOpenaiAuth":true}}}') }
    'model/list' { [Console]::Out.WriteLine('{"id":3,"result":{"data":[{"id":"fixture-model","contextWindow":32768,"supportsTools":true}]}}') }
    'thread/start' {
      $review=($message.params.sandbox -eq 'read-only')
      [Console]::Out.WriteLine((@{id=$message.id;result=@{thread=@{id='synthetic-thread'}}}|ConvertTo-Json -Compress -Depth 10))
    }
    'turn/start' {
      if ($review) {
        $prompt=$message.params.input[0].text
        $parts=$prompt -split 'REVIEW_CONTEXT_JSON:',2
        $context=$parts[1]|ConvertFrom-Json
        if (-not [IO.File]::Exists((Join-Path (Get-Location) 'result.txt'))) { throw 'implementation missing' }
        $reply=@{sourceFingerprint=$context.source.fingerprint;passed=$true;findings=@();manualAcceptance=@();details='Synthetic independent review observed result.txt.'}|ConvertTo-Json -Compress -Depth 10
      } else {
        [IO.File]::WriteAllText((Join-Path (Get-Location) 'result.txt'),'verified fixture result')
        $reply='Synthetic implementation complete.'
      }
      if (-not $review) {
        [IO.File]::WriteAllText('__BARRIER__-ready','ready')
        $deadline=[DateTime]::UtcNow.AddSeconds(30)
        for ($i=1; $i -le 10000; $i++) {
          if ([IO.File]::Exists('__BARRIER__-release')) { break }
          if ([DateTime]::UtcNow -gt $deadline) { throw 'status polling barrier timed out' }
          $delta='DELTA-{0:D6};' -f $i
          [Console]::Out.WriteLine((@{method='item/agentMessage/delta';params=@{delta=$delta}}|ConvertTo-Json -Compress -Depth 10))
          Start-Sleep -Milliseconds 10
        }
        if ($i -lt 130) { throw 'status poll released stream before 129 distinct deltas' }
      } else {
        [Console]::Out.WriteLine((@{method='item/agentMessage/delta';params=@{delta=$reply}}|ConvertTo-Json -Compress -Depth 10))
      }
      [Console]::Out.WriteLine('{"method":"thread/tokenUsage/updated","params":{"tokenUsage":{"last":{"totalTokens":17}}}}')
      [Console]::Out.WriteLine('{"method":"turn/completed","params":{"turn":{"status":"completed"}}}')
    }
  }
}
"#.replace("__BARRIER__", &temp.path().join("polling-barrier").to_string_lossy()).as_bytes()).unwrap();
    let mut settings = Settings::default();
    settings.providers = vec![ProviderProfile {
        id: "fixture-codex".into(),
        kind: ProviderKind::Codex,
        name: "Synthetic provider".into(),
        executable: harness.to_string_lossy().into_owned(),
        endpoint: None,
        secret_ref: None,
        models: vec![ModelProfile {
            id: "fixture-model".into(),
            name: "Synthetic model".into(),
            reasoning_efforts: vec![],
            context_window_tokens: Some(32768),
            supports_tools: Some(true),
            quality_rating: None,
        }],
    }];
    let settings_path = temp.path().join("settings.json");
    fs::write(&settings_path, serde_json::to_vec(&settings).unwrap()).unwrap();
    let acceptance = vec!["result.txt contains the synthetic expected result".into()];
    let task = |id: &str, kind, parent| PlannedTask {
        task_id: id.into(),
        parent_task_id: parent,
        kind,
        files: vec!["result.txt".into()],
        acceptance: acceptance.clone(),
        limits: limits(1000, 15),
        provider_id: Some("fixture-codex".into()),
        model_id: Some("fixture-model".into()),
        route_requirements: None,
    };
    let powershell = std::path::PathBuf::from(std::env::var_os("SystemRoot").unwrap())
        .join("System32/WindowsPowerShell/v1.0/powershell.exe");
    let request = BatchRequest {
        batch_id: "worker-mode-fixture".into(),
        settings_fingerprint: None,
        paid_policies: std::collections::BTreeMap::new(),
        resolved_routes: Default::default(),
        provider_observation_fingerprint: None,
        dispatch_refresh_consent: navis_fabrica_runner_core::provider_observations::ModelFreeRefreshConsent::Disabled,
        route_commitments: Default::default(),
        issue_sources: std::collections::BTreeMap::from([("TEST-L001".into(), issue_path)]), project_path: repo.clone(),
        managed_worktree_root: temp.path().join("managed"), build_root: temp.path().join("builds"), settings_path,
        verified_base_commit: head.clone(),
        jobs: vec![SubmitJob {
            idempotency_key: "detached-fixture-TEST-L001".into(), issue_id: "TEST-L001".into(), issue_type: "task".into(), parent_issue_ids: vec![],
            source_fingerprint: navis_fabrica_runner_core::issues::revision(issue_raw), issue_snapshot: issue_raw.into(), dependencies: vec![],
            plan: JobPlan { tasks: vec![task("implement", TaskKind::Implementation, None), task("review", TaskKind::Reviewer, Some("implement".into()))], limits: limits(2000, 40) },
        }],
        verification: vec![VerificationPolicy { issue_id: "TEST-L001".into(), acceptance_criteria: acceptance,
            reviewer: ReviewerIdentity { task_id: "review".into(), provider_id: "fixture-codex".into(), model_id: "fixture-model".into(), reasoning_effort: None },
            checks: vec![CheckSpec { id: "expected-output".into(), program: powershell, args: vec!["-NoProfile".into(), "-NonInteractive".into(), "-Command".into(), "if ([IO.File]::ReadAllText((Join-Path (Get-Location) 'result.txt')) -ne 'verified fixture result') { exit 1 }".into()], cwd: ".".into(), timeout_ms: 5000, output_limit_bytes: 4096, adapter: None }],
        }],
        policy: BatchPolicy { window_seconds: 120, max_concurrency: 1, max_auth_retries: 0, max_network_retries: 0,
            budget: BudgetPolicy { max_tokens: 10000, max_cost_micros: 0, max_seconds: 120, max_concurrency: 1 },
            permitted_effects: vec![PermittedEffect::ProviderExecution, PermittedEffect::WorkspaceWrites, PermittedEffect::LocalChecks],
        },
    };
    let data_root = temp.path().join("durable state");
    // Preview is storage-free, including construction of any databases.
    Supervisor::dry_run_request(&request).unwrap();
    assert!(!data_root.exists());
    let worker = Path::new(env!("CARGO_BIN_EXE_fabrica-worker"));
    common::refresh_provider_observations(&data_root, &request.settings_path);
    let preview = preview_batch_request(&data_root, &request, worker).unwrap();
    // Submit and hosted fallback must use the exact normalized request bound by
    // preview, including its observation fingerprint and resolved routes.
    let request = preview.request.clone();
    let submit = || {
        execute(CoreRequest {
            protocol_version: PROTOCOL_VERSION,
            operation: Operation::Submit,
            project_path: None,
            config_path: None,
            settings_path: None,
            allow_live_probe: false,
            issue_id: None,
            offset: None,
            limit: None,
            expected_revision: None,
            record: None,
            runtime_request: None,
            data_root: Some(data_root.clone()),
            batch_request: Some(request.clone()),
            batch_request_path: None,
            batch_id: None,
            cursor: None,
            worker_executable: Some(worker.to_path_buf()),
            preview_fingerprint: Some(preview.fingerprint.clone()),
            recovery: None,
        })
    };
    let first = submit();
    let detached_supported = if first.ok {
        assert!(matches!(
            first
                .result
                .and_then(|value| serde_json::from_value::<SupervisorOperationResult>(value).ok()),
            Some(SupervisorOperationResult::Submitted(_))
        ));
        // A second submit must reconnect to the same durable job and worker.
        let second = submit();
        assert!(second.ok, "detached reconnect failed: {:?}", second.error);
        true
    } else {
        let error = first
            .error
            .expect("failed submit must include a typed error");
        assert_eq!(
            error.code, "WORKER_LAUNCH_FAILED",
            "unexpected submit failure: {error:?}"
        );
        assert!(
            error.message.contains("foreground-host mode"),
            "only an explicit detached breakaway refusal may use the hosted fallback: {error:?}"
        );
        let supervisor = Supervisor::open(&data_root).unwrap();
        let refused = supervisor.status_view(&request.batch_id).unwrap();
        assert_eq!(refused.jobs.len(), 1);
        assert_eq!(refused.jobs[0].state, JobState::Queued);
        assert_eq!(refused.jobs[0].attempt_count, 0);
        assert_eq!(refused.budget.reservations, 0);
        assert_eq!(refused.budget.reserved, 0);
        assert_eq!(refused.budget.dispatched, 0);
        assert!(!repo.join("result.txt").exists());
        assert!(
            refused.snapshot.state == BatchState::Starting
                || refused.snapshot.state == BatchState::Running
        );
        false
    };
    let foreground = if detached_supported {
        None
    } else {
        let foreground_request = request.clone();
        let foreground_worker = worker.to_path_buf();
        let foreground_root = data_root.clone();
        Some(thread::spawn(move || {
            Supervisor::open(&foreground_root)
                .unwrap()
                .run_batch_foreground(&foreground_request, &foreground_worker, now())
        }))
    };
    {
        let ready = temp.path().join("polling-barrier-ready");
        let release = temp.path().join("polling-barrier-release");
        let _release_on_panic = ReleaseBarrier(release.clone());
        let ready_deadline = Instant::now() + Duration::from_secs(20);
        while !ready.exists() {
            assert!(
                Instant::now() < ready_deadline,
                "fake app-server never reached polling barrier"
            );
            thread::sleep(Duration::from_millis(20));
        }
        let runner = Path::new(env!("CARGO_BIN_EXE_fabrica-runner"));
        let mut observed_checkpoint_counts = Vec::new();
        let mut successful_polls = 0;
        let mut advancing_snapshots = 0;
        let mut previous_sequence = None;
        let polling_deadline = Instant::now() + Duration::from_secs(20);
        while successful_polls < 5
            || advancing_snapshots < 2
            || previous_sequence.unwrap_or(0) < 129
        {
            assert!(Instant::now() < polling_deadline, "status polling did not observe 5 reads and 2 advancing stream checkpoints; polls={successful_polls}, advances={advancing_snapshots}");
            let output = status_output(runner, &data_root, &request.batch_id);
            assert!(
                output.status.success(),
                "status CLI failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            let response: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
            assert_eq!(
                response["ok"], true,
                "status CLI returned error: {response}"
            );
            let job = &response["result"]["value"]["jobs"][0];
            assert_eq!(
                job["state"], "running",
                "worker should remain held at barrier: {response}"
            );
            let checkpoints = job["checkpoints"]
                .as_array()
                .expect("status checkpoints array");
            observed_checkpoint_counts.push(checkpoints.len());
            if let Some(sequence) = latest_delta_sequence(checkpoints) {
                if previous_sequence.is_some_and(|previous| sequence > previous) {
                    advancing_snapshots += 1;
                }
                previous_sequence = Some(sequence);
            }
            successful_polls += 1;
        }
        assert!(previous_sequence.unwrap_or(0) >= 129, "status exposed fewer than 129 distinct streamed deltas: {previous_sequence:?}; counts={observed_checkpoint_counts:?}");
        fs::write(release, "release").unwrap();
    }
    let reconnected = Supervisor::open(&data_root).unwrap();
    let job_id = reconnected.status_view(&request.batch_id).unwrap().jobs[0].id;
    if let Some(foreground) = foreground {
        let foreground_result = foreground.join().expect("foreground host thread panicked");
        let foreground = match foreground_result {
            Ok(snapshot) => snapshot,
            Err(error) => {
                let status = reconnected
                    .status_view(&request.batch_id)
                    .map(|status| format!("{status:?}"))
                    .unwrap_or_else(|diagnostic| format!("status read failed: {diagnostic}"));
                let events = reconnected
                    .events(&request.batch_id, 0, 100)
                    .map(|page| format!("{:?}", page.events))
                    .unwrap_or_else(|diagnostic| format!("event read failed: {diagnostic}"));
                let preserved = temp.keep();
                panic!(
                    "explicit hosted run failed after detached refusal: {error}; status={status}; events={events}; fixture preserved at {}. Worker process exit codes are unavailable because the launcher discards the child handle.",
                    preserved.display()
                );
            }
        };
        assert_eq!(foreground.state, BatchState::Completed);
    }
    let deadline = Instant::now() + Duration::from_secs(45);
    loop {
        let job = JobsStore::open(reconnected.paths().jobs.clone())
            .unwrap()
            .get(job_id)
            .unwrap();
        if matches!(
            job.state,
            JobState::Verified
                | JobState::Failed
                | JobState::NeedsInput
                | JobState::ReconciliationRequired
        ) {
            assert_eq!(
                job.state,
                JobState::Verified,
                "worker failed: {:?}; summary: {:?}",
                job,
                reconnected.morning_summary(&request.batch_id)
            );
            assert_eq!(job.attempt_count, 1);
            break;
        }
        assert!(Instant::now() < deadline, "worker timed out: {job:?}");
        thread::sleep(Duration::from_millis(100));
    }
    let summary = reconnected.morning_summary(&request.batch_id).unwrap();
    assert_eq!(summary.completed, vec!["TEST-L001"]);
    assert_eq!(
        fs::read_to_string(repo.join("README.md")).unwrap(),
        "User's unrelated local change\n"
    );
    let durable_status = reconnected.status_view(&request.batch_id).unwrap();
    let durable_checkpoints = durable_status.jobs[0]
        .checkpoints
        .iter()
        .map(|checkpoint| serde_json::to_value(checkpoint).unwrap())
        .collect::<Vec<_>>();
    assert!(
        latest_delta_sequence(&durable_checkpoints).unwrap_or(0) >= 129,
        "terminal job did not retain at least 129 distinct output deltas: {:?}",
        durable_checkpoints
    );
    assert!(!repo.join("result.txt").exists());
    assert_eq!(git(&repo, &["rev-parse", "HEAD"]), head);
    // Let the worker host observe terminal state before TempDir cleanup.
    let deadline = Instant::now() + Duration::from_secs(10);
    while reconnected.status(&request.batch_id).unwrap().state != BatchState::Completed {
        assert!(Instant::now() < deadline);
        thread::sleep(Duration::from_millis(100));
    }
}
