//! Trusted native lifecycle only. No provider key, inference or operator-control command.
use crate::{error, Cancellation, ClientError, PortalClient};
use serde::Deserialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::fs::File;
use std::future::Future;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::process::{Child, Command, Stdio};
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc, Mutex,
};
use std::task::{Context, Poll, Waker};
use std::time::{Duration, Instant};

#[cfg(windows)]
#[path = "host_windows.rs"]
pub(crate) mod native;
#[cfg(not(windows))]
mod native {
    use super::*;
    fn unsupported() -> ClientError {
        error("not_sent", "HOST_PLATFORM_UNSUPPORTED")
    }
    pub fn root() -> Result<PathBuf, ClientError> {
        Err(unsupported())
    }
    pub fn sid() -> Result<String, ClientError> {
        Err(unsupported())
    }
    pub fn private(_: &Path) -> Result<(), ClientError> {
        Err(unsupported())
    }
    pub fn directories(_: &Path) -> Result<Vec<File>, ClientError> {
        Err(unsupported())
    }
    pub fn file(_: &Path) -> Result<File, ClientError> {
        Err(unsupported())
    }
    pub fn pipe_available(_: &std::process::ChildStdout) -> Result<Option<u32>, ClientError> {
        Err(unsupported())
    }
    pub fn peer(_: &std::net::TcpStream, _: &Peer) -> Result<(), ClientError> {
        Err(unsupported())
    }
    pub fn broker_pipe() -> Result<Option<String>, ClientError> {
        Ok(None)
    }
    pub fn broker_request(
        _: &str, _: bool, _: Instant, _: &Cancellation, _: &Cancellation,
        _: &Registration,
    ) -> Result<Value, ClientError> {
        Err(unsupported())
    }
}

