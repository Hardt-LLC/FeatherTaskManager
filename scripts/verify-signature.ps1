#requires -Version 5.1
[CmdletBinding()]
param(
    [Parameter(Mandatory)][string]$FilePath,
    [string]$ExpectedSubject = $env:FEATHER_SIGNING_SUBJECT,
    [switch]$RequireTimestamp
)
$ErrorActionPreference = 'Stop'
if ([string]::IsNullOrWhiteSpace($ExpectedSubject)) { throw 'FEATHER_SIGNING_SUBJECT must specify the expected certificate subject.' }
$signature = Get-AuthenticodeSignature -LiteralPath $FilePath
if ($signature.Status -ne 'Valid' -or $null -eq $signature.SignerCertificate) {
    throw "Authenticode validation failed for $FilePath ($($signature.Status))."
}
if ($signature.SignerCertificate.Subject -cne $ExpectedSubject) { throw 'The Authenticode publisher does not match the expected signing identity.' }
if ($RequireTimestamp -and $null -eq $signature.TimeStamperCertificate) { throw 'The release signature has no trusted timestamp.' }
[pscustomobject]@{
    File = (Get-Item -LiteralPath $FilePath).FullName
    Subject = $signature.SignerCertificate.Subject
    Thumbprint = $signature.SignerCertificate.Thumbprint
    Timestamped = $null -ne $signature.TimeStamperCertificate
}
