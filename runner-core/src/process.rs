//! Safe policy for bounded discovery and provider subprocesses.

use serde::{Deserialize, Serialize};
use std::ffi::{OsStr, OsString};
use std::io::{BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

pub(crate) fn command(program: impl AsRef<OsStr>) -> Command {
    let mut command = Command::new(program);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_NO_WINDOW);
    }
    command
}

fn is_provider_credential_environment_name(name: &OsStr) -> bool {
    let name = name.to_string_lossy().to_ascii_uppercase();
    name.contains("API_KEY")
        || name.contains("API_TOKEN")
        || name.ends_with("_TOKEN")
        || name.ends_with("_AUTH_TOKEN")
        || name.ends_with("_ACCESS_TOKEN")
        || name.contains("SECRET")
        || name.ends_with("_SECRET_KEY")
        || name.ends_with("_ACCESS_KEY")
        || name.ends_with("_PASSWORD")
        || name.ends_with("_CREDENTIALS")
        || matches!(
            name.as_str(),
            "CODEX_API_KEY" | "OPENROUTER_API_KEY" | "OLLAMA_API_KEY"
        )
}

fn strip_provider_credentials(command: &mut Command) {
    for (name, _) in std::env::vars_os() {
        if is_provider_credential_environment_name(&name) {
            command.env_remove(name);
        }
    }
}

fn configured_command(program: &Path, args: &[String]) -> std::io::Result<Command> {
    #[cfg(windows)]
    {
        match program
            .extension()
            .and_then(OsStr::to_str)
            .map(str::to_ascii_lowercase)
            .as_deref()
        {
            Some("cmd") | Some("bat") => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    "batch files are not accepted as provider executables",
                ))
            }
            Some("ps1") => {
                let system_root = std::env::var_os("SystemRoot").ok_or_else(|| {
                    std::io::Error::new(
                        std::io::ErrorKind::NotFound,
                        "Windows PowerShell is unavailable",
                    )
                })?;
                let powershell = PathBuf::from(system_root)
                    .join("System32/WindowsPowerShell/v1.0/powershell.exe");
                let mut command = command(powershell);
                command.args([
                    "-NoLogo",
                    "-NoProfile",
                    "-NonInteractive",
                    "-ExecutionPolicy",
                    "RemoteSigned",
                    "-File",
                ]);
                command.arg(program).args(args);
                return Ok(command);
            }
            _ => {}
        }
    }
    let mut command = command(program);
    command.args(args);
    Ok(command)
}

#[derive(Debug)]
pub(crate) struct CapturedOutput {
    pub status: Option<ExitStatus>,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub timed_out: bool,
    pub truncated: bool,
}

/// Capture diagnostic subprocess output with a deadline and memory cap while
/// continuing to drain pipes so a noisy child cannot deadlock the parent.
pub(crate) fn capture(
    program: &Path,
    args: &[String],
    cwd: &Path,
    timeout: Duration,
    limit: usize,
) -> std::io::Result<CapturedOutput> {
    capture_with_environment(program, args, cwd, timeout, limit, None)
}

/// Capture a local check while routing Cargo's build artifacts to the
/// supervisor-owned workspace output directory. All other inherited
/// environment values remain unchanged.
pub(crate) fn capture_in_build_output(
    program: &Path,
    args: &[String],
    cwd: &Path,
    timeout: Duration,
    limit: usize,
    build_output: &Path,
) -> std::io::Result<CapturedOutput> {
    let mut command = configured_command(program, args)?;
    command
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .env("CARGO_TARGET_DIR", build_output);
    capture_command(command, timeout, limit)
}

pub(crate) fn capture_with_environment(
    program: &Path,
    args: &[String],
    cwd: &Path,
    timeout: Duration,
    limit: usize,
    environment: Option<&[(OsString, OsString)]>,
) -> std::io::Result<CapturedOutput> {
    let mut command = configured_command(program, args)?;
    command
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(environment) = environment {
        command.env_clear();
        for (name, value) in environment {
            command.env(name, value);
        }
    }
    capture_command(command, timeout, limit)
}

