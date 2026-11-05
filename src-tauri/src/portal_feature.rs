use portal_client::lifecycle::{HostConnection, PrivatePin, SharedPortal};
use portal_client::Cancellation;
use serde::Deserialize;
use serde_json::{json, Value};

use std::collections::{BTreeMap, HashMap};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};
use tauri::State;

const MAX_PROMPT: usize = 8_000;
const MAX_OUTPUT: usize = 65_536;
const MAX_HELPER_BYTES: usize = 16 * 1024 * 1024;
const MAX_TIMEOUT: Duration = Duration::from_secs(180);
const REVIEWED_RUN_ID: &str = "fabrica.portal-extract";
const APP_HELPER_SOURCE_MANIFEST: &str = include_str!("../portal-helper-source.json");
const PYTHON_SHIM: &str = "import runpy,sys; p=sys.argv[1]; sys.path.insert(0,p.rsplit('\\\\',1)[0]); sys.argv=[p,*sys.argv[2:]]; runpy.run_path(p,run_name='__main__')";

#[cfg(feature = "portal-fixtures")]
const FIXTURE_SHIM: &str = r#"import importlib,importlib.util,sys,json
from pathlib import Path
p=Path(sys.argv[1]).resolve(); root=Path(sys.argv[2]).resolve(); sys.path.insert(0,str(p.parent))
spec=importlib.util.spec_from_file_location("_fabrica_test_helper",p); helper=importlib.util.module_from_spec(spec); sys.modules[spec.name]=helper; spec.loader.exec_module(helper)
assert helper.ROOT.resolve()==(root/"fabrica-helper").resolve(); helper._fence.default_root=lambda:root/"accounting-authority"
args=sys.argv[3:]; profile=Path(args[args.index("--portal-profile")+1]); document=json.loads(profile.read_text(encoding="utf-8"))
fixture_target="credential-store:Navis-Portal/Fabrica/fixture"
fixture_app_key="synthetic-fabrica-portal-app-key-01234567890123456789"
if document.get("app_key_ref")!=fixture_target: raise RuntimeError("FIXTURE_CREDENTIAL_TARGET_INVALID")
def fixture_credential(reference):
    if reference!=fixture_target: raise helper.Refusal("Fixture credential target refused")
    return fixture_app_key
