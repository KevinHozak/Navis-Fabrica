//! Local operator authority only. Call execute only after trusted native confirmation.
use super::*;
use serde::Serialize;
use std::io::Write;

#[derive(Clone, Copy, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Action {
    Start,
    Stop,
    Pause,
    Resume,
}
pub fn user_identity() -> Result<String, ClientError> {
    native::sid()
}
/// Installation must explicitly create this protected profile; startup never does.
pub fn ui_directory() -> Result<PathBuf, ClientError> {
    let path = native::root()?.join(ui_directory_name(native::operator_session_required()));
    native::private(&path)?;
    Ok(path)
}
fn ui_directory_name(monitor: bool) -> &'static str {
    if monitor { "portal-ui-persistent-01" } else { "portal-ui" }
}
#[cfg(feature = "host-fixtures")]
pub fn fixture_ui_directory(root: &Path) -> Result<PathBuf, ClientError> {
    let path = root.join(ui_directory_name(native::operator_session_required()));
    native::private(&path)?;
    Ok(path)
}

#[cfg(test)]
mod ui_directory_tests {
    use super::ui_directory_name;

    #[test]
    fn monitor_uses_isolated_ui_directory() {
        assert_eq!(ui_directory_name(false), "portal-ui");
        assert_eq!(ui_directory_name(true), "portal-ui-persistent-01");
    }
}

#[cfg(test)]
mod monitor_tests {
    use super::*;
    use std::sync::Mutex;