fn capture_command(
    mut command: Command,
    timeout: Duration,
    limit: usize,
) -> std::io::Result<CapturedOutput> {
    let mut child = command.spawn()?;
    let identity = process_identity(child.id()).ok();
    let stdout = child.stdout.take().expect("piped stdout");
    let stderr = child.stderr.take().expect("piped stderr");
    let (out_sender, out_receiver) = mpsc::sync_channel(1);
    let (err_sender, err_receiver) = mpsc::sync_channel(1);
    thread::spawn(move || {
        let _ = out_sender.send(read_bounded(stdout, limit));
    });
    thread::spawn(move || {
        let _ = err_sender.send(read_bounded(stderr, limit));
    });
    let deadline = Instant::now() + timeout;
    let mut timed_out = false;
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break Some(status);
        }
        if Instant::now() >= deadline {
            timed_out = true;
            if let Some(identity) = identity {
                let _ = terminate_owned_process(identity);
            }
            let _ = child.kill();
            break child.wait().ok();
        }
        thread::sleep(Duration::from_millis(20));
    };
    // A provider may leave grandchildren holding inherited pipe handles after
    // its direct process exits. Never block the caller indefinitely joining a
    // reader in that case; unfinished streams are reported as truncated.
    let (stdout, out_truncated) = out_receiver
        .recv_timeout(Duration::from_millis(200))
        .unwrap_or_else(|_| (Vec::new(), true));
    let (stderr, err_truncated) = err_receiver
        .recv_timeout(Duration::from_millis(200))
        .unwrap_or_else(|_| (Vec::new(), true));
    Ok(CapturedOutput {
        status,
        stdout,
        stderr,
        timed_out,
        truncated: out_truncated || err_truncated,
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum OutputStream {
    Stdout,
    Stderr,
}

#[derive(Debug)]
pub(crate) struct ProcessOutput {
    pub stream: OutputStream,
    pub bytes: Vec<u8>,
    pub truncated: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ProcessControl {
    Continue,
    Cancel,
}

#[derive(Debug)]
pub(crate) struct ProcessResult {
    pub status: Option<ExitStatus>,
    pub timed_out: bool,
    pub cancelled: bool,
    pub output_incomplete: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProcessIdentity {
    pub pid: u32,
    pub creation_time_100ns: Option<u64>,
}

fn process_identity(pid: u32) -> std::io::Result<ProcessIdentity> {
    #[cfg(windows)]
    {
        use windows_sys::Win32::Foundation::{CloseHandle, FILETIME};
        use windows_sys::Win32::System::Threading::{
            GetProcessTimes, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
        };
        let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
        if handle.is_null() {
            return Err(std::io::Error::last_os_error());
        }
        let mut created = FILETIME::default();
        let mut exited = FILETIME::default();
        let mut kernel = FILETIME::default();
        let mut user = FILETIME::default();
        let ok =
            unsafe { GetProcessTimes(handle, &mut created, &mut exited, &mut kernel, &mut user) };
        unsafe {
            CloseHandle(handle);
        }
        if ok == 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(ProcessIdentity {
            pid,
            creation_time_100ns: Some(
                (u64::from(created.dwHighDateTime) << 32) | u64::from(created.dwLowDateTime),
            ),
        })
    }
    #[cfg(not(windows))]
    {
        Ok(ProcessIdentity {
            pid,
            creation_time_100ns: None,
        })
    }
}

/// Return the current process identity for detached worker self-validation.
pub fn current_process_identity() -> std::io::Result<ProcessIdentity> {
    process_identity(std::process::id())
}

#[cfg(test)]
mod environment_tests {
    use super::is_provider_credential_environment_name;
    use std::ffi::OsStr;

    #[test]
    fn provider_children_do_not_inherit_billing_credentials_or_secrets() {
        for name in [
            "OPENAI_API_KEY",
            "CODEX_API_KEY",
            "AWS_SECRET_ACCESS_KEY",
            "GITHUB_TOKEN",
            "APP_PASSWORD",
        ] {
            assert!(
                is_provider_credential_environment_name(OsStr::new(name)),
                "{name}"
            );
        }
        for name in ["PATH", "SYSTEMROOT", "CODEX_HOME", "OLLAMA_API_BASE"] {
            assert!(
                !is_provider_credential_environment_name(OsStr::new(name)),
                "{name}"
            );
        }
    }
}

/// Check a persisted process identity without signaling the process.
pub fn process_identity_matches(identity: ProcessIdentity) -> std::io::Result<bool> {
    if identity.pid == 0 || identity.creation_time_100ns.is_none() {
        return Ok(false);
    }
    #[cfg(windows)]
    {
        use windows_sys::Win32::Foundation::{WAIT_FAILED, WAIT_OBJECT_0, WAIT_TIMEOUT};
        use windows_sys::Win32::System::Threading::WaitForSingleObject;

        let Some(process) = open_matching_process(identity)? else {
            return Ok(false);
        };
        match unsafe { WaitForSingleObject(process.0, 0) } {
            WAIT_OBJECT_0 => Ok(false),
            WAIT_TIMEOUT => Ok(true),
            WAIT_FAILED => Err(std::io::Error::last_os_error()),
            _ => Err(std::io::Error::other("unexpected process wait result")),
        }
    }
    #[cfg(not(windows))]
    {
        match process_identity(identity.pid) {
            Ok(current) => Ok(current.creation_time_100ns == identity.creation_time_100ns),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(error) => Err(error),
        }
    }
}

#[cfg(windows)]
struct PinnedProcessHandle(windows_sys::Win32::Foundation::HANDLE);

#[cfg(windows)]
impl Drop for PinnedProcessHandle {
    fn drop(&mut self) {
        unsafe { windows_sys::Win32::Foundation::CloseHandle(self.0) };
    }
}

#[cfg(windows)]
fn open_matching_process(
    identity: ProcessIdentity,
) -> std::io::Result<Option<PinnedProcessHandle>> {
    use windows_sys::Win32::Foundation::{ERROR_INVALID_PARAMETER, FILETIME};
    use windows_sys::Win32::System::Threading::{
        GetProcessTimes, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
    };
    const SYNCHRONIZE_PROCESS: u32 = 0x0010_0000;
    if identity.pid == 0 || identity.creation_time_100ns.is_none() {
        return Ok(None);
    }
    let handle = unsafe {
        OpenProcess(
            PROCESS_QUERY_LIMITED_INFORMATION | SYNCHRONIZE_PROCESS,
            0,
            identity.pid,
        )
    };
    if handle.is_null() {
        let error = std::io::Error::last_os_error();
        if error.raw_os_error() == Some(ERROR_INVALID_PARAMETER as i32) {
            return Ok(None);
        }
        return Err(error);
    }
    let pinned = PinnedProcessHandle(handle);
    let mut created = FILETIME::default();
    let mut exited = FILETIME::default();
    let mut kernel = FILETIME::default();
    let mut user = FILETIME::default();
    if unsafe { GetProcessTimes(handle, &mut created, &mut exited, &mut kernel, &mut user) } == 0 {
        return Err(std::io::Error::last_os_error());
    }
    let actual = (u64::from(created.dwHighDateTime) << 32) | u64::from(created.dwLowDateTime);
    if Some(actual) == identity.creation_time_100ns {
        Ok(Some(pinned))
    } else {
        Ok(None)
    }
}

/// Terminate only a process whose PID and creation timestamp match the saved
/// runtime identity. On Windows, `/T` also terminates descendants.
pub fn terminate_owned_process(identity: ProcessIdentity) -> std::io::Result<bool> {
    terminate_owned_process_until(identity, Instant::now() + Duration::from_secs(2))
}

/// Initiate the existing identity-checked descendant cleanup and wait only
/// until `deadline`. A timeout is an unverifiable cleanup result; callers must
/// keep the operation fail-closed while the cleanup child may still finish.
pub(crate) fn terminate_child_tree_until(
    child: &mut std::process::Child,
    identity: ProcessIdentity,
    deadline: Instant,
) -> std::io::Result<()> {
    // Even when the deadline has just elapsed, attempt identity-checked tree
    // termination and direct-child kill. The result remains an error if the
    // caller's bound leaves no time to verify that cleanup completed.
    let tree_result = terminate_owned_process_until(identity, deadline);
    let direct_result = match child.try_wait() {
        Ok(Some(_)) => Ok(()),
        Ok(None) => child.kill().or_else(|error| match child.try_wait() {
            Ok(Some(_)) => Ok(()),
            _ => Err(error),
        }),
        Err(error) => Err(error),
    };
    let mut exited = false;
    while Instant::now() < deadline {
        match child.try_wait() {
            Ok(Some(_)) => {
                exited = true;
                break;
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(10)),
            Err(error) => return Err(error),
        }
    }
    if !exited {
        return Err(std::io::Error::new(
            std::io::ErrorKind::TimedOut,
            "owned process did not exit before cleanup deadline",
        ));
    }
    match tree_result? {
        true => {}
        false => {
            return Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "owned process-tree identity was no longer verifiable",
            ))
        }
    }
    direct_result?;
    Ok(())
}

fn terminate_owned_process_until(
    identity: ProcessIdentity,
    deadline: Instant,
) -> std::io::Result<bool> {
    #[cfg(windows)]
    {
        let Some(_pinned_process) = open_matching_process(identity)? else {
            return Ok(false);
        };
        let root = std::env::var_os("SystemRoot").ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::NotFound, "SystemRoot is unavailable")
        })?;
        let taskkill = Path::new(&root).join("System32/taskkill.exe");
        let pid = identity.pid.to_string();
        let mut child = command(taskkill)
            .args(["/PID", pid.as_str(), "/T", "/F"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()?;
        loop {
            if let Some(status) = child.try_wait()? {
                return Ok(status.success());
            }
            if Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.try_wait();
                return Err(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    "taskkill process-tree cleanup exceeded its deadline",
                ));
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }
    #[cfg(not(windows))]
    {
        let _ = (identity, deadline);
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "identity-checked process-tree cleanup is only available on Windows",
        ))
    }
}

