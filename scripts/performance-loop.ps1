#requires -Version 5.1
<#
.SYNOPSIS
Capture repeated local benchmarks, or compare a candidate with an explicit baseline.
.DESCRIPTION
Does not build, change settings, promote baselines, edit source, or publish anything.
Generated captures stay under ignored target/performance-loop by default. A comparison
throws on regression, excessive noise, missing data, or incompatible conditions.
Startup measures input-idle observation, not first paint or complete data readiness.
.EXAMPLE
.\scripts\performance-loop.ps1 -Label 'Reviewed release build'
.EXAMPLE
.\scripts\performance-loop.ps1 -BaselinePath target\performance-loop\baseline\capture.json
.EXAMPLE
.\scripts\performance-loop.ps1 -BaselinePath baseline\capture.json -CandidatePath candidate\capture.json
#>
[CmdletBinding()]
param(
    [string]$ExePath,
    [string]$BaselinePath,
    [string]$CandidatePath,
    [string]$OutputDirectory,
    [ValidateSet('processes', 'performance', 'startup', 'services')]
    [string[]]$Pages = @('processes', 'performance'),
    [ValidateRange(1, 60)][int]$DurationSeconds = 10,
    [ValidateRange(3, 9)][int]$Iterations = 3,
    [string]$Label = 'Existing binary; source provenance unverified'
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
$projectRoot = Split-Path -Parent $PSScriptRoot
$measureScript = Join-Path $PSScriptRoot 'measure.ps1'
if ([string]::IsNullOrWhiteSpace($OutputDirectory)) {
    $OutputDirectory = Join-Path $projectRoot 'target\performance-loop'
}
if ([string]::IsNullOrWhiteSpace($ExePath)) {
    $ExePath = Join-Path $projectRoot 'target\release\FeatherTaskManager.exe'
}
if ($CandidatePath -and -not $BaselinePath) {
    throw 'CandidatePath requires BaselinePath; compare two existing captures explicitly.'
}
if ($Pages.Count -eq 0 -or @($Pages | Select-Object -Unique).Count -ne $Pages.Count) {
    throw 'Select at least one page, with no duplicates.'
}
$Pages = @($Pages | Sort-Object)

function Write-Json([string]$Path, $Value) {
    $json = $Value | ConvertTo-Json -Depth 15
    [IO.File]::WriteAllText($Path, $json + [Environment]::NewLine, (New-Object Text.UTF8Encoding($false)))
}

function Read-Capture([string]$Path) {
    $data = Get-Content -LiteralPath $Path -Raw | ConvertFrom-Json
    if ($data.schemaVersion -ne 1 -or $data.kind -ne 'FeatherTaskPerformanceCapture') {
        throw "Unsupported capture: $Path. Use this script's capture.json, not a measure.ps1 raw report."
    }
    if ($data.context.iterations -lt 3 -or $data.context.iterations -gt 9 -or
        $data.context.durationSeconds -lt 1 -or $data.context.durationSeconds -gt 60) {
        throw "Invalid benchmark parameters in $Path."
    }
    $capturePages = @($data.context.pages)
    if ($capturePages.Count -eq 0 -or @($capturePages | Select-Object -Unique).Count -ne $capturePages.Count -or
        @($data.measurements).Count -ne $capturePages.Count) {
        throw "Missing or duplicate pages in $Path."
    }
    foreach ($page in $capturePages) {
        if ($page -notin @('processes', 'performance', 'startup', 'services')) { throw "Invalid page in $Path." }
        $reports = @($data.measurements | Where-Object { $_.initialPage -eq $page })
        if ($reports.Count -ne 1) { throw "Expected one report for $page in $Path." }
        $report = $reports[0]
        if ($report.schemaVersion -ne 1 -or $report.requestedDurationSeconds -ne $data.context.durationSeconds -or
            $report.launchWindowStyle -ne 'Hidden' -or $report.logicalProcessors -ne $data.context.logicalProcessors -or
            $report.executableSha256 -ne $data.build.executableSha256 -or
            @($report.runs).Count -ne $data.context.iterations) {
            throw "Inconsistent measurement metadata for $page in $Path."
        }
        foreach ($run in $report.runs) {
            if ($run.status -ne 'ok' -or $run.sampleDurationSeconds -lt $data.context.durationSeconds -or $run.sampleCount -lt 1) {
                throw "Failed or incomplete measurement for $page in $Path."
            }
        }
    }
    return $data
}

function Get-Median([double[]]$Values) {
    $sorted = @($Values | Sort-Object)
    if ($sorted.Count -eq 0) { throw 'Cannot summarize an empty sample.' }
    $middle = [int][Math]::Floor($sorted.Count / 2)
    if ($sorted.Count % 2) { return $sorted[$middle] }
    return ($sorted[$middle - 1] + $sorted[$middle]) / 2
}

function Get-Statistics($Runs, [string]$Name) {
    $values = @()
    foreach ($run in $Runs) {
        $property = $run.PSObject.Properties[$Name]
        if ($null -eq $property) { throw "Missing metric: $Name." }
        if ($null -eq $property.Value) { continue }
        if ($property.Value -is [string] -or $property.Value -is [bool]) { throw "Non-numeric metric: $Name." }
        $value = [double]$property.Value
        if ([double]::IsNaN($value) -or [double]::IsInfinity($value) -or $value -lt 0) {
            throw "Invalid metric: $Name."
        }
        $values += $value
    }
    if ($values.Count -ne @($Runs).Count) {
        return [pscustomobject]@{ count = $values.Count; median = $null; mad = $null }
    }
    $median = Get-Median $values
    $deviations = @($values | ForEach-Object { [Math]::Abs($_ - $median) })
    return [pscustomobject]@{ count = $values.Count; median = $median; mad = (Get-Median $deviations) }
}

function Get-RegistryValues([string]$Path, [string[]]$Names) {
    $key = [Microsoft.Win32.Registry]::CurrentUser.OpenSubKey($Path, $false)
    try {
        $result = [ordered]@{}
        foreach ($name in $Names) {
            $result[$name] = if ($null -eq $key) { $null } else { $key.GetValue($name, $null) }
        }
        return [pscustomobject]$result
    } finally { if ($null -ne $key) { $key.Dispose() } }
}

function Get-Conditions {
    $powerOutput = & "$env:SystemRoot\System32\powercfg.exe" /getactivescheme
    if ($LASTEXITCODE -ne 0) { throw 'Cannot record the active power scheme.' }
    $scheme = [regex]::Match(($powerOutput -join ' '), '[0-9a-fA-F]{8}(?:-[0-9a-fA-F]{4}){3}-[0-9a-fA-F]{12}')
    if (-not $scheme.Success) { throw 'Cannot parse the active power scheme.' }
    $identity = [Security.Principal.WindowsIdentity]::GetCurrent()
    try {
        $principal = New-Object Security.Principal.WindowsPrincipal($identity)
        $elevated = $principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)
    } finally { $identity.Dispose() }
    return [pscustomobject][ordered]@{
        machine = [Environment]::MachineName
        operatingSystem = [Environment]::OSVersion.VersionString
        logicalProcessors = [Environment]::ProcessorCount
        processorIdentifier = $env:PROCESSOR_IDENTIFIER
        process64Bit = [Environment]::Is64BitProcess
        powerScheme = $scheme.Value.ToLowerInvariant()
        elevated = $elevated
        uiCulture = [Globalization.CultureInfo]::CurrentUICulture.Name
        preferences = Get-RegistryValues 'Software\FeatherTask\Preferences' @('Theme', 'RefreshMs', 'StartPage', 'Topmost', 'MinimizeToTray', 'AlwaysRunAsAdministrator', 'Language')
        systemTheme = Get-RegistryValues 'Software\Microsoft\Windows\CurrentVersion\Themes\Personalize' @('AppsUseLightTheme')
        measurementScriptSha256 = (Get-FileHash -LiteralPath $measureScript -Algorithm SHA256).Hash.ToLowerInvariant()
        pages = $Pages
        iterations = $Iterations
        durationSeconds = $DurationSeconds
        launchWindowStyle = 'Hidden'
    }
}

