use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::{BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tauri::{AppHandle, Emitter, Manager};

const HISTORY_FILE: &str = "workflow-runs.sqlite3";
const MAX_RUNS: usize = 100;
const MAX_EVENTS: usize = 2_000;
const MAX_OUTPUT_BYTES: usize = 512 * 1024;
const MAX_OUTPUT_CHUNK: usize = 4 * 1024;
const MAX_COMMAND_BYTES: usize = 64 * 1024;
const MAX_TIMEOUT_SECS: u64 = 24 * 60 * 60;
const POLL_INTERVAL: Duration = Duration::from_millis(100);

static RUNS: OnceLock<Mutex<RunStore>> = OnceLock::new();
static NEXT_ID: AtomicU64 = AtomicU64::new(1);

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct StartRequest {
    pub executable: String,
    pub arguments: Vec<String>,
    pub cwd: String,
    pub timeout_seconds: u64,
    pub mutating: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RunEvent {
    pub sequence: u64,
    pub timestamp_ms: u128,
    pub kind: String,
    pub text: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RunRecord {
    pub id: String,
    pub executable: String,
    pub cwd: String,
    pub mutating: bool,
    pub state: String,
    pub started_at_ms: u128,
    pub finished_at_ms: Option<u128>,
    pub exit_code: Option<i32>,
    pub next_sequence: u64,
    pub events: Vec<RunEvent>,
}

#[derive(Default)]
struct RunStore {
    records: Vec<RunRecord>,
    active: HashMap<String, Arc<Mutex<Child>>>,
    mutating_paths: HashSet<String>,
    output_bytes: HashMap<String, usize>,
    cancelled: HashSet<String>,
    redaction_values: HashMap<String, Vec<String>>,
}

fn store() -> &'static Mutex<RunStore> {
    RUNS.get_or_init(|| Mutex::new(RunStore::default()))
}

fn now_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
}

fn validate_request(request: &StartRequest) -> Result<(PathBuf, PathBuf, String), String> {
    if request.executable.is_empty()
        || request.executable.len() > 32_767
        || request.executable.chars().any(char::is_control)
        || request.executable.contains('\0')
    {
        return Err("Choose a valid executable path or command name.".into());
    }
    if request.arguments.len() > 256
        || request
            .arguments
            .iter()
            .any(|arg| arg.contains('\0') || arg.len() > 32_767)
        || request.arguments.iter().map(String::len).sum::<usize>() > MAX_COMMAND_BYTES
    {
        return Err("The argument array is invalid or exceeds its size limit.".into());
    }
    if request.timeout_seconds == 0 || request.timeout_seconds > MAX_TIMEOUT_SECS {
        return Err("Timeout must be between 1 second and 24 hours.".into());
    }
    let cwd = Path::new(request.cwd.trim())
        .canonicalize()
        .map_err(|_| "The working directory could not be resolved.".to_string())?;
    if !cwd.is_dir() {
        return Err("The working directory must be an existing folder.".into());
    }

    let entered = PathBuf::from(request.executable.trim());
    let executable = if entered.components().count() > 1 || entered.is_absolute() {
        let absolute = if entered.is_absolute() {
            entered
        } else {
            cwd.join(entered)
        };
        absolute
            .canonicalize()
            .map_err(|_| "The executable could not be resolved.".to_string())?
    } else {
        PathBuf::from(request.executable.trim())
    };
    if executable.components().count() > 1 && !executable.is_file() {
        return Err("The executable must be an existing file.".into());
    }
    #[cfg(windows)]
    {
        let command_units = executable
            .as_os_str()
            .to_string_lossy()
            .encode_utf16()
            .count()
            + request
                .arguments
                .iter()
                .map(|argument| argument.encode_utf16().count() + 2)
                .sum::<usize>()
            + 32;
        if command_units > 32_767 {
            return Err(
                "The executable and arguments exceed the Windows command-line limit.".into(),
            );
        }
    }

    let suffix = executable
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or("");
    #[cfg(windows)]
    if suffix.eq_ignore_ascii_case("ps1") {
        return Ok((executable, cwd, "powershell".into()));
    }
    #[cfg(windows)]
    if suffix.eq_ignore_ascii_case("cmd") || suffix.eq_ignore_ascii_case("bat") {
        if executable
            .to_string_lossy()
            .chars()
            .any(|c| "&|<>^%!\"\r\n".contains(c))
            || request
                .arguments
                .iter()
                .any(|arg| arg.chars().any(|c| "&|<>^%!\"\r\n".contains(c)))
        {
            return Err("CMD arguments containing shell metacharacters are not supported.".into());
        }
        return Ok((executable, cwd, "cmd".into()));
    }
    let _ = suffix;
    Ok((executable, cwd, "native".into()))
}

fn mutation_scope(cwd: &Path) -> String {
    cwd.ancestors()
        .find(|folder| folder.join(".git").exists())
        .unwrap_or(cwd)
        .to_string_lossy()
        .to_lowercase()
}

fn command_for(
    executable: &Path,
    cwd: &Path,
    host: &str,
    args: &[String],
) -> Result<Command, String> {
    let mut command = match host {
        "powershell" => {
            let mut command = Command::new("powershell.exe");
            command.args([
                "-NoLogo",
                "-NoProfile",
                "-NonInteractive",
                "-ExecutionPolicy",
                "Bypass",
                "-File",
            ]);
            command.arg(executable);
            command.args(args);
            command
        }
        "cmd" => {
            let mut command = Command::new("cmd.exe");
            command.args(["/d", "/v:off", "/s", "/c"]);
            let mut line = format!("\"\"{}\"", executable.display());
            for arg in args {
                line.push(' ');
                line.push('"');
                line.push_str(arg);
                line.push('"');
            }
            line.push('"');
            #[cfg(windows)]
            {
                use std::os::windows::process::CommandExt;
                command.raw_arg(&line);
            }
            command
        }
        _ => {
            let mut command = Command::new(executable);
            command.args(args);
            command
        }
    };
    command
        .current_dir(cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    Ok(command)
}

fn history_path(app: &AppHandle) -> Result<PathBuf, String> {
    let directory = app
        .path()
        .app_data_dir()
        .map_err(|_| "Could not locate run history storage.".to_string())?;
    fs::create_dir_all(&directory)
        .map_err(|_| "Could not create run history storage.".to_string())?;
    Ok(directory.join(HISTORY_FILE))
}

fn persist_records(path: &Path, records: &[RunRecord]) -> Result<(), String> {
    let mut connection =
        Connection::open(path).map_err(|_| "Could not open run history.".to_string())?;
    connection
        .busy_timeout(Duration::from_secs(2))
        .map_err(|_| "Could not configure run history.".to_string())?;
    connection
        .execute_batch(
            "CREATE TABLE IF NOT EXISTS workflow_runs (
            id TEXT PRIMARY KEY NOT NULL,
            started_at_ms INTEGER NOT NULL,
            record_json TEXT NOT NULL
        );",
        )
        .map_err(|_| "Could not initialize run history.".to_string())?;
    let transaction = connection
        .transaction()
        .map_err(|_| "Could not begin run history update.".to_string())?;
    for record in records {
        let json = serde_json::to_string(record)
            .map_err(|_| "Could not encode run history.".to_string())?;
        transaction.execute(
            "INSERT OR REPLACE INTO workflow_runs (id, started_at_ms, record_json) VALUES (?1, ?2, ?3)",
            params![record.id, record.started_at_ms as i64, json],
        ).map_err(|_| "Could not save run history.".to_string())?;
    }
    transaction.execute(
        "DELETE FROM workflow_runs WHERE id NOT IN (SELECT id FROM workflow_runs ORDER BY started_at_ms DESC LIMIT ?1)",
        [MAX_RUNS as i64],
    ).map_err(|_| "Could not trim run history.".to_string())?;
    transaction
        .commit()
        .map_err(|_| "Could not commit run history.".to_string())?;
    Ok(())
}

fn load_records(path: &Path) -> Result<Vec<RunRecord>, String> {
    if !path.exists() {
        return Ok(Vec::new());
    }
    let connection =
        Connection::open(path).map_err(|_| "Could not open run history.".to_string())?;
    connection
        .busy_timeout(Duration::from_secs(2))
        .map_err(|_| "Could not configure run history.".to_string())?;
    let mut statement = connection
        .prepare("SELECT record_json FROM workflow_runs ORDER BY started_at_ms DESC LIMIT ?1")
        .map_err(|_| "Could not read run history.".to_string())?;
    let rows = statement
        .query_map([MAX_RUNS as i64], |row| row.get::<_, String>(0))
        .map_err(|_| "Could not read run history.".to_string())?;
    let mut records = Vec::new();
    for row in rows {
        let json = row.map_err(|_| "Could not read run history row.".to_string())?;
        records.push(
            serde_json::from_str::<RunRecord>(&json)
                .map_err(|_| "Run history contains a malformed record.".to_string())?,
        );
    }
    let mut recovered = false;
    for record in &mut records {
        if matches!(
            record.state.as_str(),
            "queued" | "running" | "needsInput" | "cancelling"
        ) {
            record.state = "interrupted".into();
            record.finished_at_ms = Some(now_ms());
            recovered = true;
        }
    }
    records.truncate(MAX_RUNS);
    if recovered {
        persist_records(path, &records)?;
    }
    Ok(records)
}

fn redact(line: &str, secrets: &[String]) -> String {
    let mut value = line.to_owned();
    for secret in secrets.iter().filter(|secret| secret.len() >= 4) {
        value = value.replace(secret, "[REDACTED]");
    }
    for marker in ["ghp_", "gho_", "ghu_", "ghs_", "github_pat_"] {
        let mut offset = 0;
        while let Some(index) = value[offset..].find(marker) {
            let start = offset + index;
            let end = value[start..]
                .find(char::is_whitespace)
                .map(|n| start + n)
                .unwrap_or(value.len());
            value.replace_range(start..end, "[REDACTED]");
            offset = start + 10;
            if offset >= value.len() {
                break;
            }
        }
    }
    for label in [
        "bearer ",
        "token=",
        "token:",
        "password=",
        "password:",
        "secret=",
        "secret:",
        "api_key=",
        "api_key:",
        "apikey=",
        "authorization:",
    ] {
        let mut offset = 0;
        while let Some(found) = value[offset..].to_ascii_lowercase().find(label) {
            let start = offset + found + label.len();
            let end = value[start..]
                .find(char::is_whitespace)
                .map(|index| start + index)
                .unwrap_or(value.len());
            if start < end {
                value.replace_range(start..end, "[REDACTED]");
            }
            offset = start.saturating_add(10);
            if offset >= value.len() {
                break;
            }
        }
    }
    value
}

fn emit_event(app: &AppHandle, path: &Path, id: &str, kind: &str, text: &str) {
    let mut store = match store().lock() {
        Ok(value) => value,
        Err(_) => return,
    };
    if !store.records.iter().any(|record| record.id == id) {
        return;
    }
    if *store.output_bytes.get(id).unwrap_or(&0) >= MAX_OUTPUT_BYTES {
        return;
    }
    let secrets = store.redaction_values.get(id).cloned().unwrap_or_default();
    let safe_text = redact(text, &secrets);
    let output_bytes = store.output_bytes.entry(id.to_owned()).or_default();
    let accepted = safe_text
        .len()
        .min(MAX_OUTPUT_BYTES.saturating_sub(*output_bytes));
    let truncated = accepted < safe_text.len();
    let mut end = accepted;
    while !safe_text.is_char_boundary(end) {
        end -= 1;
    }
    let text = safe_text[..end].to_owned();
    *output_bytes += accepted;
    let run_id = id.to_owned();
    let Some(record) = store.records.iter_mut().find(|record| record.id == id) else {
        return;
    };
    let event = RunEvent {
        sequence: record.next_sequence,
        timestamp_ms: now_ms(),
        kind: kind.to_owned(),
        text,
    };
    record.next_sequence += 1;
    record.events.push(event.clone());
    if truncated
        && record
            .events
            .last()
            .is_some_and(|entry| entry.kind != "outputLimit")
    {
        record.events.push(RunEvent {
            sequence: record.next_sequence,
            timestamp_ms: now_ms(),
            kind: "outputLimit".into(),
            text: "Output limit reached; additional output was discarded.".into(),
        });
        record.next_sequence += 1;
    }
    if record.events.len() > MAX_EVENTS {
        let remove = record.events.len() - MAX_EVENTS;
        record.events.drain(0..remove);
    }
    let _ = persist_records(path, &store.records);
    let _ = app.emit("workflow-run-event", (run_id, event));
}

fn set_state(app: &AppHandle, path: &Path, id: &str, state: &str, exit_code: Option<i32>) {
    let mut store = match store().lock() {
        Ok(value) => value,
        Err(_) => return,
    };
    if let Some(record) = store.records.iter_mut().find(|record| record.id == id) {
        record.state = state.into();
        record.exit_code = exit_code;
        if matches!(
            state,
            "completed" | "failed" | "cancelled" | "timedOut" | "interrupted"
        ) {
            record.finished_at_ms = Some(now_ms());
        }
        let snapshot = record.clone();
        let _ = persist_records(path, &store.records);
        let _ = app.emit("workflow-run-updated", snapshot);
    }
}

fn pump_output(
    app: AppHandle,
    path: PathBuf,
    id: String,
    kind: &'static str,
    reader: impl std::io::Read + Send + 'static,
) {
    thread::spawn(move || {
        let mut reader = BufReader::new(reader);
        let mut buffer = [0u8; MAX_OUTPUT_CHUNK];
        let mut pending = Vec::with_capacity(MAX_OUTPUT_CHUNK);
        loop {
            match reader.read(&mut buffer) {
                Ok(0) => {
                    if !pending.is_empty() {
                        emit_event(&app, &path, &id, kind, &String::from_utf8_lossy(&pending));
                    }
                    break;
                }
                Ok(length) => {
                    for byte in &buffer[..length] {
                        if *byte == b'\n' {
                            if pending.last() == Some(&b'\r') {
                                pending.pop();
                            }
                            emit_event(&app, &path, &id, kind, &String::from_utf8_lossy(&pending));
                            pending.clear();
                        } else {
                            pending.push(*byte);
                            if pending.len() == MAX_OUTPUT_CHUNK {
                                emit_event(
                                    &app,
                                    &path,
                                    &id,
                                    kind,
                                    &String::from_utf8_lossy(&pending),
                                );
                                pending.clear();
                            }
                        }
                    }
                }
                Err(_) => {
                    emit_event(&app, &path, &id, "system", "Could not read process output.");
                    break;
                }
            }
        }
    });
}

#[tauri::command]
pub fn list_workflow_runs(app: AppHandle) -> Result<Vec<RunRecord>, String> {
    let path = history_path(&app)?;
    let mut state = store()
        .lock()
        .map_err(|_| "Could not access workflow runs.".to_string())?;
    if state.records.is_empty() {
        state.records = load_records(&path)?;
    }
    Ok(state.records.clone())
}

#[tauri::command]
pub fn start_workflow_run(app: AppHandle, request: StartRequest) -> Result<RunRecord, String> {
    let (executable, cwd, host) = validate_request(&request)?;
    let canonical_key = mutation_scope(&cwd);
    let path = history_path(&app)?;
    let id = format!(
        "run-{}-{}",
        now_ms(),
        NEXT_ID.fetch_add(1, Ordering::Relaxed)
    );
    let redaction_values = std::env::vars_os()
        .filter_map(|(name, value)| {
            let name = name.to_string_lossy().to_ascii_lowercase();
            (name.contains("token")
                || name.contains("secret")
                || name.contains("password")
                || name.contains("credential")
                || name.contains("api_key")
                || name.contains("apikey")
                || name.ends_with("_auth"))
            .then(|| value.to_string_lossy().into_owned())
        })
        .chain(
            request
                .arguments
                .iter()
                .filter(|arg| {
                    arg.starts_with("ghp_")
                        || arg.starts_with("gho_")
                        || arg.starts_with("github_pat_")
                        || arg.starts_with("sk-")
                })
                .cloned(),
        )
        .collect::<Vec<_>>();
    let record = RunRecord {
        id: id.clone(),
        executable: executable.to_string_lossy().into_owned(),
        cwd: cwd.to_string_lossy().into_owned(),
        mutating: request.mutating,
        state: "queued".into(),
        started_at_ms: now_ms(),
        finished_at_ms: None,
        exit_code: None,
        next_sequence: 1,
        events: Vec::new(),
    };
    {
        let mut state = store()
            .lock()
            .map_err(|_| "Could not access workflow runs.".to_string())?;
        if state.records.is_empty() {
            state.records = load_records(&path)?;
        }
        if request.mutating && !state.mutating_paths.insert(canonical_key.clone()) {
            return Err(
                "A mutating workflow is already active for this repository or worktree.".into(),
            );
        }
        state.records.insert(0, record.clone());
        state.records.truncate(MAX_RUNS);
        state.redaction_values.insert(id.clone(), redaction_values);
        if let Err(error) = persist_records(&path, &state.records) {
            state.records.retain(|entry| entry.id != id);
            state.redaction_values.remove(&id);
            if request.mutating {
                state.mutating_paths.remove(&canonical_key);
            }
            return Err(error);
        }
    }
    let mut command = match command_for(&executable, &cwd, &host, &request.arguments) {
        Ok(command) => command,
        Err(error) => {
            set_state(&app, &path, &id, "failed", None);
            return Err(error);
        }
    };
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) => {
            set_state(&app, &path, &id, "failed", None);
            if request.mutating {
                if let Ok(mut state) = store().lock() {
                    state.mutating_paths.remove(&canonical_key);
                }
            }
            return Err(format!("Could not start configured executable: {error}"));
        }
    };
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let child = Arc::new(Mutex::new(child));
    if let Ok(mut state) = store().lock() {
        state.active.insert(id.clone(), child.clone());
    }
    if let Some(stdout) = stdout {
        pump_output(app.clone(), path.clone(), id.clone(), "stdout", stdout);
    }
    if let Some(stderr) = stderr {
        pump_output(app.clone(), path.clone(), id.clone(), "stderr", stderr);
    }
    set_state(&app, &path, &id, "running", None);
    let worker_app = app.clone();
    let worker_id = id.clone();
    thread::spawn(move || {
        let started = Instant::now();
        let outcome = loop {
            if started.elapsed() >= Duration::from_secs(request.timeout_seconds) {
                if let Ok(mut child) = child.lock() {
                    terminate_process_tree(&mut child);
                }
                break "timedOut";
            }
            let result = child
                .lock()
                .map_err(|_| ())
                .and_then(|mut process| process.try_wait().map_err(|_| ()));
            match result {
                Ok(Some(status)) => {
                    let was_cancelled = store()
                        .lock()
                        .map(|mut state| state.cancelled.remove(&worker_id))
                        .unwrap_or(false);
                    if was_cancelled {
                        set_state(&worker_app, &path, &worker_id, "cancelled", status.code());
                        break "cancelled";
                    }
                    if status.success() {
                        set_state(&worker_app, &path, &worker_id, "completed", status.code());
                        break "completed";
                    }
                    set_state(&worker_app, &path, &worker_id, "failed", status.code());
                    break "failed";
                }
                Ok(None) => thread::sleep(POLL_INTERVAL),
                Err(()) => {
                    set_state(&worker_app, &path, &worker_id, "failed", None);
                    break "failed";
                }
            }
        };
        if outcome == "timedOut" {
            set_state(&worker_app, &path, &worker_id, outcome, None);
        }
        if let Ok(mut state) = store().lock() {
            state.active.remove(&worker_id);
            state.output_bytes.remove(&worker_id);
            state.cancelled.remove(&worker_id);
            state.redaction_values.remove(&worker_id);
            if request.mutating {
                state.mutating_paths.remove(&canonical_key);
            }
        }
    });
    Ok(record)
}

