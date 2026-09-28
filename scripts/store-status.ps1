#requires -Version 7.0
# Read-only view of the Microsoft Store draft and any submission in progress.
# It is also the connection test after setting up credentials.
[CmdletBinding()]
param(
    [string]$SubmissionId,
    [string]$MetadataPath
)
$ErrorActionPreference = 'Stop'
Import-Module (Join-Path $PSScriptRoot 'store-api.psm1')
$config = Read-StoreConfig $MetadataPath
$state = Get-StoreSubmissionState $config
$tracked = if ($SubmissionId) {
    if ($SubmissionId -notmatch '^\d{1,20}$') { throw 'SubmissionId must be numeric.' }
    Invoke-StoreApi $config GET (Get-StoreProductPath $config "/submission/$SubmissionId/status")
}
$packages = @((Invoke-StoreApi $config GET (Get-StoreProductPath $config '/packages')).packages)
$languages = @($config.Languages) -join ','
$listings = @((Invoke-StoreApi $config GET (Get-StoreProductPath $config "/metadata/listings?languages=$languages")).listings)
[pscustomobject]@{
    DraftReady = $state.IsReady
    OngoingSubmission = $state.OngoingSubmissionId
    OngoingStatus = $state.PublishingStatus
    Submission = $SubmissionId
    SubmissionStatus = if ($tracked) { "$($tracked.publishingStatus)$(if ($tracked.hasFailed) { ' (failed)' })" }
    Packages = @($packages | ForEach-Object { "$(@($_.architectures) -join '/') $($_.packageUrl)" })
    WhatsNew = @($listings | ForEach-Object { "$($_.language): $((([string]$_.whatsNew) -split "`r?`n")[0])" })
    Messages = @($state.Errors | ForEach-Object { "$($_.target): $($_.message) [$($_.code)]" })
}