/// Start an owned native worker detached from the current CLI process. The
/// caller must persist the returned identity before treating work as started.
pub fn spawn_detached_worker(
    program: &Path,
    args: &[String],
    cwd: &Path,
) -> std::io::Result<ProcessIdentity> {
    spawn_worker(program, args, cwd, true)
}

/// Start an owned worker that remains inside the current host job. The caller
/// must remain alive and supervise durable batch state until terminal status.
pub fn spawn_hosted_worker(
    program: &Path,
    args: &[String],
    cwd: &Path,
) -> std::io::Result<ProcessIdentity> {
    spawn_worker(program, args, cwd, false)
}

fn spawn_worker(
    program: &Path,
    args: &[String],
    cwd: &Path,
    breakaway: bool,
) -> std::io::Result<ProcessIdentity> {
    #[cfg(not(windows))]
    let _ = breakaway;
    if !program.is_absolute() || !program.is_file() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "worker executable must be an existing absolute file",
        ));
    }
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        if args.iter().any(|argument| argument.contains('\0'))
            || program.as_os_str().encode_wide().any(|unit| unit == 0)
            || cwd.as_os_str().encode_wide().any(|unit| unit == 0)
        {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "worker executable, arguments, and working directory cannot contain NUL",
            ));
        }
    }
    #[cfg(windows)]
    if !program
        .extension()
        .and_then(OsStr::to_str)
        .is_some_and(|extension| extension.eq_ignore_ascii_case("exe"))
    {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "detached workers must be native executables",
        ));
    }
    #[cfg(windows)]
    {
        return spawn_detached_worker_windows(program, args, cwd, breakaway);
    }
    #[cfg(not(windows))]
    {
        let mut command = command(program);
        command
            .args(args)
            .current_dir(cwd)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        strip_provider_credentials(&mut command);
        let mut child = command.spawn()?;
        match process_identity(child.id()) {
            Ok(identity) => Ok(identity),
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                Err(error)
            }
        }
    }
}

#[cfg(windows)]
mod suspended_worker_launch {
    use std::io;
    use windows_sys::Win32::Foundation::{GetLastError, HANDLE, WAIT_EVENT};
    use windows_sys::Win32::System::Threading::{TerminateProcess, WaitForSingleObject};

    const CLEANUP_WAIT_MS: u32 = 2_000;
    const WAIT_OBJECT_0_VALUE: WAIT_EVENT = 0;
    const WAIT_TIMEOUT_VALUE: WAIT_EVENT = 258;
    const WAIT_FAILED_VALUE: WAIT_EVENT = u32::MAX;

    fn last_error() -> io::Error {
        io::Error::from_raw_os_error(unsafe { GetLastError() } as i32)
    }

    pub(super) fn cleanup_result(
        pid: u32,
        terminate: Result<(), io::Error>,
        wait: Result<WAIT_EVENT, io::Error>,
    ) -> io::Result<()> {
        let terminate_error = terminate.err();
        let termination_detail = terminate_error
            .as_ref()
            .map(|error| format!("; TerminateProcess failed: {error}"))
            .unwrap_or_default();
        match wait {
            Ok(WAIT_OBJECT_0_VALUE) => match terminate_error {
                None => Ok(()),
                Some(error) => Err(io::Error::new(
                    error.kind(),
                    format!("worker pid={pid} was signaled but TerminateProcess failed: {error}"),
                )),
            },
            Ok(WAIT_TIMEOUT_VALUE) => Err(io::Error::new(
                io::ErrorKind::TimedOut,
                format!("worker cleanup timed out after {CLEANUP_WAIT_MS} ms (pid={pid}){termination_detail}"),
            )),
            Ok(WAIT_FAILED_VALUE) => Err(io::Error::new(
                io::ErrorKind::Other,
                format!("worker cleanup wait failed (pid={pid}){termination_detail}"),
            )),
            Ok(status) => Err(io::Error::new(
                io::ErrorKind::Other,
                format!("worker cleanup returned unexpected wait status {status} (pid={pid}){termination_detail}"),
            )),
            Err(error) => Err(io::Error::new(
                error.kind(),
                format!("worker cleanup wait API failed (pid={pid}): {error}{termination_detail}"),
            )),
        }
    }

    fn terminate_and_wait(process: HANDLE, pid: u32) -> io::Result<()> {
        let terminate = if unsafe { TerminateProcess(process, 1) } != 0 {
            Ok(())
        } else {
            // Read last error before WaitForSingleObject can replace it.
            Err(last_error())
        };
        let wait = match unsafe { WaitForSingleObject(process, CLEANUP_WAIT_MS) } {
            WAIT_OBJECT_0_VALUE => Ok(WAIT_OBJECT_0_VALUE),
            WAIT_TIMEOUT_VALUE => Ok(WAIT_TIMEOUT_VALUE),
            WAIT_FAILED_VALUE => Err(last_error()),
            status => Ok(status),
        };
        cleanup_result(pid, terminate, wait)
    }

    pub(super) struct CleanupGuard {
        process: HANDLE,
        pid: u32,
        armed: bool,
    }

    impl CleanupGuard {
        pub(super) fn new(process: HANDLE, pid: u32) -> Self {
            Self {
                process,
                pid,
                armed: true,
            }
        }

        pub(super) fn reject(&mut self, original: io::Error) -> io::Error {
            // Disarm before explicit cleanup so Drop cannot terminate twice.
            self.armed = false;
            match terminate_and_wait(self.process, self.pid) {
                Ok(()) => io::Error::new(
                    original.kind(),
                    format!("{original}; worker pid={} cleanup completed after launch rejection", self.pid),
                ),
                Err(cleanup) => io::Error::new(
                    original.kind(),
                    format!("{original}; worker pid={} cleanup was not confirmed; manual reconciliation required: {cleanup}", self.pid),
                ),
            }
        }