helper.credential=fixture_credential
release=Path(document["portal_release_root"]).resolve(); sys.path.insert(0,str(release))
native=importlib.import_module("portal_gateway.host_native"); native.canonical_root=lambda:root
sys.argv=[str(p),*args]; raise SystemExit(helper.main())
"#;
#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct FilePin {
    path: PathBuf,
    sha256: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ReviewedProfile {
    schema: u32,
    contract: String,
    run_id: String,
    runtime: FilePin,
    helper: FilePin,
    config: FilePin,
    feature_profile: FilePin,
    helper_inputs: BTreeMap<PathBuf, String>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct HelperSourceManifest {
    schema: u32,
    encoding: String,
    line_endings: String,
    files: BTreeMap<String, String>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct HelperResult {
    complete: bool,
    billing_state: String,
    #[serde(default)]
    text: Option<String>,
    #[serde(default, rename = "code")]
    _code: Option<String>,
    #[serde(default)]
    call_id: Option<String>,
    #[serde(default)]
    operation_id: Option<String>,
    #[serde(default)]
    portal_correlation: Option<String>,
    #[serde(default)]
    accounting_owner: Option<String>,
    #[serde(default, rename = "cost_usd")]
    _cost_usd: Option<Value>,
    #[serde(default)]
    usage: Option<Usage>,
    #[serde(default, rename = "daily_totals")]
    _daily_totals: Option<serde::de::IgnoredAny>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Usage {
    reported_cost_usd: Option<String>,
    input_tokens: Option<u64>,
    output_tokens: Option<u64>,
}
#[derive(Clone, Default)]
pub struct PortalFeatureState {
    operations: Arc<Mutex<HashMap<String, Cancellation>>>,
}
impl PortalFeatureState {
    fn register(&self, operation_id: &str) -> Result<Cancellation, &'static str> {
        if !id(operation_id) {
            return Err("denied");
        }
        let mut operations = self.operations.lock().map_err(|_| "unavailable")?;
        if operations.contains_key(operation_id) {
            return Err("unknown");
        }
        let cancel = Cancellation::default();
        operations.insert(operation_id.to_owned(), cancel.clone());
        Ok(cancel)
    }

    fn cancel(&self, operation_id: &str) -> Result<(), &'static str> {
        if !id(operation_id) {
            return Err("denied");
        }
        if let Some(cancel) = self
            .operations
            .lock()
            .map_err(|_| "unavailable")?
            .get(operation_id)
        {
            cancel.cancel();
        }
        Ok(())
    }

    pub(crate) fn cancel_all(&self) {
        if let Ok(operations) = self.operations.lock() {
            for cancel in operations.values() {
                cancel.cancel();
            }
        }
    }

    fn finish(&self, operation_id: &str) {
        if let Ok(mut operations) = self.operations.lock() {
            operations.remove(operation_id);
        }
    }
}
#[derive(Clone)]
struct FeatureBackend {
    portal: SharedPortal,
    profile_path: Option<PathBuf>,
    fixture_root: Option<PathBuf>,
}
impl FeatureBackend {
    fn registered() -> Self {
        Self {
            portal: SharedPortal::registered(),
            profile_path: None,
            fixture_root: None,
        }
    }
    #[cfg(feature = "portal-fixtures")]
    fn fixture(root: PathBuf, worker: PathBuf, worker_sha256: String) -> Self {
        Self {
            portal: SharedPortal::fixture(root.clone(), worker, worker_sha256),
            profile_path: Some(root.join("launches/fabrica-extract.json")),
            fixture_root: Some(root),
        }
    }
}

#[cfg(feature = "portal-fixtures")]
#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum FixtureCancelMode {
    #[default]
    None,
    Operation,
    Exit,
}

