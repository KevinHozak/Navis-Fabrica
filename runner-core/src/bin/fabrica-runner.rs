use navis_fabrica_runner_core::{execute, CoreRequest, CoreResponse, Operation, PROTOCOL_VERSION};
use std::io::{self, Read};
use std::path::PathBuf;

const MAX_INPUT_BYTES: u64 = 1024 * 1024;

fn main() {
    std::process::exit(run());
}

fn run() -> i32 {
    let mut args = std::env::args().skip(1);
    let Some(command) = args.next() else {
        return emit_error("INVALID_ARGUMENT", "command is required", 2);
    };
    if command == "--version" || command == "version" {
        println!(
            "{}",
            serde_json::json!({"protocolVersion":PROTOCOL_VERSION,"runnerVersion":env!("CARGO_PKG_VERSION")})
        );
        return 0;
    }
    if command == "--help" || command == "help" {
        println!(
            "{}",
            serde_json::json!({"protocolVersion":PROTOCOL_VERSION,"usage":"fabrica-runner <capabilities|doctor|issues-list|issues-page|issues-show|issues-create|issues-update|issues-delete|issues-validate|plan|provider-settings|providers-doctor|provider-observations-refresh|runtime-run|economy-trial-run|batch-preview|submit|run-batch|status|events|pause|resume|stop|cancel|reconcile|recover> [--project PATH --config PATH] [--settings PATH] [--data-root PATH --batch-id ID --worker-executable EXE] [--cursor N --limit N] [--batch-request PATH] [--preview-fingerprint HASH] [--offset N] [--live-probe] [--id ID] [--expected-revision HASH]","input":"economy-trial-run requires --settings PATH --data-root NEW_PRIVATE_PATH --model-id EXACT --task-id FIXTURE_ID --fixture-dir PATH --synthetic-policy-consent and dispatches one fixed synthetic fixture pair; issues-create and issues-update read one IssueRecord JSON object; runtime-run reads one RuntimeRequest JSON object; batch-preview, submit, and run-batch read one BatchRequest JSON object from stdin or --batch-request PATH; provider-observations-refresh requires --settings PATH and --data-root PATH, and --live-probe explicitly permits a live OpenRouter catalog/auth probe; recover reads one RecoveryRequest JSON object","output":"one JSON CoreResponse per normal invocation; flat bounded TrialReceipt for economy-trial-run"})
        );
        return 0;
    }
    if command == "economy-trial-run" {
        return run_economy_trial(args);
    }
    let Some(operation) = Operation::from_cli(&command) else {
        return emit_error(
            "INVALID_ARGUMENT",
            format!("unknown command {command:?}; use --help"),
            2,
        );
    };
    let mut project_path = None;
    let mut config_path = None;
    let mut settings_path = None;
    let mut allow_live_probe = false;
    let mut issue_id = None;
    let mut offset = None;
    let mut limit = None;
    let mut data_root = None;
    let mut batch_id = None;
    let mut cursor = None;
    let mut worker_executable = None;
    let mut batch_request_path = None;
    let mut preview_fingerprint = None;
    let mut expected_revision = None;
    while let Some(flag) = args.next() {
        if flag == "--live-probe" {
            allow_live_probe = true;
            continue;
        }
        let Some(value) = args.next() else {
            return emit_error("INVALID_ARGUMENT", format!("{flag} requires a value"), 2);
        };
        match flag.as_str() {
            "--project" => project_path = Some(PathBuf::from(value)),
            "--config" => config_path = Some(PathBuf::from(value)),
            "--settings" => settings_path = Some(PathBuf::from(value)),
            "--id" => issue_id = Some(value),
            "--offset" => {
                offset = match value.parse() {
                    Ok(n) => Some(n),
                    Err(_) => {
                        return emit_error(
                            "INVALID_ARGUMENT",
                            "offset must be a non-negative integer",
                            2,
                        )
                    }
                }
            }
            "--limit" => {
                limit = match value.parse() {
                    Ok(n) => Some(n),
                    Err(_) => {
                        return emit_error(
                            "INVALID_ARGUMENT",
                            "limit must be a positive integer",
                            2,
                        )
                    }
                }
            }
            "--data-root" => data_root = Some(PathBuf::from(value)),
            "--batch-id" => batch_id = Some(value),
            "--worker-executable" => worker_executable = Some(PathBuf::from(value)),
            "--batch-request" => batch_request_path = Some(PathBuf::from(value)),
            "--preview-fingerprint" => preview_fingerprint = Some(value),
            "--cursor" => {
                cursor = match value.parse() {
                    Ok(n) => Some(n),
                    Err(_) => {
                        return emit_error(
                            "INVALID_ARGUMENT",
                            "cursor must be a non-negative integer",
                            2,
                        )
                    }
                }
            }
            "--expected-revision" => expected_revision = Some(value),
            _ => return emit_error("INVALID_ARGUMENT", format!("unknown option {flag:?}"), 2),
        }
    }
    let record = if matches!(operation, Operation::IssuesCreate | Operation::IssuesUpdate) {
        let mut input = String::new();
        if let Err(error) = io::stdin()
            .take(MAX_INPUT_BYTES + 1)
            .read_to_string(&mut input)
        {
            return emit_error("IO_ERROR", error.to_string(), 6);
        }
        if input.len() as u64 > MAX_INPUT_BYTES {
            return emit_error(
                "INPUT_TOO_LARGE",
                "JSON record exceeds the 1 MiB input limit",
                2,
            );
        }
        match serde_json::from_str(&input) {
            Ok(record) => Some(record),
            Err(error) => {
                return emit_error(
                    "INVALID_ARGUMENT",
                    format!("stdin must contain one IssueRecord JSON object: {error}"),
                    2,
                )
            }
        }
    } else {
        None
    };
    let runtime_request = if matches!(operation, Operation::RuntimeRun) {
        let mut input = String::new();
        if let Err(error) = io::stdin()
            .take(MAX_INPUT_BYTES + 1)
            .read_to_string(&mut input)
        {
            return emit_error("IO_ERROR", error.to_string(), 6);
        }
        if input.len() as u64 > MAX_INPUT_BYTES {
            return emit_error(
                "INPUT_TOO_LARGE",
                "RuntimeRequest exceeds the 1 MiB input limit",
                2,
            );
        }
        match serde_json::from_str(&input) {
            Ok(request) => Some(request),
            Err(error) => {
                return emit_error(
                    "INVALID_ARGUMENT",
                    format!("stdin must contain one RuntimeRequest JSON object: {error}"),
                    2,
                )
            }
        }
    } else {
        None
    };
    let batch_request = if matches!(
        operation,
        Operation::BatchPreview | Operation::Submit | Operation::RunBatch
    ) {
        let mut input = String::new();
        if let Some(path) = batch_request_path.as_ref() {
            let Ok(metadata) = std::fs::metadata(path) else {
                return emit_error("IO_ERROR", "batch request file could not be inspected", 6);
            };
            if metadata.len() > MAX_INPUT_BYTES {
                return emit_error(
                    "INPUT_TOO_LARGE",
                    "BatchRequest exceeds the 1 MiB input limit",
                    2,
                );
            }
            match std::fs::read_to_string(path) {
                Ok(text) => input = text,
                Err(error) => return emit_error("IO_ERROR", error.to_string(), 6),
            }
        } else if let Err(error) = io::stdin()
            .take(MAX_INPUT_BYTES + 1)
            .read_to_string(&mut input)
        {
            return emit_error("IO_ERROR", error.to_string(), 6);
        }
        if input.len() as u64 > MAX_INPUT_BYTES {
            return emit_error(
                "INPUT_TOO_LARGE",
                "BatchRequest exceeds the 1 MiB input limit",
                2,
            );
        }
        match serde_json::from_str(&input) {
            Ok(request) => Some(request),
            Err(error) => {
                return emit_error(
                    "INVALID_ARGUMENT",
                    format!("input must contain one BatchRequest JSON object: {error}"),
                    2,
                )
            }
        }
    } else {
        None
    };
    let recovery = if matches!(operation, Operation::Recover) {
        let mut input = String::new();
        if let Err(error) = io::stdin()
            .take(MAX_INPUT_BYTES + 1)
            .read_to_string(&mut input)
        {
            return emit_error("IO_ERROR", error.to_string(), 6);
        }
        if input.len() as u64 > MAX_INPUT_BYTES {
            return emit_error(
                "INPUT_TOO_LARGE",
                "RecoveryRequest exceeds the 1 MiB input limit",
                2,
            );
        }
        match serde_json::from_str(&input) {
            Ok(value) => Some(value),
            Err(error) => {
                return emit_error(
                    "INVALID_ARGUMENT",
                    format!("input must contain one RecoveryRequest JSON object: {error}"),
                    2,
                )
            }
        }
    } else {
        None
    };
    emit(execute(CoreRequest {
        protocol_version: PROTOCOL_VERSION,
        operation,
        project_path,
        config_path,
        settings_path,
        allow_live_probe,
        issue_id,
        offset,
        limit,
        expected_revision,
        record,
        runtime_request,
        data_root,
        batch_request,
        batch_request_path,
        batch_id,
        cursor,
        worker_executable,
        preview_fingerprint,
        recovery,
    }))
}

