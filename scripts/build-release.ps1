#requires -Version 7.0
[CmdletBinding()]
param([switch]$Offline, [switch]$SkipBuild)
$ErrorActionPreference = 'Stop'
$projectRoot = Split-Path -Parent $PSScriptRoot
if (-not $env:FEATHER_SIGNING_METADATA -or -not $env:FEATHER_SIGNING_SUBJECT) {
    throw 'Configure the signing metadata and expected publisher before building a release.'
}
& (Join-Path $PSScriptRoot 'build.ps1') -Offline:$Offline -SkipBuild:$SkipBuild
$dist = Join-Path $projectRoot 'dist'
$binary = Join-Path $dist 'FeatherTaskManager.exe'
$version = & (Join-Path $PSScriptRoot 'version.ps1') -BinaryPath $binary
& (Join-Path $PSScriptRoot 'test-loader-policy.ps1') -FilePath $binary | Out-Host
# Sign the app before it is embedded. Inno then signs both Setup and Uninstall
# through exactly the same signer, including verification after every call.
& (Join-Path $PSScriptRoot 'sign-artifact.ps1') -FilePath $binary | Out-Host
& (Join-Path $PSScriptRoot 'build-installer.ps1') | Out-Host
$portableName = "FeatherTaskManager-$($version.Version)-Portable-x64.exe"
$setupName = "FeatherTaskManager-$($version.Version)-Setup-x64.exe"
$zipName = "FeatherTaskManager-$($version.Version)-Portable-x64.zip"
Copy-Item -LiteralPath $binary -Destination (Join-Path $dist $portableName) -Force
$archiveNames = @('FeatherTaskManager.exe', 'Restore-WindowsTaskManager.ps1', 'README.md', 'README.ko.md', 'INSTALLER.md', 'SECURITY.md', 'FEATURES-v2.md', 'LICENSE', 'THIRD_PARTY_NOTICES.txt', 'RUST_LIBRARY_NOTICES.html')
Compress-Archive -LiteralPath ($archiveNames | ForEach-Object { Join-Path $dist $_ }) -DestinationPath (Join-Path $dist $zipName) -Force
$assets = @($portableName, $setupName, $zipName)
$entries = foreach ($name in $assets) {
    $path = Join-Path $dist $name
    if ($name.EndsWith('.exe')) {
        & (Join-Path $PSScriptRoot 'verify-signature.ps1') -FilePath $path -RequireTimestamp | Out-Null
        $info = (Get-Item -LiteralPath $path).VersionInfo
        if ($info.FileVersion.Trim() -ne $version.WindowsVersion) { throw "Wrong release version on $name." }
    }
    [ordered]@{ Name = $name; Sha256 = (Get-FileHash -LiteralPath $path -Algorithm SHA256).Hash.ToLowerInvariant(); Bytes = (Get-Item -LiteralPath $path).Length }
}
$hashText = ($entries | ForEach-Object { "$($_.Sha256)  $($_.Name)" }) -join "`n"
[IO.File]::WriteAllText((Join-Path $dist 'SHA256SUMS.txt'), $hashText + "`n", [Text.UTF8Encoding]::new($false))
$manifest = [ordered]@{
    Version = $version.Version
    WindowsVersion = $version.WindowsVersion
    Tag = $version.Tag
    Signed = $true
    Publisher = $env:FEATHER_SIGNING_SUBJECT
    BuiltAtUtc = [DateTime]::UtcNow.ToString('o')
    Assets = @($entries)
}
[IO.File]::WriteAllText((Join-Path $dist 'release-manifest.json'), ($manifest | ConvertTo-Json -Depth 5) + "`n", [Text.UTF8Encoding]::new($false))
Write-Output "Signed release assets verified: $($version.Tag)"
