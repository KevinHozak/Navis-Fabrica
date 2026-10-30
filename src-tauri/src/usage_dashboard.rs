//! Typed, bounded native boundary for the metadata-only usage collector.
use serde::{Deserialize, Serialize};
use std::{
    fs,
    io::{Read, Write},
    path::{Path, PathBuf},
    process::Stdio,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    thread,
    time::{Duration, Instant},
};
use tauri::AppHandle;
static COLLECTION: Mutex<()> = Mutex::new(());
const LIMIT: usize = 2 * 1024 * 1024;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub schema_version: u32,
    pub codex_root: String,
    pub fabrica_databases: Vec<String>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Query {
    pub from_ms: u64,
    pub to_ms: u64,
    pub source: Option<String>,
    pub provider: Option<String>,
    pub model: Option<String>,
}
impl Query {
    fn validate(&self) -> Result<(), String> {
        if self.from_ms >= self.to_ms || self.to_ms - self.from_ms > 366 * 86400000 {
            return Err("Choose a UTC range of at most 366 days.".into());
        }
        for value in [&self.source, &self.provider, &self.model]
            .into_iter()
            .flatten()
        {
            if value.len() > 160 || value.chars().any(char::is_control) {
                return Err("Invalid usage filter.".into());
            }
        }
        Ok(())
    }
}
#[derive(Debug, Serialize, Deserialize)]
pub struct Tokens {
    input_tokens: Option<f64>,
    cached_input_tokens: Option<f64>,
    cache_write_input_tokens: Option<f64>,
    output_tokens: Option<f64>,
    reasoning_output_tokens: Option<f64>,
    total_tokens: Option<f64>,
}
#[derive(Debug, Serialize, Deserialize)]
pub struct Pricing {
    source: String,
    verified_on: String,
}
#[derive(Debug, Serialize, Deserialize)]
pub struct Group {
    source: String,
    provider: String,
    model: String,
    records: u64,
    tokens: Tokens,
    actual_usd: Option<f64>,
    actual_sources: Vec<String>,
    estimate_low_usd: Option<f64>,
    estimate_high_usd: Option<f64>,
    events: u64,
    unknown_cost_events: u64,
    unpriced_events: u64,
    cache_write_unknown_events: u64,
    elapsed_seconds: Option<f64>,
    active_seconds: Option<f64>,
    unknown_active_records: u64,
    partial_records: u64,
    pricing: Option<Pricing>,
}
#[derive(Debug, Serialize, Deserialize)]
pub struct Detail {
    id: String,
    source: String,
    provider: String,
    model: String,
    state: String,
    elapsed_seconds: Option<f64>,
    active_seconds: Option<f64>,
    actual_usd: Option<f64>,
    estimate_low_usd: Option<f64>,
    estimate_high_usd: Option<f64>,
    partial: bool,
}
#[derive(Debug, Serialize, Deserialize)]
pub struct Filters {
    sources: Vec<String>,
    providers: Vec<String>,
    models: Vec<String>,
}
#[derive(Debug, Serialize, Deserialize)]
pub struct Collection {
    at_ms: Option<u64>,
    errors: Vec<String>,
    imported_sessions: u64,
    native_databases_found: u64,
}
#[derive(Debug, Serialize, Deserialize)]
pub struct Report {
    schema_version: u32,
    collection: Collection,
    query: Query,
    groups: Vec<Group>,
    details: Vec<Detail>,
    detail_count: u64,
    details_truncated: bool,
    untimed_records: u64,
    filters: Filters,
}