fn run_economy_trial(mut args: impl Iterator<Item = String>) -> i32 {
    let mut settings = None;
    let mut data_root = None;
    let mut model_id = None;
    let mut task_id = None;
    let mut fixture_dir = None;
    let mut consent = false;
    while let Some(flag) = args.next() {
        if flag == "--synthetic-policy-consent" {
            if consent {
                return emit_trial_error(
                    "INVALID_TRIAL_ARGUMENT",
                    model_id.as_deref(),
                    task_id.as_deref(),
                );
            }
            consent = true;
            continue;
        }
        let Some(value) = args.next() else {
            return emit_trial_error(
                "INVALID_TRIAL_ARGUMENT",
                model_id.as_deref(),
                task_id.as_deref(),
            );
        };
        let target = match flag.as_str() {
            "--settings" => &mut settings,
            "--data-root" => &mut data_root,
            "--model-id" => &mut model_id,
            "--task-id" => &mut task_id,
            "--fixture-dir" => &mut fixture_dir,
            _ => {
                return emit_trial_error(
                    "INVALID_TRIAL_ARGUMENT",
                    model_id.as_deref(),
                    task_id.as_deref(),
                )
            }
        };
        if target.is_some() {
            return emit_trial_error(
                "INVALID_TRIAL_ARGUMENT",
                model_id.as_deref(),
                task_id.as_deref(),
            );
        }
        *target = Some(value);
    }
    let (Some(settings), Some(data_root), Some(model_id), Some(task_id), Some(fixture_dir)) = (
        settings,
        data_root,
        model_id.clone(),
        task_id.clone(),
        fixture_dir,
    ) else {
        return emit_trial_error(
            "INVALID_TRIAL_ARGUMENT",
            model_id.as_deref(),
            task_id.as_deref(),
        );
    };
    let receipt = navis_fabrica_runner_core::economy::execute_trial(
        navis_fabrica_runner_core::economy::TrialRunArguments {
            settings_path: std::path::Path::new(&settings),
            data_root: std::path::Path::new(&data_root),
            model_id: &model_id,
            task_id: &task_id,
            fixture_dir: std::path::Path::new(&fixture_dir),
            synthetic_policy_consent: consent,
        },
    );
    println!(
        "{}",
        serde_json::to_string(&receipt).expect("trial receipt serializes")
    );
    if receipt.ok {
        0
    } else {
        1
    }
}

