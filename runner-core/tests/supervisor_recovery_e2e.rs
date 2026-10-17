//! Real worker crash/recovery coverage with a local fake app-server only.
#![cfg(windows)]
mod common;

use navis_fabrica_runner_core::{
    budgets::{BudgetPolicy, BudgetStore, ReservationState},
    jobs::{JobPlan, JobState, JobsStore, PlannedTask, ResourceLimits, SubmitJob, TaskKind},
    process::ProcessIdentity,
    providers::{ModelProfile, ProviderKind, ProviderProfile},
    settings::Settings,
    supervisor::{
        BatchPolicy, BatchRequest, PermittedEffect, RecoveryAction, RecoveryRequest, Supervisor,
        VerificationPolicy,
    },
    verification::{CheckSpec, ReviewerIdentity},
};
use rusqlite::Connection;
use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

fn git(root: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap().trim().to_owned()
}
fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64
}
fn identity(pid: u32, creation: i64) -> ProcessIdentity {
    ProcessIdentity {
        pid,
        creation_time_100ns: Some(creation as u64),
    }
}

struct RecordedProcessCleanup(Vec<ProcessIdentity>);
impl RecordedProcessCleanup {
    fn record(&mut self, process: ProcessIdentity) {
        self.0.push(process);
    }
}
impl Drop for RecordedProcessCleanup {
    fn drop(&mut self) {
        for process in self.0.drain(..) {
            match navis_fabrica_runner_core::process::process_identity_matches(process) {
                Ok(true) => {
                    if let Err(error) = try_terminate_exact(process) {
                        eprintln!(
                            "fixture cleanup failed for exact PID/birth {:?}: {error}",
                            process
                        );
                    }
                }
                Ok(false) => {}
                Err(error) => eprintln!(
                    "fixture liveness check failed for exact PID/birth {:?}: {error}",
                    process
                ),
            }
        }
    }
}

/// Terminate a process only after pinning its handle and matching its creation time.
fn terminate_exact(expected: ProcessIdentity) {
    try_terminate_exact(expected)
        .unwrap_or_else(|error| panic!("recorded PID {} cleanup failed: {error}", expected.pid));
}
fn try_terminate_exact(expected: ProcessIdentity) -> Result<(), String> {
    use windows_sys::Win32::{
        Foundation::{CloseHandle, FILETIME, WAIT_OBJECT_0},
        System::Threading::{
            GetProcessTimes, OpenProcess, TerminateProcess, WaitForSingleObject,
            PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_TERMINATE,
        },
    };
    const SYNCHRONIZE_PROCESS: u32 = 0x0010_0000;
    for _ in 0..20 {
        let h = unsafe {
            OpenProcess(
                PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_TERMINATE | SYNCHRONIZE_PROCESS,
                0,
                expected.pid,
            )
        };
        if h.is_null() {
            match navis_fabrica_runner_core::process::process_identity_matches(expected) {
                Ok(false) => return Ok(()),
                Ok(true) => thread::sleep(Duration::from_millis(25)),
                Err(error) => return Err(format!("recorded process liveness is unknown: {error}")),
            }
            continue;
        }
        unsafe {
            let (mut created, mut exited, mut kernel, mut user) = (
                FILETIME::default(),
                FILETIME::default(),
                FILETIME::default(),
                FILETIME::default(),
            );
            let ok = GetProcessTimes(h, &mut created, &mut exited, &mut kernel, &mut user);
            let actual =
                (u64::from(created.dwHighDateTime) << 32) | u64::from(created.dwLowDateTime);
            if ok == 0 {
                if WaitForSingleObject(h, 0) == WAIT_OBJECT_0 {
                    CloseHandle(h);
                    return Ok(());
                }
                CloseHandle(h);
                return Err("could not verify recorded process creation time".into());
            }
            if Some(actual) != expected.creation_time_100ns {
                CloseHandle(h);
                return Err("PID creation time changed; refusing to terminate".into());
            }
            if WaitForSingleObject(h, 0) != WAIT_OBJECT_0 {
                let _ = TerminateProcess(h, 71);
                if WaitForSingleObject(h, 10_000) != WAIT_OBJECT_0 {
                    CloseHandle(h);
                    return Err("recorded process did not exit before cleanup deadline".into());
                }
            }
            CloseHandle(h);
            return Ok(());
        }
    }
    Err("recorded process remained live but could not be opened for exact cleanup".into())
}
fn wait_job(
    jobs: &JobsStore,
    id: i64,
    until: Instant,
    predicate: impl Fn(&JobState) -> bool,
) -> navis_fabrica_runner_core::jobs::Job {
    loop {
        let job = jobs.get(id).unwrap();
        if predicate(&job.state) {
            return job;
        }
        assert!(
            Instant::now() < until,
            "timed out waiting for job {id}: {job:?}"
        );
        thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn crashed_worker_reconnect_preserves_ambiguous_job_and_recovers_only_after_provider_stops() {
    let temp = tempfile::tempdir().unwrap();
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        run_recovery_fixture(&temp, None)
    }));
    if let Err(panic) = result {
        let path = temp.keep();
        eprintln!(
            "preserved recovery fixture for diagnosis at {}",
            path.display()
        );
        std::panic::resume_unwind(panic);
    }
}