        pub(super) fn disarm(&mut self) {
            self.armed = false;
        }
    }

    impl Drop for CleanupGuard {
        fn drop(&mut self) {
            if self.armed {
                self.armed = false;
                if let Err(error) = terminate_and_wait(self.process, self.pid) {
                    use std::io::Write as _;
                    let _ = writeln!(
                        std::io::stderr().lock(),
                        "worker pid={} suspended-child panic cleanup was not confirmed; manual reconciliation required: {error}",
                        self.pid,
                    );
                }
            }
        }
    }

    pub(super) fn validate_then_resume<V, R>(
        guard: &mut CleanupGuard,
        validate: V,
        resume: R,
    ) -> io::Result<super::ProcessIdentity>
    where
        V: FnOnce() -> io::Result<super::ProcessIdentity>,
        R: FnOnce() -> io::Result<()>,
    {
        // One explicit result/match owns all rejection cleanup. Panics before
        // disarm are covered by the one-shot guard's bounded Drop path.
        let result = validate().and_then(|identity| resume().map(|()| identity));
        match result {
            Ok(identity) => {
                guard.disarm();
                Ok(identity)
            }
            Err(error) => Err(guard.reject(error)),
        }
    }
}

#[cfg(windows)]
fn spawn_detached_worker_windows(
    program: &Path,
    args: &[String],
    cwd: &Path,
    breakaway: bool,
) -> std::io::Result<ProcessIdentity> {
    spawn_detached_worker_windows_with_validator(
        program,
        args,
        cwd,
        breakaway,
        validate_detached_worker,
    )
}

#[cfg(windows)]
fn validate_detached_worker(
    process: windows_sys::Win32::Foundation::HANDLE,
    pid: u32,
    breakaway: bool,
    parent_in_job: bool,
) -> std::io::Result<ProcessIdentity> {
    use windows_sys::Win32::Foundation::{GetLastError, HANDLE};
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn IsProcessInJob(process: HANDLE, job: HANDLE, result: *mut i32) -> i32;
    }
    if breakaway {
        let mut child_in_job = 0;
        if unsafe { IsProcessInJob(process, std::ptr::null_mut(), &mut child_in_job) } == 0 {
            let error = std::io::Error::from_raw_os_error(unsafe { GetLastError() } as i32);
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                format!(
                    "Windows could not query detached worker job membership (parent_in_job={parent_in_job}): {error}; use foreground-host mode when available"
                ),
            ));
        }
        if child_in_job != 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                format!(
                    "Windows confirmed the detached worker remains in a job (parent_in_job={parent_in_job}, child_in_job=true); use foreground-host mode when available"
                ),
            ));
        }
    }
    process_identity(pid)
}

