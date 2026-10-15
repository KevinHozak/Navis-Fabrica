# Windows backend child processes

All app backend child processes are noninteractive and must be created with `crate::process::command`. On Windows the shared constructor sets `CREATE_NO_WINDOW`; standard input, output, and error configuration remains owned by each caller. This prevents transient console windows without changing captured output, exit status, cancellation, or process-tree cleanup.

## Launch paths reviewed for issue #56

- **Project discovery and Git facts:** `scanner::git_output` starts Git and reads piped stdout/stderr while retaining timeout and cancellation handling.
- **Provider discovery:** `providers::probe_antigravity` runs the configured Antigravity executable for `--version` and `models`, capturing each result.
- **Workflow setup and validation:** `runs` invokes PowerShell to inspect artifact/process state, invokes Git to validate and fingerprint worktrees, and invokes `kill` on non-Windows systems.
- **Workflow execution:** `runs::command_for` prepares PowerShell, `cmd.exe`, or the configured executable with piped stdin/stdout/stderr; the run manager streams output and records exit status.
- **Cancellation:** `runs::terminate_process_tree` invokes `taskkill.exe` on Windows, then retains the existing child kill/wait cleanup.

These calls previously relied on the platform's default console creation behavior. Source inspection identified these launch paths, but the original report did not identify a single triggering action and no live reproduction was performed for this change. Interactive terminal behavior is outside this backend API; no backend launch is intentionally interactive.

A Windows-only regression contract test in `process.rs` launches the Rust test executable through the shared constructor and checks captured output and successful exit status. Existing scanner and workflow paths retain their current piped streams, cancellation polling, and cleanup logic.
