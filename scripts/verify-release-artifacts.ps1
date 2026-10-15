param([switch]$LaunchSmoke)

$ErrorActionPreference = "Stop"

$root = (Resolve-Path (Join-Path $PSScriptRoot "..")).Path
$package = Get-Content (Join-Path $root "package.json") -Raw | ConvertFrom-Json
$version = $package.version
$releaseDirectory = Join-Path $root "src-tauri\target\release"
$executable = Get-ChildItem $releaseDirectory -Filter "*.exe" -File | Where-Object {
    $_.BaseName -eq "navis-fabrica"
} | Select-Object -First 1
$installerDirectory = Join-Path $releaseDirectory "bundle\nsis"
$installer = Get-ChildItem $installerDirectory -Filter "*-setup.exe" -File | Select-Object -First 1

if (-not $executable) { throw "The release executable was not found under $releaseDirectory." }
if (-not $installer) { throw "The NSIS setup executable was not found under $installerDirectory." }

foreach ($artifact in @($executable, $installer)) {
    $metadata = $artifact.VersionInfo
    if ($metadata.ProductVersion -ne $version -or $metadata.FileVersion -ne $version) {
        throw "$($artifact.FullName) has FileVersion '$($metadata.FileVersion)' and ProductVersion '$($metadata.ProductVersion)'; expected $version."
    }
    if ($metadata.ProductName -notlike "*Navis-Fabrica*") {
        throw "$($artifact.FullName) does not identify Navis-Fabrica in its Windows version metadata."
    }
    Write-Output ("{0}: FileVersion={1}; ProductVersion={2}; ProductName={3}; Bytes={4}" -f $artifact.FullName, $metadata.FileVersion, $metadata.ProductVersion, $metadata.ProductName, $artifact.Length)
}

if ($LaunchSmoke) {
    $isolatedProfile = Join-Path ([System.IO.Path]::GetTempPath()) ("navis-fabrica-release-smoke-" + [guid]::NewGuid().ToString("N"))
    New-Item -ItemType Directory -Path $isolatedProfile | Out-Null
    $startInfo = New-Object System.Diagnostics.ProcessStartInfo
    $startInfo.FileName = $executable.FullName
    $startInfo.WorkingDirectory = $isolatedProfile
    $startInfo.UseShellExecute = $false
    $startInfo.EnvironmentVariables["APPDATA"] = Join-Path $isolatedProfile "Roaming"
    $startInfo.EnvironmentVariables["LOCALAPPDATA"] = Join-Path $isolatedProfile "Local"
    New-Item -ItemType Directory -Path $startInfo.EnvironmentVariables["APPDATA"] | Out-Null
    New-Item -ItemType Directory -Path $startInfo.EnvironmentVariables["LOCALAPPDATA"] | Out-Null
    $process = [System.Diagnostics.Process]::Start($startInfo)
    try {
        Start-Sleep -Seconds 8
        $process.Refresh()
        if ($process.HasExited) { throw "The release app exited during its isolated launch smoke test with code $($process.ExitCode)." }
        Write-Output "Release executable stayed running for 8 seconds with isolated APPDATA/LOCALAPPDATA directories."
    } finally {
        if (-not $process.HasExited) {
            [void]$process.CloseMainWindow()
            if (-not $process.WaitForExit(5000)) { $process.Kill(); $process.WaitForExit() }
        }
        Remove-Item -LiteralPath $isolatedProfile -Recurse -Force -ErrorAction SilentlyContinue
    }
}

Write-Output "Release artifact metadata verified for v$version."