fn id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value.as_bytes()[0].is_ascii_alphanumeric()
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._:-".contains(&b))
}
fn sha(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
fn within_root(root: &Path, path: &Path) -> bool {
    path.is_absolute()
        && path.starts_with(root)
        && path != root
        && !path.components().any(|part| {
            matches!(
                part,
                std::path::Component::CurDir | std::path::Component::ParentDir
            )
        })
}
fn pin_file(
    spec: &FilePin,
    root: &Path,
    limit: usize,
    fixture_root: Option<&Path>,
) -> Result<PrivatePin, &'static str> {
    if !within_root(root, &spec.path) || !sha(&spec.sha256) {
        return Err("denied");
    }
    #[cfg(feature = "portal-fixtures")]
    if let Some(fixture) = fixture_root {
        return portal_client::lifecycle::pin_private_fixture(
            fixture,
            &spec.path,
            Some(&spec.sha256),
            limit,
        )
        .map_err(|_| "unavailable");
    }
    #[cfg(not(feature = "portal-fixtures"))]
    if fixture_root.is_some() {
        return Err("denied");
    }
    portal_client::lifecycle::pin_private(&spec.path, Some(&spec.sha256), limit)
        .map_err(|_| "unavailable")
}
fn load_profile(
    test_profile: Option<&Path>,
    fixture_root: Option<&Path>,
) -> Result<(ReviewedProfile, Vec<PrivatePin>, PathBuf), &'static str> {
    let root = fixture_root
        .map(Path::to_path_buf)
        .map(Ok)
        .unwrap_or_else(portal_client::lifecycle::registered_root)
        .map_err(|_| "unavailable")?;
    let path = test_profile
        .map(Path::to_path_buf)
        .unwrap_or(root.join("launches/fabrica-extract.json"));
    if path != root.join("launches/fabrica-extract.json") {
        return Err("denied");
    }
    if !path.exists() {
        return Err("disabled");
    }
    #[cfg(feature = "portal-fixtures")]
    let profile_pin = if fixture_root.is_some() {
        portal_client::lifecycle::pin_private_fixture(&root, &path, None, 16_384)
    } else {
        portal_client::lifecycle::pin_private(&path, None, 16_384)
    }
    .map_err(|_| "unavailable")?;
    #[cfg(not(feature = "portal-fixtures"))]
    let profile_pin =
        portal_client::lifecycle::pin_private(&path, None, 16_384).map_err(|_| "unavailable")?;
    if !portal_client::lifecycle::validate_unique_json(profile_pin.bytes()) {
        return Err("denied");
    }
    let profile: ReviewedProfile =
        serde_json::from_slice(profile_pin.bytes()).map_err(|_| "denied")?;
    if profile.schema != 1
        || profile.contract != "fabrica.portal-extract/1"
        || profile.run_id != REVIEWED_RUN_ID
        || profile.helper_inputs.len() != 4
    {
        return Err("denied");
    }
    verify_helper_sources(
        APP_HELPER_SOURCE_MANIFEST.as_bytes(),
        &root,
        &profile.helper_inputs,
    )?;
    let mut pins = vec![profile_pin];
    #[cfg(feature = "portal-fixtures")]
    let registered = if let Some(fixture) = fixture_root {
        portal_client::lifecycle::registered_runtime_fixture(fixture)
    } else {
        portal_client::lifecycle::registered_runtime()
    }
    .map_err(|_| "unavailable")?;
    #[cfg(not(feature = "portal-fixtures"))]
    let registered = portal_client::lifecycle::registered_runtime().map_err(|_| "unavailable")?;
    if profile.runtime.path != registered.path() || profile.runtime.sha256 != registered.sha256() {
        return Err("denied");
    }
    pins.push(
        portal_client::lifecycle::pin_registered_runtime(registered).map_err(|_| "unavailable")?,
    );
    let scripts = root.join("fabrica-helper/scripts");
    if profile.helper.path != scripts.join("model_helper.py")
        || profile.config.path != root.join("helper-config.json")
        || profile.feature_profile.path != root.join("portal-state/fabrica-extract.json")
    {
        return Err("denied");
    }
    pins.push(pin_file(&profile.helper, &root, 1_048_576, fixture_root)?);
    pins.push(pin_file(&profile.config, &root, 65_536, fixture_root)?);
    pins.push(pin_file(
        &profile.feature_profile,
        &root,
        65_536,
        fixture_root,
    )?);
    let required = [
        scripts.join("model_helper.py"),
        scripts.join("ledger_fence.py"),
        scripts.join("portal_private_files.py"),
        scripts.join("portal_bridge.py"),
    ];
    if required
        .iter()
        .any(|path| !profile.helper_inputs.contains_key(path))
    {
        return Err("denied");
    }
    let mut total = 0usize;
    for (path, expected) in &profile.helper_inputs {
        if !within_root(&root, path) || !sha(expected) {
            return Err("denied");
        }
        let pin = pin_file(
            &FilePin {
                path: path.clone(),
                sha256: expected.clone(),
            },
            &root,
            MAX_HELPER_BYTES,
            fixture_root,
        )?;
        total = total.checked_add(pin.bytes().len()).ok_or("denied")?;
        if total > MAX_HELPER_BYTES {
            return Err("denied");
        }
        pins.push(pin)
    }
    Ok((profile, pins, root))
}
fn verify_helper_sources(
    raw: &[u8],
    root: &Path,
    inputs: &BTreeMap<PathBuf, String>,
) -> Result<(), &'static str> {
    if !portal_client::lifecycle::validate_unique_json(raw) {
        return Err("denied");
    }
    let manifest: HelperSourceManifest = serde_json::from_slice(raw).map_err(|_| "denied")?;
    let required = [
        "fabrica-helper/scripts/model_helper.py",
        "fabrica-helper/scripts/portal_bridge.py",
        "fabrica-helper/scripts/ledger_fence.py",
        "fabrica-helper/scripts/portal_private_files.py",
    ];
    if manifest.schema != 1
        || manifest.files.len() != required.len()
        || manifest.encoding != "utf-8"
        || manifest.line_endings != "LF"
        || inputs.len() != required.len()
        || required
            .iter()
            .any(|path| !manifest.files.contains_key(*path))
    {
        return Err("denied");
    }
    for (relative, hash) in manifest.files {
        if !required.contains(&relative.as_str())
            || !sha(&hash)
            || inputs.get(&root.join(&relative)) != Some(&hash)
        {
            return Err("denied");
        }
    }
    Ok(())
}
fn valid_cost(cost: &str) -> bool {
    if cost.is_empty() || cost.len() > 32 {
        return false;
    }
    let mut parts = cost.split('.');
    let whole = parts.next().unwrap_or("");
    let fraction = parts.next();
    if parts.next().is_some()
        || whole.is_empty()
        || whole.len() > 18
        || !whole.bytes().all(|b| b.is_ascii_digit())
        || whole.len() > 1 && whole.starts_with('0')
    {
        return false;
    }
    fraction.map_or(true, |digits| {
        !digits.is_empty() && digits.len() <= 12 && digits.bytes().all(|b| b.is_ascii_digit())
    })
}
fn helper_exit_failure(code: Option<i32>) -> &'static str {
    #[cfg(feature = "portal-fixtures")]
    if code == Some(2) {
        return "helper_exit_2";
    }
    #[cfg(not(feature = "portal-fixtures"))]
    let _ = code;
    "unknown"
}
fn sanitize_result(raw: &[u8], operation_id: &str) -> Result<Value, &'static str> {
    if raw.len() > MAX_OUTPUT || !portal_client::lifecycle::validate_unique_json(raw) {
        return Err("json");
    }
    let result: HelperResult = serde_json::from_slice(raw).map_err(|_| "schema")?;
    if result.billing_state == "not_sent" {
        return Ok(
            json!({"status":"denied","text":null,"usage":{"state":"denied","cost":null,"inputTokens":null,"outputTokens":null}}),
        );
    }
    if result.billing_state != "settled" {
        return Err("billing");
    }
    if result.operation_id.as_deref() != Some(operation_id)
        || result.call_id.as_deref().is_none_or(|value| !id(value))
        || result
            .portal_correlation
            .as_deref()
            .is_none_or(|value| !id(value))
        || result.accounting_owner.as_deref() != Some("portal-provider/fabrica-cap-mirror")
    {
        return Err("binding");
    }
    let usage = result.usage.ok_or("usage")?;
    let cost = usage
        .reported_cost_usd
        .filter(|value| valid_cost(value))
        .ok_or("usage")?;
    let input = usage
        .input_tokens
        .filter(|value| *value <= i64::MAX as u64)
        .ok_or("usage")?;
    let output = usage
        .output_tokens
        .filter(|value| *value <= i64::MAX as u64)
        .ok_or("usage")?;
    let status = if result.complete {
        "complete"
    } else {
        "partial"
    };
    let text = result.text.filter(|value| value.len() <= MAX_OUTPUT);
    Ok(
        json!({"status":status,"text":text,"usage":{"state":"known","cost":cost,"inputTokens":input,"outputTokens":output}}),
    )
}
fn run_owned(
    mut child: Child,
    input: Vec<u8>,
    cancel: &Cancellation,
    timeout: Duration,
) -> Result<Vec<u8>, &'static str> {
    let Some(mut stdout) = child.stdout.take() else {
        let _ = child.kill();
        let _ = child.wait();
        return Err("unavailable");
    };
    let Some(mut stdin) = child.stdin.take() else {
        let _ = child.kill();
        let _ = child.wait();
        return Err("unavailable");
    };
    let reader = thread::spawn(move || {
        let mut bytes = Vec::new();
        let _ = stdout
            .by_ref()
            .take((MAX_OUTPUT + 1) as u64)
            .read_to_end(&mut bytes);
        bytes
    });
    let writer = thread::spawn(move || stdin.write_all(&input));
    let started = Instant::now();
    let exit = loop {
        if cancel.is_cancelled() {
            let _ = child.kill();
            let _ = child.wait();
            let _ = reader.join();
            let _ = writer.join();
            return Err("cancelled");
        }
        if started.elapsed() > timeout {
            let _ = child.kill();
            let _ = child.wait();
            let _ = reader.join();
            let _ = writer.join();
            return Err("unknown");
        }
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {}
            Err(_) => {
                let _ = child.kill();
                let _ = child.wait();
                let _ = reader.join();
                let _ = writer.join();
                return Err("unknown");
            }
        }
        if reader.is_finished() {
            let out = match reader.join() {
                Ok(bytes) => bytes,
                Err(_) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    let _ = writer.join();
                    return Err("unknown");
                }
            };
            if out.len() > MAX_OUTPUT {
                let _ = child.kill();
                let _ = child.wait();
                let _ = writer.join();
                return Err("unknown");
            }
            let mut status = None;
            while started.elapsed() <= timeout && !cancel.is_cancelled() {
                match child.try_wait() {
                    Ok(Some(value)) => {
                        status = Some(value);
                        break;
                    }
                    Ok(None) => thread::sleep(Duration::from_millis(10)),
                    Err(_) => break,
                }
            }
            if cancel.is_cancelled() {
                let _ = child.kill();
                let _ = child.wait();
                let _ = writer.join();
                return Err("cancelled");
            }
            let status = match status {
                Some(value) => value,
                None => {
                    let _ = child.kill();
                    let _ = child.wait();
                    let _ = writer.join();
                    return Err("unknown");
                }
            };
            if !matches!(status.code(), Some(0) | Some(3)) {
                let _ = writer.join();
                return Err(helper_exit_failure(status.code()));
            }
            if writer.join().map_or(true, |result| result.is_err()) {
                return Err("unknown");
            }
            return Ok(out);
        }
        thread::sleep(Duration::from_millis(10));
    };
    let out = match reader.join() {
        Ok(bytes) => bytes,
        Err(_) => return Err("unknown"),
    };
    if out.len() > MAX_OUTPUT {
        return Err("unknown");
    }
    if !matches!(exit.code(), Some(0) | Some(3)) {
        let _ = writer.join();
        return Err(helper_exit_failure(exit.code()));
    }
    if writer.join().map_or(true, |result| result.is_err()) {
        return Err("unknown");
    }
    Ok(out)
}
async fn run_feature(
    backend: FeatureBackend,
    prompt: String,
    operation_id: String,
    cancel: Cancellation,
) -> Result<Value, &'static str> {
    let (profile, pins, root) = load_profile(
        backend.profile_path.as_deref(),
        backend.fixture_root.as_deref(),
    )?;
    let host = backend
        .portal
        .acquire(Duration::from_secs(30), &cancel)
        .await
        .map_err(|_| {
            if cancel.is_cancelled() {
                "cancelled"
            } else {
                "unavailable"
            }
        })?;
    let generation = host.generation().ok_or("unavailable")?.to_owned();
    if host.state() != "running" {
        return Err("unavailable");
    }
    if host.receipt()["readiness"]["ready"] != true {
        return Err("disabled");
    }
    let fixture_root = backend.fixture_root.clone();
    tauri::async_runtime::spawn_blocking(move || {
        execute(
            profile,
            pins,
            root,
            prompt,
            operation_id,
            generation,
            cancel,
            host,
            fixture_root,
        )
    })
    .await
    .map_err(|_| "unknown")?
}

