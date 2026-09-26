#requires -Version 7.0
[CmdletBinding()]
param(
    [Parameter(Mandatory)][string]$Tag,
    [string]$RepositoryPath = (Split-Path -Parent $PSScriptRoot)
)
$ErrorActionPreference = 'Stop'
if ($Tag -cnotmatch '^v20\d{2}\.([1-9]|1[0-2])\.([1-9]\d{0,4})$') {
    throw 'An explicit yyyy.m.x release tag is required.'
}
function Read-GitRevision([string]$Revision) {
    $value = & git -c "safe.directory=$RepositoryPath" -C $RepositoryPath rev-parse --verify $Revision 2>$null
    if ($LASTEXITCODE -ne 0 -or $value -notmatch '^[0-9a-f]{40}$') { throw 'The release source revision could not be resolved.' }
    return $value
}
$headCommit = Read-GitRevision 'HEAD^{commit}'
$tagCommit = Read-GitRevision ('refs/tags/' + $Tag + '^{commit}')
if ($headCommit -cne $tagCommit) { throw 'HEAD must exactly match the release tag commit.' }
$mainCommit = Read-GitRevision 'refs/remotes/origin/main^{commit}'
& git -c "safe.directory=$RepositoryPath" -C $RepositoryPath merge-base --is-ancestor $headCommit $mainCommit
if ($LASTEXITCODE -ne 0) { throw 'The release tag must belong to origin/main history.' }
& git -c "safe.directory=$RepositoryPath" -C $RepositoryPath diff --quiet HEAD --
if ($LASTEXITCODE -ne 0) { throw 'Tracked source changes must be committed before publishing.' }
Write-Output $headCommit