fn run_recovery_fixture(temp: &tempfile::TempDir, stage_handoff: Option<&Path>) {
    let repo = temp.path().join("recovery fixture");
    fs::create_dir_all(repo.join("issues")).unwrap();
    git(&repo, &["init", "-q"]);
    git(&repo, &["config", "user.name", "Fixture"]);
    git(&repo, &["config", "user.email", "fixture@example.invalid"]);
    fs::write(repo.join("README.md"), "fixture baseline\n").unwrap();
    let mut issue_sources = std::collections::BTreeMap::new();
    let mut submit_jobs = Vec::new();
    let mut verification = Vec::new();
    for (id, title) in [
        ("TEST-L001", "ambiguous crash"),
        ("TEST-L002", "gated independent work"),
        ("TEST-L003", "queued independent work"),
    ] {
        let path = repo.join(format!("issues/{id}.md"));
        let raw = format!("---\nschema_version: 1\nid: \"{id}\"\ntitle: \"{title}\"\ntype: \"task\"\nstatus: \"ready\"\nparent: []\ndepends_on: []\n---\n\n## Acceptance criteria\n- [ ] Produce result.txt.\n");
        fs::write(&path, &raw).unwrap();
        issue_sources.insert(id.to_owned(), path);
        let acceptance = vec!["result.txt contains the synthetic expected result".to_owned()];
        let task = |task_id: &str, kind, parent| PlannedTask {
            task_id: task_id.into(),
            parent_task_id: parent,
            kind,
            files: vec!["result.txt".into()],
            acceptance: acceptance.clone(),
            limits: ResourceLimits {
                max_attempts: 2,
                max_tokens: Some(1000),
                max_cost_micros: Some(0),
                max_seconds: Some(120),
                max_concurrency: 1,
            },
            provider_id: Some("fixture-codex".into()),
            model_id: Some("fixture-model".into()),
            route_requirements: None,
        };
        submit_jobs.push(SubmitJob {
            idempotency_key: format!("recovery-{id}"),
            issue_id: id.into(),
            issue_type: "task".into(),
            parent_issue_ids: vec![],
            source_fingerprint: navis_fabrica_runner_core::issues::revision(&raw),
            issue_snapshot: raw,
            dependencies: vec![],
            plan: JobPlan {
                tasks: vec![
                    task("implement", TaskKind::Implementation, None),
                    task("review", TaskKind::Reviewer, Some("implement".into())),
                ],
                limits: ResourceLimits {
                    max_attempts: 2,
                    max_tokens: Some(10000),
                    max_cost_micros: Some(0),
                    max_seconds: Some(480),
                    max_concurrency: 1,
                },
            },
        });
        let powershell = Path::new(&std::env::var_os("SystemRoot").unwrap())
            .join("System32/WindowsPowerShell/v1.0/powershell.exe");
        verification.push(VerificationPolicy {
            issue_id: id.into(),
            acceptance_criteria: acceptance,
            reviewer: ReviewerIdentity {
                task_id: "review".into(),
                provider_id: "fixture-codex".into(),
                model_id: "fixture-model".into(),
                reasoning_effort: None,
            },
            checks: vec![CheckSpec {
                id: "expected-result".into(),
                program: powershell,
                args: vec![
                    "-NoProfile".into(),
                    "-NonInteractive".into(),
                    "-Command".into(),
                    "if (!(Test-Path result.txt)) { exit 1 }".into(),
                ],
                cwd: ".".into(),
                timeout_ms: 5000,
                output_limit_bytes: 4096,
                adapter: None,
            }],
        });
    }
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-q", "-m", "synthetic baseline"]);
    let head = git(&repo, &["rev-parse", "HEAD"]);
    let temp_marker = temp.path().join("first-implementation.marker");
    let release_marker = temp.path().join("release-independent.marker");
    let invocation_dir = temp.path().join("implementation-invocations");
    fs::create_dir(&invocation_dir).unwrap();
    let invocation_literal = serde_json::to_string(&invocation_dir.to_string_lossy()).unwrap();
    let harness = temp.path().join("fake-app-server.ps1");
    let marker_literal = serde_json::to_string(&temp_marker.to_string_lossy()).unwrap();
    let release_literal = serde_json::to_string(&release_marker.to_string_lossy()).unwrap();
    fs::write(&harness, format!(r#"$ErrorActionPreference='Stop'
$marker={marker_literal}
$release={release_literal}
$callsRoot={invocation_literal}
$review=$false
while ($line=[Console]::In.ReadLine()) {{
  $m=$line|ConvertFrom-Json
  switch ($m.method) {{
    'initialize' {{ [Console]::Out.WriteLine((@{{id=$m.id;result=@{{}}}}|ConvertTo-Json -Compress)) }}
    'account/read' {{ [Console]::Out.WriteLine('{{"id":2,"result":{{"account":{{"type":"chatgpt","requiresOpenaiAuth":true}}}}}}') }}
    'model/list' {{ [Console]::Out.WriteLine('{{"id":3,"result":{{"data":[{{"id":"fixture-model","contextWindow":32768,"supportsTools":true}}]}}}}') }}
    'thread/start' {{ $review=($m.params.sandbox -eq 'read-only'); [Console]::Out.WriteLine((@{{id=$m.id;result=@{{thread=@{{id='synthetic'}}}}}}|ConvertTo-Json -Compress -Depth 8)) }}
    'turn/start' {{
      if ($review) {{ $p=$m.params.input[0].text; $c=($p -split 'REVIEW_CONTEXT_JSON:',2)[1]|ConvertFrom-Json; $reply=@{{sourceFingerprint=$c.source.fingerprint;passed=$true;findings=@();manualAcceptance=@();details='synthetic review'}}|ConvertTo-Json -Compress -Depth 8 }}
      else {{
        $prompt=$m.params.input[0].text
        if ($prompt -match 'TEST-L001') {{ Add-Content -Path (Join-Path $callsRoot 'TEST-L001.calls') -Value 'started' }} elseif ($prompt -match 'TEST-L002') {{ Add-Content -Path (Join-Path $callsRoot 'TEST-L002.calls') -Value 'started' }} elseif ($prompt -match 'TEST-L003') {{ Add-Content -Path (Join-Path $callsRoot 'TEST-L003.calls') -Value 'started' }}
        if ($prompt -match 'TEST-L001' -and !(Test-Path $marker)) {{ Set-Content -Path $marker -Value 'started'; Start-Sleep -Seconds 600 }}
        if ($prompt -match 'TEST-L002') {{ while (!(Test-Path $release)) {{ Start-Sleep -Milliseconds 50 }} }}
        Set-Content -Path result.txt -Value 'verified fixture result'; $reply='synthetic implementation'
      }}
      [Console]::Out.WriteLine((@{{method='item/agentMessage/delta';params=@{{delta=$reply}}}}|ConvertTo-Json -Compress -Depth 8)); [Console]::Out.WriteLine('{{"method":"turn/completed","params":{{"turn":{{"status":"completed"}}}}}}')
    }}
  }}
}}
"#)).unwrap();
    let mut settings = Settings::default();
    settings.providers = vec![ProviderProfile {
        id: "fixture-codex".into(),
        kind: ProviderKind::Codex,
        name: "fake".into(),
        executable: harness.to_string_lossy().into_owned(),
        endpoint: None,
        secret_ref: None,
        models: vec![ModelProfile {
            id: "fixture-model".into(),
            name: "fake".into(),
            reasoning_efforts: vec![],
            context_window_tokens: Some(32768),
            supports_tools: Some(true),
            quality_rating: None,
        }],
    }];
    let settings_path = temp.path().join("settings.json");
    fs::write(&settings_path, serde_json::to_vec(&settings).unwrap()).unwrap();
    let request = BatchRequest {
        batch_id: "worker-crash-recovery".into(),
        settings_fingerprint: None,
        paid_policies: Default::default(),
        resolved_routes: Default::default(),
        provider_observation_fingerprint: None,
        dispatch_refresh_consent:
            navis_fabrica_runner_core::provider_observations::ModelFreeRefreshConsent::Disabled,
        route_commitments: Default::default(),
        issue_sources,
        project_path: repo.clone(),
        managed_worktree_root: temp.path().join("managed"),
        build_root: temp.path().join("build"),
        settings_path,
        verified_base_commit: head.clone(),
        jobs: submit_jobs,
        verification,
        policy: BatchPolicy {
            window_seconds: 1200,
            max_concurrency: 2,
            max_auth_retries: 0,
            max_network_retries: 0,
            budget: BudgetPolicy {
                max_tokens: 30000,
                max_cost_micros: 0,
                max_seconds: 1200,
                max_concurrency: 2,
            },
            permitted_effects: vec![
                PermittedEffect::ProviderExecution,
                PermittedEffect::WorkspaceWrites,
                PermittedEffect::LocalChecks,
            ],
        },
    };
    let data = temp.path().join("durable");
    let supervisor = Supervisor::open(&data).unwrap();
    let worker = Path::new(env!("CARGO_BIN_EXE_fabrica-worker"));
    common::refresh_provider_observations(&data, &request.settings_path);
    let preview =
        navis_fabrica_runner_core::supervisor::preview_batch_request(&data, &request, worker)
            .unwrap();
    let mut cleanup = RecordedProcessCleanup(Vec::new());
    let started = supervisor
        .start_and_launch_hosted(&preview.request, worker, now())
        .unwrap();
    assert_eq!(started.job_ids.len(), 3);
    let jobs_store = JobsStore::open(supervisor.paths().jobs.clone()).unwrap();
    let conn = Connection::open(supervisor.paths().batches.clone()).unwrap();
    let find_job = |issue: &str| {
        started
            .job_ids
            .iter()
            .map(|id| jobs_store.get(*id).unwrap())
            .find(|job| job.issue_id == issue)
            .unwrap()
            .id
    };
    let (crash_id, gated_id, independent_id) = (
        find_job("TEST-L001"),
        find_job("TEST-L002"),
        find_job("TEST-L003"),
    );
    {
        let mut query = conn.prepare("SELECT pid,creation_time_100ns FROM batch_workers WHERE batch_id=?1 AND state='running'").unwrap();
        let processes = query
            .query_map([&request.batch_id], |row| {
                Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?))
            })
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        assert_eq!(
            processes.len(),
            2,
            "both hosted workers should be registered before the fixture can fail"
        );
        for (pid, created) in processes {
            cleanup.record(identity(pid as u32, created));
        }
    }
    let crash_job = wait_job(
        &jobs_store,
        crash_id,
        Instant::now() + Duration::from_secs(40),
        |s| *s == JobState::Running,
    );
    let checkpoint_deadline = Instant::now() + Duration::from_secs(30);
    let runtime_cp = loop {
        if let Some(value) = jobs_store
            .get_checkpoint(crash_id, "runtime_process_identity")
            .unwrap()
        {
            if value
                .get("pid")
                .and_then(serde_json::Value::as_u64)
                .is_some()
                && value
                    .get("creationTime100ns")
                    .and_then(serde_json::Value::as_u64)
                    .is_some()
            {
                break value;
            }
        }
        assert!(Instant::now() < checkpoint_deadline, "implementation process was not journaled; issue={} job={crash_job:?}; global={:?}; task_process={:?}; marker_exists={}; events={:?}; workers={:?}", crash_job.issue_id, jobs_store.get_checkpoint(crash_id, "runtime_process_identity").unwrap(), jobs_store.get_checkpoint(crash_id, "runtime_process_implement").unwrap(), temp_marker.exists(), supervisor.events(&request.batch_id, 0, 100).unwrap().events, conn.prepare("SELECT worker_id,pid,creation_time_100ns,state FROM batch_workers WHERE batch_id=?1").unwrap().query_map([&request.batch_id], |r| Ok((r.get::<_,String>(0)?,r.get::<_,Option<i64>>(1)?,r.get::<_,Option<i64>>(2)?,r.get::<_,String>(3)?))).unwrap().collect::<Result<Vec<_>,_>>().unwrap());
        thread::sleep(Duration::from_millis(50));
    };
    let provider =
        serde_json::from_value::<ProcessIdentity>(runtime_cp).expect("recorded provider identity");
    cleanup.record(provider);
    let effect_deadline = Instant::now() + Duration::from_secs(20);
    while !temp_marker.exists() {
        assert!(Instant::now() < effect_deadline, "synthetic provider effect did not begin before worker termination; provider may have received EOF; events={:?}", supervisor.events(&request.batch_id, 0, 100).unwrap().events);
        thread::sleep(Duration::from_millis(25));
    }
    assert!(
        navis_fabrica_runner_core::process::process_identity_matches(provider).unwrap(),
        "fake provider exited before the crash point"
    );
    wait_job(
        &jobs_store,
        gated_id,
        Instant::now() + Duration::from_secs(20),
        |s| *s == JobState::Running,
    );
    let gated_cp_deadline = Instant::now() + Duration::from_secs(20);
    let gated_cp = loop {
        if let Some(value) = jobs_store
            .get_checkpoint(gated_id, "runtime_process_identity")
            .unwrap()
        {
            if value
                .get("pid")
                .and_then(serde_json::Value::as_u64)
                .is_some()
                && value
                    .get("creationTime100ns")
                    .and_then(serde_json::Value::as_u64)
                    .is_some()
            {
                break value;
            }
        }
        assert!(
            Instant::now() < gated_cp_deadline,
            "gated independent provider did not start; task_process={:?}",
            jobs_store
                .get_checkpoint(gated_id, "runtime_process_implement")
                .unwrap()
        );
        thread::sleep(Duration::from_millis(50));
    };
    let gated_provider =
        serde_json::from_value::<ProcessIdentity>(gated_cp).expect("gated provider identity");
    cleanup.record(gated_provider);
    if let Some(handoff_path) = stage_handoff {
        assert_eq!(cleanup.0.len(), 4, "stage must record both hosted workers and both fake providers before waiting for effects");
        wait_for_initial_implementation_calls(
            &invocation_dir,
            &cleanup.0,
            Instant::now() + Duration::from_secs(20),
        )
        .unwrap_or_else(|error| {
            panic!("stage did not reach the bounded implementation-marker barrier: {error}")
        });
        // Recheck immediately before writing the handoff: both initial effects
        // are durable and the same four recorded processes must still be live.
        assert_initial_implementation_calls(&invocation_dir);
        for owned in cleanup.0.iter().copied() {
            assert!(
                navis_fabrica_runner_core::process::process_identity_matches(owned).unwrap(),
                "recorded stage process {} exited before handoff",
                owned.pid
            );
        }
        let crash_lease = jobs_store.get(crash_id).unwrap().lease_until_ms.unwrap();
        let gated_lease = jobs_store.get(gated_id).unwrap().lease_until_ms.unwrap();
        assert_eq!(jobs_store.get(crash_id).unwrap().attempt_count, 1);
        assert_eq!(jobs_store.get(gated_id).unwrap().attempt_count, 1);
        assert!(cleanup.0.iter().all(|p| p.creation_time_100ns.is_some()));
        let handoff = serde_json::json!({
            "fixtureRoot": temp.path(),
            "dataRoot": data,
            "batchId": request.batch_id,
            "crashJobId": crash_id,
            "gatedJobId": gated_id,
            "independentJobId": independent_id,
            "crashLeaseUntilMs": crash_lease,
            "gatedLeaseUntilMs": gated_lease,
            "ownedProcesses": cleanup.0,
            "invocationDir": invocation_dir,
            "releaseMarker": release_marker,
        });
        fs::write(handoff_path, serde_json::to_vec_pretty(&handoff).unwrap()).unwrap();
        // The child host exits normally while its exact fixture workers and fake
        // providers remain for the parent to stop by PID plus creation time.
        std::mem::forget(cleanup);
        return;
    }
    let (worker_pid, worker_creation): (u32, i64) = conn
        .query_row(
            "SELECT pid,creation_time_100ns FROM batch_workers WHERE batch_id=?1 AND worker_id=?2",
            rusqlite::params![
                request.batch_id,
                jobs_store
                    .recovery_attempt_info(crash_id)
                    .unwrap()
                    .worker_id
            ],
            |r| Ok((r.get::<_, i64>(0)? as u32, r.get(1)?)),
        )
        .unwrap();
    let crashed_worker = identity(worker_pid, worker_creation);
    terminate_exact(crashed_worker);
    assert!(
        navis_fabrica_runner_core::process::process_identity_matches(provider).unwrap(),
        "direct worker termination must leave fake provider alive"
    );
    let lease = jobs_store
        .get(crash_id)
        .unwrap()
        .lease_until_ms
        .expect("active crashed-attempt lease");
    jobs_store
        .reconcile_expired_ids(&[crash_id], lease + 1)
        .unwrap();
    assert_eq!(
        jobs_store.get(crash_id).unwrap().state,
        JobState::ReconciliationRequired
    );

    let reattached = Supervisor::open(&data).unwrap();
    reattached
        .resume_and_launch_hosted(&request.batch_id, worker, now())
        .unwrap();
    // Track every replacement worker identity for failure-path cleanup as well.
    let replacement_workers: Vec<(u32, i64)> = conn.prepare("SELECT pid,creation_time_100ns FROM batch_workers WHERE batch_id=?1 AND state='running'").unwrap().query_map([&request.batch_id], |r| Ok((r.get::<_,i64>(0)? as u32,r.get(1)?))).unwrap().collect::<Result<_,_>>().unwrap();
    for (pid, created) in replacement_workers {
        cleanup.record(identity(pid, created));
    }
    assert_eq!(
        jobs_store.get(crash_id).unwrap().attempt_count,
        1,
        "reattachment duplicated ambiguous job"
    );
    assert!(navis_fabrica_runner_core::process::process_identity_matches(provider).unwrap());
    fs::write(&release_marker, "continue").unwrap();
    // Workspace creation, source snapshots, implementation, and the final gate
    // all run under real Git/process boundaries; bound the observation within
    // the 1200-second batch window without relaxing any individual check cap.
    let verification_deadline = Instant::now() + Duration::from_secs(180);
    let independent = wait_job(&jobs_store, independent_id, verification_deadline, |s| {
        *s == JobState::Verified
    });
    assert_eq!(independent.attempt_count, 1);
    let gated = wait_job(&jobs_store, gated_id, verification_deadline, |s| {
        *s == JobState::Verified
    });
    assert_eq!(gated.state, JobState::Verified);
    assert_eq!(gated.attempt_count, 1);

    let denied = reattached.recover_job(
        &request.batch_id,
        RecoveryRequest {
            job_id: crash_id,
            decision: RecoveryAction::SafeToRetry,
            evidence: "synthetic provider intentionally left running".into(),
            process_confirmed_stopped: true,
            effects_reconciled: true,
        },
        lease + 2,
    );
    assert!(
        denied.is_err(),
        "must not retry while old provider process is live"
    );
    assert_eq!(
        jobs_store.get(crash_id).unwrap().state,
        JobState::ReconciliationRequired
    );

    terminate_exact(provider);
    assert!(!navis_fabrica_runner_core::process::process_identity_matches(provider).unwrap());
    reattached.recover_job(&request.batch_id, RecoveryRequest { job_id: crash_id, decision: RecoveryAction::SafeToRetry, evidence: "exact recorded synthetic provider PID and creation time terminated; no fixture effects persisted".into(), process_confirmed_stopped: true, effects_reconciled: true }, lease+3).unwrap();
    let orphan_hold = BudgetStore::open(supervisor.paths().budgets.clone())
        .unwrap()
        .get(&format!("worker-crash-recovery-{crash_id}-implement-1"))
        .unwrap();
    assert_eq!(
        orphan_hold.state,
        ReservationState::Unresolved,
        "recovery releases active concurrency but preserves unknown usage as an unresolved hold"
    );
    assert_eq!(
        orphan_hold.max_tokens, 1000,
        "the original token ceiling remains charged after explicit recovery"
    );
    assert_eq!(
        orphan_hold.settlement_source.as_deref(),
        Some("explicit-recovery-unknown")
    );
    reattached
        .resume_and_launch_hosted(&request.batch_id, worker, lease + 4)
        .unwrap();
    let retried = wait_job(
        &jobs_store,
        crash_id,
        Instant::now() + Duration::from_secs(45),
        |s| *s == JobState::Verified,
    );
    assert_eq!(retried.attempt_count, 2);
    assert_eq!(
        git(&repo, &["rev-parse", "HEAD"]),
        head,
        "the synthetic primary stays unchanged"
    );
    let complete_deadline = Instant::now() + Duration::from_secs(10);
    while reattached.status(&request.batch_id).unwrap().state
        != navis_fabrica_runner_core::supervisor::BatchState::Completed
    {
        assert!(
            Instant::now() < complete_deadline,
            "verified jobs did not produce terminal batch status"
        );
        thread::sleep(Duration::from_millis(50));
    }
}

