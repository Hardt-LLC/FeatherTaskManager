#requires -Version 7.0
[CmdletBinding()]
param([switch]$SkipCompiler)
$ErrorActionPreference = 'Stop'
$projectRoot = Split-Path -Parent $PSScriptRoot
$toolsPath = Join-Path $projectRoot 'target\tools'
$modulesPath = Join-Path $toolsPath 'modules'
$null = New-Item -ItemType Directory -Path $modulesPath -Force
if (-not (Test-Path -LiteralPath (Join-Path $modulesPath 'ArtifactSigning\0.1.8\ArtifactSigning.psd1'))) {
    Save-Module ArtifactSigning -RequiredVersion 0.1.8 -Repository PSGallery -Path $modulesPath -Force
}
$env:PSModulePath = $modulesPath + [IO.Path]::PathSeparator + $env:PSModulePath
if (-not $SkipCompiler) {
    $compilerDir = Join-Path $toolsPath 'inno-6.7.3'
    $compiler = Join-Path $compilerDir 'ISCC.exe'
    if (-not (Test-Path -LiteralPath $compiler)) {
        $download = Join-Path $toolsPath 'innosetup-6.7.3.exe'
        Invoke-WebRequest -Uri 'https://github.com/jrsoftware/issrc/releases/download/is-6_7_3/innosetup-6.7.3.exe' -OutFile $download
        $hash = (Get-FileHash -LiteralPath $download -Algorithm SHA256).Hash.ToLowerInvariant()
        if ($hash -ne '9c73c3bae7ed48d44112a0f48e66742c00090bdb5bef71d9d3c056c66e97b732') { throw 'Inno Setup download checksum mismatch.' }
        $signature = Get-AuthenticodeSignature -LiteralPath $download
        if ($signature.Status -ne 'Valid' -or $signature.SignerCertificate.Subject -notmatch '(?:^|, )O=Pyrsys B\.V\.(?:,|$)') { throw 'Inno Setup download publisher verification failed.' }
        # Official /PORTABLE=1 mode disables uninstall registration and shortcuts.
        $arguments = @('/PORTABLE=1', '/CURRENTUSER', '/VERYSILENT', '/SUPPRESSMSGBOXES', '/NORESTART', '/NOICONS', ('/DIR="' + $compilerDir + '"'))
        $process = Start-Process -FilePath $download -ArgumentList $arguments -WindowStyle Hidden -PassThru -Wait
        if ($process.ExitCode -ne 0) { throw 'Portable Inno Setup extraction failed.' }
    }
    $env:FEATHER_INNO_COMPILER = $compiler
}
if ($env:GITHUB_ENV) {
    'PSModulePath=' + $env:PSModulePath | Out-File -FilePath $env:GITHUB_ENV -Append -Encoding utf8
    if ($env:FEATHER_INNO_COMPILER) { 'FEATHER_INNO_COMPILER=' + $env:FEATHER_INNO_COMPILER | Out-File -FilePath $env:GITHUB_ENV -Append -Encoding utf8 }
}
Write-Output 'Release tooling is ready in target/tools. No signing credentials were stored.'