pub const HOST_CONTRACT: &str = "portal.host/1";
/// Explicitly privileged local operator API, absent from ordinary app clients.
#[cfg(feature = "operator-controls")]
#[path = "operator.rs"]
pub mod operator;
const MAX_FRAME: usize = 65536;
static WORKERS: AtomicUsize = AtomicUsize::new(0);
struct WorkerSlot;
// Only the owned discovery broker is terminated; never the shared host.
struct Broker(Child);
impl Drop for Broker {
    fn drop(&mut self) {
        if !matches!(self.0.try_wait(), Ok(Some(_))) {
            let _ = self.0.kill();
        }
    }
}
impl Drop for WorkerSlot {
    fn drop(&mut self) {
        WORKERS.fetch_sub(1, Ordering::SeqCst);
    }
}
fn invalid() -> ClientError {
    error("not_sent", "HOST_PROFILE_INVALID")
}
fn document<T: serde::de::DeserializeOwned>(raw: &[u8]) -> Result<T, ClientError> {
    if !super::unique_json(raw) {
        return Err(invalid());
    }
    serde_json::from_slice(raw).map_err(|_| invalid())
}
fn digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}
fn hex(value: &str, n: usize) -> bool {
    value.len() == n
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RuntimePin {
    executable: PathBuf,
    sha256: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Profile {
    schema: u32,
    contract: String,
    sid: String,
    state: PathBuf,
    release: PathBuf,
    release_id: String,
    version: String,
    runtime: RuntimePin,
    lifetime: String,
    files: BTreeMap<PathBuf, String>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Manifest {
    schema: u32,
    version: String,
    runtime: Value,
    ledger_schema: u32,
    files: BTreeMap<String, String>,
}
#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Peer {
    pub(crate) pid: u32,
    pub(crate) created: u64,
    pub(crate) executable: String,
    pub(crate) sid: String,
}
struct Registration {
    profile: Profile,
    root: PathBuf,
    profile_sha: String,
    _pins: Vec<File>,
}
fn release_inventory(release: &Path, files: &BTreeMap<String, String>) -> Result<(), ClientError> {
    let expected: BTreeSet<PathBuf> = files
        .keys()
        .map(|name| release.join(name))
        .chain(std::iter::once(release.join("manifest.json")))
        .collect();
    let mut directories = BTreeSet::new();
    directories.insert(release.to_path_buf());
    for path in &expected {
        for parent in path
            .ancestors()
            .skip(1)
            .take_while(|p| p.starts_with(release))
        {
            directories.insert(parent.to_path_buf());
        }
    }
    if directories.len() > 128 {
        return Err(invalid());
    }
    let mut pending = vec![release.to_path_buf()];
    let mut seen = BTreeSet::new();
    while let Some(directory) = pending.pop() {
        for entry in std::fs::read_dir(directory).map_err(|_| invalid())? {
            let path = entry.map_err(|_| invalid())?.path();
            native::private(&path)?;
            if directories.contains(&path) && path.is_dir() {
                pending.push(path)
            } else if expected.contains(&path) && path.is_file() {
                seen.insert(path);
            } else {
                return Err(invalid());
            }
        }
    }
    if seen != expected {
        return Err(invalid());
    }
    Ok(())
}
fn read_pinned(path: &Path, private: bool, max: usize) -> Result<(File, Vec<u8>), ClientError> {
    let mut file = native::file(path)?;
    if private {
        native::private(path)?
    }
    let mut bytes = Vec::new();
    file.by_ref()
        .take((max + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|_| invalid())?;
    if bytes.len() > max {
        return Err(invalid());
    }
    Ok((file, bytes))
}
impl Registration {
    fn load(root: PathBuf) -> Result<Self, ClientError> {
        for name in ["", "portal-state", "portal-releases", "launches"] {
            native::private(&root.join(name))?
        }
        let path = root.join("launches/portal-background.json");
        let mut pins = native::directories(&path)?;
        let (file, raw) = read_pinned(&path, true, 65536)?;
        pins.push(file);
        let p: Profile = document(&raw)?;
        let numbers: Vec<_> = p.version.split('.').map(str::parse::<u32>).collect();
        if p.schema != 1
            || p.contract != HOST_CONTRACT
            || p.sid != native::sid()?
            || p.lifetime != "operator-session"
            || p.state != root.join("portal-state")
            || !hex(&p.release_id, 64)
            || p.release != root.join("portal-releases").join(&p.release_id)
            || numbers.len() != 3
            || numbers.iter().any(Result::is_err)
            || numbers[0].as_ref() != Ok(&0)
            || numbers[1].as_ref() != Ok(&2)
            || numbers[2].as_ref().is_ok_and(|n| *n < 15)
            || !p.runtime.executable.is_absolute()
            || !hex(&p.runtime.sha256, 64)
            || p.files.len() > 64
        {
            return Err(invalid());
        }
        let manifest_path = p.release.join("manifest.json");
        let (file, raw_manifest) = read_pinned(&manifest_path, true, 65536)?;
        pins.push(file);
        let manifest: Manifest = document(&raw_manifest)?;
        let manifest_value: Value = document(&raw_manifest)?;
        let canonical = serde_json::to_vec(&manifest_value).map_err(|_| invalid())?;
        if manifest.schema != 1
            || manifest.version != p.version
            || manifest.ledger_schema != 2
            || manifest.runtime
                != json!({"implementation":"CPython","version":"3.14.8","platform":"win32","bits":64})
            || digest(&canonical) != p.release_id
            || manifest.files.len() > 64
        {
            return Err(error("not_sent", "HOST_INCOMPATIBLE"));
        }
        for name in [
            "bootstrap.py",
            "portal_gateway/service.py",
            "portal_gateway/background_host.py",
            "portal_gateway/host_native.py",
            "portal_gateway/host_profile.py",
        ] {
            if !manifest.files.contains_key(name) {
                return Err(invalid());
            }
        }
        let mut required = BTreeMap::new();
        for (name, hash) in &manifest.files {
            if Path::new(name).is_absolute()
                || name.contains('\\')
                || name
                    .split('/')
                    .any(|x| x.is_empty() || x == "." || x == "..")
                || !hex(hash, 64)
            {
                return Err(invalid());
            }
            required.insert(p.release.join(name), hash.clone());
        }
        required.insert(manifest_path, digest(&raw_manifest));
        required.insert(p.runtime.executable.clone(), p.runtime.sha256.clone());
        for name in ["config.json", "release.json"] {
            let path = p.state.join(name);
            let hash = p
                .files
                .get(&path)
                .filter(|s| hex(s, 64))
                .ok_or_else(invalid)?;
            required.insert(path, hash.clone());
        }
        if p.files != required {
            return Err(invalid());
        }
        let mut package_bytes = 0usize;
        for (path, expected) in &p.files {
            pins.extend(native::directories(path)?);
            let payload = path.starts_with(&p.release);
            let max = if path == &p.runtime.executable {
                64 * 1024 * 1024
            } else if payload {
                16 * 1024 * 1024
            } else {
                65536
            };
            let (file, bytes) = read_pinned(path, path.starts_with(&root), max)?;
            if payload {
                package_bytes = package_bytes.checked_add(bytes.len()).ok_or_else(invalid)?;
                if package_bytes > 16 * 1024 * 1024 {
                    return Err(invalid());
                }
            }
            if digest(&bytes) != *expected {
                return Err(error("not_sent", "HOST_IDENTITY_MISMATCH"));
            }
            pins.push(file);
        }
        release_inventory(&p.release, &manifest.files)?;
        Ok(Self {
            profile: p,
            root,
            profile_sha: digest(&raw),
            _pins: pins,
        })
    }
    fn peer(&self, frame: &Value) -> Result<Peer, ClientError> {
        let path = self.root.join("portal-state/background.json");
        let (_file, bytes) = read_pinned(&path, true, 65536)?;
        let record: Value = document(&bytes)?;
        if record["contract"] != HOST_CONTRACT
            || record["profile_sha256"] != self.profile_sha
            || record["generation"] != frame["generation"]
        {
            return Err(error("not_sent", "HOST_IDENTITY_MISMATCH"));
        }
        let peer: Peer = serde_json::from_value(record["host"].clone()).map_err(|_| invalid())?;
        if peer.pid == 0
            || peer.created == 0
            || peer.sid != self.profile.sid
            || peer.executable.to_lowercase()
                != self
                    .profile
                    .runtime
                    .executable
                    .to_string_lossy()
                    .to_lowercase()
        {
            return Err(invalid());
        }
        Ok(peer)
    }
}

/// A protected file and its validated transient bytes, retained to prevent replacement.
pub struct PrivatePin { _file: File, bytes: Vec<u8>, _directories: Vec<File>, _registration: Option<Arc<Registration>> }
impl PrivatePin { pub fn bytes(&self) -> &[u8] { &self.bytes } }

/// Reject lexical traversal even when normalized prefixes would still match.
fn safe_private_child(root: &Path, path: &Path) -> bool {
    path.is_absolute() && path.starts_with(root) && path != root
        && !path.components().any(|component| matches!(component, std::path::Component::CurDir | std::path::Component::ParentDir))
}
fn safe_trusted_root(root: &Path) -> bool {
    root.is_absolute()
        && root.components().all(|component| {
            !matches!(component, std::path::Component::CurDir | std::path::Component::ParentDir)
        })
}
/// Exposes the SDK bounded duplicate-key validator for native feature profiles.
pub fn validate_unique_json(raw: &[u8]) -> bool { super::unique_json(raw) }
/// Read and pin a small private file after validating every in-root ancestor.
fn pin_private_at(root: &Path, path: &Path, expected_sha256: Option<&str>, max_bytes: usize) -> Result<PrivatePin, ClientError> {
    if max_bytes == 0 || max_bytes > 64 * 1024 * 1024 || expected_sha256.is_some_and(|hash| !hex(hash, 64)) || !safe_private_child(root,path) { return Err(invalid()); }
    let directories = native::directories(path)?;
    for parent in path.ancestors().take_while(|p| p.starts_with(root)) { native::private(parent)?; }
    let (file, bytes) = read_pinned(path, true, max_bytes)?;
    if expected_sha256.is_some_and(|hash| digest(&bytes) != hash) { return Err(error("not_sent", "HOST_IDENTITY_MISMATCH")); }
    Ok(PrivatePin { _file:file, bytes, _directories:directories, _registration:None })
}
pub fn pin_private(path: &Path, expected_sha256: Option<&str>, max_bytes: usize) -> Result<PrivatePin, ClientError> {
    pin_private_at(&native::root()?,path,expected_sha256,max_bytes)
}
/// Pin a private file below a root selected by trusted native code, such as an app-owned config directory.
/// Never pass a root or path supplied by a webview. The root and every in-root ancestor must satisfy the
/// same no-reparse and private-ACL checks as registered Portal inputs.
pub fn pin_private_from_trusted_root(root: &Path, path: &Path, expected_sha256: Option<&str>, max_bytes: usize) -> Result<PrivatePin, ClientError> {
    if !safe_trusted_root(root) { return Err(invalid()); }
    pin_private_at(root,path,expected_sha256,max_bytes)
}
/// Pin a public app-owned source input under a trusted root using an exact reviewed hash.
/// This deliberately omits the private-ACL check and must never be used for profiles, keys, or configuration.
pub fn pin_reviewed_source_from_trusted_root(root: &Path, path: &Path, expected_sha256: &str, max_bytes: usize) -> Result<PrivatePin, ClientError> {
    if !safe_trusted_root(root) || max_bytes == 0 || max_bytes > 64 * 1024 * 1024
        || !hex(expected_sha256, 64) || !safe_private_child(root,path) { return Err(invalid()); }
    let directories = native::directories(path)?;
    let (file, bytes) = read_pinned(path, false, max_bytes)?;
    if digest(&bytes) != expected_sha256 { return Err(error("not_sent", "HOST_IDENTITY_MISMATCH")); }
    Ok(PrivatePin { _file:file, bytes, _directories:directories, _registration:None })
}
#[cfg(feature = "host-fixtures")]
pub fn pin_private_fixture(root: &Path, path: &Path, expected_sha256: Option<&str>, max_bytes: usize) -> Result<PrivatePin, ClientError> {
    pin_private_at(root,path,expected_sha256,max_bytes)
}
/// Runtime identity inherited from the independently validated Portal host registration.
pub struct RegisteredRuntime { path: PathBuf, sha256: String, registration: Arc<Registration> }
impl RegisteredRuntime { pub fn path(&self) -> &Path { &self.path } pub fn sha256(&self) -> &str { &self.sha256 } }
pub fn registered_runtime() -> Result<RegisteredRuntime, ClientError> { registered_runtime_at(&native::root()?) }
#[cfg(feature = "host-fixtures")]
pub fn registered_runtime_fixture(root: &Path) -> Result<RegisteredRuntime, ClientError> { registered_runtime_at(root) }
fn registered_runtime_at(root: &Path) -> Result<RegisteredRuntime, ClientError> {
    let registration = Arc::new(Registration::load(root.to_path_buf())?);
    Ok(RegisteredRuntime { path: registration.profile.runtime.executable.clone(), sha256: registration.profile.runtime.sha256.clone(), registration })
}/// Open and hash only the runtime selected by the validated host registration.
pub fn pin_registered_runtime(runtime: RegisteredRuntime) -> Result<PrivatePin, ClientError> {
    if runtime.path != runtime.registration.profile.runtime.executable || runtime.sha256 != runtime.registration.profile.runtime.sha256 { return Err(invalid()); }
    let (file, bytes) = read_pinned(&runtime.path, false, 64 * 1024 * 1024)?;
    if digest(&bytes) != runtime.sha256 { return Err(error("not_sent", "HOST_IDENTITY_MISMATCH")); }
    Ok(PrivatePin { _file:file, bytes, _directories:Vec::new(), _registration:Some(runtime.registration) })
}
/// Canonical current-user Portal installation root used only by trusted native callers.
pub fn registered_root() -> Result<PathBuf, ClientError> { native::root() }
/// Backend-owned tightening limits, not a spending grant or renderer settings.
pub struct ReviewedPolicy {
    pub(crate) alias: String,
    pub(crate) commitment: String,
    pub(crate) usd: u64,
    pub(crate) tokens: u64,
    pub(crate) output: u32,
}
impl ReviewedPolicy {
    pub fn new(
        alias: &str,
        commitment: &str,
        usd: u64,
        tokens: u64,
        output: u32,
    ) -> Result<Self, ClientError> {
        if !super::identifier(alias, 64, false)
            || !hex(commitment, 64)
            || usd == 0
            || tokens == 0
            || usd > i64::MAX as u64
            || tokens > i64::MAX as u64
            || !(1..=8192).contains(&output)
        {
            return Err(invalid());
        }
        Ok(Self {
            alias: alias.into(),
            commitment: commitment.into(),
            usd,
            tokens,
            output,
        })
    }
}
pub(crate) struct ManagedPeer {
    registration: Arc<Registration>,
    identity: Peer,
    pub(crate) policy: ReviewedPolicy,
}
impl ManagedPeer {
    pub(crate) fn verify(&self, socket: &std::net::TcpStream) -> Result<(), ClientError> {
        // Retained registration pins prevent code/profile replacement. The
        // actual connected server process is checked before app-key bytes.
        let _ = &self.registration;
        native::peer(socket, &self.identity)
    }
}

/// Metadata only. Intentionally neither Debug nor Serialize: use receipt().
pub struct HostConnection {
    state: String,
    frame: Value,
    registration: Arc<Registration>,
    peer: Option<Peer>,
}
/// Reviewed launch facts derived from a verified `HostConnection`.
///
/// This is trusted-backend-only data. It intentionally implements none of
/// `Debug`, `Serialize`, or `Clone`, and retains the registration (including
/// its open profile, runtime, and release pins) until the broker exits.
pub struct HostLaunchInputs {
    runtime: PathBuf,
    runtime_sha256: String,
    bootstrap: PathBuf,
    bootstrap_sha256: String,
    state: PathBuf,
    generation: String,
    _registration: Arc<Registration>,
    _peer: Peer,
}
impl HostLaunchInputs {
    pub fn runtime(&self) -> &Path { &self.runtime }
    pub fn runtime_sha256(&self) -> &str { &self.runtime_sha256 }
    pub fn bootstrap(&self) -> &Path { &self.bootstrap }
    pub fn bootstrap_sha256(&self) -> &str { &self.bootstrap_sha256 }
    pub fn state(&self) -> &Path { &self.state }
    /// Exact running generation, for backend-only broker binding.
    pub fn generation(&self) -> &str { &self.generation }
}
impl HostConnection {
    pub fn state(&self) -> &str {
        &self.state
    }
    /// Backend-only generation binding; never serialize this into the webview.
    pub fn generation(&self) -> Option<&str> {
        self.frame["generation"].as_str().filter(|value| hex(value, 32))
    }
    /// Return only the selected runtime/bootstrap/state and generation already
    /// validated by this connection. No descriptor, environment variable,
    /// renderer input, or mutable registration is consulted again.
    pub fn launch_inputs(&self) -> Result<HostLaunchInputs, ClientError> {
        if self.state != "running" {
            return Err(error("not_sent", "HOST_UNAVAILABLE"));
        }
        let generation = self.generation().ok_or_else(invalid)?.to_owned();
        let peer = self.peer.clone().ok_or_else(invalid)?;
        let profile = &self.registration.profile;
        if profile.state != self.registration.root.join("portal-state") {
            return Err(invalid());
        }
        let runtime = profile.runtime.executable.clone();
        let runtime_sha256 = profile.runtime.sha256.clone();
        let bootstrap = profile.release.join("bootstrap.py");
        let bootstrap_sha256 = profile.files.get(&bootstrap).ok_or_else(invalid)?.clone();
        if !hex(&runtime_sha256, 64)
            || profile.files.get(&runtime) != Some(&runtime_sha256)
            || !hex(&bootstrap_sha256, 64)
        {
            return Err(invalid());
        }
        Ok(HostLaunchInputs {
            runtime,
            runtime_sha256,
            bootstrap,
            bootstrap_sha256,
            state: profile.state.clone(),
            generation,
            _registration: self.registration.clone(),
            _peer: peer,
        })
    }
    pub fn receipt(&self) -> Value {
        let code = match self.frame["readiness"]["code"].as_str().unwrap_or("") {
            "ACCEPTING" => "ACCEPTING",
            "LIVE_DISPATCH_DISABLED" => "LIVE_DISPATCH_DISABLED",
            "OPERATOR_PAUSED" => "OPERATOR_PAUSED",
            "OPERATOR_REVOKED" => "OPERATOR_REVOKED",
            "POLICY_APPROVAL_REQUIRED" => "POLICY_APPROVAL_REQUIRED",
            _ => "HOST_UNAVAILABLE",
        };
        json!({"contract":HOST_CONTRACT,"state":self.state,"readiness":if self.state=="running"{json!({"ready":self.frame["readiness"]["ready"],"code":code})}else{Value::Null},
               "version":self.frame.get("version").cloned().unwrap_or(Value::Null)})
    }
    pub fn client(
        self,
        app_key: String,
        policy: ReviewedPolicy,
    ) -> Result<PortalClient, ClientError> {
        let code = self.frame["readiness"]["code"].as_str().unwrap_or("");
        if self.state != "running" {
            return Err(error("not_sent", "HOST_UNAVAILABLE"));
        }
        if self.frame["readiness"]["ready"] != true || code != "ACCEPTING" {
            return Err(error(
                "not_sent",
                match code {
                    "LIVE_DISPATCH_DISABLED" => "LIVE_DISPATCH_DISABLED",
                    "OPERATOR_PAUSED" => "OPERATOR_PAUSED",
                    _ => "HOST_UNAVAILABLE",
                },
            ));
        }
        let port = self.frame["service"]["port"]
            .as_u64()
            .filter(|v| *v > 0 && *v <= 65535)
            .ok_or_else(invalid)? as u16;
        let mut client = PortalClient::new(port, app_key)?;
        client.managed = Some(ManagedPeer {
            registration: self.registration,
            identity: self.peer.ok_or_else(invalid)?,
            policy,
        });
        Ok(client)
    }
}

#[derive(Clone, Default)]
pub struct SharedPortal {
    #[cfg(any(test, feature = "host-fixtures"))]
    fixture: Option<(PathBuf, PathBuf, String)>,
}
impl SharedPortal {
    /// No paths, commands, ports or keys are accepted from a webview.
    pub fn registered() -> Self {
        Self::default()
    }
    #[cfg(any(test, feature = "host-fixtures"))]
    pub fn fixture(root: PathBuf, worker: PathBuf, sha256: String) -> Self {
        Self {
            fixture: Some((root, worker, sha256)),
        }
    }
    pub fn acquire(&self, timeout: Duration, cancel: &Cancellation) -> LifecycleTask {
        self.start(true, timeout, cancel)
    }
    pub fn status(&self, timeout: Duration, cancel: &Cancellation) -> LifecycleTask {
        self.start(false, timeout, cancel)
    }
    fn start(&self, acquire: bool, timeout: Duration, cancel: &Cancellation) -> LifecycleTask {
        let state = Arc::new(Mutex::new(TaskState {
            result: None,
            waker: None,
            finished: false,
        }));
        let dropped = Cancellation::default();
        let task = LifecycleTask {
            state: state.clone(),
            dropped: dropped.clone(),
        };
        let external = cancel.clone();
        let context = self.clone();
        if timeout < Duration::from_millis(1) || timeout > Duration::from_secs(30) {
            publish(&state, Err(error("not_sent", "HOST_TIMEOUT_INVALID")));
            return task;
        }
        if WORKERS
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| {
                (n < 4).then_some(n + 1)
            })
            .is_err()
        {
            publish(&state, Err(error("not_sent", "HOST_UNAVAILABLE")));
            return task;
        }
        let slot = Arc::new(WorkerSlot);
        let worker_slot = slot.clone();
        let timer_slot = slot.clone();
        let worker_state = state.clone();
        let timer_state = state.clone();
        let timer_drop = dropped.clone();
        let timer_cancel = cancel.clone();
        let deadline = Instant::now() + timeout;
        let result = std::thread::Builder::new()
            .name("portal-lifecycle".into())
            .spawn(move || {
                let _slot = worker_slot;
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    context.run(acquire, timeout, deadline, &external, &dropped)
                }))
                .unwrap_or_else(|_| Err(error("not_sent", "HOST_UNAVAILABLE")));
                publish(&worker_state, result);
            });
        if result.is_err() {
            publish(&state, Err(error("not_sent", "HOST_UNAVAILABLE")));
            return task;
        }
        let failed_timer = timer_drop.clone();
        if std::thread::Builder::new()
            .name("portal-lifecycle-deadline".into())
            .spawn(move || {
                let _slot = timer_slot;
                loop {
                    if timer_state
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .finished
                    {
                        return;
                    }
                    if timer_drop.is_cancelled() || timer_cancel.is_cancelled() {
                        publish(&timer_state, Err(error("not_sent", "HOST_CANCELLED")));
                        return;
                    }
                    if Instant::now() >= deadline {
                        publish(&timer_state, Err(error("not_sent", "HOST_START_TIMEOUT")));
                        timer_drop.cancel();
                        return;
                    }
                    std::thread::sleep(Duration::from_millis(2));
                }
            })
            .is_err()
        {
            failed_timer.cancel();
            publish(&state, Err(error("not_sent", "HOST_UNAVAILABLE")))
        }
        task
    }
    fn run(
        &self,
        acquire: bool,
        timeout: Duration,
        deadline: Instant,
        cancel: &Cancellation,
        dropped: &Cancellation,
    ) -> Result<HostConnection, ClientError> {
        let check = || {
            if cancel.is_cancelled() || dropped.is_cancelled() {
                Err(error("not_sent", "HOST_CANCELLED"))
            } else if Instant::now() >= deadline {
                Err(error("not_sent", "HOST_START_TIMEOUT"))
            } else {
                Ok(())
            }
        };
        check()?;
        #[cfg(any(test, feature = "host-fixtures"))]
        let root = match &self.fixture {
            Some((root, _, _)) => root.clone(),
            None => native::root()?,
        };
        #[cfg(not(any(test, feature = "host-fixtures")))]
        let root = native::root()?;
        let registration = Arc::new(Registration::load(root)?);
        check()?;
        if let Some(pipe) = native::broker_pipe()? {
            let frame = native::broker_request(
                &pipe, acquire, deadline, cancel, dropped, &registration,
            )?;
            validate_frame(&frame, &registration)?;
            if frame["state"] == "unavailable" {
                return Err(frame_error(&frame));
            }
            let state = frame["state"].as_str().ok_or_else(invalid)?.to_owned();
            let peer = if state == "running" {
                Some(registration.peer(&frame)?)
            } else {
                None
            };
            return Ok(HostConnection { state, frame, registration, peer });
        }
        let p = &registration.profile;
        let mut command = Command::new(&p.runtime.executable);
        command.args(["-I", "-B"]);
        #[cfg(any(test, feature = "host-fixtures"))]
        let mut fixture_pin: Option<File> = None;
        #[cfg(any(test, feature = "host-fixtures"))]
        if let Some((root, worker, expected)) = &self.fixture {
            let (file, bytes) = read_pinned(worker, false, 1024 * 1024)?;
            if digest(&bytes) != *expected {
                return Err(invalid());
            }
            fixture_pin = Some(file);
            command
                .arg(worker)
                .arg(if acquire { "acquire" } else { "status" })
                .arg(root)
                .arg(&p.release);
        } else {
            command
                .arg(p.release.join("bootstrap.py"))
                .arg(if acquire {
                    "background-acquire"
                } else {
                    "background-status"
                })
                .arg("--state")
                .arg(&p.state)
                .arg("--timeout-ms")
                .arg(timeout.as_millis().to_string());
        }
        #[cfg(not(any(test, feature = "host-fixtures")))]
        command
            .arg(p.release.join("bootstrap.py"))
            .arg(if acquire {
                "background-acquire"
            } else {
                "background-status"
            })
            .arg("--state")
            .arg(&p.state)
            .arg("--timeout-ms")
            .arg(timeout.as_millis().to_string());
        command.env_clear();
        for key in ["SystemRoot", "WINDIR", "TEMP", "TMP"] {
            if let Some(value) = std::env::var_os(key) {
                command.env(key, value);
            }
        }
        if let Some(system) = std::env::var_os("SystemRoot") {
            command.env("PATH", PathBuf::from(system).join("System32"));
        }
        command
            .current_dir(&p.release)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            command.creation_flags(0x08000000);
        }
        check()?;
        let mut broker = Broker(
            command
                .spawn()
                .map_err(|_| error("not_sent", "HOST_LAUNCH_REFUSED"))?,
        );
        let child = &mut broker.0;
        let mut output = child.stdout.take().ok_or_else(invalid)?;
        let mut bytes = Vec::new();
        let reading = (|| {
            loop {
                check()?;
                match native::pipe_available(&output)? {
                    Some(n) if n > 0 => {
                        let mut chunk = [0u8; 4096];
                        let limit = (n as usize).min(chunk.len());
                        let count = output
                            .read(&mut chunk[..limit])
                            .map_err(|_| error("not_sent", "HOST_UNAVAILABLE"))?;
                        bytes.extend_from_slice(&chunk[..count]);
                        if bytes.len() > MAX_FRAME {
                            return Err(invalid());
                        }
                    }
                    None => break,
                    _ => {
                        // Exit can race this empty-pipe observation. Drain to
                        // actual EOF so the broker's final bytes are not lost.
                        std::thread::sleep(Duration::from_millis(2));
                    }
                }
            }
            Ok(())
        })();
        reading?;
        let exit = loop {
            check()?;
            if let Some(exit) = child
                .try_wait()
                .map_err(|_| error("not_sent", "HOST_UNAVAILABLE"))?
            {
                break exit;
            }
            std::thread::sleep(Duration::from_millis(2));
        };
        #[cfg(any(test, feature = "host-fixtures"))]
        drop(fixture_pin);
        check()?;
        let frame: Value = document(&bytes)?;
        validate_frame(&frame, &registration)?;
        if !exit.success() || frame["state"] == "unavailable" {
            return Err(frame_error(&frame));
        }
        let state = frame["state"].as_str().ok_or_else(invalid)?.to_owned();
        let peer = if state == "running" {
            Some(registration.peer(&frame)?)
        } else {
            None
        };
        Ok(HostConnection {
            state,
            frame,
            registration,
            peer,
        })
    }
}