#[derive(Debug, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct ColdRestartHandoff {
    fixture_root: PathBuf,
    data_root: PathBuf,
    batch_id: String,
    crash_job_id: i64,
    gated_job_id: i64,
    independent_job_id: i64,
    crash_lease_until_ms: i64,
    gated_lease_until_ms: i64,
    owned_processes: Vec<ProcessIdentity>,
    invocation_dir: PathBuf,
    release_marker: PathBuf,
}

fn process_identity_for_pid(pid: u32) -> ProcessIdentity {
    use windows_sys::Win32::{
        Foundation::{CloseHandle, FILETIME},
        System::Threading::{GetProcessTimes, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION},
    };
    let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    assert!(!handle.is_null(), "could not pin child process {pid}");
    let (mut created, mut exited, mut kernel, mut user) = (
        FILETIME::default(),
        FILETIME::default(),
        FILETIME::default(),
        FILETIME::default(),
    );
    let ok = unsafe { GetProcessTimes(handle, &mut created, &mut exited, &mut kernel, &mut user) };
    if ok == 0 {
        unsafe { CloseHandle(handle) };
        panic!("could not read child process {pid} creation time");
    }
    let birth = (u64::from(created.dwHighDateTime) << 32) | u64::from(created.dwLowDateTime);
    unsafe { CloseHandle(handle) };
    identity(pid, birth as i64)
}

