#requires -Version 7.0
$ErrorActionPreference = 'Stop'
$gate = Join-Path $PSScriptRoot 'verify-remote-release-tag.ps1'
$commit = 'a' * 40
$tagObject = 'b' * 40
$route = 'repos/owner/repository/git/ref/tags/v2026.9.2'
$tagRoute = "repos/owner/repository/git/tags/$tagObject"
$responses = @{}
function gh([string]$Command, [string]$Route) {
    if ($Command -cne 'api' -or -not $responses.ContainsKey($Route)) {
        $global:LASTEXITCODE = 1
        return
    }
    $global:LASTEXITCODE = 0
    $responses[$Route] | ConvertTo-Json -Depth 5
}
function Test-Gate {
    & $gate -Repository 'owner/repository' -Tag 'v2026.9.2' -ExpectedCommit $commit
}
function Expect-Rejected([string]$Reason) {
    try { Test-Gate }
    catch {
        if ($_.Exception.Message.Contains($Reason)) { return }
        throw
    }
    throw "Remote tag gate accepted a rejected fixture: $Reason"
}
$responses[$route] = @{ref='refs/tags/v2026.9.2'; object=@{type='commit'; sha=$commit}}
Test-Gate
$responses[$route].object = @{type='tag'; sha=$tagObject}
$responses[$tagRoute] = @{object=@{type='commit'; sha=$commit}}
Test-Gate
$responses[$tagRoute].object.sha = 'c' * 40
Expect-Rejected 'does not match'
$responses[$route].ref = 'refs/tags/v2026.9.1'
Expect-Rejected 'unexpected tag reference'
$responses[$route].ref = 'refs/tags/v2026.9.2'
$responses[$tagRoute].object = @{type='blob'; sha=$commit}
Expect-Rejected 'does not resolve to a commit'
$responses[$tagRoute].object = @{type='tag'; sha=$tagObject}
Expect-Rejected 'nesting exceeds'
$responses[$route].object.sha = 'not-a-sha'
Expect-Rejected 'Invalid remote tag object'
$responses.Clear()
Expect-Rejected 'could not be verified'
Write-Output 'PASS: remote tag gate accepted 2 valid states and rejected 6 invalid states without network calls.'