fn frame_error(frame: &Value) -> ClientError {
    let code = match frame["code"].as_str().unwrap_or("") {
        "HOST_CANCELLED" => "HOST_CANCELLED",
        "HOST_START_TIMEOUT" => "HOST_START_TIMEOUT",
        "HOST_LAUNCH_REFUSED" => "HOST_LAUNCH_REFUSED",
        "HOST_LAUNCH_CLEANUP_PENDING" => "HOST_LAUNCH_CLEANUP_PENDING",
        "HOST_PROFILE_INVALID" => "HOST_PROFILE_INVALID",
        "HOST_IDENTITY_MISMATCH" => "HOST_IDENTITY_MISMATCH",
        "HOST_INCOMPATIBLE" => "HOST_INCOMPATIBLE",
        "HOST_LISTENER_UNTRUSTED" => "HOST_LISTENER_UNTRUSTED",
        _ => "HOST_UNAVAILABLE",
    };
    error("not_sent", code)
}
fn validate_frame(frame: &Value, r: &Registration) -> Result<(), ClientError> {
    if frame["contract"] != HOST_CONTRACT {
        return Err(invalid());
    }
    let state = frame["state"].as_str().ok_or_else(invalid)?;
    if !["stopped", "starting", "running", "unavailable"].contains(&state) {
        return Err(invalid());
    }
    let object = frame.as_object().ok_or_else(invalid)?;
    if object.keys().any(|k| {
        ![
            "contract",
            "state",
            "readiness",
            "code",
            "generation",
            "release_id",
            "version",
            "service",
        ]
        .contains(&k.as_str())
    }) {
        return Err(invalid());
    }
    if state == "running" {
        if frame["release_id"] != r.profile.release_id
            || frame["version"] != r.profile.version
            || !frame["generation"].as_str().is_some_and(|x| hex(x, 32))
            || frame["service"]["host"] != "127.0.0.1"
            || frame["service"]["contract"] != "portal.service/1"
            || frame["service"]["ledger_schema"] != 2
            || frame["service"]["text_contract"] != "portal.text/1"
            || frame["service"]["version"] != r.profile.version
            || !frame["service"]["instance"]
                .as_str()
                .is_some_and(|x| hex(x, 32))
            || !frame["service"]["port"]
                .as_u64()
                .is_some_and(|p| p > 0 && p <= 65535)
            || !frame["readiness"]["ready"].is_boolean()
            || !frame["readiness"]["code"].is_string()
        {
            return Err(invalid());
        }
    }
    Ok(())
}

