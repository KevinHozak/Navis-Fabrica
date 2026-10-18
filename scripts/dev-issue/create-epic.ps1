<# Retired compatibility entry point. dev-issue now creates local Markdown issues. #>
[CmdletBinding()]
param([string]$SpecFile)
throw 'create-epic.ps1 is retired. Use dev-issue with the project runner.json and fabrica-runner issues-create/issues-update to create and verify local epics. No remote operations were performed.'