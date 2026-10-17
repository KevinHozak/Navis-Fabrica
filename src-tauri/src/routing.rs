//! Shared deterministic routing policy lives in runner-core so the CLI and
//! desktop IPC use the same decisions and eligibility rules.
pub use navis_fabrica_runner_core::routing::*;