const BROKER_CONTRACT: u32 = 1;
fn broker_request_bytes(request_id: &str, op: &str) -> Result<Vec<u8>, ClientError> {
    if !hex(request_id, 32) || !matches!(op, "status" | "acquire" | "cancel") {
        return Err(error("not_sent", "HOST_PROFILE_INVALID"));
    }
    let bytes = serde_json::to_vec(&json!({"schema":BROKER_CONTRACT,"request_id":request_id,"op":op}))
        .map_err(|_| invalid())?;
    if bytes.len() > 512 { return Err(invalid()); }
    Ok(bytes)
}
fn broker_reply_frame(raw: &[u8], request_id: &str) -> Result<Value, ClientError> {
    let value: Value = document(raw)?;
    let object = value.as_object().ok_or_else(invalid)?;
    if value["schema"] != BROKER_CONTRACT || value["request_id"] != request_id
        || !value["ok"].is_boolean() {
        return Err(error("not_sent", "LIFECYCLE_UNRESOLVED"));
    }
    if value["ok"] == true {
        if object.len()==4 && object.keys().all(|k|["schema","request_id","ok","cancelled"].contains(&k.as_str()))
            && value["cancelled"]==true {
            return Err(error("not_sent","HOST_CANCELLED"));
        }
        if object.len() != 4 || object.keys().any(|k| !["schema","request_id","ok","frame"].contains(&k.as_str()))
            || !value["frame"].is_object() {
            return Err(error("not_sent", "LIFECYCLE_UNRESOLVED"));
        }
        return Ok(value["frame"].clone());
    }
    if object.len() != 4 || object.keys().any(|k| !["schema","request_id","ok","code"].contains(&k.as_str())) {
        return Err(error("not_sent", "LIFECYCLE_UNRESOLVED"));
    }
    let safe = match value["code"].as_str().unwrap_or("") {
        "GUARDIAN_REQUEST_INVALID" => "GUARDIAN_REQUEST_INVALID",
        "GUARDIAN_CALLER_REFUSED" => "GUARDIAN_CALLER_REFUSED",
        "GUARDIAN_BUSY" => "GUARDIAN_BUSY",
        "GUARDIAN_PROFILE_CHANGED" => "GUARDIAN_PROFILE_CHANGED",
        "GUARDIAN_COLD_START_BLOCKED" => "GUARDIAN_COLD_START_BLOCKED",
        "GUARDIAN_BROKER_UNAVAILABLE" => "GUARDIAN_BROKER_UNAVAILABLE",
        "LIFECYCLE_UNRESOLVED" => "LIFECYCLE_UNRESOLVED",
        "HOST_CANCELLED" => "HOST_CANCELLED",
        "HOST_START_TIMEOUT" => "HOST_START_TIMEOUT",
        "HOST_LAUNCH_REFUSED" => "HOST_LAUNCH_REFUSED",
        "HOST_LAUNCH_CLEANUP_PENDING" => "HOST_LAUNCH_CLEANUP_PENDING",
        "HOST_PROFILE_INVALID" => "HOST_PROFILE_INVALID",
        "HOST_IDENTITY_MISMATCH" => "HOST_IDENTITY_MISMATCH",
        "HOST_INCOMPATIBLE" => "HOST_INCOMPATIBLE",
        "HOST_LISTENER_UNTRUSTED" => "HOST_LISTENER_UNTRUSTED",
        "HOST_UNAVAILABLE" => "HOST_UNAVAILABLE",
        "HOST_STOP_PENDING" => "HOST_STOP_PENDING",
        "HOST_TIMEOUT_INVALID" => "HOST_TIMEOUT_INVALID",
        "HOST_REGISTRATION_EXISTS" => "HOST_REGISTRATION_EXISTS",
        "HOST_SELECTED_RELEASE_REQUIRED" => "HOST_SELECTED_RELEASE_REQUIRED",
        "HOST_INPUT_PIN_FAILED" => "HOST_INPUT_PIN_FAILED",
        "HOST_PLATFORM_UNSUPPORTED" => "HOST_PLATFORM_UNSUPPORTED",
        _ => "LIFECYCLE_UNRESOLVED",
    };
    Err(error("not_sent", safe))
}