fn invocation_count(directory: &Path, issue: &str) -> usize {
    fs::read_to_string(directory.join(format!("{issue}.calls")))
        .unwrap_or_default()
        .lines()
        .filter(|line| *line == "started")
        .count()
}

#[test]
#[ignore = "child mode launched only by the cold-restart parent test"]
fn cold_restart_child_mode() {
    let Ok(mode) = std::env::var("FABRICA_COLD_RESTART_CHILD_MODE") else {
        return;
    };
    let handoff = PathBuf::from(std::env::var_os("FABRICA_COLD_RESTART_HANDOFF").unwrap());
    match mode.as_str() {
        "stage" => {
            let parent_root =
                PathBuf::from(std::env::var_os("FABRICA_COLD_RESTART_PARENT_ROOT").unwrap());
            let temp = tempfile::Builder::new()
                .prefix("fabrica-cold-restart-")
                .tempdir_in(&parent_root)
                .unwrap();
            let root_note =
                PathBuf::from(std::env::var_os("FABRICA_COLD_RESTART_ROOT_NOTE").unwrap());
            fs::write(root_note, temp.path().to_string_lossy().as_bytes()).unwrap();
            let staged = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                run_recovery_fixture(&temp, Some(&handoff));
            }));
            match staged {
                Ok(()) => {
                    let _preserved_fixture_root = temp.keep();
                }
                Err(panic) => {
                    let preserved = temp.keep();
                    eprintln!(
                        "stage fixture preserved after panic at {}",
                        preserved.display()
                    );
                    std::panic::resume_unwind(panic);
                }
            }
        }
        "recover" => run_cold_restart_recovery(&handoff),
        other => panic!("unknown cold-restart child mode: {other}"),
    }
}

