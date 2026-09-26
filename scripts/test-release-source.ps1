#requires -Version 7.0
$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot
$fixture = Join-Path $root ('target\security-review\source-gate-' + [guid]::NewGuid().ToString('N'))
$null = New-Item -ItemType Directory -Path $fixture
$gate = Join-Path $PSScriptRoot 'verify-release-source.ps1'
function Invoke-FixtureGit([string[]]$Arguments) {
    & git -c "safe.directory=$fixture" -C $fixture @Arguments | Out-Null
    if ($LASTEXITCODE -ne 0) { throw 'Fixture Git command failed.' }
}
function Expect-Rejected([string]$Tag, [string]$Reason) {
    try { & $gate -RepositoryPath $fixture -Tag $Tag | Out-Null }
    catch {
        if ($_.Exception.Message.Contains($Reason)) { return }
        throw
    }
    throw "Source gate accepted a rejected fixture: $Reason"
}
Invoke-FixtureGit @('init', '-q', '-b', 'main')
Invoke-FixtureGit @('config', 'user.name', 'Release Gate Fixture')
Invoke-FixtureGit @('config', 'user.email', 'fixture@example.invalid')
$file = Join-Path $fixture 'fixture.txt'
[IO.File]::WriteAllText($file, "original`n")
Invoke-FixtureGit @('add', 'fixture.txt')
Invoke-FixtureGit @('commit', '-qm', 'Fixture baseline')
Invoke-FixtureGit @('tag', 'v2026.9.1')
Invoke-FixtureGit @('update-ref', 'refs/remotes/origin/main', 'HEAD')
$commit = & $gate -RepositoryPath $fixture -Tag 'v2026.9.1'
if ($commit -notmatch '^[0-9a-f]{40}$') { throw 'Clean tagged source was rejected.' }
Expect-Rejected 'main' 'explicit yyyy.m.x'
Expect-Rejected 'v2026.9.99' 'could not be resolved'
[IO.File]::WriteAllText($file, "changed`n")
Expect-Rejected 'v2026.9.1' 'Tracked source changes'
Invoke-FixtureGit @('add', 'fixture.txt')
Expect-Rejected 'v2026.9.1' 'Tracked source changes'
Invoke-FixtureGit @('commit', '-qm', 'Fixture divergent source')
Expect-Rejected 'v2026.9.1' 'HEAD must exactly match'
Invoke-FixtureGit @('tag', 'v2026.9.2')
Expect-Rejected 'v2026.9.2' 'origin/main history'
Invoke-FixtureGit @('update-ref', 'refs/remotes/origin/main', 'HEAD')
& $gate -RepositoryPath $fixture -Tag 'v2026.9.2' | Out-Null
Write-Output 'PASS: release source gate accepted 2 valid states and rejected 6 invalid states.'