    static ENV_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn monitor_profile_refuses_operator_actions_before_host_access() {
        let _guard = ENV_LOCK.lock().unwrap();
        let previous = std::env::var_os("NAVIS_PORTAL_OPERATOR_SESSION_REQUIRED");
        std::env::set_var("NAVIS_PORTAL_OPERATOR_SESSION_REQUIRED", "1");
        let operator = Operator::registered();
        let result = operator.prepare(Action::Pause).err().expect("monitor action must refuse");
        assert_eq!(result.receipt()["code"], "OPERATOR_ROLE_READ_ONLY");
        if let Some(value) = previous {
            std::env::set_var("NAVIS_PORTAL_OPERATOR_SESSION_REQUIRED", value);
        } else {
            std::env::remove_var("NAVIS_PORTAL_OPERATOR_SESSION_REQUIRED");
        }
    }
}
impl Action {
    pub fn name(self) -> &'static str {
        match self {
            Self::Start => "start",
            Self::Stop => "stop",
            Self::Pause => "pause",
            Self::Resume => "resume",
        }
    }
}
#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Usage {
    settled_usd_micros: String,
    settled_calls: String,
    settled_tokens: String,
    held_usd_micros: String,
    held_calls: String,
    held_tokens: String,
    unresolved_calls: String,
}
impl Usage {
    fn valid(&self) -> bool {
        [
            &self.settled_usd_micros,
            &self.settled_calls,
            &self.settled_tokens,
            &self.held_usd_micros,
            &self.held_calls,
            &self.held_tokens,
            &self.unresolved_calls,
        ]
        .into_iter()
        .all(|s| {
            !s.is_empty()
                && s.len() <= 19
                && s.bytes().all(|b| b.is_ascii_digit())
                && s.parse::<u64>().is_ok_and(|n| n <= i64::MAX as u64)
        })
    }
}
#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct AppView {
    label: String,
    routes: u32,
    usage: Usage,
    eligibility: String,
}
#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct View {
    observed_ms: u64,
    dispatch: String,
    control: String,
    sealed: bool,
    global: Usage,
    apps: Vec<AppView>,
    configured_routes: u32,
    route_eligibility: String,
    openrouter_account: String,
    helper_authority: String,
}
pub struct Report {
    view: View,
    host: Value,
    revision: u64,
    policy: String,
    generation: Option<String>,
    registration: Arc<Registration>,
}
impl Report {
    pub fn view(&self) -> Value {
        let code = match self.host["readiness"]["code"].as_str().unwrap_or("") {
            "ACCEPTING" => "ACCEPTING",
            "LIVE_DISPATCH_DISABLED" => "LIVE_DISPATCH_DISABLED",
            "OPERATOR_PAUSED" => "OPERATOR_PAUSED",
            _ => "POLICY_GATED",
        };
        json!({"host":self.host["state"],"readiness":code,"accounting":self.view})
    }
}
pub struct Operator {
    context: SharedPortal,
}
impl Operator {
    pub fn registered() -> Self {
        Self {
            context: SharedPortal::registered(),
        }
    }
    #[cfg(feature = "host-fixtures")]
    pub fn fixture(root: PathBuf, worker: PathBuf, hash: String) -> Self {
        Self {
            context: SharedPortal::fixture(root, worker, hash),
        }
    }
    pub fn report(&self) -> Result<Report, ClientError> {
        #[cfg(feature = "host-fixtures")]
        let root = match &self.context.fixture {
            Some((p, _, _)) => p.clone(),
            None => native::root()?,
        };
        #[cfg(not(feature = "host-fixtures"))]
        let root = native::root()?;
        let registration = Arc::new(Registration::load(root)?);
        if !registration.profile.files.contains_key(
            &registration
                .profile
                .release
                .join("portal_gateway/tray_operator.py"),
        ) {
            return Err(error("not_sent", "HOST_INCOMPATIBLE"));
        }
        if let Some(pipe) = native::operator_session_pipe()? {
            let safe = native::operator_session_request(&pipe, &registration)?;
            let host_state = safe["host"].as_str().filter(|v|
                ["stopped", "starting", "running", "unavailable"].contains(v))
                .ok_or_else(|| error("not_sent", "OPERATOR_SESSION_UNAVAILABLE"))?;
            let readiness = safe["readiness"].as_str().unwrap_or("POLICY_GATED");
            let view: View = serde_json::from_value(safe["accounting"].clone()).map_err(|_| invalid())?;
            validate_view(&view)?;
            let host = json!({"state":host_state,"readiness":{"code":readiness}});
            return Ok(Report { view, host, revision: 0, policy: "0".repeat(64),
                generation: None, registration });
        }
        let frame = broker(&self.context, &registration, "report", None, None)?;
        validate_frame(&frame["host"], &registration)?;
        let view: View = serde_json::from_value(frame["view"].clone()).map_err(|_| invalid())?;
        validate_view(&view)?;
        let revision = frame["operator"]["revision"]
            .as_u64()
            .filter(|v| *v < i64::MAX as u64)
            .ok_or_else(invalid)?;
        let policy = frame["operator"]["policy_sha256"]
            .as_str()
            .filter(|s| hex(s, 64))
            .ok_or_else(invalid)?
            .to_owned();
        let generation = frame["operator"]["generation"]
            .as_str()
            .filter(|s| hex(s, 32))
            .map(str::to_owned);
        Ok(Report {
            view,
            host: frame["host"].clone(),
            revision,
            policy,
            generation,
            registration,
        })
    }
    pub fn monitor_mode(&self) -> bool {
        native::operator_session_required()
    }
    pub fn prepare(&self, action: Action) -> Result<Plan, ClientError> {
        if self.monitor_mode() {
            return Err(error("not_sent", "OPERATOR_ROLE_READ_ONLY"));
        }
        let report = self.report()?;
        if report.view.sealed
            || matches!(action, Action::Start) && report.host["state"] != "stopped"
            || matches!(action, Action::Stop) && report.generation.is_none() {
            return Err(error("not_sent", if report.view.sealed {"LEDGER_SEALED"} else {"HOST_UNAVAILABLE"}));
        }
        Ok(Plan { context: self.context.clone(), action, report })
    }
}