#[cfg(windows)]
fn spawn_detached_worker_windows_with_validator<V>(
    program: &Path,
    args: &[String],
    cwd: &Path,
    breakaway: bool,
    validator: V,
) -> std::io::Result<ProcessIdentity>
where
    V: FnOnce(
        windows_sys::Win32::Foundation::HANDLE,
        u32,
        bool,
        bool,
    ) -> std::io::Result<ProcessIdentity>,
{
    use std::mem::{size_of, size_of_val};
    use std::os::windows::ffi::OsStrExt;
    use suspended_worker_launch::{validate_then_resume, CleanupGuard};
    use windows_sys::Win32::Foundation::{CloseHandle, GetLastError, HANDLE, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::Security::SECURITY_ATTRIBUTES;
    use windows_sys::Win32::Storage::FileSystem::{
        CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
    };
    use windows_sys::Win32::System::Threading::{
        CreateProcessW, DeleteProcThreadAttributeList, GetCurrentProcess,
        InitializeProcThreadAttributeList, ResumeThread, UpdateProcThreadAttribute,
        CREATE_BREAKAWAY_FROM_JOB, CREATE_NEW_PROCESS_GROUP, CREATE_NO_WINDOW, CREATE_SUSPENDED,
        CREATE_UNICODE_ENVIRONMENT, EXTENDED_STARTUPINFO_PRESENT, PROCESS_INFORMATION,
        PROC_THREAD_ATTRIBUTE_HANDLE_LIST, STARTF_USESTDHANDLES, STARTUPINFOEXW,
    };
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn IsProcessInJob(process: HANDLE, job: HANDLE, result: *mut i32) -> i32;
    }

    struct OwnedHandle(HANDLE);
    impl Drop for OwnedHandle {
        fn drop(&mut self) {
            if !self.0.is_null() && self.0 != INVALID_HANDLE_VALUE {
                unsafe { CloseHandle(self.0) };
            }
        }
    }
    struct AttributeList(*mut core::ffi::c_void);
    impl Drop for AttributeList {
        fn drop(&mut self) {
            if !self.0.is_null() {
                unsafe { DeleteProcThreadAttributeList(self.0.cast()) };
            }
        }
    }
    fn last_error() -> std::io::Error {
        std::io::Error::from_raw_os_error(unsafe { GetLastError() } as i32)
    }
    fn nul_handle(read: bool) -> std::io::Result<OwnedHandle> {
        let name: Vec<u16> = "\\\\.\\NUL".encode_utf16().chain([0]).collect();
        let mut security = SECURITY_ATTRIBUTES {
            nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: std::ptr::null_mut(),
            bInheritHandle: 1,
        };
        let handle = unsafe {
            CreateFileW(
                name.as_ptr(),
                if read { 0x8000_0000 } else { 0x4000_0000 },
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                &mut security,
                OPEN_EXISTING,
                FILE_ATTRIBUTE_NORMAL,
                std::ptr::null_mut(),
            )
        };
        if handle.is_null() || handle == INVALID_HANDLE_VALUE {
            Err(last_error())
        } else {
            Ok(OwnedHandle(handle))
        }
    }

    let stdin = nul_handle(true)?;
    let stdout = nul_handle(false)?;
    let stderr = nul_handle(false)?;
    let handles = [stdin.0, stdout.0, stderr.0];

    let mut attribute_bytes = 0usize;
    unsafe {
        InitializeProcThreadAttributeList(std::ptr::null_mut(), 1, 0, &mut attribute_bytes);
    }
    if attribute_bytes == 0 {
        return Err(last_error());
    }
    let words = attribute_bytes.div_ceil(size_of::<usize>());
    let mut attribute_storage = vec![0usize; words];
    let attribute_ptr = attribute_storage.as_mut_ptr().cast();
    if unsafe { InitializeProcThreadAttributeList(attribute_ptr, 1, 0, &mut attribute_bytes) } == 0
    {
        return Err(last_error());
    }
    let _attributes = AttributeList(attribute_ptr);
    if unsafe {
        UpdateProcThreadAttribute(
            attribute_ptr,
            0,
            PROC_THREAD_ATTRIBUTE_HANDLE_LIST as usize,
            handles.as_ptr().cast(),
            size_of_val(&handles),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    } == 0
    {
        return Err(last_error());
    }

    let mut startup = STARTUPINFOEXW::default();
    startup.StartupInfo.cb = size_of::<STARTUPINFOEXW>() as u32;
    startup.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
    startup.StartupInfo.hStdInput = stdin.0;
    startup.StartupInfo.hStdOutput = stdout.0;
    startup.StartupInfo.hStdError = stderr.0;
    startup.lpAttributeList = attribute_ptr;

    let mut command_line = Vec::<u16>::new();
    command_line
        .extend(quote_windows_argument(&program.as_os_str().to_string_lossy()).encode_utf16());
    for argument in args {
        command_line.push(u16::from(b' '));
        command_line.extend(quote_windows_argument(argument).encode_utf16());
    }
    command_line.push(0);
    let mut application = program
        .as_os_str()
        .encode_wide()
        .chain([0])
        .collect::<Vec<_>>();
    let mut directory = cwd.as_os_str().encode_wide().chain([0]).collect::<Vec<_>>();
    let mut environment = filtered_windows_environment();
    let mut process = PROCESS_INFORMATION::default();
    let mut parent_in_job = 0;
    if breakaway
        && unsafe {
            IsProcessInJob(
                GetCurrentProcess(),
                std::ptr::null_mut(),
                &mut parent_in_job,
            )
        } == 0
    {
        return Err(last_error());
    }
    let breakaway_flag = if breakaway && parent_in_job != 0 {
        CREATE_BREAKAWAY_FROM_JOB
    } else {
        0
    };
    let created = unsafe {
        CreateProcessW(
            application.as_mut_ptr(),
            command_line.as_mut_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            1,
            CREATE_UNICODE_ENVIRONMENT
                | EXTENDED_STARTUPINFO_PRESENT
                | breakaway_flag
                | CREATE_NEW_PROCESS_GROUP
                | CREATE_NO_WINDOW
                | CREATE_SUSPENDED,
            environment.as_mut_ptr().cast(),
            directory.as_mut_ptr(),
            (&startup as *const STARTUPINFOEXW).cast(),
            &mut process,
        )
    };
    if created == 0 {
        let error = last_error();
        if breakaway_flag != 0 {
            return Err(std::io::Error::new(
                error.kind(),
                format!(
                    "Windows did not allow the detached worker to break away from the parent job; use the foreground-host mode when available: {error}"
                ),
            ));
        }
        return Err(error);
    }
    let process_handle = OwnedHandle(process.hProcess);
    let thread_handle = OwnedHandle(process.hThread);
    let mut cleanup = CleanupGuard::new(process_handle.0, process.dwProcessId);
    validate_then_resume(
        &mut cleanup,
        || {
            validator(
                process_handle.0,
                process.dwProcessId,
                breakaway,
                parent_in_job != 0,
            )
        },
        || {
            let previous_count = unsafe { ResumeThread(thread_handle.0) };
            if previous_count == u32::MAX {
                Err(last_error())
            } else if previous_count != 1 {
                Err(std::io::Error::new(
                    std::io::ErrorKind::Other,
                    format!("worker thread resumed with unexpected suspend count {previous_count}"),
                ))
            } else {
                Ok(())
            }
        },
    )
}

#[cfg(windows)]
fn quote_windows_argument(argument: &str) -> String {
    if !argument.is_empty() && !argument.chars().any(|ch| ch.is_whitespace() || ch == '"') {
        return argument.to_owned();
    }
    let mut quoted = String::from("\"");
    let mut slashes = 0usize;
    for ch in argument.chars() {
        match ch {
            '\\' => slashes += 1,
            '"' => {
                quoted.extend(std::iter::repeat('\\').take(slashes * 2 + 1));
                quoted.push('"');
                slashes = 0;
            }
            _ => {
                quoted.extend(std::iter::repeat('\\').take(slashes));
                slashes = 0;
                quoted.push(ch);
            }
        }
    }
    quoted.extend(std::iter::repeat('\\').take(slashes * 2));
    quoted.push('"');
    quoted
}

#[cfg(windows)]
fn filtered_windows_environment() -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;
    let mut entries = std::env::vars_os()
        .filter(|(name, _)| !is_provider_credential_environment_name(name))
        .map(|(name, value)| (name.to_string_lossy().to_ascii_uppercase(), name, value))
        .collect::<Vec<_>>();
    entries.sort_by(|left, right| left.0.cmp(&right.0));
    let mut block = Vec::new();
    for (_, name, value) in entries {
        block.extend(name.encode_wide());
        block.push(u16::from(b'='));
        block.extend(value.encode_wide());
        block.push(0);
    }
    if block.is_empty() {
        block.push(0);
    }
    block.push(0);
    block
}

#[cfg(all(test, windows))]
mod windows_process_tests {
    use super::{
        process_identity, process_identity_matches, quote_windows_argument, spawn_detached_worker,
        terminate_owned_process,
    };
    use std::path::Path;

