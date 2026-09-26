#requires -Version 7.0
[CmdletBinding()]
param(
    [string]$Repository = 'Hardt-LLC/FeatherTaskManager',
    [Parameter(Mandatory)][string]$NotesFile
)
$ErrorActionPreference = 'Stop'
$projectRoot = Split-Path -Parent $PSScriptRoot
$dist = Join-Path $projectRoot 'dist'
$version = & (Join-Path $PSScriptRoot 'version.ps1')
$sourceCommit = & (Join-Path $PSScriptRoot 'verify-release-source.ps1') -Tag $version.Tag
$manifestPath = Join-Path $dist 'release-manifest.json'
$manifest = Get-Content -LiteralPath $manifestPath -Raw | ConvertFrom-Json
if (-not $manifest.Signed -or $manifest.Version -cne $version.Version -or $manifest.Tag -cne $version.Tag) { throw 'Release manifest version/signing state is invalid.' }
if ($manifest.Publisher -cne $env:FEATHER_SIGNING_SUBJECT) { throw 'Configure the expected signing publisher before publishing.' }
$expectedNames = @("FeatherTaskManager-$($version.Version)-Portable-x64.exe", "FeatherTaskManager-$($version.Version)-Setup-x64.exe", "FeatherTaskManager-$($version.Version)-Portable-x64.zip")
if (@($manifest.Assets).Count -ne 3) { throw 'Unexpected release assets.' }
$assets = foreach ($name in $expectedNames) {
    $entry = @($manifest.Assets | Where-Object { $_.Name -ceq $name })
    if ($entry.Count -ne 1) { throw "Missing or duplicate release asset $name." }
    $path = Join-Path $dist $name
    if ((Get-FileHash -LiteralPath $path -Algorithm SHA256).Hash.ToLowerInvariant() -cne $entry[0].Sha256) { throw "Release checksum mismatch: $name." }
    if ($name.EndsWith('.exe')) { & (Join-Path $PSScriptRoot 'verify-signature.ps1') -FilePath $path -RequireTimestamp | Out-Null }
    if ($name.EndsWith('-Portable-x64.exe')) { & (Join-Path $PSScriptRoot 'test-loader-policy.ps1') -FilePath $path | Out-Null }
    $path
}
$assets += Join-Path $dist 'SHA256SUMS.txt'
$assets += $manifestPath
$expectedHashes = ($manifest.Assets | ForEach-Object { "$($_.Sha256)  $($_.Name)" }) -join "`n"
if ((Get-Content -LiteralPath (Join-Path $dist 'SHA256SUMS.txt') -Raw).TrimEnd() -cne $expectedHashes) { throw 'The published checksum list does not match the release manifest.' }
$notes = (Resolve-Path -LiteralPath $NotesFile).Path
& (Join-Path $PSScriptRoot 'verify-remote-release-tag.ps1') -Repository $Repository -Tag $version.Tag -ExpectedCommit $sourceCommit
# Tag creation and replacing published assets are deliberately separate actions.
# This command requires an existing pushed tag and refuses to clobber a release.
& gh release create $version.Tag @assets --repo $Repository --verify-tag --title "Feather Task Manager $($version.Version)" --notes-file $notes
if ($LASTEXITCODE -ne 0) { throw 'GitHub Release publication failed; no fallback or clobber was attempted.' }
