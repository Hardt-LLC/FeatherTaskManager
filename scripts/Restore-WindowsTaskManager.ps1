#requires -Version 5.1
#requires -RunAsAdministrator
[CmdletBinding()]
param()

$ErrorActionPreference = 'Stop'
if (-not [Environment]::Is64BitProcess) {
    throw 'Run this script in an administrator 64-bit PowerShell window.'
}

# Resolve the Windows known folder, never a caller-controlled environment variable.
$programFiles = [Environment]::GetFolderPath([Environment+SpecialFolder]::ProgramFiles)
if ([string]::IsNullOrWhiteSpace($programFiles) -or -not [IO.Path]::IsPathRooted($programFiles)) {
    throw 'The Windows Program Files known folder could not be resolved.'
}
$featherExe = Join-Path $programFiles 'Feather Task Manager\FeatherTaskManager.exe'
$expectedCommand = '"' + $featherExe + '" --task-manager'
$expectedBytes = [Text.Encoding]::Unicode.GetBytes($expectedCommand + [char]0)

# Check raw bytes so embedded NULs, odd lengths, wrong types and extra data
# cannot be mistaken for the exact REG_SZ written by Feather.
Add-Type -TypeDefinition @'
using System;
using System.Runtime.InteropServices;
public static class FeatherTaskRecoveryRegistry {
    [DllImport("advapi32.dll", CharSet = CharSet.Unicode, ExactSpelling = true)]
    public static extern int RegQueryValueExW(
        IntPtr key, string name, IntPtr reserved, out uint type,
        [Out] byte[] data, ref uint size);
}
'@

function Assert-FeatherDebuggerValue {
    param([Microsoft.Win32.RegistryKey]$Key, [byte[]]$Expected)
    [uint32]$kind = 0
    [uint32]$size = $Expected.Length
    $actual = New-Object byte[] $Expected.Length
    $status = [FeatherTaskRecoveryRegistry]::RegQueryValueExW(
        $Key.Handle.DangerousGetHandle(), 'Debugger', [IntPtr]::Zero,
        [ref]$kind, $actual, [ref]$size)
    if ($status -eq 2) { return $false } # Value already absent.
    if ($status -ne 0 -or $kind -ne 1 -or $size -ne $Expected.Length) {
        throw 'Debugger is foreign, malformed, or unreadable. No registry setting was changed.'
    }
    for ($index = 0; $index -lt $Expected.Length; $index++) {
        if ($actual[$index] -ne $Expected[$index]) {
            throw 'Debugger does not exactly match Feather. No registry setting was changed.'
        }
    }
    return $true
}

$baseKey = [Microsoft.Win32.RegistryKey]::OpenBaseKey(
    [Microsoft.Win32.RegistryHive]::LocalMachine,
    [Microsoft.Win32.RegistryView]::Registry64)
$taskManagerKey = $null
try {
    $taskManagerKey = $baseKey.OpenSubKey(
        'SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\taskmgr.exe',
        $true)
    if ($null -eq $taskManagerKey -or
        -not (Assert-FeatherDebuggerValue -Key $taskManagerKey -Expected $expectedBytes)) {
        Write-Output 'No Feather Task Manager connection needs to be removed.'
        return
    }
    # Preserve the key itself and every value except the verified Debugger value.
    $taskManagerKey.DeleteValue('Debugger', $false)
    Write-Output 'Feather connection removed. Other Windows settings were preserved.'
} finally {
    if ($null -ne $taskManagerKey) { $taskManagerKey.Dispose() }
    $baseKey.Dispose()
}