#[test]
fn fresh_supervisor_reopens_held_jobs_after_all_fixture_processes_exit() {
    let parent_temp = tempfile::tempdir().unwrap();
    let parent_root = parent_temp.keep();
    let handoff_path = parent_root.join("stage-handoff.json");
    let stage_stdout = fs::File::create(parent_root.join("stage.stdout.log")).unwrap();
    let stage_stderr = fs::File::create(parent_root.join("stage.stderr.log")).unwrap();
    let stage_root_note = parent_root.join("stage-fixture-root.txt");
    let mut stage = Command::new(std::env::current_exe().unwrap())
        .args([
            "--ignored",
            "--exact",
            "cold_restart_child_mode",
            "--nocapture",
            "--test-threads=1",
        ])
        .env("FABRICA_COLD_RESTART_CHILD_MODE", "stage")
        .env("FABRICA_COLD_RESTART_HANDOFF", &handoff_path)
        .env("FABRICA_COLD_RESTART_PARENT_ROOT", &parent_root)
        .env("FABRICA_COLD_RESTART_ROOT_NOTE", &stage_root_note)
        .stdout(stage_stdout)
        .stderr(stage_stderr)
        .spawn()
        .unwrap();
    let stage_identity = process_identity_for_pid(stage.id());
    let stage_deadline = Instant::now() + Duration::from_secs(150);
    let stage_status = loop {
        if let Some(status) = stage.try_wait().unwrap() {
            break status;
        }
        if Instant::now() >= stage_deadline {
            terminate_exact(stage_identity);
            let _ = stage.wait();
            if let Ok(root) = fs::read_to_string(&stage_root_note) {
                terminate_fixture_processes_from_root(Path::new(root.trim()));
            }
            let preserved = parent_root.clone();
            panic!(
                "stage child host exceeded its deadline; preserved fixture parent at {}",
                preserved.display()
            );
        }
        thread::sleep(Duration::from_millis(50));
    };
    if !stage_status.success() {
        if let Ok(root) = fs::read_to_string(&stage_root_note) {
            terminate_fixture_processes_from_root(Path::new(root.trim()));
        }
        let preserved = parent_root.clone();
        panic!(
            "stage child failed; parent evidence preserved at {} stdout={} stderr={}",
            preserved.display(),
            fs::read_to_string(preserved.join("stage.stdout.log")).unwrap_or_default(),
            fs::read_to_string(preserved.join("stage.stderr.log")).unwrap_or_default()
        );
    }
    assert!(
        !navis_fabrica_runner_core::process::process_identity_matches(stage_identity).unwrap(),
        "stage host identity remained live after child exit"
    );
    let handoff: ColdRestartHandoff =
        serde_json::from_slice(&fs::read(&handoff_path).unwrap()).unwrap();
    let mut exact_cleanup = RecordedProcessCleanup(Vec::new());
    for owned in handoff.owned_processes.iter().copied() {
        exact_cleanup.record(owned);
    }
    assert_eq!(
        handoff.owned_processes.len(),
        4,
        "expected two hosted workers and two fake providers"
    );
    assert!(handoff
        .owned_processes
        .iter()
        .all(|p| p.creation_time_100ns.is_some()));

    // Stop only fixture PIDs whose process birth identities were recorded by
    // the staging child. No process-name or tree-wide termination is used.
    for owned in handoff.owned_processes.iter().copied() {
        terminate_exact(owned);
    }
    for owned in handoff.owned_processes.iter().copied() {
        assert!(
            !navis_fabrica_runner_core::process::process_identity_matches(owned).unwrap(),
            "fixture process {} remained live",
            owned.pid
        );
    }
    assert_initial_implementation_calls(&handoff.invocation_dir);

    let recovery_stdout = fs::File::create(parent_root.join("recovery.stdout.log")).unwrap();
    let recovery_stderr = fs::File::create(parent_root.join("recovery.stderr.log")).unwrap();
    let mut recovery = Command::new(std::env::current_exe().unwrap())
        .args([
            "--ignored",
            "--exact",
            "cold_restart_child_mode",
            "--nocapture",
            "--test-threads=1",
        ])
        .env("FABRICA_COLD_RESTART_CHILD_MODE", "recover")
        .env("FABRICA_COLD_RESTART_HANDOFF", &handoff_path)
        .stdout(recovery_stdout)
        .stderr(recovery_stderr)
        .spawn()
        .unwrap();
    let recovery_identity = process_identity_for_pid(recovery.id());
    let recovery_deadline = Instant::now() + Duration::from_secs(240);
    let recovery_status = loop {
        if let Some(status) = recovery.try_wait().unwrap() {
            break status;
        }
        if Instant::now() >= recovery_deadline {
            terminate_exact(recovery_identity);
            let _ = recovery.wait();
            for owned in recorded_fixture_processes(&handoff) {
                if navis_fabrica_runner_core::process::process_identity_matches(owned)
                    .unwrap_or(false)
                {
                    terminate_exact(owned);
                }
            }
            let preserved = parent_root.clone();
            panic!(
                "fresh recovery child exceeded its deadline; evidence preserved at {}",
                preserved.display()
            );
        }
        thread::sleep(Duration::from_millis(50));
    };
    if !recovery_status.success() {
        if navis_fabrica_runner_core::process::process_identity_matches(recovery_identity).unwrap()
        {
            terminate_exact(recovery_identity);
        }
        for owned in recorded_fixture_processes(&handoff) {
            if navis_fabrica_runner_core::process::process_identity_matches(owned).unwrap() {
                terminate_exact(owned);
            }
        }
        let preserved = parent_root.clone();
        panic!(
            "fresh recovery child failed; fixture preserved at {} stdout={} stderr={}",
            preserved.display(),
            fs::read_to_string(preserved.join("recovery.stdout.log")).unwrap_or_default(),
            fs::read_to_string(preserved.join("recovery.stderr.log")).unwrap_or_default()
        );
    }
    assert!(
        !navis_fabrica_runner_core::process::process_identity_matches(recovery_identity).unwrap(),
        "fresh recovery host identity remained live after success"
    );
    // Terminal batch state is durable before the hosted worker OS process has
    // necessarily completed its normal return/drop path. Give every recorded
    // fixture worker/provider identity up to ten seconds to drain naturally.
    // Only after this monotonic deadline may exact-identity cleanup terminate
    // any residual process; no name/tree-based cleanup is used.
    let drain_deadline = Instant::now() + Duration::from_secs(10);
    let residual = wait_for_recorded_fixture_process_drain(&handoff, drain_deadline)
        .unwrap_or_else(|error| panic!("fixture process drain observation failed: {error}"));
    for owned in residual.iter().copied() {
        terminate_exact(owned);
    }
    assert!(
        residual.is_empty(),
        "fixture-owned workers/providers remained after bounded normal drain: {residual:?}"
    );
    assert!(exact_cleanup
        .0
        .iter()
        .all(|p| !navis_fabrica_runner_core::process::process_identity_matches(*p).unwrap()));
    let owned_parent = fs::canonicalize(&parent_root).unwrap();
    let owned_fixture = fs::canonicalize(&handoff.fixture_root).unwrap();
    assert_eq!(
        owned_fixture.parent(),
        Some(owned_parent.as_path()),
        "fixture cleanup target escaped the test-owned parent"
    );
    assert!(owned_fixture
        .file_name()
        .unwrap()
        .to_string_lossy()
        .starts_with("fabrica-cold-restart-"));
    assert!(owned_fixture.starts_with(&owned_parent));
    // Retain the owned fixture and logs as durable test evidence. The parent
    // temp root is intentionally kept at test start; no recursive deletion runs.
    let _retained_fixture_evidence = owned_fixture;
}