fn config_path(app: &AppHandle) -> Result<PathBuf, String> {
    Ok(crate::storage::config_root(app)?.join("usage-dashboard-config.json"))
}
fn defaults() -> Config {
    Config {
        schema_version: 1,
        codex_root: std::env::var_os("CODEX_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("USERPROFILE").map(|v| PathBuf::from(v).join(".codex")))
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned(),
        fabrica_databases: vec![],
    }
}
fn read_config(app: &AppHandle) -> Result<Config, String> {
    let path = config_path(app)?;
    if !path.exists() {
        return Ok(defaults());
    }
    let mut bytes = Vec::new();
    fs::File::open(path)
        .map_err(|_| "Could not read usage source configuration.")?
        .take(16385)
        .read_to_end(&mut bytes)
        .map_err(|_| "Could not read usage source configuration.")?;
    if bytes.len() > 16384 {
        return Err("Usage source configuration exceeds limit.".into());
    }
    let value: Config =
        serde_json::from_slice(&bytes).map_err(|_| "Invalid usage source configuration.")?;
    if value.schema_version != 1 || value.fabrica_databases.len() > 8 {
        return Err("Unsupported usage source configuration.".into());
    }
    Ok(value)
}
fn verified_path(value: &str, directory: bool) -> Result<String, String> {
    let path = Path::new(value);
    if value.len() > 2048 || !path.is_absolute() {
        return Err("Usage sources require absolute paths.".into());
    }
    let path = path
        .canonicalize()
        .map_err(|_| "Usage source is unavailable; check the selected location.")?;
    if (directory && !path.is_dir()) || (!directory && !path.is_file()) {
        return Err("Usage source has the wrong file type.".into());
    }
    Ok(path.to_string_lossy().into_owned())
}
#[tauri::command]
pub async fn get_usage_dashboard_config(app: AppHandle) -> Result<Config, String> {
    read_config(&app)
}
#[tauri::command]
pub async fn save_usage_dashboard_config(app: AppHandle, config: Config) -> Result<Config, String> {
    let _lock = COLLECTION
        .try_lock()
        .map_err(|_| "Usage collection is busy. Try saving after it finishes.")?;
    if config.schema_version != 1 || config.fabrica_databases.len() > 8 {
        return Err("Unsupported usage source configuration.".into());
    }
    let config = Config {
        schema_version: 1,
        codex_root: if config.codex_root.trim().is_empty() {
            String::new()
        } else {
            verified_path(&config.codex_root, true)?
        },
        fabrica_databases: config
            .fabrica_databases
            .iter()
            .map(|v| verified_path(v, false))
            .collect::<Result<_, _>>()?,
    };
    let path = config_path(&app)?;
    fs::create_dir_all(path.parent().unwrap())
        .map_err(|_| "Could not create usage configuration folder.")?;
    let mut file = tempfile::NamedTempFile::new_in(path.parent().unwrap())
        .map_err(|_| "Could not save usage source configuration.")?;
    file.write_all(&serde_json::to_vec(&config).map_err(|_| "Invalid usage configuration.")?)
        .map_err(|_| "Could not save usage configuration.")?;
    file.persist(path)
        .map_err(|_| "Could not replace usage configuration.")?;
    Ok(config)
}
fn materialize(root: &Path) -> Result<PathBuf, String> {
    fs::create_dir_all(root).map_err(|_| "Could not create private usage tools folder.")?;
    for (name, source) in [
        (
            "usage_dashboard.py",
            include_str!("../../scripts/usage_dashboard.py"),
        ),
        (
            "model_usage_stats.py",
            include_str!("../../scripts/model_usage_stats.py"),
        ),
        (
            "usage_metrics.py",
            include_str!("../../scripts/usage_metrics.py"),
        ),
    ] {
        let path = root.join(name);
        if fs::read_to_string(&path).ok().as_deref() != Some(source) {
            fs::write(path, source).map_err(|_| "Could not prepare private usage collector.")?;
        }
    }
    Ok(root.join("usage_dashboard.py"))
}
fn collect(app: &AppHandle, query: Query, refresh: bool) -> Result<Report, String> {
    let _lock = COLLECTION
        .try_lock()
        .map_err(|_| "Usage collection is already running.")?;
    query.validate()?;
    let config = read_config(app)?;
    let data = crate::storage::data_root(app)?;
    let root = data.join("usage-dashboard");
    let script = materialize(&root.join("tools"))?;
    let mut databases = vec![
        data.join("workflow-runs.sqlite3"),
        data.join("runner/jobs.sqlite3"),
        data.join("runner/budgets.sqlite3"),
    ];
    databases.extend(config.fabrica_databases.iter().map(PathBuf::from));
    let pricing: serde_json::Value = serde_json::from_str(
        include_str!("../../config/model-pricing.json").trim_start_matches('\u{feff}'),
    )
    .map_err(|_| "Bundled pricing snapshot is invalid.")?;
    let request = serde_json::json!({"schema_version":1,"state_dir":root,"codex_root":config.codex_root,"fabrica_databases":databases,"pricing":pricing,"query":query,"refresh":refresh});
    let input = serde_json::to_vec(&request).map_err(|_| "Invalid usage request.")?;
    if input.len() > 65536 {
        return Err("Usage request exceeds limit.".into());
    }
    let mut child = crate::process::command("python")
        .arg(&script)
        .env("PYTHONUTF8", "1")
        .env("PYTHONIOENCODING", "utf-8")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|_| "Usage collection needs Python 3.10+ on PATH.")?;
    let mut stdin = child
        .stdin
        .take()
        .ok_or("Could not open usage collector input.")?;
    if stdin.write_all(&input).is_err() {
        crate::runs::terminate_process_tree(&mut child);
        let _ = child.wait();
        return Err("Could not send usage collector input.".into());
    }
    drop(stdin);
    let mut stdout = child
        .stdout
        .take()
        .ok_or("Could not read usage collector output.")?;
    let overflow = Arc::new(AtomicBool::new(false));
    let flag = overflow.clone();
    let reader = thread::spawn(move || {
        let mut bytes = Vec::new();
        let result = stdout
            .by_ref()
            .take((LIMIT + 1) as u64)
            .read_to_end(&mut bytes);
        if bytes.len() > LIMIT {
            flag.store(true, Ordering::SeqCst)
        }
        (result, bytes)
    });
    let started = Instant::now();
    let status = loop {
        if overflow.load(Ordering::SeqCst) || started.elapsed() > Duration::from_secs(55) {
            crate::runs::terminate_process_tree(&mut child);
            let _ = child.wait();
            let _ = reader.join();
            return Err("Usage collection exceeded its time or output limit. Retained data is preserved; retry refresh.".into());
        }
        if let Some(status) = child
            .try_wait()
            .map_err(|_| "Could not observe usage collection.")?
        {
            break status;
        }
        thread::sleep(Duration::from_millis(25));
    };
    let (read, bytes) = reader
        .join()
        .map_err(|_| "Could not read usage collection.")?;
    if read.is_err() || bytes.len() > LIMIT || !status.success() {
        return Err(
            "Usage collection failed. Check configured sources and private storage.".into(),
        );
    }
    let result: Report = serde_json::from_slice(&bytes)
        .map_err(|_| "Usage collector returned an incompatible schema.")?;
    if result.schema_version != 1 || result.groups.len() > 2000 || result.details.len() > 2000 {
        return Err("Usage response exceeds schema bounds.".into());
    }
    Ok(result)
}
#[tauri::command]
pub async fn get_usage_dashboard(
    app: AppHandle,
    query: Query,
    refresh: bool,
) -> Result<Report, String> {
    tauri::async_runtime::spawn_blocking(move || collect(&app, query, refresh))
        .await
        .map_err(|_| "Usage collection did not finish.".to_string())?
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rejects_unbounded_ranges_and_controls() {
        let mut q = Query {
            from_ms: 0,
            to_ms: 86400000,
            source: None,
            model: None,
            provider: None,
        };
        assert!(q.validate().is_ok());
        q.to_ms = 367 * 86400000;
        assert!(q.validate().is_err());
        q.to_ms = 1;
        q.model = Some("x\n".into());
        assert!(q.validate().is_err());
    }
    #[test]
    fn source_paths_are_explicit_and_verified() {
        let dir = tempfile::tempdir().unwrap();
        assert!(verified_path("relative", true).is_err());
        assert!(verified_path(dir.path().to_str().unwrap(), true).is_ok());
        assert!(verified_path(dir.path().to_str().unwrap(), false).is_err());
    }
}
