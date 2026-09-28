#requires -Version 7.0
# Updates the Microsoft Store draft for a released version and, with -Submit,
# sends it to certification. Without -Submit only the draft changes.
[CmdletBinding()]
param(
    [string]$Version,
    [string]$PackageUrl,
    [string]$WhatsNewFile,
    [switch]$SyncListings,
    [string]$AssetsDirectory,
    [switch]$Submit,
    [string]$MetadataPath,
    [string]$Repository = 'Hardt-LLC/FeatherTaskManager'
)
$ErrorActionPreference = 'Stop'
Import-Module (Join-Path $PSScriptRoot 'store-api.psm1')
$root = Split-Path -Parent $PSScriptRoot
$config = Read-StoreConfig $MetadataPath
if (-not $Version) { $Version = (& (Join-Path $PSScriptRoot 'version.ps1')).Version }
if ($Version -cnotmatch '^20\d{2}\.([1-9]|1[0-2])\.([1-9]\d{0,4})$') { throw 'An explicit yyyy.m.x version is required.' }
$languages = [string[]]@($config.Languages)
if (-not $WhatsNewFile) { $WhatsNewFile = Join-Path $root "releases\$Version.store.json" }
# Validate every local input before anything is pushed or changed remotely.
$whatsNew = Read-StoreWhatsNew -Path $WhatsNewFile -Languages $languages
$listings = if ($SyncListings) { Read-StoreListingFiles -Directory (Join-Path $root 'store\listings') -Languages $languages } else { $null }
if ($AssetsDirectory) { $null = Get-StoreAssetPlan $AssetsDirectory }
$state = Get-StoreSubmissionState $config
if ($state.OngoingSubmissionId) { throw "Submission $($state.OngoingSubmissionId) is $($state.PublishingStatus). Wait until it finishes (scripts/store-status.ps1)." }

if (-not $PackageUrl) {
    $staged = & (Join-Path $PSScriptRoot 'stage-store-package.ps1') -Version $Version -Repository $Repository -PackageRepository $config.PackageRepository
    $PackageUrl = $staged.PackageUrl
}
if (-not ([uri]$PackageUrl).AbsolutePath.EndsWith("/$Version/FeatherTaskManager-$Version-Setup-x64.exe")) { throw "The package URL is not the $Version installer." }

$result = Update-StoreDraft -Config $config -PackageUrl $PackageUrl -WhatsNew $whatsNew -Listings $listings -AssetsDirectory $AssetsDirectory -Submit:$Submit
$changes = if ($result.Changes.Count) { $result.Changes -join ', ' } else { 'nothing (draft already current)' }
Write-Host "Store draft for $Version updated: $changes."
if ($result.Submitted) {
    Write-Host "Submitted for certification as $($result.SubmissionId). Follow it with ./scripts/store-status.ps1 -SubmissionId $($result.SubmissionId)."
} else {
    Write-Host 'The draft is ready. Run again with -Submit to send it to certification.'
}
$result