fn wait_for_recorded_fixture_process_drain(
    handoff: &ColdRestartHandoff,
    deadline: Instant,
) -> Result<Vec<ProcessIdentity>, String> {
    loop {
        let mut live = Vec::new();
        for process in recorded_fixture_processes(handoff) {
            match navis_fabrica_runner_core::process::process_identity_matches(process) {
                Ok(true) => live.push(process),
                Ok(false) => {}
                Err(error) => {
                    return Err(format!(
                        "could not verify fixture process {} birth identity: {error}",
                        process.pid
                    ));
                }
            }
        }
        if live.is_empty() || Instant::now() >= deadline {
            return Ok(live);
        }
        thread::sleep(Duration::from_millis(50));
    }
}

fn terminate_fixture_processes_from_root(root: &Path) {
    let data_root = root.join("durable");
    for process in recorded_processes_from_data_root(&data_root) {
        if navis_fabrica_runner_core::process::process_identity_matches(process).unwrap_or(false) {
            terminate_exact(process);
        }
    }
}

fn recorded_fixture_processes(handoff: &ColdRestartHandoff) -> Vec<ProcessIdentity> {
    let mut processes = handoff.owned_processes.clone();
    processes.extend(recorded_processes_from_data_root(&handoff.data_root));
    processes.sort_by_key(|p| (p.pid, p.creation_time_100ns));
    processes.dedup();
    processes
}