    #[test]
    fn cleanup_result_requires_termination_and_bounded_wait_confirmation() {
        use super::suspended_worker_launch::cleanup_result;
        use windows_sys::Win32::Foundation::WAIT_EVENT;

        assert!(cleanup_result(17, Ok(()), Ok(0 as WAIT_EVENT)).is_ok());
        let termination = cleanup_result(
            17,
            Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "access denied",
            )),
            Ok(0 as WAIT_EVENT),
        )
        .unwrap_err();
        assert!(termination.to_string().contains("TerminateProcess failed"));
        let timeout = cleanup_result(17, Ok(()), Ok(258 as WAIT_EVENT)).unwrap_err();
        assert_eq!(timeout.kind(), std::io::ErrorKind::TimedOut);
        assert!(timeout.to_string().contains("2000 ms"));
        assert!(timeout.to_string().contains("pid=17"));
    }

    fn marker_worker() -> (std::path::PathBuf, std::path::PathBuf, Vec<String>) {
        use std::time::{SystemTime, UNIX_EPOCH};
        let root = std::env::var_os("SystemRoot").unwrap();
        let powershell = Path::new(&root).join("System32/WindowsPowerShell/v1.0/powershell.exe");
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let marker = std::env::temp_dir().join(format!("fabrica-suspended-{stamp}.txt"));
        assert!(
            !marker.exists(),
            "unique marker unexpectedly already exists"
        );
        let marker_literal = marker.to_string_lossy().replace('\'', "''");
        let script = format!(
            "[IO.File]::WriteAllText('{marker_literal}', 'started'); Start-Sleep -Seconds 20"
        );
        let args = vec![
            "-NoProfile".to_owned(),
            "-NonInteractive".to_owned(),
            "-Command".to_owned(),
            script.to_owned(),
        ];
        (marker, powershell, args)
    }

    fn marker_appears_within(marker: &Path, timeout: std::time::Duration) -> bool {
        let deadline = std::time::Instant::now() + timeout;
        while std::time::Instant::now() < deadline {
            if marker.exists() {
                return true;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        marker.exists()
    }

    #[test]
    fn production_create_path_refusal_prevents_marker_and_confirms_cleanup() {
        use super::spawn_detached_worker_windows_with_validator;
        use std::cell::Cell;
        use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, WAIT_EVENT};
        use windows_sys::Win32::System::Threading::{
            OpenProcess, TerminateProcess, WaitForSingleObject, PROCESS_TERMINATE,
        };

        let (marker, program, args) = marker_worker();
        let called = Cell::new(false);
        let user_code_seen_before_refusal = Cell::new(false);
        let child_pid = Cell::new(0u32);
        let wait_handle = Cell::new(std::ptr::null_mut::<std::ffi::c_void>());
        let result = spawn_detached_worker_windows_with_validator(
            &program,
            &args,
            &std::env::current_dir().unwrap(),
            false,
            |_, pid, _, _| {
                called.set(true);
                child_pid.set(pid);
                // If CREATE_SUSPENDED is removed from production, allow the
                // tiny marker command a bounded chance to expose that regression.
                user_code_seen_before_refusal.set(marker_appears_within(
                    &marker,
                    std::time::Duration::from_secs(1),
                ));
                wait_handle.set(unsafe { OpenProcess(0x0010_0000, 0, pid) });
                Err(std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    "synthetic prevalidation refusal",
                ))
            },
        );
        let error = result.unwrap_err();
        assert!(
            called.get(),
            "injected validation was not reached after CreateProcessW"
        );
        assert!(
            !user_code_seen_before_refusal.get(),
            "worker user code ran before validation refused it"
        );
        assert!(error
            .to_string()
            .contains("synthetic prevalidation refusal"));
        assert!(error
            .to_string()
            .contains(&format!("pid={}", child_pid.get())));

        let process: HANDLE = wait_handle.get();
        if process.is_null() {
            let cleanup_handle =
                unsafe { OpenProcess(PROCESS_TERMINATE | 0x0010_0000, 0, child_pid.get()) };
            if !cleanup_handle.is_null() {
                unsafe {
                    TerminateProcess(cleanup_handle, 1);
                    WaitForSingleObject(cleanup_handle, 1_000);
                    CloseHandle(cleanup_handle);
                }
            }
            panic!("could not retain rejected child for exit verification");
        }
        let mut waited = unsafe { WaitForSingleObject(process, 1_000) };
        if waited != 0 as WAIT_EVENT {
            let cleanup_handle =
                unsafe { OpenProcess(PROCESS_TERMINATE | 0x0010_0000, 0, child_pid.get()) };
            if !cleanup_handle.is_null() {
                unsafe {
                    TerminateProcess(cleanup_handle, 1);
                    WaitForSingleObject(cleanup_handle, 1_000);
                    waited = WaitForSingleObject(process, 1_000);
                    CloseHandle(cleanup_handle);
                }
            }
        }
        unsafe { CloseHandle(process) };
        assert_eq!(
            waited, 0 as WAIT_EVENT,
            "rejected production child was not signaled"
        );
        assert!(
            !marker.exists(),
            "user-code marker appeared before validation"
        );
        let _ = std::fs::remove_file(marker);
    }

    #[test]
    fn production_create_path_resumes_only_after_valid_identity_and_runs_marker() {
        use super::{
            process_identity, spawn_detached_worker_windows_with_validator, terminate_owned_process,
        };
        use std::time::{Duration, Instant};

        let (marker, program, args) = marker_worker();
        let identity = spawn_detached_worker_windows_with_validator(
            &program,
            &args,
            &std::env::current_dir().unwrap(),
            false,
            |_, pid, _, _| {
                assert!(
                    !marker_appears_within(&marker, Duration::from_secs(1)),
                    "worker user code ran before identity validation completed"
                );
                process_identity(pid)
            },
        )
        .expect("valid identity should permit exactly one resume");
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline && !marker.exists() {
            std::thread::sleep(Duration::from_millis(20));
        }
        let marker_written = marker.exists();
        let cleanup = terminate_owned_process(identity);
        assert!(
            marker_written,
            "validated child did not execute its marker code"
        );
        cleanup.expect("test worker cleanup should complete");
        let _ = std::fs::remove_file(marker);
    }

    #[test]
    fn native_worker_arguments_use_windows_command_line_quoting() {
        assert_eq!(quote_windows_argument("plain"), "plain");
        assert_eq!(quote_windows_argument(""), "\"\"");
        assert_eq!(quote_windows_argument("alpha beta"), "\"alpha beta\"");
        assert_eq!(
            quote_windows_argument("C:\\Program Files\\"),
            "\"C:\\Program Files\\\\\""
        );
        assert_eq!(quote_windows_argument("say\"hi"), "\"say\\\"hi\"");
    }

    #[test]
    fn exited_process_identity_does_not_match_after_its_pid_stops() {
        let root = std::env::var_os("SystemRoot").unwrap();
        let powershell = Path::new(&root).join("System32/WindowsPowerShell/v1.0/powershell.exe");
        let mut child = std::process::Command::new(powershell)
            .args([
                "-NoProfile",
                "-NonInteractive",
                "-Command",
                "Start-Sleep -Seconds 30",
            ])
            .spawn()
            .unwrap();
        let identity = process_identity(child.id()).unwrap();
        assert!(process_identity_matches(identity).unwrap());
        child.kill().unwrap();
        child.wait().unwrap();
        assert!(!process_identity_matches(identity).unwrap());
    }

    #[test]
    fn detached_launch_fails_closed_if_job_breakaway_is_denied() {
        let root = std::env::var_os("SystemRoot").unwrap();
        let powershell = Path::new(&root).join("System32/WindowsPowerShell/v1.0/powershell.exe");
        let args = vec![
            "-NoProfile".into(),
            "-NonInteractive".into(),
            "-Command".into(),
            "Start-Sleep -Seconds 5".into(),
        ];
        match spawn_detached_worker(&powershell, &args, &std::env::current_dir().unwrap()) {
            Err(error) => {
                assert!(
                    (error.to_string().contains("break away from the parent job")
                        || error.to_string().contains("remains in a job")
                        || error
                            .to_string()
                            .contains("could not query detached worker job membership"))
                        && error.to_string().contains("foreground-host mode"),
                    "detached launch failed without actionable breakaway guidance: {error}"
                );
            }
            Ok(identity) => {
                use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
                use windows_sys::Win32::System::Threading::{
                    OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
                };
                #[link(name = "kernel32")]
                unsafe extern "system" {
                    fn IsProcessInJob(process: HANDLE, job: HANDLE, result: *mut i32) -> i32;
                }
                let process =
                    unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, identity.pid) };
                assert!(
                    !process.is_null(),
                    "launched worker exited before its job check"
                );
                let mut in_job = 0;
                let queried = unsafe { IsProcessInJob(process, std::ptr::null_mut(), &mut in_job) };
                unsafe { CloseHandle(process) };
                assert_ne!(
                    queried, 0,
                    "could not verify detached worker job membership"
                );
                assert_eq!(in_job, 0, "detached worker remained in a host job");
                terminate_owned_process(identity).unwrap();
            }
        }
    }
}

