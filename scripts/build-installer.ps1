#requires -Version 7.0
[CmdletBinding()]
param(
    [string]$CompilerPath = $(if ($env:FEATHER_INNO_COMPILER) { $env:FEATHER_INNO_COMPILER } else { "${env:ProgramFiles(x86)}\Inno Setup 6\ISCC.exe" }),
    [switch]$UnsignedDevelopment
)
$ErrorActionPreference = 'Stop'
$projectRoot = Split-Path -Parent $PSScriptRoot
$binary = Join-Path $projectRoot 'dist\FeatherTaskManager.exe'
$version = & (Join-Path $PSScriptRoot 'version.ps1') -BinaryPath $binary
if (-not (Test-Path -LiteralPath $CompilerPath -PathType Leaf)) { throw 'Install Inno Setup 6.7.3 or pass -CompilerPath to its ISCC.exe.' }
$compilerSignature = Get-AuthenticodeSignature -LiteralPath $CompilerPath
if ($compilerSignature.Status -ne 'Valid' -or $compilerSignature.SignerCertificate.Subject -notmatch '(?:^|, )O=Pyrsys B\.V\.(?:,|$)') {
    throw 'Inno Setup compiler publisher verification failed.'
}
$expectedCompilerHash = '0a8757031b33777e4c9cbffee40f11a5062b36d25cbe144c1db73b6102b80ad7'
if ((Get-FileHash -LiteralPath $CompilerPath -Algorithm SHA256).Hash.ToLowerInvariant() -ne $expectedCompilerHash) {
    throw 'Expected the pinned Inno Setup 6.7.3 compiler. Verify an upgrade before changing its hash.'
}
$argsList = @('/Qp', "/DAppVersion=$($version.Version)", "/DWindowsVersion=$($version.WindowsVersion)")
if ($UnsignedDevelopment) {
    $argsList += '/DUnsignedDevelopment=1'
} else {
    & (Join-Path $PSScriptRoot 'verify-signature.ps1') -FilePath $binary -RequireTimestamp | Out-Null
    if (-not $env:FEATHER_SIGNING_METADATA) { throw 'FEATHER_SIGNING_METADATA must be set to sign the installer and embedded uninstaller.' }
    $shellPath = Join-Path $PSHOME 'pwsh.exe'
    if (-not (Test-Path -LiteralPath $shellPath -PathType Leaf)) { throw 'The PowerShell 7 executable for Inno signing could not be found.' }
    $signScript = Join-Path $PSScriptRoot 'sign-artifact.ps1'
    # Inno expands $q after native argument parsing; literal embedded quotes can
    # be consumed by the compiler's Windows command-line parser.
    $signCommand = '$q' + $shellPath + '$q -NoProfile -NonInteractive -File $q' + $signScript + '$q -FilePath $f'
    $argsList += '/SArtifactSigning=' + $signCommand
}
$argsList += Join-Path $projectRoot 'installer\FeatherTaskManager.iss'
& $CompilerPath @argsList
if ($LASTEXITCODE -ne 0) { throw 'Inno Setup compilation or signing failed.' }
$suffix = if ($UnsignedDevelopment) { '-UNSIGNED-DEVELOPMENT' } else { '' }
$installer = Join-Path $projectRoot "dist\FeatherTaskManager-$($version.Version)-Setup-x64$suffix.exe"
if (-not $UnsignedDevelopment) {
    & (Join-Path $PSScriptRoot 'verify-signature.ps1') -FilePath $installer -RequireTimestamp | Out-Null
    # Inno's SignTool mode signs uninst.e32.tmp, embeds its verified bytes, and
    # deletes the temporary file. SignedUninstallerDir is not a retained cache in
    # this mode. sign-artifact.ps1 already verified publisher and timestamp before
    # returning from that callback; any failure aborts the compiler above.
    # See Inno 6.7.3 Compiler.SetupCompiler.pas, SignSetupMemoryFile.
}
Write-Output $installer
