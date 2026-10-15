//! Process launch policy shared by noninteractive backend work.
//!
//! Every child process started by the app backend must use this constructor so
//! Windows does not show a console window for scans, probes, or captured runs.

use std::ffi::OsStr;
use std::process::Command;

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

#[cfg(all(test, windows))]
mod tests {
    use super::command;

    #[test]
    fn hidden_noninteractive_launch_keeps_output_and_exit_status() {
        if std::env::var_os("NAVIS_HIDDEN_PROCESS_CHILD").is_some() {
            let console = unsafe { windows_sys::Win32::System::Console::GetConsoleWindow() };
            assert!(console.is_null(), "child unexpectedly has a console window");
            println!("hidden-process-child-output");
            return;
        }

        let executable = std::env::current_exe().unwrap();
        let output = command(executable)
            .args([
                "--exact",
                "process::tests::hidden_noninteractive_launch_keeps_output_and_exit_status",
                "--nocapture",
            ])
            .env("NAVIS_HIDDEN_PROCESS_CHILD", "1")
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .output()
            .unwrap();

        assert!(output.status.success());
        assert!(String::from_utf8_lossy(&output.stdout).contains("hidden-process-child-output"));
    }
}
