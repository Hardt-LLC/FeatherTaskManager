#requires -Version 7.0
[CmdletBinding()]
param(
    [Parameter(Mandatory)][string]$FilePath,
    [string]$MetadataPath = $env:FEATHER_SIGNING_METADATA,
    [string]$ExpectedSubject = $env:FEATHER_SIGNING_SUBJECT,
    [ValidateSet('Environment', 'AzureCli')]
    [string]$CredentialMode = $(if ($env:FEATHER_SIGNING_CREDENTIAL_MODE) { $env:FEATHER_SIGNING_CREDENTIAL_MODE } else { 'Environment' })
)
$ErrorActionPreference = 'Stop'
if ([string]::IsNullOrWhiteSpace($MetadataPath) -or [string]::IsNullOrWhiteSpace($ExpectedSubject)) {
    throw 'Signing requires FEATHER_SIGNING_METADATA and FEATHER_SIGNING_SUBJECT. No unsigned fallback is permitted.'
}
if ($CredentialMode -eq 'Environment') {
    foreach ($name in @('AZURE_TENANT_ID', 'AZURE_CLIENT_ID', 'AZURE_CLIENT_SECRET')) {
        if ([string]::IsNullOrWhiteSpace([Environment]::GetEnvironmentVariable($name))) { throw "Signing requires the $name process environment variable." }
    }
}
$file = (Resolve-Path -LiteralPath $FilePath).Path
if ($file.Contains(',')) { throw 'ArtifactSigning does not support commas in an individual file path.' }
$metadata = Get-Content -LiteralPath $MetadataPath -Raw | ConvertFrom-Json
foreach ($name in @('Endpoint', 'CodeSigningAccountName', 'CertificateProfileName')) {
    if ([string]::IsNullOrWhiteSpace($metadata.$name)) { throw "Signing metadata is missing $name." }
}
$endpoint = [uri]$metadata.Endpoint
if ($endpoint.Scheme -ne 'https' -or -not $endpoint.Host.EndsWith('.codesigning.azure.net')) {
    throw 'Signing endpoint must be an HTTPS Azure Artifact Signing regional endpoint.'
}
# The version is the one used by Azure/artifact-signing-action v2.0.0.
Import-Module ArtifactSigning -RequiredVersion 0.1.8 -ErrorAction Stop
$parameters = @{
    Endpoint = $metadata.Endpoint
    CodeSigningAccountName = $metadata.CodeSigningAccountName
    CertificateProfileName = $metadata.CertificateProfileName
    Files = $file
    FileDigest = 'SHA256'
    TimestampRfc3161 = 'http://timestamp.acs.microsoft.com'
    TimestampDigest = 'SHA256'
    ExcludeEnvironmentCredential = $CredentialMode -ne 'Environment'
    ExcludeWorkloadIdentityCredential = $true
    ExcludeAzureCliCredential = $CredentialMode -ne 'AzureCli'
    ExcludeInteractiveBrowserCredential = $true
    ExcludeManagedIdentityCredential = $true
    ExcludeSharedTokenCacheCredential = $true
    ExcludeVisualStudioCredential = $true
    ExcludeVisualStudioCodeCredential = $true
    ExcludeAzurePowerShellCredential = $true
    ExcludeAzureDeveloperCliCredential = $true
    Timeout = 300
}
Invoke-ArtifactSigning @parameters
& (Join-Path $PSScriptRoot 'verify-signature.ps1') -FilePath $file -ExpectedSubject $ExpectedSubject -RequireTimestamp