async fn run_registered_feature(
    state: &PortalFeatureState,
    backend: FeatureBackend,
    prompt: String,
    operation_id: String,
) -> Result<Value, &'static str> {
    let cancel = state.register(&operation_id)?;
    let result = run_feature(backend, prompt, operation_id.clone(), cancel).await;
    state.finish(&operation_id);
    result
}

#[cfg(feature = "portal-fixtures")]
pub async fn run_fixture(
    root: PathBuf,
    worker: PathBuf,
    worker_sha256: String,
    prompt: String,
    operation_id: String,
) -> Result<Value, String> {
    run_fixture_with_cancel(
        root,
        worker,
        worker_sha256,
        prompt,
        operation_id,
        FixtureCancelMode::None,
    )
    .await
}

#[cfg(feature = "portal-fixtures")]
pub async fn run_fixture_with_cancel(
    root: PathBuf,
    worker: PathBuf,
    worker_sha256: String,
    prompt: String,
    operation_id: String,
    cancel_mode: FixtureCancelMode,
) -> Result<Value, String> {
    if prompt.trim().is_empty()
        || prompt.len() > MAX_PROMPT
        || !id(&operation_id)
        || !sha(&worker_sha256)
    {
        return Err("denied".into());
    }
    let backend = FeatureBackend::fixture(root.clone(), worker, worker_sha256);
    let state = PortalFeatureState::default();
    let cancel = state
        .register(&operation_id)
        .map_err(str::to_owned)?;
    let finished = Arc::new(AtomicBool::new(false));
    let dispatch_path = root.join("dispatch.flag");
    let watcher = if cancel_mode == FixtureCancelMode::None {
        None
    } else {
        match std::fs::symlink_metadata(&dispatch_path) {
            Ok(_) => {
                state.finish(&operation_id);
                return Err("fixture_dispatch_marker_must_be_new".into());
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => {
                state.finish(&operation_id);
                return Err("fixture_dispatch_marker_unavailable".into());
            }
        }
        let watcher_state = state.clone();
        let operation = operation_id.clone();
        let watcher_finished = finished.clone();
        let spawned = thread::Builder::new()
            .name("portal-fixture-cancel".into())
            .spawn(move || {
                let deadline = Instant::now() + Duration::from_secs(30);
                loop {
                    if watcher_finished.load(Ordering::SeqCst) {
                        return false;
                    }
                    if std::fs::symlink_metadata(&dispatch_path).is_ok() {
                        let marker = portal_client::lifecycle::pin_private_fixture(
                            &root,
                            &dispatch_path,
                            None,
                            64,
                        );
                        if marker.map(|pin| pin.bytes() == b"unknown").unwrap_or(false) {
                            let result = match cancel_mode {
                                FixtureCancelMode::Operation => watcher_state.cancel(&operation),
                                FixtureCancelMode::Exit => {
                                    watcher_state.cancel_all();
                                    Ok(())
                                }
                                FixtureCancelMode::None => Ok(()),
                            };
                            return result.is_ok();
                        }
                        return false;
                    }
                    if Instant::now() >= deadline {
                        let _ = watcher_state.cancel(&operation);
                        return false;
                    }
                    thread::sleep(Duration::from_millis(10));
                }
        });
        match spawned {
            Ok(watcher) => Some(watcher),
            Err(_) => {
                state.finish(&operation_id);
                return Err("fixture_cancel_signal_unavailable".into());
            }
        }
    };
    let result = run_feature(backend, prompt, operation_id.clone(), cancel).await;
    finished.store(true, Ordering::SeqCst);
    state.finish(&operation_id);
    if let Some(watcher) = watcher {
        match watcher.join() {
            Ok(true) => {}
            Ok(false) => return Err("fixture_cancel_signal_missing".into()),
            Err(_) => return Err("fixture_cancel_signal_unavailable".into()),
        }
    }
    result.map_err(str::to_owned)
}
#[tauri::command]
pub async fn start_portal_extract(
    prompt: String,
    operation_id: String,
    state: State<'_, PortalFeatureState>,
) -> Result<Value, String> {
    if prompt.trim().is_empty() || prompt.len() > MAX_PROMPT || !id(&operation_id) {
        return Err("denied".into());
    }
    let result = run_registered_feature(
        &state,
        FeatureBackend::registered(),
        prompt,
        operation_id,
    )
    .await;
    result.map_err(str::to_owned)
}
#[tauri::command]
pub fn cancel_portal_extract(
    operation_id: String,
    state: State<'_, PortalFeatureState>,
) -> Result<(), String> {
    if !id(&operation_id) {
        return Err("denied".into());
    }
    state.cancel(&operation_id).map_err(str::to_owned)
}
fn execute(
    profile: ReviewedProfile,
    _pins: Vec<PrivatePin>,
    root: PathBuf,
    prompt: String,
    operation_id: String,
    generation: String,
    cancel: Cancellation,
    _host: HostConnection,
    fixture_root: Option<PathBuf>,
) -> Result<Value, &'static str> {
    #[cfg(not(feature = "portal-fixtures"))]
    let _ = &fixture_root;
    let mut command = Command::new(&profile.runtime.path);
    command.args(["-I", "-B", "-c"]);
    #[cfg(feature = "portal-fixtures")]
    if fixture_root.is_some() {
        command.arg(FIXTURE_SHIM);
    } else {
        command.arg(PYTHON_SHIM);
    }
    #[cfg(not(feature = "portal-fixtures"))]
    command.arg(PYTHON_SHIM);
    command.arg(&profile.helper.path);
    #[cfg(feature = "portal-fixtures")]
    if let Some(fixture) = fixture_root.as_ref() {
        command.arg(fixture);
    }
    command
        .args(["run", "--task", "portal-extract", "--config"])
        .arg(&profile.config.path)
        .args(["--portal-profile"])
        .arg(&profile.feature_profile.path)
        .args(["--portal-shared-host", "--portal-host-generation"])
        .arg(generation)
        .args(["--operation-id"])
        .arg(&operation_id)
        .args(["--run-id"])
        .arg(&profile.run_id)
        .current_dir(&root)
        .env_clear()
        .env("PYTHONNOUSERSITE", "1")
        .env("PYTHONDONTWRITEBYTECODE", "1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    for key in ["SystemRoot", "WINDIR", "TEMP", "TMP"] {
        if let Some(value) = std::env::var_os(key) {
            command.env(key, value);
        }
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x08000000);
    }
    if let Some(system) = std::env::var_os("SystemRoot") {
        command.env("PATH", PathBuf::from(system).join("System32"));
    }
    let child = command.spawn().map_err(|_| "unavailable")?;
    let raw = run_owned(child, prompt.into_bytes(), &cancel, MAX_TIMEOUT)?;
    if raw.is_empty() {
        return Err("unknown");
    }
    match sanitize_result(&raw, &operation_id) {
        Ok(result) => Ok(result),
        Err(reason) => {
            #[cfg(feature = "portal-fixtures")]
            if fixture_root.is_some() {
                return Err(match reason {
                    "json" => "helper_reply_invalid_json",
                    "schema" => "helper_reply_invalid_schema",
                    "billing" => "helper_reply_invalid_billing",
                    "binding" => "helper_reply_invalid_binding",
                    "usage" => "helper_reply_invalid_usage",
                    _ => "helper_reply_invalid",
                });
            }
            let _ = reason;
            Err("unknown")
        }
    }
}
#[cfg(all(test, feature = "portal-fixtures"))]
mod tests {
    use super::*;
    #[test]
    fn operation_registration_is_shared_and_cancel_is_scoped_by_id() {
        let state = PortalFeatureState::default();
        let first = state.register("op-first").unwrap();
        let second = state.register("op-second").unwrap();
        assert_eq!(state.register("op-first").err(), Some("unknown"));
        state.cancel("op-first").unwrap();
        assert!(first.is_cancelled());
        assert!(!second.is_cancelled());
        state.finish("op-first");
        assert!(state.register("op-first").is_ok());
    }

