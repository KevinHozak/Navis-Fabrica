//! Fixed native Windows folder picker; no shell text comes from the webview.
use std::path::PathBuf;

#[cfg(windows)]
pub fn pick_folder() -> Result<Option<PathBuf>, String> {
    // Windows PowerShell ships with Windows and provides the system folder dialog.
    // STA is required by WinForms; output is explicitly UTF-8 for non-ASCII paths.
    let script = r#"
$ErrorActionPreference = 'Stop'
[Console]::OutputEncoding = [System.Text.UTF8Encoding]::new($false)
Add-Type -AssemblyName System.Windows.Forms
$dialog = [System.Windows.Forms.FolderBrowserDialog]::new()
$dialog.Description = 'Add project'
$dialog.SelectedPath = 'C:\Projects'
$dialog.ShowNewFolderButton = $false
try {
    if ($dialog.ShowDialog() -eq [System.Windows.Forms.DialogResult]::OK) {
        [Console]::Write($dialog.SelectedPath)
    }
} finally { $dialog.Dispose() }
"#;
    let windows = std::env::var_os("SystemRoot").ok_or("Windows system folder is unavailable.")?;
    let executable = PathBuf::from(windows).join("System32/WindowsPowerShell/v1.0/powershell.exe");
    let output = crate::process::command(executable)
        .args(["-NoProfile", "-NonInteractive", "-STA", "-Command", script])
        .stdin(std::process::Stdio::null())
        .output()
        .map_err(|error| format!("Could not open project folder picker: {error}"))?;
    if !output.status.success() {
        return Err("Could not open the Windows project folder picker.".into());
    }
    let path =
        String::from_utf8(output.stdout).map_err(|_| "Folder picker returned an invalid path.")?;
    if path.is_empty() {
        Ok(None)
    } else {
        Ok(Some(PathBuf::from(path)))
    }
}

#[cfg(not(windows))]
pub fn pick_folder() -> Result<Option<PathBuf>, String> {
    Err("The project folder picker requires Windows.".into())
}