fn recorded_processes_from_data_root(data_root: &Path) -> Vec<ProcessIdentity> {
    let mut processes = Vec::new();
    if !data_root.join("jobs.sqlite3").exists() || !data_root.join("batches.sqlite3").exists() {
        return processes;
    }
    let jobs = JobsStore::open(data_root.join("jobs.sqlite3")).unwrap();
    for job in jobs.list(100).unwrap() {
        let id = job.id;
        for name in [
            "runtime_process_identity",
            "runtime_process_implement",
            "runtime_process_review",
        ] {
            if let Some(value) = jobs.get_checkpoint(id, name).unwrap() {
                if let Ok(identity) = serde_json::from_value::<ProcessIdentity>(value) {
                    if identity.creation_time_100ns.is_some() {
                        processes.push(identity);
                    }
                }
            }
        }
    }
    let conn = Connection::open(data_root.join("batches.sqlite3")).unwrap();
    let mut stmt = conn.prepare("SELECT pid,creation_time_100ns FROM batch_workers WHERE pid IS NOT NULL AND creation_time_100ns IS NOT NULL").unwrap();
    let rows = stmt
        .query_map([], |row| {
            Ok((row.get::<_, i64>(0)? as u32, row.get::<_, i64>(1)?))
        })
        .unwrap();
    for row in rows {
        let (pid, birth) = row.unwrap();
        processes.push(identity(pid, birth));
    }
    processes.sort_by_key(|p| (p.pid, p.creation_time_100ns));
    processes.dedup();
    processes
}

