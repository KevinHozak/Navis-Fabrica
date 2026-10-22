param([Parameter(Mandatory=$true)][string]$RequestPath)
$ErrorActionPreference = 'Stop'
[Console]::OutputEncoding = New-Object System.Text.UTF8Encoding($false)
$env:GIT_LITERAL_PATHSPECS = '1'
$env:GIT_TERMINAL_PROMPT = '0'
$env:GCM_INTERACTIVE = 'never'
$env:GH_PROMPT_DISABLED = '1'
$env:NO_COLOR = '1'
$env:TERM = 'dumb'
function Native-Path([string]$Path) {
    if ($Path.StartsWith('\\?\UNC\')) { return '\\' + $Path.Substring(8) }
    if ($Path.StartsWith('\\?\')) { return $Path.Substring(4) }
    return $Path
}
$RequestPath = Native-Path $RequestPath
$request = Get-Content -LiteralPath $RequestPath -Raw -Encoding UTF8 | ConvertFrom-Json
Set-Location -LiteralPath (Native-Path $request.cwd)
$effects = @{ commitSha = ''; remoteBranch = ''; pullRequestUrl = '' }
$script:mergeState = ''
$script:localSyncState = ''
$waitSeconds = if ($request.mergeWaitSeconds) { [Math]::Max(1, [Math]::Min(540, [int]$request.mergeWaitSeconds)) } else { 540 }
$script:mergeDeadline = [DateTime]::UtcNow.AddSeconds($waitSeconds)
# Read native pipes directly: Windows PowerShell otherwise wraps stderr lines
# from Git hooks in RemoteException records and buffers them until Git exits.
$gitExecutable = (Get-Command git.exe -ErrorAction Stop).Source
function Quote-Argument([string]$Value) {
    $escaped = [regex]::Replace($Value, '(\\*)"', '$1$1\"')
    $escaped = [regex]::Replace($escaped, '(\\+)$', '$1$1')
    return '"' + $escaped + '"'
}
function Run-Native([string]$Program, [string[]]$Arguments, [switch]$Stream, [int[]]$AllowedExitCodes = @(0), [switch]$ReturnCode) {
    $info = New-Object Diagnostics.ProcessStartInfo
    $info.FileName = Native-Path $Program
    $info.Arguments = (($Arguments | ForEach-Object { Quote-Argument $_ }) -join ' ')
    $info.WorkingDirectory = (Get-Location).Path
    $info.UseShellExecute = $false
    $info.CreateNoWindow = $true
    $info.RedirectStandardOutput = $true
    $info.RedirectStandardError = $true
    $info.StandardOutputEncoding = New-Object Text.UTF8Encoding($false)
    $info.StandardErrorEncoding = New-Object Text.UTF8Encoding($false)
    $process = New-Object Diagnostics.Process
    $process.StartInfo = $info
    $stdout = New-Object 'Collections.Generic.List[string]'
    $stderr = New-Object 'Collections.Generic.List[string]'
    $script:lastNativeOutput = ''
    $label = if ($Program -eq $gitExecutable) { "Git" } else { "GitHub CLI" }
    try {
        $null = $process.Start()
        if (-not $Stream) {
            # Drain both pipes concurrently in bulk. Inventory output can have
            # thousands of lines; polling the silent pipe per line adds minutes.
            $outRead = $process.StandardOutput.ReadToEndAsync()
            $errRead = $process.StandardError.ReadToEndAsync()
            $process.WaitForExit()
            $output = [regex]::Replace($outRead.GetAwaiter().GetResult(), '\x1B\[[0-?]*[ -/]*[@-~]', '').Replace("`r`n", "`n").TrimEnd("`r", "`n")
            $errorOutput = [regex]::Replace($errRead.GetAwaiter().GetResult(), '\x1B\[[0-?]*[ -/]*[@-~]', '').Replace("`r`n", "`n").TrimEnd("`r", "`n")
            $code = $process.ExitCode
            if ($AllowedExitCodes -notcontains $code) { throw "$label $($Arguments[0]) failed. $errorOutput`n$output" }
            if ($ReturnCode) { return $code }
            return $output
        }
        $outTask = $process.StandardOutput.ReadLineAsync()
        $errTask = $process.StandardError.ReadLineAsync()
        while ($null -ne $outTask -or $null -ne $errTask) {
            if ($null -ne $outTask -and $outTask.IsCompleted) {
                $line = $outTask.Result
                if ($null -eq $line) { $outTask = $null } else {
                    $line = [regex]::Replace($line, '\x1B\[[0-?]*[ -/]*[@-~]', '')
                    $script:lastNativeOutput += $line + "`n"
                    if ($script:lastNativeOutput.Length -gt 40000) { $script:lastNativeOutput = $script:lastNativeOutput.Substring($script:lastNativeOutput.Length - 40000) }
                    if ($Stream) { Write-Output $line } else { $stdout.Add($line) }
                    $outTask = $process.StandardOutput.ReadLineAsync()
                }
            }
            if ($null -ne $errTask -and $errTask.IsCompleted) {
                $line = $errTask.Result
                if ($null -eq $line) { $errTask = $null } else {
                    $line = [regex]::Replace($line, '\x1B\[[0-?]*[ -/]*[@-~]', '')
                    $script:lastNativeOutput += $line + "`n"
                    if ($script:lastNativeOutput.Length -gt 40000) { $script:lastNativeOutput = $script:lastNativeOutput.Substring($script:lastNativeOutput.Length - 40000) }
                    if ($Stream) { Write-Output $line } else { $stderr.Add($line) }
                    $errTask = $process.StandardError.ReadLineAsync()
                }
            }
            # Wait only when neither pipe has data ready, never once per line.
            $pending = @(@($outTask, $errTask) | Where-Object { $null -ne $_ })
            if ($pending.Count -gt 0) { $null = [Threading.Tasks.Task]::WaitAny([Threading.Tasks.Task[]]$pending, 20) }
        }
        $process.WaitForExit()
        $code = $process.ExitCode
        if ($AllowedExitCodes -notcontains $code) {
            if ($Stream) { throw "$label $($Arguments[0]) failed (exit $code)." }
            throw "$label $($Arguments[0]) failed. $(($stderr + $stdout) -join "`n")"
        }
        if ($ReturnCode) { return $code }
        if (-not $Stream) { return ($stdout -join "`n").TrimEnd("`r", "`n") }
    } finally { $process.Dispose() }
}
function Git([string[]]$Arguments, [switch]$Stream, [int[]]$AllowedExitCodes = @(0), [switch]$ReturnCode) {
    Run-Native -Program $gitExecutable @PSBoundParameters
}
function Gh([string[]]$Arguments) {
    Run-Native -Program $request.ghExecutable @PSBoundParameters
}
function Find-PullRequest([string]$Branch, [string]$ExpectedHead) {
    $items = @(Gh -Arguments @('pr', 'list', '--repo', $request.githubRepository, '--base', $request.remoteBranch, '--head', $Branch, '--state', 'open', '--json', 'url,headRefOid,baseRefName,headRefName', '--limit', '10') | ConvertFrom-Json)
    return @($items | Where-Object { $_.headRefName -ceq $Branch -and $_.baseRefName -ceq $request.remoteBranch -and $_.headRefOid -ceq $ExpectedHead } | Select-Object -First 1)
}
function Report-Merge([string]$State, [string]$Commit = '') {
    $script:mergeState = $State
    Write-Output ('NAVIS_PR_MERGE:' + (@{ state = $State; url = $effects.pullRequestUrl; mergeCommit = $Commit } | ConvertTo-Json -Compress))
}
function Read-VerifiedPullRequest([string]$ExpectedHead) {
    $view = Gh -Arguments @('pr', 'view', $effects.pullRequestUrl, '--repo', $request.githubRepository, '--json', 'url,headRefOid,baseRefName,headRefName,state,statusCheckRollup,autoMergeRequest,mergeStateStatus,reviewDecision,mergedAt,mergeCommit') | ConvertFrom-Json
    if ($view.headRefOid -cne $ExpectedHead -or $view.baseRefName -cne $request.remoteBranch -or $view.headRefName -cne ('codex/fabrica-push/' + $ExpectedHead)) {
        throw 'The PR target or commit changed. Automatic merging stopped.'
    }
    return $view
}
function Report-Sync([string]$State, [string]$Message, [string]$Commit = '') {
    $script:localSyncState = $State
    Write-Output ('NAVIS_LOCAL_SYNC:' + (@{ state = $State; message = $Message; branch = $request.branch; commit = $Commit } | ConvertTo-Json -Compress))
}
function Sync-LocalBase([string]$ExpectedHead, [string]$MergeCommit) {
    $updated = $false
    try {
        if ($request.branch -cne $request.remoteBranch) { throw 'The reviewed checkout is not the PR base branch.' }
        Check-Operation
        Check-Target
        if ((Head) -cne $ExpectedHead) { throw 'The local branch gained commits or changed while the PR was merging.' }
        Check-Clean -Message 'Local changes were detected; sync stopped to preserve them.'
        $tracking = "refs/remotes/$($request.remote)/$($request.remoteBranch)"
        $upstream = Git -Arguments @('for-each-ref', '--format=%(upstream)', "refs/heads/$($request.branch)")
        if ($upstream -and $upstream -cne $tracking) { throw 'The local upstream differs from the reviewed PR target.' }
        Step 'sync' "Fetching merged $($request.remoteBranch) for local sync"
        Git -Arguments @('fetch', '--no-tags', '--no-recurse-submodules', '--', $request.remote, "refs/heads/$($request.remoteBranch):$tracking") -Stream
        $target = Git -Arguments @('rev-parse', '--verify', "$tracking^{commit}")
        $contains = Git -Arguments @('merge-base', '--is-ancestor', $MergeCommit, $target) -AllowedExitCodes @(0, 1) -ReturnCode
        if ($contains -ne 0) { throw 'The remote base no longer contains the verified merge commit.' }
        Check-Operation
        Check-Target
        if ((Head) -cne $ExpectedHead) { throw 'The local branch changed during the sync fetch.' }
        Check-Clean -Message 'Local changes were detected; sync stopped to preserve them.'
        # Git checkout/rebase can overwrite ignored files that become tracked
        # upstream. Inspect only newly tracked target paths, not all build caches.
        $added = Git -Arguments @('diff', '--name-only', '--diff-filter=A', '-z', $ExpectedHead, $target)
        foreach ($path in ($added -split [char]0)) {
            if (-not $path) { continue }
            if ($path -and (Test-Path -LiteralPath $path)) { throw "An existing local path would be replaced by upstream: $path" }
            $parent = Split-Path -Parent $path
            while ($parent -and $parent -ne '.') {
                if (Test-Path -LiteralPath $parent -PathType Leaf) { throw "An existing local file blocks an upstream directory: $parent" }
                $parent = Split-Path -Parent $parent
            }
        }
        if ($ExpectedHead -cne $target) {
            $backup = 'refs/fabrica/after-merge/' + [Guid]::NewGuid().ToString('N')
            $null = Git -Arguments @('update-ref', $backup, $ExpectedHead, '')
            Write-Output "Pre-sync branch saved at $backup ($ExpectedHead)."
            $ancestor = Git -Arguments @('merge-base', '--is-ancestor', $ExpectedHead, $target) -AllowedExitCodes @(0, 1) -ReturnCode
            if ($ancestor -eq 0) {
                Step 'sync' "Fast-forwarding local $($request.branch)"
                Git -Arguments @('merge', '--ff-only', '--no-edit', $target) -Stream
            } else {
                # Squash/rebase merges replace the published commit IDs. With
                # HEAD pinned to the reviewed tip, this replays zero commits.
                # Any concurrent new commits are preserved by rebase, not reset.
                Step 'sync' "Aligning local $($request.branch) after squash/rebase merge"
                $env:GIT_EDITOR = 'true'
                $env:GIT_SEQUENCE_EDITOR = 'true'
                Git -Arguments @('-c', 'rebase.autoStash=false', '-c', 'rebase.updateRefs=false', '-c', 'rerere.enabled=false', 'rebase', '--no-autostash', '--no-update-refs', '--no-fork-point', '--no-rebase-merges', '--no-autosquash', '--onto', $target, $ExpectedHead) -Stream
            }
            $updated = $true
        }
        if (-not $upstream) { $null = Git -Arguments @('branch', "--set-upstream-to=$($request.remote)/$($request.remoteBranch)", '--', $request.branch) }
        Check-Operation
        Check-Target
        if ((Head) -cne $target) { throw 'The local branch no longer matches the fetched remote base.' }
        Check-Clean -Message 'Local changes were detected; sync stopped to preserve them.'
        $message = "Local $($request.branch) is synced with $($request.remote)/$($request.remoteBranch) ($target)"
        Report-Sync -State 'synced' -Message $message -Commit $target
        Step 'synced' $message
    } catch {
        $state = if ($updated) { 'attention' } else { 'skipped' }
        $message = "PR merged; local sync requires attention: $($_.Exception.Message)"
        Report-Sync -State $state -Message $message
        Step 'sync-stopped' $message
    }
}
function Record-VersionPush([string]$ExpectedHead) {
    if ($preparedVersion -and $preparedVersion.ready) {
        $null = Version-Step -Action 'record-push' -Extra @('--commit', $ExpectedHead, '--remote-head', $ExpectedHead)
        Write-Output "Version $($preparedVersion.version) push receipt saved."
    }
}
function Check-Merged($View) {
    if ($View.state -eq 'MERGED') {
        $commit = $View.mergeCommit.oid
        if (-not $View.mergedAt -or $commit -notmatch '^[0-9a-fA-F]{40,64}$') { throw 'Could not verify the merge commit.' }
        Report-Merge -State 'merged' -Commit $commit
        Step 'merged' "Pull request merged and verified ($commit)"
        Sync-LocalBase -ExpectedHead $View.headRefOid -MergeCommit $commit
        return
    }
    throw 'GitHub did not confirm a completed merge.'
}
function Merge-ReviewedPullRequest([string]$ExpectedHead) {
    Step 'merge' 'Preparing automatic pull request merge'
    $view = Read-VerifiedPullRequest -ExpectedHead $ExpectedHead
    if ($view.state -eq 'MERGED') { Check-Merged $view; return }
    if ($view.state -ne 'OPEN') { throw 'The pull request is closed without merging.' }
    $repo = Gh -Arguments @('api', ('repos/' + $request.githubRepository)) | ConvertFrom-Json
    $method = if ($repo.allow_merge_commit) { '--merge' } elseif ($repo.allow_squash_merge) { '--squash' } elseif ($repo.allow_rebase_merge) { '--rebase' } else { throw 'No supported merge method is enabled for this repository.' }
    if ($repo.allow_auto_merge) {
        Step 'auto-merge' 'Enabling GitHub auto-merge after required checks and reviews'
        try { $null = Gh -Arguments @('pr', 'merge', $effects.pullRequestUrl, '--repo', $request.githubRepository, $method, '--auto', '--match-head-commit', $ExpectedHead) }
        catch { Write-Output 'Reading back merge state after the merge request.' }
        $view = Read-VerifiedPullRequest -ExpectedHead $ExpectedHead
        if ($view.state -eq 'MERGED') { Check-Merged $view; return }
        if ($view.state -eq 'OPEN' -and $view.autoMergeRequest) {
            Report-Merge -State 'queued'
            Step 'queued' 'Auto-merge enabled; watching for merge before local sync'
            Report-Sync -State 'waiting' -Message 'Waiting for the queued PR to merge before local sync'
            while ([DateTime]::UtcNow -lt $script:mergeDeadline) {
                $view = Read-VerifiedPullRequest -ExpectedHead $ExpectedHead
                if ($view.state -eq 'MERGED') { Check-Merged $view; return }
                if ($view.state -ne 'OPEN' -or -not $view.autoMergeRequest) { throw 'The PR closed or auto-merge was disabled before a merge was confirmed.' }
                Start-Sleep -Seconds 3
            }
            Report-Sync -State 'pending' -Message 'GitHub auto-merge remains enabled; this run ended before the merge, so local sync is still pending'
            Step 'sync-pending' 'Auto-merge remains queued; local sync is pending because the run ended before merge'
            return
        }
        throw 'GitHub did not confirm auto-merge. The PR is preserved; inspect its requirements before retrying.'
    }
    Write-Output 'GitHub auto-merge is disabled; waiting here for merge requirements.'
    Report-Merge -State 'waiting'
    $lastStatus = ''
    while ([DateTime]::UtcNow -lt $script:mergeDeadline) {
        $view = Read-VerifiedPullRequest -ExpectedHead $ExpectedHead
        if ($view.state -eq 'MERGED') { Check-Merged $view; return }
        if ($view.state -ne 'OPEN') { throw 'The pull request is closed without merging.' }
        $failed = @($view.statusCheckRollup | Where-Object {
            $_.conclusion -in @('FAILURE', 'TIMED_OUT', 'CANCELLED', 'ACTION_REQUIRED', 'STARTUP_FAILURE', 'STALE') -or $_.state -in @('FAILURE', 'ERROR')
        })
        if ($failed.Count -gt 0) { throw 'A pull request check failed. Automatic merging stopped; inspect the PR checks.' }
        if ($view.reviewDecision -eq 'CHANGES_REQUESTED') { throw 'A review requested changes. Automatic merging stopped.' }
        $pending = @($view.statusCheckRollup | Where-Object { ($_.status -and $_.status -ne 'COMPLETED') -or $_.state -eq 'PENDING' })
        if ($view.mergeStateStatus -eq 'CLEAN' -and $view.reviewDecision -ne 'REVIEW_REQUIRED' -and $pending.Count -eq 0) {
            Step 'merge' 'Merging the verified commit after required checks and reviews'
            try { $null = Gh -Arguments @('pr', 'merge', $effects.pullRequestUrl, '--repo', $request.githubRepository, $method, '--match-head-commit', $ExpectedHead) }
            catch { Write-Output 'Reading back merge state after the merge request.' }
            $view = Read-VerifiedPullRequest -ExpectedHead $ExpectedHead
            Check-Merged $view
            return
        }
        if ($view.mergeStateStatus -eq 'DIRTY') { throw 'The PR has merge conflicts. Automatic merging stopped.' }
        $status = "$($view.mergeStateStatus) / $($view.reviewDecision) / $($pending.Count) pending checks"
        if ($status -cne $lastStatus) { Step 'waiting' "Waiting for GitHub requirements: $status"; $lastStatus = $status }
        Start-Sleep -Seconds 3
    }
    throw 'The PR is open, but merge requirements did not finish within this run. Automatic merging stopped; retry after checks and reviews complete.'
}
function Open-ProtectedPullRequest([string]$ExpectedHead) {
    if (-not $request.allowPullRequest) { throw 'The branch requires a pull request. Reopen the review to permit the PR workflow.' }
    if (-not $request.ghExecutable) { throw 'The branch requires a pull request. Install and authenticate GitHub CLI, then retry the existing commit.' }
    $null = Gh -Arguments @('auth', 'status', '--hostname', 'github.com')
    $fallback = 'codex/fabrica-push/' + $ExpectedHead
    $fallbackRef = 'refs/heads/' + $fallback
    Step 'branch' 'Protected branch: publishing a pull request branch'
    Check-Target
    Check-Clean
    if ((Head) -cne $ExpectedHead) { throw 'HEAD changed before PR publication. Push stopped.' }
    $existing = Git -Arguments @('ls-remote', '--refs', '--', $request.remote, $fallbackRef)
    if ($existing -and ($existing -split '\s+')[0] -cne $ExpectedHead) { throw 'The recovery branch points to another commit. Publication stopped.' }
    if (-not $existing) { Git -Arguments @('push', '--', $request.remote, "${ExpectedHead}:$fallbackRef") -Stream }
    $observed = Git -Arguments @('ls-remote', '--exit-code', $request.remote, $fallbackRef)
    $parts = $observed -split '\s+'
    if ($parts.Count -ne 2 -or $parts[0] -cne $ExpectedHead -or $parts[1] -cne $fallbackRef) { throw 'PR branch verification failed. Inspect the remote before retrying.' }
    $effects.remoteBranch = $fallback
    Report-Effects
    Record-VersionPush -ExpectedHead $ExpectedHead
    Step 'pr' 'Opening a pull request'
    $pr = @(Find-PullRequest -Branch $fallback -ExpectedHead $ExpectedHead)
    if ($pr.Count -eq 0) {
        $title = if ($request.subject) { $request.subject } else { Git -Arguments @('log', '-1', '--format=%s') }
        $body = if ($request.body) { $request.body } else { Git -Arguments @('log', '-1', '--format=%b') }
        $prBodyPath = Join-Path (Split-Path -Parent $RequestPath) 'pull-request-body.txt'
        [IO.File]::WriteAllText($prBodyPath, $body + "`n", (New-Object Text.UTF8Encoding($false)))
        try {
            $null = Gh -Arguments @('pr', 'create', '--repo', $request.githubRepository, '--base', $request.remoteBranch, '--head', $fallback, '--title', $title, '--body-file', $prBodyPath)
        } catch {
            # A timeout can follow successful creation. Read back before deciding;
            # never issue a second create request in this run.
            Write-Output 'Checking whether GitHub created the pull request.'
            $pr = @(Find-PullRequest -Branch $fallback -ExpectedHead $ExpectedHead)
            if ($pr.Count -eq 0) { throw 'PR creation could not be confirmed. The verified branch is preserved; retry to check for an existing PR.' }
        }
        if ($pr.Count -eq 0) { $pr = @(Find-PullRequest -Branch $fallback -ExpectedHead $ExpectedHead) }
    }
    if ($pr.Count -ne 1 -or $pr[0].url -notmatch ('^https://github\.com/' + [regex]::Escape($request.githubRepository) + '/pull/[0-9]+$')) { throw 'Could not verify the pull request URL.' }
    $effects.pullRequestUrl = $pr[0].url
    Report-Effects
    Write-Output "Pull request: $($effects.pullRequestUrl)"
    Step 'checks' 'Reading pull request checks'
    $view = $null
    try {
        $view = Gh -Arguments @('pr', 'view', $effects.pullRequestUrl, '--repo', $request.githubRepository, '--json', 'url,headRefOid,baseRefName,headRefName,state,statusCheckRollup') | ConvertFrom-Json
        $checks = @($view.statusCheckRollup)
        if ($checks.Count -eq 0) { Write-Output 'Checks have not started yet. Open the pull request to follow them.' }
        foreach ($check in $checks) {
            $name = if ($check.name) { $check.name } else { $check.context }
            $state = if ($check.conclusion) { $check.conclusion } elseif ($check.status) { $check.status } else { $check.state }
            Write-Output "Check ${name}: $state"
        }
        Write-Output 'Pull request is open; merge remains subject to repository checks and review.'
    } catch { Write-Output 'Check status is unavailable. Open the verified pull request to follow checks and review.' }
    if ($view -and ($view.headRefOid -cne $ExpectedHead -or $view.baseRefName -cne $request.remoteBranch -or $view.headRefName -cne $fallback)) { throw 'The PR target or commit changed. Inspect the PR before continuing.' }
    if ($request.autoMergePullRequest) { Merge-ReviewedPullRequest -ExpectedHead $ExpectedHead }
}
function Step([string]$Name, [string]$Message) { Write-Output "GIT_STEP:${Name}:$Message" }
function Check-Operation {
    foreach ($name in @('rebase-merge', 'rebase-apply', 'MERGE_HEAD', 'CHERRY_PICK_HEAD', 'REVERT_HEAD')) {
        $path = Git -Arguments @('rev-parse', '--git-path', $name)
        if (Test-Path -LiteralPath $path) { throw 'A Git operation is already in progress. Resolve or abort it before retrying.' }
    }
}
function Check-Clean([string]$Message = 'Uncommitted changes remain. Rebase and push stopped; no files were stashed.') {
    if (Git -Arguments @('status', '--porcelain=v1', '--untracked-files=all')) {
        throw $Message
    }
}

function Head {
    try { return Git -Arguments @('rev-parse', '--verify', 'HEAD') } catch { return '' }
}
function Check-Files {
    foreach ($file in $request.files) {
        $hash = ''
        if (Test-Path -LiteralPath $file.path -PathType Leaf) {
            $hash = Git -Arguments @('hash-object', '--no-filters', '--', $file.path)
        } elseif (Test-Path -LiteralPath $file.path) { throw 'Directory and submodule changes require separate review.' }
        if ($hash -cne $file.hash) { throw "Reviewed file changed: $($file.path). Click Push to retry." }
    }
}
function Check-Target {
    if ((Git -Arguments @('branch', '--show-current')) -cne $request.branch) { throw 'The reviewed branch changed.' }
    if ($request.remote -and (Git -Arguments @('remote', 'get-url', $request.remote)) -cne $request.remoteUrl) { throw 'The reviewed remote URL changed.' }
}
function Report-Effects { Write-Output ('NAVIS_SIDE_EFFECTS:' + ($effects | ConvertTo-Json -Compress)) }
function Version-Step([string]$Action, [string[]]$Extra = @()) {
    $priorPreference = $ErrorActionPreference
    try {
        $ErrorActionPreference = 'Continue'
        $result = @(& $request.python $request.versionHelper --repo (Native-Path $request.cwd) --state-dir (Native-Path $request.versionStateDir) --action $Action @Extra 2>&1)
        $code = $LASTEXITCODE
    } finally { $ErrorActionPreference = $priorPreference }
    if ($code -ne 0) { throw "Version $Action failed: $($result -join "`n")" }
    return ($result -join "`n") | ConvertFrom-Json
}
try {
    Step 'review' 'Checking reviewed repository state'
    Check-Operation
    Check-Target
    if ((Head) -cne $request.head) { throw 'HEAD changed. Click Push to retry.' }
    $currentStatus = [string](Git -Arguments @('status', '--porcelain=v1', '--untracked-files=all'))
    if ($currentStatus -cne [string]$request.status) {
        $before = @(([string]$request.status -split "`n") | Where-Object { $_ })
        $after = @(($currentStatus -split "`n") | Where-Object { $_ })
        # Show bounded path/status differences, never file contents or credentials.
        $changed = @($before | Where-Object { $after -cnotcontains $_ }) + @($after | Where-Object { $before -cnotcontains $_ })
        $details = (@($changed | Select-Object -First 8) -join '; ')
        if (-not $details) { $details = 'Git status formatting changed' }
        throw "Repository changed before commit: $details. Wait for edits to finish, then click Push again."
    }
    if ((Git -Arguments @('ls-files', '--stage')) -cne $request.index) { throw 'The Git index changed. Click Push to retry.' }
    Check-Files
    $preparedVersion = $null
    if ($request.versionHelper) {
        $extra = @()
        if (-not $request.commit) { $extra = @('--push-only') }
        $preparedVersion = Version-Step -Action 'inspect' -Extra $extra
        Write-Output "Using reviewed version $($preparedVersion.version)."
    }
    if ($request.commit) {
        Step 'commit' 'Staging and committing reviewed changes'
        $stagingPath = Join-Path (Split-Path -Parent $RequestPath) 'paths.txt'
        $paths = (@($request.files | ForEach-Object { $_.path }) -join [char]0) + [char]0
        [IO.File]::WriteAllBytes($stagingPath, [Text.Encoding]::UTF8.GetBytes($paths))
        $null = Git -Arguments @('add', '--all', "--pathspec-from-file=$stagingPath", '--pathspec-file-nul')
        Check-Files
        Check-Target
        if ((Head) -cne $request.head) { throw 'HEAD changed before commit.' }
        $tree = Git -Arguments @('write-tree')
        $message = $request.subject
        if ($request.body) { $message += "`n`n" + $request.body }
        $messagePath = Join-Path (Split-Path -Parent $RequestPath) 'message.txt'
        [IO.File]::WriteAllText($messagePath, $message + "`n", (New-Object Text.UTF8Encoding($false)))
        Git -Arguments @('commit', '--cleanup=verbatim', '--file', $messagePath) -Stream
        $effects.commitSha = Head
        Report-Effects
        if ((Git -Arguments @('rev-parse', 'HEAD^{tree}')) -cne $tree) { throw 'A hook changed the committed tree. Review the commit before pushing.' }
        if ((Git -Arguments @('log', '-1', '--format=%B')) -cne $message.TrimEnd("`r", "`n")) { throw 'A hook changed the approved message. Review the commit before pushing.' }
    }
    if ($request.push) {
        Check-Target
        $expectedHead = Head
        if (-not $expectedHead) { throw 'There is no commit to push.' }
        if ($request.commit -and $expectedHead -cne $effects.commitSha) { throw 'HEAD changed after commit.' }
        Check-Clean
        Step 'fetch' 'Fetching the remote branch'
        $remoteRef = "refs/heads/$($request.remoteBranch)"
        $listing = Git -Arguments @('ls-remote', '--refs', '--', $request.remote, $remoteRef)
        $beforeSync = $expectedHead
        if ($listing) {
            Git -Arguments @('fetch', '--no-tags', '--no-recurse-submodules', '--', $request.remote, $remoteRef) -Stream
            $remoteHead = Git -Arguments @('rev-parse', '--verify', 'FETCH_HEAD^{commit}')
            Check-Target
            if ((Head) -cne $beforeSync) { throw 'HEAD changed during fetch. Rebase stopped.' }
            Check-Clean
            $ancestor = Git -Arguments @('merge-base', '--is-ancestor', $remoteHead, 'HEAD') -AllowedExitCodes @(0, 1) -ReturnCode
            if ($ancestor -eq 1) {
                $commonBase = Git -Arguments @('merge-base', $remoteHead, 'HEAD') -AllowedExitCodes @(0, 1) -ReturnCode
                if ($commonBase -ne 0) { throw 'Local and remote histories are unrelated. Rebase and push stopped.' }
                # Keep a durable original tip for conflict recovery and cancellation.
                $backup = 'refs/fabrica/before-push/' + [Guid]::NewGuid().ToString('N')
                $null = Git -Arguments @('update-ref', $backup, $beforeSync, '')
                Write-Output "Original branch saved at $backup ($beforeSync)."
                Step 'rebase' 'Rebasing local commits onto the remote branch'
                $env:GIT_EDITOR = 'true'
                $env:GIT_SEQUENCE_EDITOR = 'true'
                try {
                    Git -Arguments @('-c', 'rebase.autoStash=false', '-c', 'rebase.updateRefs=false', '-c', 'rerere.enabled=false', 'rebase', '--rebase-merges', '--no-autostash', '--no-update-refs', $remoteHead) -Stream
                } catch {
                    $conflicts = Git -Arguments @('diff', '--name-only', '--diff-filter=U')
                    if ($conflicts) {
                        Step 'conflict' 'Rebase stopped: resolve conflicts before pushing'
                        Write-Output "Conflicted files:`n$conflicts"
                        Write-Output 'Resolve the files and run git rebase --continue, or run git rebase --abort, before retrying.'
                    } else { Step 'stopped' 'Rebase stopped; inspect the Git output before retrying' }
                    throw 'Rebase did not complete. Nothing was pushed.'
                }
                Check-Operation
                Check-Target
                Check-Clean
                $expectedHead = Head
                if ($request.commit) { $effects.commitSha = $expectedHead; Report-Effects }
                if ($request.versionHelper) {
                    # Upstream app changes must not inherit a stale preparation receipt.
                    $preparedVersion = Version-Step -Action 'inspect' -Extra @('--push-only')
                    if (-not $preparedVersion.ready) { Write-Output 'Upstream changes altered prepared app content; no stale version receipt will be recorded.' }
                }
            } else { Step 'rebase' 'Local commits already include the remote branch' }
        } else { Step 'rebase' 'New remote branch; no upstream commits to rebase' }
        Check-Target
        Check-Clean
        if ((Head) -cne $expectedHead) { throw 'HEAD changed before push. Push stopped.' }
        Step 'push' 'Pushing the rebased branch'
        $protected = $false
        try { Git -Arguments @('push', '--set-upstream', '--', $request.remote, "HEAD:$remoteRef") -Stream }
        catch {
            $pushOutput = $script:lastNativeOutput
            if ($pushOutput -match 'GH00?6|GH013' -and $pushOutput -match 'Changes must be made through a pull request') {
                $protected = $true
                Open-ProtectedPullRequest -ExpectedHead $expectedHead
            } else { throw }
        }
        if (-not $protected) {
            Step 'verify' 'Verifying the remote commit'
            $listing = Git -Arguments @('ls-remote', '--exit-code', $request.remote, "refs/heads/$($request.remoteBranch)")
            $parts = $listing -split '\s+'
            if ($parts.Count -ne 2 -or $parts[0] -cne $expectedHead -or $parts[1] -cne "refs/heads/$($request.remoteBranch)") { throw 'Remote verification failed. Inspect the remote before retrying.' }
            $effects.remoteBranch = $request.remoteBranch
            Report-Effects
        }
        if (-not $protected) { Record-VersionPush -ExpectedHead $expectedHead }
    }
    if ($script:mergeState -eq 'merged' -and $script:localSyncState -eq 'synced') { Step 'complete' 'Pull request merged; local branch synced and verified' }
    elseif ($script:mergeState -eq 'merged') { Step 'complete' 'Pull request merged; local sync requires attention' }
    elseif ($script:mergeState -eq 'queued') { Step 'complete' 'Auto-merge enabled; waiting for GitHub requirements' }
    elseif ($effects.pullRequestUrl) { Step 'complete' 'Pull request opened; checks and review are required before merging' }
    else { Step 'complete' 'Reviewed Git operation completed and verified' }
    exit 0
} catch {
    if ($request.autoMergePullRequest -and $effects.pullRequestUrl -and $script:mergeState -notin @('merged', 'queued')) { Report-Merge -State 'stopped' }
    Report-Effects
    Write-Output $_.Exception.Message
    exit 1
} finally {
    # Remove only the exact files created for this request, never a tree.
    Remove-Item -LiteralPath $RequestPath -Force -ErrorAction SilentlyContinue
    $messagePath = Join-Path (Split-Path -Parent $RequestPath) 'message.txt'
    Remove-Item -LiteralPath $messagePath -Force -ErrorAction SilentlyContinue
    $prBodyPath = Join-Path (Split-Path -Parent $RequestPath) 'pull-request-body.txt'
    Remove-Item -LiteralPath $prBodyPath -Force -ErrorAction SilentlyContinue
    $stagingPath = Join-Path (Split-Path -Parent $RequestPath) 'paths.txt'
    Remove-Item -LiteralPath $stagingPath -Force -ErrorAction SilentlyContinue
}
