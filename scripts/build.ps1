#requires -Version 5.1
[CmdletBinding()]
param([switch]$Offline, [switch]$SkipBuild)
$ErrorActionPreference = 'Stop'
$projectRoot = Split-Path -Parent $PSScriptRoot
Push-Location $projectRoot
try {
    $cargoArgs = @('build', '--release', '--locked')
    if ($Offline) { $cargoArgs += '--offline' }
    if (-not $SkipBuild) {
        & cargo @cargoArgs
        if ($LASTEXITCODE -ne 0) { throw 'Release build failed.' }
    }
    $distPath = Join-Path $projectRoot 'dist'
    $null = New-Item -ItemType Directory -Path $distPath -Force
    Copy-Item -LiteralPath (Join-Path $projectRoot 'target\release\FeatherTaskManager.exe') -Destination $distPath -Force
    Copy-Item -LiteralPath (Join-Path $projectRoot 'scripts\Restore-WindowsTaskManager.ps1') -Destination $distPath -Force
    foreach ($name in @('README.md', 'README.en.md', 'INSTALLER.md', 'SIGNING.md', 'FEATURES-v2.md', 'BENCHMARK.md', 'VALIDATION.md', 'LICENSE', 'THIRD_PARTY_NOTICES.txt', 'RUST_LIBRARY_NOTICES.html')) {
        Copy-Item -LiteralPath (Join-Path $projectRoot $name) -Destination $distPath -Force
    }
    $designPath = Join-Path $distPath 'design-system\feather-task'
    $null = New-Item -ItemType Directory -Path $designPath -Force
    Copy-Item -LiteralPath (Join-Path $projectRoot 'design-system\feather-task\MASTER.md') -Destination $designPath -Force
    $measurementsPath = Join-Path $distPath 'measurements'
    $null = New-Item -ItemType Directory -Path $measurementsPath -Force
    Get-ChildItem -LiteralPath (Join-Path $projectRoot 'measurements') -File | Copy-Item -Destination $measurementsPath -Force
    $binary = Join-Path $distPath 'FeatherTaskManager.exe'
    $version = & (Join-Path $PSScriptRoot 'version.ps1') -BinaryPath $binary
    $hash = (Get-FileHash -LiteralPath $binary -Algorithm SHA256).Hash.ToLowerInvariant()
    [IO.File]::WriteAllText((Join-Path $distPath 'SHA256SUMS.txt'), "$hash  FeatherTaskManager.exe`r`n")
    $files = @('FeatherTaskManager.exe', 'Restore-WindowsTaskManager.ps1', 'README.md', 'README.en.md', 'INSTALLER.md', 'SIGNING.md', 'FEATURES-v2.md', 'BENCHMARK.md', 'VALIDATION.md', 'LICENSE', 'THIRD_PARTY_NOTICES.txt', 'RUST_LIBRARY_NOTICES.html', 'SHA256SUMS.txt') | ForEach-Object { Join-Path $distPath $_ }
    $files += Join-Path $distPath 'design-system'
    $files += $measurementsPath
    Compress-Archive -LiteralPath $files -DestinationPath (Join-Path $distPath 'FeatherTaskManager-windows-x64.zip') -Force
    Write-Output "Ready: $binary"
    Write-Output "Version: $($version.Version). This command creates a local build; use build-release.ps1 for signed release assets."
} finally { Pop-Location }
