#requires -Version 5.1
[CmdletBinding()]
param([string]$Version, [switch]$Preview)
$ErrorActionPreference = 'Stop'
$projectRoot = Split-Path -Parent $PSScriptRoot
$current = & (Join-Path $PSScriptRoot 'version.ps1')
if (-not $Version) {
    $month = Get-Date -Format 'yyyy.M'
    $release = if ($current.Version.StartsWith($month + '.')) { [int]($current.Version.Split('.')[2]) + 1 } else { 1 }
    $Version = "$month.$release"
}
if ($Version -notmatch '^(20\d{2})\.([1-9]|1[0-2])\.([1-9]\d*)$' -or [int]($Version.Split('.')[2]) -gt 65535) {
    throw 'Use yyyy.m.x; month is unpadded and x is 1..65535.'
}
if ([version]$Version -le [version]$current.Version) { throw 'The new version must be greater than the current version.' }
if ($Preview) { Write-Output $Version; return }
$utf8 = [Text.UTF8Encoding]::new($false)
$cargoPath = Join-Path $projectRoot 'Cargo.toml'
$cargo = Get-Content -LiteralPath $cargoPath -Raw
$cargo = [regex]::new('(?m)^version\s*=\s*"[^"]+"\s*$').Replace($cargo, 'version = "' + $Version + '"', 1)
[IO.File]::WriteAllText($cargoPath, $cargo, $utf8)
$lockPath = Join-Path $projectRoot 'Cargo.lock'
$lock = Get-Content -LiteralPath $lockPath -Raw
$lock = [regex]::Replace($lock, '(?m)(^name = "feather-task"\r?\nversion = ")[^"]+("$)', '${1}' + $Version + '${2}')
[IO.File]::WriteAllText($lockPath, $lock, $utf8)
$rcPath = Join-Path $projectRoot 'assets\app.rc'
$rc = Get-Content -LiteralPath $rcPath -Raw
$numeric = ($Version -replace '\.', ',') + ',0'
$rc = [regex]::Replace($rc, '(?m)^(FILEVERSION|PRODUCTVERSION)\s+[\d,]+', '${1} ' + $numeric)
$rc = [regex]::Replace($rc, '(VALUE "FileVersion", ")[^"\\]+', '${1}' + $Version + '.0')
$rc = [regex]::Replace($rc, '(VALUE "ProductVersion", ")[^"\\]+', '${1}' + $Version)
[IO.File]::WriteAllText($rcPath, $rc, $utf8)
$manifestPath = Join-Path $projectRoot 'app.manifest'
$manifest = Get-Content -LiteralPath $manifestPath -Raw
$manifest = [regex]::Replace($manifest, '(<assemblyIdentity version=")[^"]+(" processorArchitecture="amd64" name="FeatherTaskManager")', '${1}' + $Version + '.0${2}')
[IO.File]::WriteAllText($manifestPath, $manifest, $utf8)
& (Join-Path $PSScriptRoot 'version.ps1')