/// Return Git's canonical object id for bounded bytes without embedding a
/// cryptographic implementation in the runner.
pub fn git_object_hash(bytes: &[u8]) -> std::io::Result<String> {
    const MAX_HASH_INPUT: usize = 64 * 1024 * 1024;
    if bytes.len() > MAX_HASH_INPUT {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "git hash input exceeds 64 MiB",
        ));
    }
    let mut child = command("git")
        .args(["hash-object", "--stdin"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()?;
    child.stdin.take().expect("piped stdin").write_all(bytes)?;
    let output = child.wait_with_output()?;
    if !output.status.success() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::Other,
            "git could not hash the source snapshot",
        ));
    }
    let hash = String::from_utf8(output.stdout).map_err(|_| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "git returned an invalid object id",
        )
    })?;
    let hash = hash.trim();
    if !matches!(hash.len(), 40 | 64) || !hash.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "git returned an invalid object id",
        ));
    }
    Ok(hash.to_ascii_lowercase())
}

pub(crate) fn spawn_interactive(
    program: &Path,
    args: &[String],
    cwd: &Path,
    env: &[(OsString, OsString)],
    remove_env: &[&str],
) -> std::io::Result<(std::process::Child, ProcessIdentity)> {
    let mut command = configured_command(program, args)?;
    command
        .current_dir(cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for name in remove_env {
        if *name == "*" {
            command.env_clear();
            break;
        }
    }
    if !remove_env.contains(&"*") {
        for name in remove_env {
            command.env_remove(name);
        }
    }
    strip_provider_credentials(&mut command);
    for (name, value) in env {
        command.env(name, value);
    }
    let mut child = command.spawn()?;
    match process_identity(child.id()) {
        Ok(identity) => Ok((child, identity)),
        Err(error) => {
            let _ = child.kill();
            let _ = child.wait();
            Err(error)
        }
    }
}

pub(crate) fn terminate_child_tree(child: &mut std::process::Child, identity: ProcessIdentity) {
    let _ = terminate_owned_process(identity);
    let _ = child.kill();
}

/// Run a configured executable without a shell, stream bounded output, and
/// terminate the child on cancellation or deadline. The sink must redact any
/// configured secrets before displaying or persisting output.
pub(crate) fn run_streamed(
    program: &Path,
    args: &[String],
    cwd: &Path,
    env: &[(OsString, OsString)],
    remove_env: &[&str],
    timeout: Duration,
    output_limit: usize,
    cancel: &AtomicBool,
    mut on_spawn: impl FnMut(ProcessIdentity) -> ProcessControl,
    mut sink: impl FnMut(ProcessOutput) -> ProcessControl,
) -> std::io::Result<ProcessResult> {
    // The timeout covers process creation and the durable on_spawn callback too.
    // In particular, a slow persistence callback must not renew a paid-run deadline.
    let deadline = Instant::now() + timeout;
    let mut command = configured_command(program, args)?;
    command
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if remove_env.contains(&"*") {
        command.env_clear();
    } else {
        for name in remove_env {
            command.env_remove(name);
        }
    }
    strip_provider_credentials(&mut command);
    for (name, value) in env {
        command.env(name, value);
    }
    let mut child = command.spawn()?;
    let identity = match process_identity(child.id()) {
        Ok(identity) => identity,
        Err(error) => {
            let _ = child.kill();
            let _ = child.wait();
            return Err(error);
        }
    };
    let mut cancelled = on_spawn(identity) == ProcessControl::Cancel;
    let (sender, receiver) = mpsc::sync_channel(32);
    spawn_output_reader(
        child.stdout.take().expect("piped stdout"),
        OutputStream::Stdout,
        output_limit,
        sender.clone(),
    );
    spawn_output_reader(
        child.stderr.take().expect("piped stderr"),
        OutputStream::Stderr,
        output_limit,
        sender,
    );
    let mut timed_out = Instant::now() >= deadline;
    if cancelled || cancel.load(Ordering::Acquire) || timed_out {
        cancelled |= cancel.load(Ordering::Acquire);
        let _ = terminate_owned_process(identity);
        let _ = child.kill();
    }
    let mut total = 0usize;
    let mut output_incomplete = false;
    let status = loop {
        while let Ok(output) = receiver.try_recv() {
            if cancel.load(Ordering::Acquire) {
                cancelled = true;
                break;
            }
            if Instant::now() >= deadline {
                timed_out = true;
                break;
            }
            total = total.saturating_add(output.bytes.len());
            if sink(output) == ProcessControl::Cancel {
                cancelled = true;
                break;
            }
            if Instant::now() >= deadline {
                timed_out = true;
                break;
            }
        }
        if cancelled || cancel.load(Ordering::Acquire) {
            cancelled = true;
            let _ = terminate_owned_process(identity);
            let _ = child.kill();
            break child.wait().ok();
        }
        if timed_out || Instant::now() >= deadline {
            timed_out = true;
            let _ = terminate_owned_process(identity);
            let _ = child.kill();
            break child.wait().ok();
        }
        if let Some(done) = child.try_wait()? {
            break Some(done);
        }
        if total >= output_limit {
            thread::sleep(Duration::from_millis(20));
        } else if let Ok(output) = receiver.recv_timeout(Duration::from_millis(20)) {
            if cancel.load(Ordering::Acquire) {
                cancelled = true;
                let _ = terminate_owned_process(identity);
                let _ = child.kill();
                break child.wait().ok();
            }
            if Instant::now() >= deadline {
                timed_out = true;
                let _ = terminate_owned_process(identity);
                let _ = child.kill();
                break child.wait().ok();
            }
            total = total.saturating_add(output.bytes.len());
            if sink(output) == ProcessControl::Cancel {
                cancelled = true;
                let _ = terminate_owned_process(identity);
                let _ = child.kill();
                break child.wait().ok();
            }
        }
    };
    // The child can exit before the reader threads have forwarded their final
    // buffered bytes. Wait for both pipes to reach EOF, but never beyond the
    // original run deadline or a short cleanup grace (including inherited pipe
    // handles held by descendants).
    let drain_deadline = deadline.min(Instant::now() + Duration::from_millis(500));
    loop {
        if cancel.load(Ordering::Acquire) {
            cancelled = true;
            output_incomplete = true;
            break;
        }
        let now = Instant::now();
        if now >= deadline {
            timed_out = true;
            output_incomplete = true;
            break;
        }
        if now >= drain_deadline {
            output_incomplete = true;
            let _ = terminate_owned_process(identity);
            break;
        }
        let wait = (drain_deadline - now).min(Duration::from_millis(20));
        match receiver.recv_timeout(wait) {
            Ok(output) => {
                if sink(output) == ProcessControl::Cancel {
                    cancelled = true;
                    output_incomplete = true;
                    let _ = terminate_owned_process(identity);
                    break;
                }
                if Instant::now() >= deadline {
                    timed_out = true;
                    output_incomplete = true;
                    break;
                }
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
            Err(mpsc::RecvTimeoutError::Timeout) => continue,
        }
    }
    Ok(ProcessResult {
        status,
        timed_out,
        cancelled,
        output_incomplete,
    })
}

fn spawn_output_reader(
    pipe: impl std::io::Read + Send + 'static,
    stream: OutputStream,
    per_stream_limit: usize,
    sender: mpsc::SyncSender<ProcessOutput>,
) {
    thread::spawn(move || {
        let mut reader = BufReader::with_capacity(8 * 1024, pipe);
        let mut saved = 0usize;
        let mut chunk = [0u8; 8 * 1024];
        loop {
            let read = reader.read(&mut chunk).unwrap_or(0);
            if read == 0 {
                break;
            }
            let available = per_stream_limit.saturating_sub(saved);
            let take = available.min(read);
            saved = saved.saturating_add(take);
            let truncated = take < read;
            if take > 0 {
                if sender
                    .send(ProcessOutput {
                        stream,
                        bytes: chunk[..take].to_vec(),
                        truncated,
                    })
                    .is_err()
                {
                    break;
                }
            }
        }
    });
}

fn read_bounded(mut reader: impl Read, limit: usize) -> (Vec<u8>, bool) {
    let mut saved = Vec::with_capacity(limit.min(16 * 1024));
    let mut truncated = false;
    let mut chunk = [0u8; 8192];
    loop {
        match reader.read(&mut chunk) {
            Ok(0) | Err(_) => break,
            Ok(count) => {
                let take = count.min(limit.saturating_sub(saved.len()));
                saved.extend_from_slice(&chunk[..take]);
                truncated |= take < count;
            }
        }
    }
    (saved, truncated)
}

#[cfg(all(test, windows))]
mod deadline_tests {
    use super::*;
    use std::sync::atomic::AtomicBool;
    use std::sync::{Arc, Mutex};

    #[test]
    fn streamed_process_waits_for_final_output_tail_before_success() {
        let directory = tempfile::tempdir().unwrap();
        let script = directory.path().join("final-output.ps1");
        std::fs::write(
            &script,
            "[Console]::Out.Write(('x' * 262144) + 'FINAL-TAIL')",
        )
        .unwrap();
        let powershell = PathBuf::from(std::env::var_os("SystemRoot").unwrap())
            .join("System32/WindowsPowerShell/v1.0/powershell.exe");
        let output = Arc::new(Mutex::new(Vec::new()));
        let output_in_sink = Arc::clone(&output);
        let result = run_streamed(
            &powershell,
            &[
                "-NoProfile".into(),
                "-NonInteractive".into(),
                "-File".into(),
                script.to_string_lossy().into_owned(),
            ],
            directory.path(),
            &[],
            &[],
            Duration::from_secs(10),
            512 * 1024,
            &AtomicBool::new(false),
            |_| ProcessControl::Continue,
            move |chunk| {
                output_in_sink.lock().unwrap().extend(chunk.bytes);
                ProcessControl::Continue
            },
        )
        .unwrap();
        assert!(result.status.is_some_and(|status| status.success()));
        assert!(!result.output_incomplete);
        assert!(
            output.lock().unwrap().ends_with(b"FINAL-TAIL"),
            "the final child output chunk must be delivered before success"
        );
    }

    #[test]
    fn streamed_timeout_includes_durable_spawn_callback() {
        let directory = tempfile::tempdir().unwrap();
        let marker = directory.path().join("child-finished.txt");
        let script = directory.path().join("delayed-child.ps1");
        std::fs::write(
            &script,
            format!(
                "Start-Sleep -Milliseconds 300; [IO.File]::WriteAllText('{}','finished')",
                marker.display()
            ),
        )
        .unwrap();
        let powershell = PathBuf::from(std::env::var_os("SystemRoot").unwrap())
            .join("System32/WindowsPowerShell/v1.0/powershell.exe");
        let callback_finished = Arc::new(Mutex::new(None));
        let callback_finished_in = Arc::clone(&callback_finished);
        let marker_in_callback = marker.clone();
        let result = run_streamed(
            &powershell,
            &[
                "-NoProfile".into(),
                "-NonInteractive".into(),
                "-File".into(),
                script.to_string_lossy().into_owned(),
            ],
            directory.path(),
            &[],
            &[],
            Duration::from_millis(600),
            1024,
            &AtomicBool::new(false),
            move |_| {
                // Wait until the child effect is observed, then keep the durable
                // callback blocked beyond the original run deadline.
                let callback_started = Instant::now();
                while !marker_in_callback.exists()
                    && callback_started.elapsed() < Duration::from_secs(5)
                {
                    thread::sleep(Duration::from_millis(10));
                }
                let remaining =
                    Duration::from_millis(800).saturating_sub(callback_started.elapsed());
                if !remaining.is_zero() {
                    thread::sleep(remaining);
                }
                *callback_finished_in.lock().unwrap() = Some(Instant::now());
                ProcessControl::Continue
            },
            |_| ProcessControl::Continue,
        )
        .unwrap();
        assert!(
            marker.exists(),
            "fixture child should finish while the durable callback is blocked"
        );
        assert!(
            result.timed_out,
            "an expired callback must not turn completed child work into success"
        );
        let after_callback = callback_finished.lock().unwrap().unwrap().elapsed();
        assert!(
            after_callback < Duration::from_secs(2),
            "expired run cleanup failed to return promptly: {after_callback:?}"
        );
    }
}