    #[test]
    fn app_exit_core_cancels_only_registered_feature_operations() {
        let state = PortalFeatureState::default();
        let first = state.register("op-first").unwrap();
        let second = state.register("op-second").unwrap();
        state.cancel_all();
        assert!(first.is_cancelled());
        assert!(second.is_cancelled());
        let operations = state.operations.lock().unwrap();
        assert_eq!(operations.len(), 2);
    }

    #[test]
    fn fixture_cancel_mode_accepts_only_fixed_actions() {
        assert_eq!(
            serde_json::from_str::<FixtureCancelMode>("\"none\"").unwrap(),
            FixtureCancelMode::None
        );
        assert_eq!(
            serde_json::from_str::<FixtureCancelMode>("\"operation\"").unwrap(),
            FixtureCancelMode::Operation
        );
        assert_eq!(
            serde_json::from_str::<FixtureCancelMode>("\"exit\"").unwrap(),
            FixtureCancelMode::Exit
        );
        assert!(serde_json::from_str::<FixtureCancelMode>("\"pid:1234\"").is_err());
    }

    #[test]
    fn renderer_ids_and_prompt_bounds_are_enforced() {
        assert!(!id("../bad"));
        assert!(!id(""));
        assert!(id("op-1"));
        assert!(MAX_PROMPT <= 8192)
    }
    #[test]
    fn compiled_helper_manifest_pins_exact_normalized_sources() {
        let manifest: HelperSourceManifest =
            serde_json::from_str(APP_HELPER_SOURCE_MANIFEST).unwrap();
        let root = Path::new(r"C:\fixture");
        let inputs = manifest
            .files
            .iter()
            .map(|(path, hash)| (root.join(path), hash.clone()))
            .collect();
        assert!(
            verify_helper_sources(APP_HELPER_SOURCE_MANIFEST.as_bytes(), root, &inputs).is_ok()
        );
        assert!(verify_helper_sources(
            br#"{"schema":1,"schema":1,"encoding":"utf-8","line_endings":"LF","files":{}}"#,
            root,
            &inputs
        )
        .is_err());
    }
    #[test]
    fn helper_result_is_allowlisted_and_operation_bound() {
        let good=br#"{"complete":true,"billing_state":"settled","call_id":"call-1","operation_id":"op-1","portal_correlation":"corr-1","accounting_owner":"portal-provider/fabrica-cap-mirror","text":"ok","usage":{"reported_cost_usd":"0.1","input_tokens":2,"output_tokens":1}}"#;
        let result = sanitize_result(good, "op-1").unwrap();
        assert_eq!(result["status"], "complete");
        assert!(sanitize_result(good, "op-2").is_err());
        let no_id = br#"{"complete":true,"billing_state":"settled","text":"secret"}"#;
        assert!(sanitize_result(no_id, "op-1").is_err());
        assert!(sanitize_result(b"{}", "op-1").is_err());
        assert!(valid_cost("0.01"));
        assert!(!valid_cost("01.00"));
        assert!(!valid_cost("1e4"))
    }
    #[test]
    fn unknown_helper_fields_and_oversize_output_fail_closed() {
        let bad = br#"{"complete":true,"billing_state":"settled","raw_error":"secret"}"#;
        assert!(sanitize_result(bad, "op-1").is_err());
        assert!(sanitize_result(&vec![b' '; MAX_OUTPUT + 1], "op-1").is_err());
        let uncertain = br#"{"complete":true,"billing_state":"unknown","text":"secret"}"#;
        assert!(sanitize_result(uncertain, "op-1").is_err())
    }
    #[test]
    fn settled_partial_is_known_and_not_sent_never_echoes_text() {
        let complete=br#"{"complete":true,"billing_state":"settled","call_id":"call-1","operation_id":"op:1","portal_correlation":"corr-1","accounting_owner":"portal-provider/fabrica-cap-mirror","text":"ok","usage":{"reported_cost_usd":"0.1","input_tokens":2,"output_tokens":1}}"#;
        let partial = std::str::from_utf8(complete)
            .unwrap()
            .replace("\"complete\":true", "\"complete\":false");
        let parsed = sanitize_result(partial.as_bytes(), "op:1").unwrap();
        assert_eq!(parsed["status"], "partial");
        assert_eq!(parsed["usage"]["state"], "known");
        let refused = br#"{"complete":false,"billing_state":"not_sent","text":"secret"}"#;
        let parsed = sanitize_result(refused, "op:1").unwrap();
        assert_eq!(parsed["status"], "denied");
        assert!(parsed["text"].is_null());
    }
}