fn terminate_process_tree(child: &mut Child) {
    #[cfg(windows)]
    {
        let id = child.id().to_string();
        let _ = Command::new("taskkill.exe")
            .args(["/pid", id.as_str(), "/t", "/f"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
    let _ = child.kill();
}

#[tauri::command]
pub fn cancel_workflow_run(app: AppHandle, run_id: String) -> Result<bool, String> {
    let child = store()
        .lock()
        .map_err(|_| "Could not access workflow runs.".to_string())?
        .active
        .get(&run_id)
        .cloned();
    let Some(child) = child else { return Ok(false) };
    let mut process = child
        .lock()
        .map_err(|_| "Could not access the running process.".to_string())?;
    if process
        .try_wait()
        .map_err(|_| "Could not inspect the running process.".to_string())?
        .is_some()
    {
        return Ok(false);
    }
    {
        let mut state = store()
            .lock()
            .map_err(|_| "Could not access workflow runs.".to_string())?;
        if !state.active.contains_key(&run_id) {
            return Ok(false);
        }
        state.cancelled.insert(run_id.clone());
        let snapshot = state
            .records
            .iter_mut()
            .find(|record| record.id == run_id)
            .map(|record| {
                record.state = "cancelling".into();
                record.clone()
            });
        persist_records(&history_path(&app)?, &state.records)?;
        if let Some(snapshot) = snapshot {
            let _ = app.emit("workflow-run-updated", snapshot);
        }
    }
    terminate_process_tree(&mut process);
    Ok(true)
}

#[tauri::command]
pub fn mark_workflow_run_needs_input(app: AppHandle, run_id: String) -> Result<bool, String> {
    let child = store()
        .lock()
        .map_err(|_| "Could not access workflow runs.".to_string())?
        .active
        .get(&run_id)
        .cloned();
    let Some(child) = child else { return Ok(false) };
    let mut process = child
        .lock()
        .map_err(|_| "Could not access the running process.".to_string())?;
    if process
        .try_wait()
        .map_err(|_| "Could not inspect the running process.".to_string())?
        .is_some()
    {
        return Ok(false);
    }
    set_state(&app, &history_path(&app)?, &run_id, "needsInput", None);
    Ok(true)
}

#[tauri::command]
pub fn write_workflow_run_input(
    app: AppHandle,
    run_id: String,
    input: String,
) -> Result<bool, String> {
    if input.len() > 64 * 1024 || input.contains('\0') {
        return Err("The input is invalid or too large.".into());
    }
    let child = store()
        .lock()
        .map_err(|_| "Could not access workflow runs.".to_string())?
        .active
        .get(&run_id)
        .cloned();
    let Some(child) = child else { return Ok(false) };
    let needs_input = store()
        .lock()
        .map_err(|_| "Could not access workflow runs.".to_string())?
        .records
        .iter()
        .find(|record| record.id == run_id)
        .is_some_and(|record| record.state == "needsInput");
    if !needs_input {
        return Err("Mark the running process as waiting for input first.".into());
    }
    let mut process = child
        .lock()
        .map_err(|_| "Could not access the running process.".to_string())?;
    let Some(stdin) = process.stdin.as_mut() else {
        return Ok(false);
    };
    stdin
        .write_all(input.as_bytes())
        .and_then(|_| stdin.flush())
        .map_err(|_| "Could not send input to the process.".to_string())?;
    let path = history_path(&app)?;
    set_state(&app, &path, &run_id, "running", None);
    emit_event(&app, &path, &run_id, "inputSent", "Input sent.");
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(executable: &str, cwd: &str, args: Vec<&str>) -> StartRequest {
        StartRequest {
            executable: executable.into(),
            cwd: cwd.into(),
            arguments: args.into_iter().map(str::to_owned).collect(),
            timeout_seconds: 30,
            mutating: false,
        }
    }

    #[test]
    fn rejects_invalid_cwd_timeout_and_argument_arrays() {
        assert!(validate_request(&request("echo", "relative", vec![])).is_err());
        let mut invalid = request("echo", env!("CARGO_MANIFEST_DIR"), vec![]);
        invalid.timeout_seconds = 0;
        assert!(validate_request(&invalid).is_err());
        let invalid = request("echo", env!("CARGO_MANIFEST_DIR"), vec!["bad\0argument"]);
        assert!(validate_request(&invalid).is_err());
        let mut invalid = request("echo", env!("CARGO_MANIFEST_DIR"), vec![]);
        invalid.arguments = vec!["x".repeat(MAX_COMMAND_BYTES + 1)];
        assert!(validate_request(&invalid).is_err());
    }

    #[test]
    fn history_survives_restart_and_orphans_become_interrupted() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join(HISTORY_FILE);
        let mut record = RunRecord {
            id: "fixture".into(),
            executable: "test".into(),
            cwd: directory.path().display().to_string(),
            mutating: true,
            state: "running".into(),
            started_at_ms: 1,
            finished_at_ms: None,
            exit_code: None,
            next_sequence: 1,
            events: vec![],
        };
        persist_records(&path, &[record.clone()]).unwrap();
        record.state = "interrupted".into();
        let restored = load_records(&path).unwrap();
        assert_eq!(restored[0].state, record.state);
        assert!(restored[0].finished_at_ms.is_some());
    }

    #[test]
    fn redacts_github_token_shapes_from_output() {
        let value = redact("token=ghp_0123456789abcdefghijklmnop", &[]);
        assert!(!value.contains("ghp_"));
        assert!(value.contains("[REDACTED]"));
    }

    #[cfg(windows)]
    #[test]
    fn script_launchers_preserve_paths_and_arguments_with_spaces() {
        let fixture = tempfile::tempdir().unwrap();
        let folder = fixture.path().join("Fixture Folder");
        fs::create_dir_all(&folder).unwrap();
        let cmd_file = folder.join("run file.cmd");
        fs::write(&cmd_file, "@echo off\necho %~1\necho %~2\n").unwrap();
        let mut cmd = command_for(
            &cmd_file,
            &folder,
            "cmd",
            &["first arg".into(), "second arg".into()],
        )
        .unwrap();
        let output = cmd.output().unwrap();
        assert!(
            output.status.success(),
            "{cmd:?}\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let text = String::from_utf8_lossy(&output.stdout);
        assert!(text.contains("first arg"), "{text}");
        assert!(text.contains("second arg"), "{text}");

        let ps_file = folder.join("run file.ps1");
        fs::write(
            &ps_file,
            "param($One, $Two)\nWrite-Output $One\nWrite-Output $Two\n",
        )
        .unwrap();
        let mut powershell = command_for(
            &ps_file,
            &folder,
            "powershell",
            &["first arg".into(), "second arg".into()],
        )
        .unwrap();
        let output = powershell.output().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let text = String::from_utf8_lossy(&output.stdout);
        assert!(text.contains("first arg"), "{text}");
        assert!(text.contains("second arg"), "{text}");
    }
}
