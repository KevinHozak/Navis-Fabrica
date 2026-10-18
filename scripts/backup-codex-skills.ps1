param(
    [Parameter(Mandatory=$true)][string]$BackupRoot,
    [string]$SkillRoot = (Join-Path $HOME '.codex/skills'),
    [string]$DiscoveryRoot = (Join-Path $HOME '.agents/skills'),
    [Alias('MimirSkillRoot')][string]$ProjectSkillRoot,
    [Parameter(Mandatory=$true)][string]$SharedSkillRoot,
    [string]$HelperRoot = $PSScriptRoot
)
$ErrorActionPreference = 'Stop'
$stamp = [DateTime]::Now.ToString('yyyy-MM-dd_HHmmss')
$publicRoot = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot '..')).TrimEnd('\','/')
$backupFull = [IO.Path]::GetFullPath($BackupRoot)
if ($backupFull -eq $publicRoot -or $backupFull.StartsWith($publicRoot + [IO.Path]::DirectorySeparatorChar, [StringComparison]::OrdinalIgnoreCase)) { throw 'Backup data must be outside the shared Fabrica checkout' }
$destination = Join-Path $BackupRoot $stamp
if (Test-Path -LiteralPath $destination) { throw "Snapshot already exists: $destination" }
$sources = @([pscustomobject]@{Root=$SkillRoot;Prefix='skills'})
if ($ProjectSkillRoot -and (Test-Path -LiteralPath $ProjectSkillRoot)) { $sources += [pscustomobject]@{Root=$ProjectSkillRoot;Prefix='projects/additional'} }
if (Test-Path -LiteralPath $SharedSkillRoot) { $sources += [pscustomobject]@{Root=$SharedSkillRoot;Prefix='shared/agent-skills'} }
$sharedFull = [IO.Path]::GetFullPath($SharedSkillRoot).TrimEnd('\','/')
$helperFull = [IO.Path]::GetFullPath($HelperRoot).TrimEnd('\','/')
$sourceLinks = @()
$records = @()
$excluded = @()
$skillPaths = @()
foreach ($source in $sources) {
    $root = (Resolve-Path -LiteralPath $source.Root).Path.TrimEnd('\')
    $links = @(Get-ChildItem -LiteralPath $root -Recurse -Force | Where-Object { $_.Attributes -band [IO.FileAttributes]::ReparsePoint })
    foreach ($link in $links) {
        $target = [IO.Path]::GetFullPath(@($link.Target)[0]).TrimEnd('\','/')
        $skillLink = $target -eq $sharedFull -or $target.StartsWith($sharedFull + [IO.Path]::DirectorySeparatorChar, [StringComparison]::OrdinalIgnoreCase)
        $helperLink = $target -eq $helperFull -or $target.StartsWith($helperFull + [IO.Path]::DirectorySeparatorChar, [StringComparison]::OrdinalIgnoreCase)
        if (-not $skillLink -and -not $helperLink) { throw "Source link needs explicit inspection: $($link.FullName)" }
        if ($root -eq $sharedFull -and -not $helperLink) { throw 'Nested skill links need explicit inspection' }
        $sourceLinks += [pscustomobject]@{path=$link.FullName;target=$target;linkType=$link.LinkType}
    }
    foreach ($file in Get-ChildItem -LiteralPath $root -Recurse -Force -File) {
        $relative = $file.FullName.Substring($root.Length + 1).Replace('\','/')
        if ($relative -match '(^|/)(__pycache__|node_modules|\.venv|\.git)(/|$)' -or $file.Name -match '\.(db|sqlite|sqlite3)(-wal|-shm)?$|\.py[co]$') {
            $excluded += [pscustomobject]@{source=$file.FullName;reason='Generated database, cache, or dependency artifact'}
            continue
        }
        $snapshotPath = "$($source.Prefix)/$relative"
        $target = Join-Path $destination $snapshotPath
        New-Item -ItemType Directory -Path (Split-Path $target -Parent) -Force | Out-Null
        $before = (Get-FileHash -LiteralPath $file.FullName -Algorithm SHA256).Hash
        Copy-Item -LiteralPath $file.FullName -Destination $target
        $copied = (Get-FileHash -LiteralPath $target -Algorithm SHA256).Hash
        $after = (Get-FileHash -LiteralPath $file.FullName -Algorithm SHA256).Hash
        if ($before -ne $copied -or $before -ne $after) { throw "Source changed or copy mismatch: $($file.FullName). Snapshot incomplete: $destination" }
        $records += [pscustomobject]@{path=$snapshotPath;source=$file.FullName;sha256=$copied;bytes=$file.Length}
        if ($file.Name -eq 'SKILL.md') { $skillPaths += $snapshotPath }
    }
}
$discovery = @()
if (Test-Path -LiteralPath $DiscoveryRoot) {
    $discovery = @(Get-ChildItem -LiteralPath $DiscoveryRoot -Force | ForEach-Object { [pscustomobject]@{name=$_.Name;linkType=$_.LinkType;target=$_.Target} })
    $independent = @($discovery | Where-Object { -not $_.linkType })
    if ($independent.Count) { throw "Independent discovery skills require adding to backup sources. Snapshot incomplete: $destination" }
}
$manifest = [ordered]@{createdAtUtc=[DateTime]::UtcNow.ToString('o');complete=$true;sources=$sources;sourceLinks=$sourceLinks;skillCount=$skillPaths.Count;fileCount=$records.Count;files=$records;excluded=$excluded;discoveryLinks=$discovery}
$manifest | ConvertTo-Json -Depth 8 | Set-Content -LiteralPath (Join-Path $destination 'manifest.json') -Encoding utf8
$index = @('# Codex skills snapshot', '', "Created: $stamp (local time)", '', "Skills: $($skillPaths.Count). Verified files: $($records.Count).", '', 'Contains personal and built-in Codex skills plus configured project and caller-owned linked skills. Plugin-managed copies and Hermes skills use separate storage. Generated databases and caches are excluded and listed in manifest.json.', '', '## Skill directory', '')
$index += $skillPaths | Sort-Object | ForEach-Object { $label = $_ -replace '/SKILL.md$',''; "- [$label]($_)" }
$index += @('', '## Restore', '', 'Use manifest.json to find each original source path and sourceLinks to recreate shared source junctions. Restore selected skill folders to their original roots, preserving unrelated current files. Recreate discovery junctions from discoveryLinks if needed; do not copy snapshot indexes or manifests into the active skill roots.')
$index | Set-Content -LiteralPath (Join-Path $destination 'INDEX.md') -Encoding utf8BOM
Write-Output "Verified $($records.Count) files for $($skillPaths.Count) skills: $destination"