fn validate_view(view: &View) -> Result<(), ClientError> {
    if !view.global.valid()
            || view.apps.len() > 64
            || view.configured_routes > 64
            || !["disabled", "paused", "approval_required", "policy_gated"]
                .contains(&view.dispatch.as_str())
            || !["active", "paused", "revoked", "emergency"].contains(&view.control.as_str())
            || view.route_eligibility != "not_inspected"
            || view.openrouter_account != "not_queried"
            || view.helper_authority != "not_inspected"
            || view.apps.iter().enumerate().any(|(i, a)| {
                a.label != format!("App {}", i + 1)
                    || a.routes > 64
                    || !a.usage.valid()
                    || a.eligibility != "not_inspected"
            })
        { return Err(invalid()); }
    Ok(())
}
/// One-use native review ticket. No Clone, Debug, Deserialize or Serialize.
pub struct Plan {
    context: SharedPortal,
    action: Action,
    report: Report,
}
impl Plan {
    pub fn execute(self) -> Result<Value, ClientError> {
        match self.action {
            Action::Start => {
                let cancel = Cancellation::default();
                let task = self.context.acquire(Duration::from_secs(10), &cancel);
                let deadline = Instant::now() + Duration::from_secs(12);
                loop {
                    if let Some(result) = task.try_result() {
                        return result.map(|v| v.receipt());
                    }
                    if Instant::now() > deadline {
                        return Err(error("not_sent", "HOST_START_TIMEOUT"));
                    }
                    std::thread::sleep(Duration::from_millis(2));
                }
            }
            Action::Stop => {
                let frame = broker(
                    &self.context,
                    &self.report.registration,
                    "stop",
                    self.report.generation.as_deref(),
                    None,
                )
                .map_err(effect_outcome)?;
                if frame["state"] != "stopped"
                    || frame["generation"] != self.report.generation.as_deref().unwrap_or("")
                {
                    return Err(error("unknown", "OPERATOR_OUTCOME_UNKNOWN"));
                }
                Ok(json!({"status":"applied","action":"stop"}))
            }
            Action::Pause | Action::Resume => {
                let input = json!({"contract":"portal.operator/1","action":self.action.name(),"expected_revision":self.report.revision,"policy_sha256":self.report.policy,"reviewed":true});
                let frame = broker(
                    &self.context,
                    &self.report.registration,
                    "control",
                    None,
                    Some(input),
                )
                .map_err(effect_outcome)?;
                let expected = if matches!(self.action, Action::Pause) {
                    "paused"
                } else {
                    "active"
                };
                if frame["control"] != expected || frame["revision"] != self.report.revision + 1 {
                    return Err(error("unknown", "OPERATOR_OUTCOME_UNKNOWN"));
                }
                Ok(json!({"status":"applied","action":self.action.name()}))
            }
        }
    }
}

