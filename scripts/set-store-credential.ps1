#requires -Version 7.0
# One-time (and rotation) setup for the Microsoft Store submission API.
# Run it yourself in an interactive terminal: the client secret is typed
# hidden and stored with DPAPI for the current Windows user, never in the repo.
[CmdletBinding()]
param(
    [Parameter(Mandatory)][string]$TenantId,
    [Parameter(Mandatory)][string]$ClientId,
    [Parameter(Mandatory)][string]$SellerId,
    [ValidateSet('Secret', 'Certificate')][string]$Mode = 'Secret',
    [string]$MetadataPath
)
$ErrorActionPreference = 'Stop'
Import-Module (Join-Path $PSScriptRoot 'store-api.psm1')
$root = Split-Path -Parent $PSScriptRoot
$MetadataPath = Get-StoreMetadataPath $MetadataPath
$example = Get-Content -LiteralPath (Join-Path $root 'installer\store-metadata.example.json') -Raw | ConvertFrom-Json
$metadata = [ordered]@{
    ProductId = $example.ProductId
    SellerId = $SellerId.Trim()
    TenantId = $TenantId.Trim()
    ClientId = $ClientId.Trim()
    CredentialMode = $Mode
    CertificateThumbprint = ''
    PackageRepository = $example.PackageRepository
    Architecture = $example.Architecture
    Languages = @($example.Languages)
}

if ($Mode -eq 'Secret') {
    $secret = Read-Host -Prompt 'Client secret value (hidden)' -AsSecureString
    if ($secret.Length -lt 16) { throw 'That is not a client secret value. Paste the secret Value, not its ID.' }
    $path = Get-StoreSecretPath $metadata.ClientId
    $null = New-Item -ItemType Directory -Force -Path (Split-Path -Parent $path)
    [IO.File]::WriteAllText($path, (ConvertFrom-SecureString -SecureString $secret))
    Write-Host "Stored the secret for $($metadata.ClientId) with DPAPI (current Windows user only)."
} else {
    # Non-exportable key: the private key never leaves this user's certificate store.
    $certificate = New-SelfSignedCertificate -Subject "CN=Feather Store submission $($metadata.ClientId)" -CertStoreLocation 'Cert:\CurrentUser\My' `
        -KeyExportPolicy NonExportable -KeyAlgorithm RSA -KeyLength 3072 -HashAlgorithm SHA256 -KeyUsage DigitalSignature -NotAfter (Get-Date).AddMonths(12)
    $publicPath = Join-Path $root "target\store-credential\$($certificate.Thumbprint).cer"
    $null = New-Item -ItemType Directory -Force -Path (Split-Path -Parent $publicPath)
    $null = Export-Certificate -Cert $certificate -FilePath $publicPath -Type CERT
    $metadata.CertificateThumbprint = $certificate.Thumbprint
    Write-Host "Upload the public certificate $publicPath to the app registration (Certificates & secrets > Certificates). It expires $($certificate.NotAfter)."
}

$utf8 = [Text.UTF8Encoding]::new($false)
[IO.File]::WriteAllText($MetadataPath, ($metadata | ConvertTo-Json) + "`n", $utf8)
$null = Read-StoreConfig $MetadataPath
Write-Host "Wrote $MetadataPath. Test the connection with ./scripts/store-status.ps1."