function Get-BuildContext([string]$Executable) {
    $head = & git -C $projectRoot rev-parse HEAD
    if ($LASTEXITCODE -ne 0) { throw 'Cannot record the source checkout HEAD.' }
    $branch = & git -C $projectRoot branch --show-current
    if ($LASTEXITCODE -ne 0) { throw 'Cannot record the source checkout branch.' }
    $changes = & git -C $projectRoot status --porcelain --untracked-files=normal
    if ($LASTEXITCODE -ne 0) { throw 'Cannot record the source checkout status.' }
    $compiler = & rustc -Vv
    if ($LASTEXITCODE -ne 0) { throw 'Cannot record the Rust compiler version.' }
    return [pscustomobject][ordered]@{
        label = $Label
        executable = $Executable
        executableSha256 = (Get-FileHash -LiteralPath $Executable -Algorithm SHA256).Hash.ToLowerInvariant()
        executableSizeBytes = (Get-Item -LiteralPath $Executable).Length
        checkoutHead = [string]$head
        checkoutBranch = [string]$branch
        checkoutDirty = (@($changes).Count -gt 0)
        rustc = $compiler -join "`n"
        sourceProvenance = 'Checkout and toolchain are observations, not proof this executable was built from them.'
    }
}

$resolvedOutput = $ExecutionContext.SessionState.Path.GetUnresolvedProviderPathFromPSPath($OutputDirectory)
$runId = [DateTime]::UtcNow.ToString('yyyyMMddTHHmmssfffZ') + '-' + [Guid]::NewGuid().ToString('N').Substring(0, 8)
$runDirectory = Join-Path $resolvedOutput $runId
$null = New-Item -ItemType Directory -Path $runDirectory
$lock = $null
try {
    $baseline = if ($BaselinePath) { Read-Capture $BaselinePath } else { $null }
    if ($CandidatePath) {
        $candidate = Read-Capture $CandidatePath
    } else {
        if ($env:OS -ne 'Windows_NT') { throw 'Capturing requires Windows.' }
        $resolvedExe = (Resolve-Path -LiteralPath $ExePath).ProviderPath
        if (-not (Test-Path -LiteralPath $resolvedExe -PathType Leaf)) { throw 'ExePath must be a file.' }
        $lockDirectory = Join-Path $projectRoot 'target\performance-loop'
        $null = New-Item -ItemType Directory -Path $lockDirectory -Force
        $lock = [IO.File]::Open((Join-Path $lockDirectory 'capture.lock'), [IO.FileMode]::OpenOrCreate, [IO.FileAccess]::ReadWrite, [IO.FileShare]::None)
        $conditions = Get-Conditions
        if ($null -ne $baseline -and ($baseline.context | ConvertTo-Json -Depth 8 -Compress) -cne ($conditions | ConvertTo-Json -Depth 8 -Compress)) {
            throw 'Baseline conditions differ. Match machine, privileges, settings, power scheme, pages, duration, repetitions, and measurement script.'
        }
        $build = Get-BuildContext $resolvedExe
        $measurements = @()
        foreach ($page in $Pages) {
            Write-Host "Measuring $page ($Iterations runs of $DurationSeconds seconds, plus bounded startup observation)."
            $rawPath = Join-Path $runDirectory ($page + '.json')
            & $measureScript -ExePath $resolvedExe -DurationSeconds $DurationSeconds -Iterations $Iterations -Page $page -OutputPath $rawPath | Out-Null
            $measurements += Get-Content -LiteralPath $rawPath -Raw | ConvertFrom-Json
        }
        $after = Get-Conditions
        if (($conditions | ConvertTo-Json -Depth 8 -Compress) -cne ($after | ConvertTo-Json -Depth 8 -Compress)) {
            throw 'Benchmark conditions changed during capture; discard this run and repeat in stable conditions.'
        }
        $capture = [pscustomobject][ordered]@{
            schemaVersion = 1
            kind = 'FeatherTaskPerformanceCapture'
            recordedAtUtc = [DateTime]::UtcNow.ToString('o')
            context = $conditions
            build = $build
            measurements = $measurements
        }
        $CandidatePath = Join-Path $runDirectory 'capture.json'
        Write-Json $CandidatePath $capture
        $candidate = Read-Capture $CandidatePath
        Write-Host "Capture: $CandidatePath"
    }
    if ($null -eq $baseline) {
        Write-Host 'Capture complete. Review it and select its capture.json explicitly as a future baseline.'
        return
    }
    if (($baseline.context | ConvertTo-Json -Depth 8 -Compress) -cne ($candidate.context | ConvertTo-Json -Depth 8 -Compress)) {
        throw 'Incompatible baseline and candidate conditions; no performance verdict is valid.'
    }
    # Each threshold must exceed both its relative allowance and absolute floor.
    # Three median absolute deviations accommodate ordinary run-to-run variation.
    $metrics = @(
        @{ name = 'cpuPercentNormalized'; relative = 0.20; floor = 0.10; unit = 'percentage points' },
        @{ name = 'sampledMaxWorkingSetBytes'; relative = 0.10; floor = 2MB; unit = 'bytes' },
        @{ name = 'sampledMaxPrivateBytes'; relative = 0.10; floor = 2MB; unit = 'bytes' },
        @{ name = 'sampledMaxHandleCount'; relative = 0.10; floor = 16; unit = 'handles' },
        @{ name = 'startupTimeToInputIdleMs'; relative = 0.20; floor = 30; unit = 'milliseconds' }
    )
    $comparisons = @()
    foreach ($page in $baseline.context.pages) {
        $beforeRuns = @($baseline.measurements | Where-Object { $_.initialPage -eq $page })[0].runs
        $afterRuns = @($candidate.measurements | Where-Object { $_.initialPage -eq $page })[0].runs
        foreach ($metric in $metrics) {
            $before = Get-Statistics $beforeRuns $metric.name
            $after = Get-Statistics $afterRuns $metric.name
            $delta = $null
            $threshold = $null
            $noise = $null
            $verdict = 'unavailable'
            if ($null -ne $before.median -and $null -ne $after.median) {
                $delta = $after.median - $before.median
                $budget = [Math]::Max([double]$metric.floor, [Math]::Abs($before.median) * $metric.relative)
                $noise = 3 * [Math]::Max($before.mad, $after.mad)
                $threshold = [Math]::Max($budget, $noise)
                $verdict = if ($delta -gt $threshold) { 'regression' }
                    elseif ($noise -gt $budget) { 'noisy' }
                    elseif ($delta -lt -$threshold) { 'improved' }
                    else { 'within-budget' }
            }
            $comparisons += [pscustomobject][ordered]@{
                page = $page; metric = $metric.name; unit = $metric.unit
                baseline = $before; candidate = $after; delta = $delta
                threshold = $threshold; noiseAllowance = $noise; verdict = $verdict
            }
        }
    }
    $failed = @($comparisons | Where-Object { $_.verdict -in @('regression', 'noisy', 'unavailable') })
    $comparisonPath = Join-Path $runDirectory 'comparison.json'
    Write-Json $comparisonPath ([pscustomobject][ordered]@{
        schemaVersion = 1
        kind = 'FeatherTaskPerformanceComparison'
        baselinePath = (Resolve-Path -LiteralPath $BaselinePath).ProviderPath
        candidatePath = (Resolve-Path -LiteralPath $CandidatePath).ProviderPath
        baselineSha256 = $baseline.build.executableSha256
        candidateSha256 = $candidate.build.executableSha256
        status = if ($failed.Count) { 'review-required' } else { 'within-budget' }
        comparisons = $comparisons
    })
    $comparisons | Select-Object page, metric, delta, threshold, verdict | Format-Table -AutoSize | Out-Host
    Write-Host "Comparison: $comparisonPath"
    if ($failed.Count) { throw 'Performance review required: regression, excessive noise, or unavailable data. Inspect comparison.json; baseline was not changed.' }
} catch {
    Write-Json (Join-Path $runDirectory 'error.json') ([pscustomobject]@{ status = 'error'; message = $_.Exception.Message })
    throw
} finally {
    if ($null -ne $lock) { $lock.Dispose() }
}
