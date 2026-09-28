#requires -Version 7.0
# Copies the signed Setup asset of an immutable GitHub release into the Store
# package repository and returns a commit-pinned, redirect-free download URL.
[CmdletBinding()]
param(
    [string]$Version,
    [string]$Repository = 'Hardt-LLC/FeatherTaskManager',
    [string]$PackageRepository = 'Hardt-LLC/feather-store-packages',
    [string]$ExpectedSubject = $env:FEATHER_SIGNING_SUBJECT
)
$ErrorActionPreference = 'Stop'
Import-Module (Join-Path $PSScriptRoot 'store-api.psm1')
$root = Split-Path -Parent $PSScriptRoot
if (-not $Version) { $Version = (& (Join-Path $PSScriptRoot 'version.ps1')).Version }
if ($Version -cnotmatch '^20\d{2}\.([1-9]|1[0-2])\.([1-9]\d{0,4})$') { throw 'An explicit yyyy.m.x version is required.' }
if ($PackageRepository -cnotmatch '^[A-Za-z0-9-]+/[A-Za-z0-9._-]+$') { throw 'PackageRepository must be owner/name.' }
if ([string]::IsNullOrWhiteSpace($ExpectedSubject)) { throw 'FEATHER_SIGNING_SUBJECT must specify the expected installer publisher.' }
$name = "FeatherTaskManager-$Version-Setup-x64.exe"
$tag = "v$Version"

# The Store must receive the exact bytes of the published, immutable release asset.
$releaseJson = & gh api "repos/$Repository/releases/tags/$tag"
if ($LASTEXITCODE -ne 0) { throw "GitHub release $tag was not found in $Repository." }
$release = $releaseJson | ConvertFrom-Json
if ($release.draft -or $release.prerelease -or -not $release.immutable) { throw "$tag must be a published, immutable, non-prerelease GitHub release." }
$asset = @($release.assets | Where-Object { $_.name -ceq $name })
$digest = if ($asset.Count -eq 1) { [regex]::Match([string]$asset[0].digest, '^sha256:([0-9a-f]{64})$') } else { $null }
if ($null -eq $digest -or -not $digest.Success) { throw "$tag has no single $name asset with a SHA-256 digest." }
$sha256 = $digest.Groups[1].Value

$download = Join-Path $root "target\store-staging\$Version"
$null = New-Item -ItemType Directory -Force -Path $download
& gh release download $tag --repo $Repository --pattern $name --dir $download --clobber
if ($LASTEXITCODE -ne 0) { throw "Downloading $name from $tag failed." }
$installer = Join-Path $download $name
if ((Get-FileHash -LiteralPath $installer -Algorithm SHA256).Hash.ToLowerInvariant() -cne $sha256) { throw 'The downloaded installer does not match the release digest.' }
& (Join-Path $PSScriptRoot 'verify-signature.ps1') -FilePath $installer -ExpectedSubject $ExpectedSubject -RequireTimestamp | Out-Null
if ((Get-Item -LiteralPath $installer).VersionInfo.ProductVersion.Trim() -cne $Version) { throw "The installer version resource is not $Version." }

$work = Join-Path $root 'target\store-packages'
$gitAuth = @('-c', 'credential.interactive=never', '-c', 'credential.helper=', '-c', 'credential.helper=!gh auth git-credential')
function Invoke-PackageGit {
    $output = & git @gitAuth -C $work @args
    if ($LASTEXITCODE -ne 0) { throw "git $($args -join ' ') failed in the Store package repository." }
    $output
}
$remote = "https://github.com/$PackageRepository.git"
if (-not (Test-Path -LiteralPath (Join-Path $work '.git'))) {
    & git @gitAuth clone -q $remote $work
    if ($LASTEXITCODE -ne 0) { throw "Cloning $PackageRepository failed." }
}
if ((Invoke-PackageGit remote get-url origin) -cne $remote) { throw "$work is not a clone of $PackageRepository." }
if (Invoke-PackageGit status --porcelain) { throw "$work has local changes. Inspect and clean it before staging." }
Invoke-PackageGit fetch -q origin | Out-Null
Invoke-PackageGit checkout -q main | Out-Null
Invoke-PackageGit merge -q --ff-only origin/main | Out-Null

$directory = Join-Path $work $Version
$target = Join-Path $directory $name
if (Test-Path -LiteralPath $target) {
    # Store package URLs are pinned to commits and must never change bytes.
    if ((Get-FileHash -LiteralPath $target -Algorithm SHA256).Hash.ToLowerInvariant() -cne $sha256) { throw "$Version/$name already exists in $PackageRepository with different bytes. Cut a new version instead." }
} else {
    $null = New-Item -ItemType Directory -Force -Path $directory
    Copy-Item -LiteralPath $installer -Destination $target
    [IO.File]::WriteAllText((Join-Path $directory 'SHA256SUMS.txt'), "$sha256 *$name`n")
    $readmePath = Join-Path $work 'README.md'
    [IO.File]::WriteAllText($readmePath, (Add-StorePackageReadmeRow -Readme ([IO.File]::ReadAllText($readmePath)) -Version $Version -FileName $name))
    Invoke-PackageGit add '--' "$Version/$name" "$Version/SHA256SUMS.txt" README.md | Out-Null
    Invoke-PackageGit commit -q -m "Add signed $Version installer for the Microsoft Store" | Out-Null
    Invoke-PackageGit push -q origin HEAD:main | Out-Null
    Invoke-PackageGit fetch -q origin | Out-Null
}
$commit = [string](Invoke-PackageGit log -1 --format=%H origin/main '--' "$Version/$name")
if ($commit -notmatch '^[0-9a-f]{40}$') { throw "$Version/$name is not on $PackageRepository main." }

$url = "https://raw.githubusercontent.com/$PackageRepository/$commit/$Version/$name"
$check = Join-Path $download "served-$name"
for ($attempt = 1; ; $attempt++) {
    $response = Invoke-WebRequest -Uri $url -OutFile $check -PassThru -MaximumRedirection 0 -SkipHttpErrorCheck -TimeoutSec 300
    if ($response.StatusCode -eq 200) { break }
    if ($attempt -ge 6) { throw "The package URL returned HTTP $($response.StatusCode) instead of the installer." }
    Start-Sleep -Seconds 10
}
if ((Get-FileHash -LiteralPath $check -Algorithm SHA256).Hash.ToLowerInvariant() -cne $sha256) { throw 'The package URL does not serve the release bytes.' }
Remove-Item -LiteralPath $check
[pscustomobject]@{ Version = $Version; PackageUrl = $url; Sha256 = $sha256; Commit = $commit }
