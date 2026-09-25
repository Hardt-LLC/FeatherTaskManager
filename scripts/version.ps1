#requires -Version 5.1
[CmdletBinding()]
param([string]$BinaryPath)
$ErrorActionPreference = 'Stop'
$projectRoot = Split-Path -Parent $PSScriptRoot
$cargo = Get-Content -LiteralPath (Join-Path $projectRoot 'Cargo.toml') -Raw
$match = [regex]::Match($cargo, '(?m)^version\s*=\s*"([^"]+)"\s*$')
if (-not $match.Success -or $match.Groups[1].Value -notmatch '^(20\d{2})\.([1-9]|1[0-2])\.([1-9]\d*)$') {
    throw 'Cargo version must use yyyy.m.x, with an unpadded month and a positive release number.'
}
$version = $match.Groups[1].Value
$parts = $version.Split('.')
if ([int]$parts[2] -gt 65535) { throw 'Release number must fit the Windows PE version field.' }
$windowsVersion = "$version.0"
$resource = Get-Content -LiteralPath (Join-Path $projectRoot 'assets\app.rc') -Raw
$numeric = ($windowsVersion -replace '\.', ',')
foreach ($field in @('FILEVERSION', 'PRODUCTVERSION')) {
    if ($resource -notmatch "(?m)^$field\s+$([regex]::Escape($numeric))\s*$") { throw "$field does not match Cargo version." }
}
$lock = Get-Content -LiteralPath (Join-Path $projectRoot 'Cargo.lock') -Raw
if ($lock -notmatch ('(?m)^name = "feather-task"\r?\nversion = "' + [regex]::Escape($version) + '"')) { throw 'Cargo.lock package version does not match Cargo.toml.' }
$manifest = Get-Content -LiteralPath (Join-Path $projectRoot 'app.manifest') -Raw
if ($manifest -notmatch ('<assemblyIdentity version="' + [regex]::Escape($windowsVersion) + '" processorArchitecture="amd64" name="FeatherTaskManager"')) { throw 'Application manifest version does not match Cargo.toml.' }
if ($BinaryPath) {
    $info = (Get-Item -LiteralPath $BinaryPath).VersionInfo
    if ($info.FileVersion.Trim() -ne $windowsVersion -or $info.ProductVersion.Trim() -ne $version) {
        throw "Binary version does not match $version / $windowsVersion. Rebuild before packaging."
    }
}
[pscustomobject]@{ Version = $version; WindowsVersion = $windowsVersion; Tag = "v$version" }
