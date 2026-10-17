//! Typed command boundary shared by the CLI and desktop bridge.
//!
//! Preview is storage-free. Every operation that can write state uses the same
//! data root, and submit only persists/launches detached workers before returning.

use crate::supervisor::{
    normalize_batch_request, preview_batch_request, BatchPreview, BatchRequest, BatchSnapshot,
    BatchStatusView, ReconcileReport, RecoveryRequest, RecoveryResult, Supervisor, SupervisorError,
};
use serde::{Deserialize, Serialize};
use std::path::Path;

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum SupervisorOperation {
    BatchPreview,
    Submit,
    RunBatch,
    Status,
    Events,
    Pause,
    Resume,
    Stop,
    Cancel,
    Reconcile,
    Recover,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", content = "value", rename_all = "kebab-case")]
pub enum SupervisorOperationResult {
    Preview(BatchPreview),
    Submitted(BatchSnapshot),
    Foreground(BatchSnapshot),
    Status(BatchStatusView),
    Events(crate::supervisor::BatchEventPage),
    Reconciled(ReconcileReport),
    Recovery(RecoveryResult),
    Control(BatchSnapshot),
}

/// Dispatch one caller-authorized supervisor operation. Request files are decoded
/// by the CLI/bridge into a bounded typed `BatchRequest` before reaching this layer.
pub fn dispatch(
    data_root: &Path,
    operation: SupervisorOperation,
    batch_request: Option<BatchRequest>,
    batch_id: Option<&str>,
    cursor: u64,
    limit: u32,
    worker_executable: Option<&Path>,
    preview_fingerprint: Option<&str>,
    recovery: Option<RecoveryRequest>,
) -> Result<SupervisorOperationResult, SupervisorError> {
    dispatch_at(
        data_root,
        operation,
        batch_request,
        batch_id,
        cursor,
        limit,
        worker_executable,
        preview_fingerprint,
        recovery,
        now_ms(),
    )
}

pub(crate) fn dispatch_at(
    data_root: &Path,
    operation: SupervisorOperation,
    batch_request: Option<BatchRequest>,
    batch_id: Option<&str>,
    cursor: u64,
    limit: u32,
    worker_executable: Option<&Path>,
    preview_fingerprint: Option<&str>,
    recovery: Option<RecoveryRequest>,
    now: i64,
) -> Result<SupervisorOperationResult, SupervisorError> {
    if operation == SupervisorOperation::BatchPreview {
        let request = batch_request.ok_or_else(|| {
            SupervisorError::Invalid("batch-preview requires batchRequest".into())
        })?;
        let worker = worker_executable.ok_or_else(|| {
            SupervisorError::Invalid(
                "batch-preview requires the trusted fabrica-worker executable identity".into(),
            )
        })?;
        return preview_batch_request(data_root, &request, worker)
            .map(SupervisorOperationResult::Preview);
    }
    let supervisor = Supervisor::open(data_root)?;
    match operation {
        SupervisorOperation::BatchPreview => unreachable!(),
        SupervisorOperation::Submit => {
            let request =
                normalize_batch_request(&batch_request.ok_or_else(|| {
                    SupervisorError::Invalid("submit requires batchRequest".into())
                })?)?;
            let worker = worker_executable.ok_or_else(|| {
                SupervisorError::Invalid(
                    "submit requires the trusted fabrica-worker executable".into(),
                )
            })?;
            let expected = preview_fingerprint.ok_or_else(|| {
                SupervisorError::Invalid("submit requires previewFingerprint".into())
            })?;
            let current = preview_batch_request(data_root, &request, worker)?;
            if current.fingerprint != expected {
                return Err(SupervisorError::Conflict("settings, request, plan, or worker identity changed after preview; review the updated plan".into()));
            }
            let request = current.request;
            supervisor.start_and_launch(&request, worker, now)?;
            Ok(SupervisorOperationResult::Submitted(
                supervisor.status(&request.batch_id)?,
            ))
        }
        SupervisorOperation::RunBatch => {
            let request = normalize_batch_request(&batch_request.ok_or_else(|| {
                SupervisorError::Invalid("run-batch requires batchRequest".into())
            })?)?;
            let worker = worker_executable.ok_or_else(|| {
                SupervisorError::Invalid(
                    "run-batch requires the trusted fabrica-worker executable".into(),
                )
            })?;
            let expected = preview_fingerprint.ok_or_else(|| {
                SupervisorError::Invalid("run-batch requires previewFingerprint".into())
            })?;
            if supervisor.admitted_request(&request.batch_id)?.is_some() {
                return Ok(SupervisorOperationResult::Foreground(
                    supervisor.run_admitted_batch_foreground(&request, worker, expected, now)?,
                ));
            }
            let current = preview_batch_request(data_root, &request, worker)?;
            if current.fingerprint != expected {
                return Err(SupervisorError::Conflict("settings, request, plan, or worker identity changed after preview; review the updated plan".into()));
            }
            Ok(SupervisorOperationResult::Foreground(
                supervisor.run_batch_foreground(&current.request, worker, now)?,
            ))
        }
        SupervisorOperation::Status => Ok(SupervisorOperationResult::Status(
            supervisor.status_view(required_batch_id(batch_id)?)?,
        )),
        SupervisorOperation::Events => Ok(SupervisorOperationResult::Events(supervisor.events(
            required_batch_id(batch_id)?,
            cursor,
            limit,
        )?)),
        SupervisorOperation::Pause => Ok(SupervisorOperationResult::Control(supervisor.pause(
            required_batch_id(batch_id)?,
            "paused by explicit supervisor control",
            now,
        )?)),
        SupervisorOperation::Resume => {
            let id = required_batch_id(batch_id)?;
            let worker = worker_executable.ok_or_else(|| {
                SupervisorError::Invalid(
                    "resume requires the trusted fabrica-worker executable".into(),
                )
            })?;
            Ok(SupervisorOperationResult::Control(
                supervisor.resume_and_launch(id, worker, now)?,
            ))
        }
        SupervisorOperation::Stop => Ok(SupervisorOperationResult::Control(supervisor.stop(
            required_batch_id(batch_id)?,
            "graceful stop requested",
            now,
        )?)),
        SupervisorOperation::Cancel => Ok(SupervisorOperationResult::Control(supervisor.cancel(
            required_batch_id(batch_id)?,
            "cancellation requested",
            now,
        )?)),
        SupervisorOperation::Reconcile => Ok(SupervisorOperationResult::Reconciled(
            supervisor.reconcile_batch(required_batch_id(batch_id)?)?,
        )),
        SupervisorOperation::Recover => Ok(SupervisorOperationResult::Recovery(
            supervisor.recover_job(
                required_batch_id(batch_id)?,
                recovery
                    .ok_or_else(|| SupervisorError::Invalid("recover requires recovery".into()))?,
                now,
            )?,
        )),
    }
}

pub use crate::supervisor::BatchPreview as PreviewResult;

fn required_batch_id(value: Option<&str>) -> Result<&str, SupervisorError> {
    value
        .filter(|id| !id.trim().is_empty())
        .ok_or_else(|| SupervisorError::Invalid("operation requires batchId".into()))
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(i64::MAX as u128) as i64
}

#[cfg(test)]
mod protocol_tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn rejected_storage_free_preview_does_not_create_runner_state() {
        let folder = tempdir().unwrap();
        let data_root = folder.path().join("runner-state");
        let result = dispatch(
            &data_root,
            SupervisorOperation::BatchPreview,
            None,
            None,
            0,
            20,
            None,
            None,
            None,
        );
        assert!(matches!(result, Err(SupervisorError::Invalid(_))));
        assert!(
            !data_root.exists(),
            "rejected preview must not create stores"
        );
    }
}