fn wait_for_initial_implementation_calls(
    directory: &Path,
    owned_processes: &[ProcessIdentity],
    deadline: Instant,
) -> Result<(), String> {
    loop {
        let crash_calls = invocation_count(directory, "TEST-L001");
        let gated_calls = invocation_count(directory, "TEST-L002");
        let independent_calls = invocation_count(directory, "TEST-L003");
        if crash_calls > 1 || gated_calls > 1 || independent_calls != 0 {
            return Err(format!(
                "unexpected initial invocation counts before handoff: L001={crash_calls}, L002={gated_calls}, L003={independent_calls}"
            ));
        }
        for process in owned_processes {
            match navis_fabrica_runner_core::process::process_identity_matches(*process) {
                Ok(true) => {}
                Ok(false) => {
                    return Err(format!(
                        "recorded stage process {} exited before both implementation markers",
                        process.pid
                    ));
                }
                Err(error) => {
                    return Err(format!(
                        "could not verify recorded stage process {}: {error}",
                        process.pid
                    ));
                }
            }
        }
        if crash_calls == 1 && gated_calls == 1 && independent_calls == 0 {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(format!(
                "20-second stage barrier expired with invocation counts L001={crash_calls}, L002={gated_calls}, L003={independent_calls}"
            ));
        }
        thread::sleep(Duration::from_millis(25));
    }
}

fn assert_initial_implementation_calls(directory: &Path) {
    assert_eq!(invocation_count(directory, "TEST-L001"), 1);
    assert_eq!(invocation_count(directory, "TEST-L002"), 1);
    assert_eq!(invocation_count(directory, "TEST-L003"), 0);
}

fn run_cold_restart_recovery(handoff_path: &Path) {
    let handoff: ColdRestartHandoff =
        serde_json::from_slice(&fs::read(handoff_path).unwrap()).unwrap();
    for owned in handoff.owned_processes.iter().copied() {
        assert!(
            !navis_fabrica_runner_core::process::process_identity_matches(owned).unwrap(),
            "old fixture process {} is still live",
            owned.pid
        );
    }
    let supervisor = Supervisor::open(&handoff.data_root).unwrap();
    let jobs = JobsStore::open(supervisor.paths().jobs.clone()).unwrap();
    let ids = [handoff.crash_job_id, handoff.gated_job_id];
    for id in ids {
        let before = jobs.get(id).unwrap();
        assert_eq!(
            before.state,
            JobState::Running,
            "job must reopen in its durable pre-reconcile state"
        );
        assert_eq!(
            before.attempt_count, 1,
            "fresh open must not duplicate a prior attempt"
        );
    }
    assert_initial_implementation_calls(&handoff.invocation_dir);

    let reconcile_at = handoff
        .crash_lease_until_ms
        .max(handoff.gated_lease_until_ms)
        + 1;
    jobs.reconcile_expired_ids(&ids, reconcile_at).unwrap();
    for id in ids {
        let held = jobs.get(id).unwrap();
        assert_eq!(held.state, JobState::ReconciliationRequired);
        assert_eq!(
            held.attempt_count, 1,
            "expired attempt remains held without retry"
        );
    }
    assert_initial_implementation_calls(&handoff.invocation_dir);
    for (id, issue) in [
        (handoff.crash_job_id, "TEST-L001"),
        (handoff.gated_job_id, "TEST-L002"),
    ] {
        supervisor.recover_job(&handoff.batch_id, RecoveryRequest {
            job_id: id,
            decision: RecoveryAction::SafeToRetry,
            evidence: format!("fresh host verified all recorded fixture PID/birth identities exited; {issue} synthetic effect was reconciled"),
            process_confirmed_stopped: true,
            effects_reconciled: true,
        }, reconcile_at + 1).unwrap();
    }
    fs::write(&handoff.release_marker, "continue after explicit recovery").unwrap();
    let worker = Path::new(env!("CARGO_BIN_EXE_fabrica-worker"));
    supervisor
        .resume_and_launch_hosted(&handoff.batch_id, worker, reconcile_at + 2)
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(180);
    for id in [
        handoff.crash_job_id,
        handoff.gated_job_id,
        handoff.independent_job_id,
    ] {
        let job = wait_job(&jobs, id, deadline, |state| *state == JobState::Verified);
        assert_eq!(
            job.attempt_count,
            if id == handoff.independent_job_id {
                1
            } else {
                2
            }
        );
    }
    assert_eq!(invocation_count(&handoff.invocation_dir, "TEST-L001"), 2);
    assert_eq!(invocation_count(&handoff.invocation_dir, "TEST-L002"), 2);
    assert_eq!(invocation_count(&handoff.invocation_dir, "TEST-L003"), 1);
    let complete_deadline = Instant::now() + Duration::from_secs(15);
    while supervisor.status(&handoff.batch_id).unwrap().state
        != navis_fabrica_runner_core::supervisor::BatchState::Completed
    {
        assert!(
            Instant::now() < complete_deadline,
            "recovered batch did not reach terminal completion"
        );
        thread::sleep(Duration::from_millis(50));
    }
}