struct TaskState {
    result: Option<Result<HostConnection, ClientError>>,
    waker: Option<Waker>,
    finished: bool,
}
fn publish(state: &Arc<Mutex<TaskState>>, result: Result<HostConnection, ClientError>) {
    let waker = {
        let mut state = state.lock().unwrap_or_else(|e| e.into_inner());
        if state.finished {
            return;
        }
        state.finished = true;
        state.result = Some(result);
        state.waker.take()
    };
    if let Some(waker) = waker {
        waker.wake()
    }
}
/// Cancellable Future; construction/poll/drop never waits for a host or joins a worker.
pub struct LifecycleTask {
    state: Arc<Mutex<TaskState>>,
    dropped: Cancellation,
}
impl LifecycleTask {
    pub fn try_result(&self) -> Option<Result<HostConnection, ClientError>> {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .result
            .take()
    }
}
impl Future for LifecycleTask {
    type Output = Result<HostConnection, ClientError>;
    fn poll(self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Self::Output> {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        match state.result.take() {
            Some(result) => Poll::Ready(result),
            None => {
                state.waker = Some(context.waker().clone());
                Poll::Pending
            }
        }
    }
}
impl Drop for LifecycleTask {
    fn drop(&mut self) {
        self.dropped.cancel();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn launch_fixture(state: &str, generation: &str, include_bootstrap: bool) -> HostConnection {
        let root = PathBuf::from(r"C:\PortalFixture");
        let release_id = "a".repeat(64);
        let release = root.join("portal-releases").join(&release_id);
        let runtime = PathBuf::from(r"C:\PythonFixture\python.exe");
        let bootstrap = release.join("bootstrap.py");
        let runtime_sha = "b".repeat(64);
        let bootstrap_sha = "c".repeat(64);
        let mut files = BTreeMap::from([(runtime.clone(), runtime_sha.clone())]);
        if include_bootstrap {
            files.insert(bootstrap.clone(), bootstrap_sha.clone());
        }
        let registration = Registration {
            profile: Profile {
                schema: 1,
                contract: HOST_CONTRACT.into(),
                sid: "S-1-fixture".into(),
                state: root.join("portal-state"),
                release,
                release_id,
                version: "0.2.19".into(),
                runtime: RuntimePin { executable: runtime.clone(), sha256: runtime_sha },
                lifetime: "operator-session".into(),
                files,
            },
            root,
            profile_sha: "d".repeat(64),
            _pins: Vec::new(),
        };
        HostConnection {
            state: state.into(),
            frame: json!({"generation": generation}),
            registration: Arc::new(registration),
            peer: Some(Peer {
                pid: 7,
                created: 9,
                executable: runtime.to_string_lossy().into_owned(),
                sid: "S-1-fixture".into(),
            }),
        }
    }
    #[test]
    fn lifecycle_documents_reject_duplicate_nested_fields() {
        assert!(document::<Value>(br#"{"files":{"a":"1","a":"2"}}"#).is_err());
        assert!(document::<Value>(br#"{"state":"stopped","state":"running"}"#).is_err());
        assert!(document::<Value>(br#"{"state":"starting"}"#).is_ok());
    }
    #[test]
    fn guardian_lifecycle_codec_is_content_free_bounded_and_exact() {
        let id="0123456789abcdef0123456789abcdef";
        let request=broker_request_bytes(id,"acquire").unwrap();
        assert!(request.len()<=512);
        let parsed:Value=document(&request).unwrap();
        assert_eq!(parsed,json!({"schema":1,"request_id":id,"op":"acquire"}));
        assert!(broker_request_bytes(id,"stop").is_err());
        let frame=json!({"contract":"portal.host/1","state":"stopped","readiness":null});
        let reply=json!({"schema":1,"request_id":id,"ok":true,"frame":frame});
        assert_eq!(broker_reply_frame(&serde_json::to_vec(&reply).unwrap(),id).unwrap(),frame);
        assert!(broker_reply_frame(br#"{"schema":1,"schema":1,"request_id":"0123456789abcdef0123456789abcdef","ok":true,"frame":{}}"#,id).is_err());
        assert!(broker_reply_frame(br#"{"schema":1,"request_id":"0123456789abcdef0123456789abcdef","ok":true,"frame":{},"path":"C:\\private"}"#,id).is_err());
        assert!(broker_reply_frame(br#"{"schema":1,"request_id":"fedcba9876543210fedcba9876543210","ok":true,"frame":{}}"#,id).is_err());
        assert!(broker_reply_frame(br#"{"schema":1,"request_id":"0123456789abcdef0123456789abcdef","ok":false,"code":"HOST_COLD_START_BLOCKED"}"#,id).is_err());
        let cleanup=json!({"schema":1,"request_id":id,"ok":false,"code":"HOST_LAUNCH_CLEANUP_PENDING"});
        let cleanup_error=broker_reply_frame(&serde_json::to_vec(&cleanup).unwrap(),id).unwrap_err();
        assert_eq!(cleanup_error.receipt()["code"],"HOST_LAUNCH_CLEANUP_PENDING");
        let unknown=json!({"schema":1,"request_id":id,"ok":false,"code":"ARBITRARY_INTERNAL_DETAIL"});
        let unknown_error=broker_reply_frame(&serde_json::to_vec(&unknown).unwrap(),id).unwrap_err();
        assert_eq!(unknown_error.receipt()["code"],"LIFECYCLE_UNRESOLVED");
    }
    #[test]
    fn reviewed_policy_refuses_unbounded_or_invalid_limits() {
        let hash = "a".repeat(64);
        assert!(ReviewedPolicy::new("fixture", &hash, 1, 1, 1).is_ok());
        for (usd, tokens, output) in [
            (0, 1, 1),
            (1, 0, 1),
            (1, 1, 0),
            (1, 1, 8193),
            (u64::MAX, 1, 1),
        ] {
            assert!(ReviewedPolicy::new("fixture", &hash, usd, tokens, output).is_err());
        }
        assert!(ReviewedPolicy::new("fixture", "unreviewed", 1, 1, 1).is_err());
    }
    #[test]
    fn launch_inputs_are_derived_from_a_running_pinned_registration() {
        let connection = launch_fixture("running", &"1".repeat(32), true);
        let inputs = connection.launch_inputs().unwrap();
        assert_eq!(inputs.runtime(), Path::new(r"C:\PythonFixture\python.exe"));
        assert_eq!(inputs.runtime_sha256(), "b".repeat(64));
        assert!(inputs.bootstrap().ends_with("bootstrap.py"));
        assert_eq!(inputs.bootstrap_sha256(), "c".repeat(64));
        assert_eq!(inputs.state(), Path::new(r"C:\PortalFixture\portal-state"));
        assert_eq!(inputs.generation(), "1".repeat(32));
    }
    #[test]
    fn launch_inputs_refuse_nonrunning_or_incomplete_host_identity() {
        assert!(launch_fixture("starting", &"1".repeat(32), true).launch_inputs().is_err());
        assert!(launch_fixture("running", &"A".repeat(32), true).launch_inputs().is_err());
        assert!(launch_fixture("running", &"1".repeat(32), false).launch_inputs().is_err());
    }
    #[test]
    fn private_paths_reject_lexical_traversal_and_duplicate_profiles() {
        let root=Path::new(r"C:\Users\fixture\NavisFabrica-Portal");
        assert!(safe_private_child(root, &root.join("launches/fabrica-extract.json")));
        assert!(safe_trusted_root(root));
        assert!(!safe_trusted_root(Path::new(r"C:\Users\fixture\..\outside")));
        assert!(!safe_trusted_root(Path::new(r"relative\root")));
        assert!(!safe_private_child(root, &root.join("scripts/../outside.py")));
        assert!(!safe_private_child(root, Path::new(r"C:\Users\fixture\elsewhere.py")));
        assert!(!validate_unique_json(br#"{"profile":{"run_id":"a","run_id":"b"}}"#));
        assert!(validate_unique_json(br#"{"profile":{"run_id":"a"}}"#));
    }    #[test]
    fn invalid_timeout_is_immediate_and_never_launches() {
        for timeout in [Duration::ZERO, Duration::from_secs(31)] {
            let task = SharedPortal::registered().acquire(timeout, &Cancellation::default());
            match task.try_result() {
                Some(Err(e)) => assert_eq!(e.receipt()["code"], "HOST_TIMEOUT_INVALID"),
                _ => panic!("missing immediate refusal"),
            }
        }
    }
}
