param([switch]$VerifyOnly)
$ErrorActionPreference = 'Stop'
$env:HTTP_PROXY = $null
$env:HTTPS_PROXY = $null
$env:ALL_PROXY = $null
$env:GH_TOKEN = $null
$env:GITHUB_TOKEN = $null
$repo = 'KevinHozak/Navis-Fabrica'
$owner = 'KevinHozak'
$projectNumber = 9
function Invoke-GhJson {
    param([string[]]$Arguments)
    $result = & gh @Arguments
    if ($LASTEXITCODE -ne 0) { throw "GitHub operation failed: $($Arguments[0..1] -join ' ')" }
    return (($result -join "`n") | ConvertFrom-Json)
}
$spec = Get-Content -Raw (Join-Path $PSScriptRoot 'issue-map.json') | ConvertFrom-Json
$fields = Invoke-GhJson @('project','field-list',"$projectNumber",'--owner',$owner,'--format','json')
$status = $fields.fields | Where-Object name -eq 'Status'
$backlog = $status.options | Where-Object name -eq 'Backlog'
$priority = $fields.fields | Where-Object name -eq 'Priority'
$project = Invoke-GhJson @('project','view',"$projectNumber",'--owner',$owner,'--format','json')
$issueList = Invoke-GhJson @('api',"repos/$repo/issues?state=all&per_page=100")
$ids = @{}
foreach ($issue in $issueList) { $ids[[int]$issue.number] = $issue.node_id }
if (-not $VerifyOnly) { foreach ($epic in $spec) {
    $existing = Invoke-GhJson @('api',"repos/$repo/issues/$($epic.number)/sub_issues?per_page=100")
    foreach ($phase in $epic.phases) {
        if ($phase.number -notin $existing.number) {
            $linked = Invoke-GhJson @('api','graphql','-f','query=mutation($parent: ID!, $child: ID!) { addSubIssue(input: {issueId: $parent, subIssueId: $child}) { issue { number } subIssue { number } } }','-F',"parent=$($ids[[int]$epic.number])",'-F',"child=$($ids[[int]$phase.number])")
            if ($linked.errors) { throw ($linked.errors | ConvertTo-Json) }
        }
        $blocking = Invoke-GhJson @('api',"repos/$repo/issues/$($phase.number)/dependencies/blocked_by?per_page=100")
        foreach ($dependency in $phase.dependencyNumbers) {
            if ($dependency -notin $blocking.number) {
                $linked = Invoke-GhJson @('api','graphql','-f','query=mutation($issue: ID!, $blocker: ID!) { addBlockedBy(input: {issueId: $issue, blockingIssueId: $blocker}) { issue { number } } }','-F',"issue=$($ids[[int]$phase.number])",'-F',"blocker=$($ids[[int]$dependency])")
                if ($linked.errors) { throw ($linked.errors | ConvertTo-Json) }
            }
        }
    }
    foreach ($number in @($epic.number) + @($epic.phases.number)) {
        $item = Invoke-GhJson @('project','item-add',"$projectNumber",'--owner',$owner,'--url',"https://github.com/$repo/issues/$number",'--format','json')
        $null = Invoke-GhJson @('project','item-edit','--id',$item.id,'--project-id',$project.id,'--field-id',$status.id,'--single-select-option-id',$backlog.id,'--format','json')
        $phase = $epic.phases | Where-Object number -eq $number
        $level = if ($phase) { $phase.priority } elseif ($epic.key -in @('Foundation','Discovery','Workflows')) { 'High' } else { 'Medium' }
        $option = $priority.options | Where-Object name -eq $level
        $null = Invoke-GhJson @('project','item-edit','--id',$item.id,'--project-id',$project.id,'--field-id',$priority.id,'--single-select-option-id',$option.id,'--format','json')
    }
    Write-Output "Verified linking and Backlog placement requested for $($epic.key)"
} }
$items = Invoke-GhJson @('project','item-list',"$projectNumber",'--owner',$owner,'--limit','100','--format','json')
if ($items.items.Count -ne 30 -or @($items.items | Where-Object status -ne 'Backlog').Count -gt 0) { throw 'Expected 30 issues, all in Backlog.' }
foreach ($epic in $spec) {
    $parent = Invoke-GhJson @('api',"repos/$repo/issues/$($epic.number)")
    $children = Invoke-GhJson @('api',"repos/$repo/issues/$($epic.number)/sub_issues?per_page=100")
    if ($parent.sub_issues_summary.total -ne $epic.phases.Count -or @(Compare-Object @($epic.phases.number | Sort-Object) @($children.number | Sort-Object)).Count -ne 0) { throw "Hierarchy verification failed for $($epic.key)" }
    foreach ($phase in $epic.phases) {
        $blocking = Invoke-GhJson @('api',"repos/$repo/issues/$($phase.number)/dependencies/blocked_by?per_page=100")
        if (@(Compare-Object @($phase.dependencyNumbers | Sort-Object) @($blocking.number | Sort-Object)).Count -ne 0) { throw "Dependency verification failed for $($phase.key)" }
    }
    Write-Output "$($epic.key): $($parent.sub_issues_summary.total) native sub-issues, $($parent.sub_issues_summary.completed) completed; dependencies verified."
}
Write-Output 'All 30 open issues verified in Backlog.'
