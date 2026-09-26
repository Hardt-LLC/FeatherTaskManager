#requires -Version 5.1
<#
.SYNOPSIS
Checks the x64 application PE loader policy without loading or running the file.
.DESCRIPTION
Requires System32-only static DLL imports and ASLR/DEP/high-entropy VA flags.
DependentLoadFlags is enforced by Windows 10 version 1607 (RS1) and later;
older Windows versions ignore it. This is an application gate, not an Inno
Setup/uninstaller gate or an Authenticode check.
https://learn.microsoft.com/cpp/build/reference/dependentloadflag
https://learn.microsoft.com/windows/win32/debug/pe-format
#>
[CmdletBinding()]
param([Parameter(Mandatory)][string]$FilePath)

$ErrorActionPreference = 'Stop'
$item = Get-Item -LiteralPath $FilePath
if ($item.PSIsContainer) { throw 'Loader policy requires an executable file.' }
# Deny concurrent writes while parsing; never map or execute the image.
$stream = [IO.File]::Open($item.FullName, [IO.FileMode]::Open, [IO.FileAccess]::Read, [IO.FileShare]::Read)
try {
    function Read-PeBytes([uint64]$Offset, [uint32]$Count) {
        if ($Offset -gt [uint64]$stream.Length -or [uint64]$Count -gt [uint64]$stream.Length - $Offset) {
            throw 'Malformed PE: a field extends beyond the file.'
        }
        $buffer = New-Object byte[] $Count
        $stream.Position = [int64]$Offset
        $read = 0
        while ($read -lt $buffer.Length) {
            $next = $stream.Read($buffer, $read, $buffer.Length - $read)
            if ($next -eq 0) { throw 'Malformed PE: truncated file.' }
            $read += $next
        }
        return ,$buffer
    }
    function Read-PeU16([uint64]$Offset) { [BitConverter]::ToUInt16((Read-PeBytes $Offset 2), 0) }
    function Read-PeU32([uint64]$Offset) { [BitConverter]::ToUInt32((Read-PeBytes $Offset 4), 0) }

    if ((Read-PeU16 0) -ne 0x5a4d) { throw 'Malformed PE: missing DOS signature.' }
    [uint64]$nt = Read-PeU32 0x3c
    if ($nt -lt 0x40 -or (Read-PeU32 $nt) -ne 0x00004550) { throw 'Malformed PE: missing NT signature.' }
    if ((Read-PeU16 ($nt + 4)) -ne 0x8664) { throw 'Loader policy requires an x64 application.' }
    $sectionCount = Read-PeU16 ($nt + 6)
    if ($sectionCount -lt 1 -or $sectionCount -gt 96) { throw 'Malformed PE: invalid section count.' }
    $characteristics = Read-PeU16 ($nt + 22)
    if (($characteristics -band 0x2002) -ne 2) { throw 'Loader policy requires an executable application, not a DLL.' }
    $optionalSize = Read-PeU16 ($nt + 20)
    # PE32+ has 112 fixed bytes; data-directory entry 10 is Load Configuration.
    if ($optionalSize -lt 200 -or $optionalSize -gt 4096) { throw 'Malformed PE: invalid optional header size.' }
    [uint64]$optional = $nt + 24
    $header = Read-PeBytes $optional $optionalSize
    if ([BitConverter]::ToUInt16($header, 0) -ne 0x20b) { throw 'Loader policy requires PE32+.' }
    $directoryCount = [BitConverter]::ToUInt32($header, 108)
    if ($directoryCount -lt 11 -or [uint64]$directoryCount -gt [Math]::Floor(($optionalSize - 112) / 8)) {
        throw 'Malformed PE: invalid data-directory count.'
    }
    $dllCharacteristics = [BitConverter]::ToUInt16($header, 70)
    # IMAGE_DLLCHARACTERISTICS_HIGH_ENTROPY_VA | DYNAMIC_BASE | NX_COMPAT.
    if (($dllCharacteristics -band 0x0160) -ne 0x0160) {
        throw ('Loader policy requires high-entropy ASLR, dynamic base and DEP (DllCharacteristics=0x{0:X4}).' -f $dllCharacteristics)
    }
    if (($characteristics -band 1) -ne 0) { throw 'Loader policy rejects an image with stripped base relocations.' }

    [uint64]$sizeOfHeaders = [BitConverter]::ToUInt32($header, 60)
    [uint64]$sectionTable = $optional + $optionalSize
    if ($sizeOfHeaders -gt [uint64]$stream.Length -or $sectionTable + [uint64]$sectionCount * 40 -gt $sizeOfHeaders) {
        throw 'Malformed PE: section table extends beyond the headers.'
    }
    $sections = for ($index = 0; $index -lt $sectionCount; $index++) {
        $section = Read-PeBytes ($sectionTable + [uint64]$index * 40) 40
        [uint64]$rawSize = [BitConverter]::ToUInt32($section, 16)
        [uint64]$rawOffset = [BitConverter]::ToUInt32($section, 20)
        if ($rawSize -gt 0 -and ($rawOffset -lt $sizeOfHeaders -or $rawOffset -gt [uint64]$stream.Length -or $rawSize -gt [uint64]$stream.Length - $rawOffset)) {
            throw 'Malformed PE: section raw data is outside the file.'
        }
        [pscustomobject]@{
            VirtualAddress = [uint64][BitConverter]::ToUInt32($section, 12)
            VirtualSize = [uint64][BitConverter]::ToUInt32($section, 8)
            RawSize = $rawSize
            RawOffset = $rawOffset
        }
    }
    function Resolve-PeRva([uint32]$Rva, [uint32]$Size) {
        if ($Rva -eq 0 -or $Size -eq 0 -or [uint64]$Rva + [uint64]$Size -gt 0x100000000) {
            throw 'Malformed PE: invalid data-directory range.'
        }
        $candidates = @()
        if ([uint64]$Rva -lt $sizeOfHeaders -and [uint64]$Size -le $sizeOfHeaders - [uint64]$Rva) {
            $candidates += [uint64]$Rva
        }
        foreach ($section in $sections) {
            [uint64]$span = [Math]::Max($section.VirtualSize, $section.RawSize)
            if ([uint64]$Rva -ge $section.VirtualAddress -and [uint64]$Rva - $section.VirtualAddress -lt $span) {
                [uint64]$delta = [uint64]$Rva - $section.VirtualAddress
                if ($delta -gt $section.RawSize -or [uint64]$Size -gt $section.RawSize - $delta) {
                    throw 'Malformed PE: data directory is not entirely file-backed.'
                }
                $candidates += $section.RawOffset + $delta
            }
        }
        if ($candidates.Count -ne 1) { throw 'Malformed PE: data-directory mapping is missing or ambiguous.' }
        return [uint64]$candidates[0]
    }

    $configRva = [BitConverter]::ToUInt32($header, 192)
    $configSize = [BitConverter]::ToUInt32($header, 196)
    if ($configSize -lt 80) { throw 'Loader policy requires a load configuration containing DependentLoadFlags.' }
    $configOffset = Resolve-PeRva $configRva $configSize
    $declaredSize = Read-PeU32 $configOffset
    if ($declaredSize -lt 80 -or $declaredSize -gt $configSize) { throw 'Malformed PE: invalid load-configuration size.' }
    $dependentFlags = Read-PeU16 ($configOffset + 78)
    if ($dependentFlags -ne 0x0800) {
        throw ('Loader policy requires System32-only static DLL imports (DependentLoadFlags=0x{0:X4}).' -f $dependentFlags)
    }
    [pscustomobject]@{
        File = $item.FullName
        DependentLoadFlags = '0x0800'
        DynamicBase = $true
        NxCompat = $true
        HighEntropyVa = $true
        MinimumPolicyWindows = 'Windows 10 version 1607 (RS1)'
    }
} finally {
    $stream.Dispose()
}
