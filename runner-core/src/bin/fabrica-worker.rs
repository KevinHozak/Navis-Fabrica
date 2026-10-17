use navis_fabrica_runner_core::supervisor::{worker_main, WorkerExit};
use serde_json::json;
use std::path::PathBuf;

fn main() {
    let code = run();
    std::process::exit(code);
}

fn run() -> i32 {
    let mut args = std::env::args().skip(1);
    let (mut data_root, mut batch_id, mut worker_id) = (None, None, None);
    while let Some(flag) = args.next() {
        let Some(value) = args.next() else {
            return emit_error("INVALID_ARGUMENT", format!("{flag} requires a value"), 2);
        };
        match flag.as_str() {
            "--data-root" => data_root = Some(PathBuf::from(value)),
            "--batch-id" => batch_id = Some(value),
            "--worker-id" => worker_id = Some(value),
            _ => return emit_error("INVALID_ARGUMENT", format!("unknown option {flag:?}"), 2),
        }
    }
    let (Some(data_root), Some(batch_id), Some(worker_id)) = (data_root, batch_id, worker_id)
    else {
        return emit_error(
            "INVALID_ARGUMENT",
            "--data-root, --batch-id, and --worker-id are required",
            2,
        );
    };
    if !data_root.is_absolute() || batch_id.is_empty() || worker_id.is_empty() {
        return emit_error(
            "INVALID_ARGUMENT",
            "data root must be absolute and batch/worker IDs nonempty",
            2,
        );
    }
    match worker_main(&data_root, &batch_id, &worker_id) {
        Ok(exit) => {
            println!(
                "{}",
                json!({"ok":true,"batchId":batch_id,"workerId":worker_id,"state":exit})
            );
            match exit {
                WorkerExit::Completed
                | WorkerExit::Paused
                | WorkerExit::WaitingForInput
                | WorkerExit::Stopping
                | WorkerExit::Cancelled
                | WorkerExit::Failed
                | WorkerExit::WindowExpired => 0,
            }
        }
        Err(error) => emit_error("WORKER_FAILED", error.to_string(), 6),
    }
}

fn emit_error(code: &str, message: impl Into<String>, exit_code: i32) -> i32 {
    println!(
        "{}",
        json!({"ok":false,"error":{"code":code,"message":message.into()}})
    );
    exit_code
}