fn effect_outcome(value: ClientError) -> ClientError {
    match value.receipt()["code"].as_str().unwrap_or("") {
        "CONTROL_REVISION_CHANGED"
        | "REVIEWED_CONTROL_REQUIRED"
        | "POLICY_APPROVAL_REQUIRED"
        | "LEDGER_SEALED" => value,
        "HOST_STOP_PENDING" => error("unknown", "HOST_STOP_PENDING"),
        _ => error("unknown", "OPERATOR_OUTCOME_UNKNOWN"),
    }
}
fn broker(
    context: &SharedPortal,
    r: &Registration,
    operation: &str,
    generation: Option<&str>,
    input: Option<Value>,
) -> Result<Value, ClientError> {
    let deadline = Instant::now() + Duration::from_secs(15);
    let p = &r.profile;
    let mut command = Command::new(&p.runtime.executable);
    command.args(["-I", "-B"]);
    #[cfg(feature = "host-fixtures")]
    let mut _worker_pin = None;
    #[cfg(feature = "host-fixtures")]
    if let Some((root, worker, hash)) = &context.fixture {
        let (file, bytes) = read_pinned(worker, false, 1024 * 1024)?;
        if digest(&bytes) != *hash {
            return Err(invalid());
        }
        _worker_pin = Some(file);
        command.arg(worker).arg(operation).arg(root).arg(&p.release);
        if let Some(value) = generation {
            command.arg(value);
        }
    } else {
        command
            .arg(p.release.join("bootstrap.py"))
            .arg(format!("background-{operation}"))
            .arg("--state")
            .arg(&p.state);
        if let Some(value) = generation {
            command
                .arg("--generation")
                .arg(value)
                .arg("--timeout-ms")
                .arg("10000");
        }
    }
    #[cfg(not(feature = "host-fixtures"))]
    {
        let _ = context;
        command
            .arg(p.release.join("bootstrap.py"))
            .arg(format!("background-{operation}"))
            .arg("--state")
            .arg(&p.state);
        if let Some(value) = generation {
            command
                .arg("--generation")
                .arg(value)
                .arg("--timeout-ms")
                .arg("10000");
        }
    }
    command.env_clear();
    for key in ["SystemRoot", "WINDIR", "TEMP", "TMP"] {
        if let Some(value) = std::env::var_os(key) {
            command.env(key, value);
        }
    }
    command
        .current_dir(&p.release)
        .stdin(if input.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x08000000);
    }
    let mut child = Broker(
        command
            .spawn()
            .map_err(|_| error("not_sent", "HOST_LAUNCH_REFUSED"))?,
    );
    if let Some(input) = input {
        let bytes = serde_json::to_vec(&input).map_err(|_| invalid())?;
        if bytes.len() > 2048 {
            return Err(invalid());
        }
        child
            .0
            .stdin
            .take()
            .ok_or_else(invalid)?
            .write_all(&bytes)
            .map_err(|_| error("unknown", "OPERATOR_OUTCOME_UNKNOWN"))?;
    }
    let mut stdout = child.0.stdout.take().ok_or_else(invalid)?;
    let mut bytes = Vec::new();
    loop {
        if Instant::now() > deadline {
            return Err(error("unknown", "OPERATOR_OUTCOME_UNKNOWN"));
        }
        match native::pipe_available(&stdout)? {
            None => break,
            Some(n) if n > 0 => {
                let mut chunk = [0u8; 4096];
                let n = (n as usize).min(4096);
                let count = stdout.read(&mut chunk[..n]).map_err(|_| invalid())?;
                bytes.extend_from_slice(&chunk[..count]);
                if bytes.len() > 65536 {
                    return Err(invalid());
                }
            }
            _ => std::thread::sleep(Duration::from_millis(2)),
        }
    }
    let exit = loop {
        if Instant::now() > deadline {
            return Err(error("unknown", "OPERATOR_OUTCOME_UNKNOWN"));
        }
        if let Some(exit) = child.0.try_wait().map_err(|_| invalid())? {
            break exit;
        }
        std::thread::sleep(Duration::from_millis(2));
    };
    let frame: Value = document(&bytes)?;
    if operation == "stop" {
        validate_frame(&frame, r)?;
        if !exit.success() {
            if frame["code"] == "HOST_STOP_PENDING" {
                return Err(error("unknown", "HOST_STOP_PENDING"));
            }
            return Err(frame_error(&frame));
        }
        return Ok(frame);
    }
    if frame["contract"] != "portal.operator/1" {
        return Err(invalid());
    }
    if !exit.success() || frame["status"] != "ok" {
        let code = match frame["code"].as_str().unwrap_or("") {
            "CONTROL_REVISION_CHANGED" => "CONTROL_REVISION_CHANGED",
            "REVIEWED_CONTROL_REQUIRED" => "REVIEWED_CONTROL_REQUIRED",
            "POLICY_APPROVAL_REQUIRED" => "POLICY_APPROVAL_REQUIRED",
            "LEDGER_SEALED" => "LEDGER_SEALED",
            _ => "OPERATOR_UNAVAILABLE",
        };
        return Err(error("not_sent", code));
    }
    Ok(frame)
}