fn emit_trial_error(message: &str, model_id: Option<&str>, task_id: Option<&str>) -> i32 {
    let receipt = navis_fabrica_runner_core::economy::TrialReceipt {
        schema_version: 1,
        ok: false,
        task_id: task_id.unwrap_or_default().to_owned(),
        model_id: model_id.unwrap_or_default().to_owned(),
        provider_id: String::new(),
        runtime_id: "aider".into(),
        runtime_version: String::new(),
        state: "failed".into(),
        request_count: 0,
        tokens_used: None,
        latency_ms: 0,
        billing: "zeroCost".into(),
        source_fingerprint: None,
        failure_code: Some(message.into()),
        observed_model_id: None,
    };
    println!(
        "{}",
        serde_json::to_string(&receipt).expect("trial receipt serializes")
    );
    2
}

fn emit(response: CoreResponse) -> i32 {
    let exit_code = response.exit_code;
    println!(
        "{}",
        serde_json::to_string(&response).expect("CoreResponse must serialize")
    );
    exit_code
}
fn emit_error(code: &str, message: impl Into<String>, exit_code: i32) -> i32 {
    emit(CoreResponse {
        protocol_version: PROTOCOL_VERSION,
        ok: false,
        exit_code,
        result: None,
        error: Some(navis_fabrica_runner_core::ProtocolError {
            code: code.into(),
            message: message.into(),
        }),
    })
}
