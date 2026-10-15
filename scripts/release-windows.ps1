$ErrorActionPreference = "Stop"

if ($env:OS -ne "Windows_NT") {
    throw "The Windows release bundle must be built on Windows."
}

$expectedNode = (Get-Content (Join-Path $PSScriptRoot "..\.node-version") -Raw).Trim()
$actualNode = (& node --version).Trim().TrimStart("v")
if ($actualNode -ne $expectedNode) { throw "Node.js $expectedNode is required; found $actualNode." }
if ((& npm --version).Trim() -ne "11.11.1") { throw "npm 11.11.1 is required for the locked release workflow." }
if ((& rustc --version).Trim() -notmatch "^rustc 1\.96\.0 ") { throw "Rust 1.96.0 is required; rust-toolchain.toml should install it." }

function Invoke-Checked {
    param([string]$Command, [string[]]$Arguments)
    & $Command @Arguments
    if ($LASTEXITCODE -ne 0) {
        throw "$Command $($Arguments -join ' ') failed with exit code $LASTEXITCODE."
    }
}

Invoke-Checked "npm" @("ci", "--no-audit", "--no-fund")
Invoke-Checked "npm" @("run", "release:verify")
Invoke-Checked "cargo" @("fmt", "--manifest-path", "src-tauri/Cargo.toml", "--", "--check")
Invoke-Checked "cargo" @("test", "--locked", "--manifest-path", "src-tauri/Cargo.toml")
Invoke-Checked "npm" @("run", "tauri", "--", "build", "--bundles", "nsis", "--ci", "--", "--locked")
Invoke-Checked "powershell" @("-NoProfile", "-ExecutionPolicy", "Bypass", "-File", "scripts/verify-release-artifacts.ps1", "-LaunchSmoke")
