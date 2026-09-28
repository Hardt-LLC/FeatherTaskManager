#requires -Version 5.1
<#
.SYNOPSIS
Measures a newly launched FeatherTask process and writes reproducible JSON.
.DESCRIPTION
Only the process launched by this script is closed. Main-window detection is
not a measurement of first paint or full UI readiness. Hidden launches can
legitimately have no discoverable MainWindowHandle; that metric is then null.
CPU percent is normalized across the logical processor count (0-100%).
.EXAMPLE
powershell -NoProfile -ExecutionPolicy Bypass -File .\scripts\measure.ps1 -DurationSeconds 10 -Iterations 3
#>
[CmdletBinding()]
param(
    [string]$ExePath,
    [ValidateRange(1, 3600)]
    [int]$DurationSeconds = 10,
    [ValidateRange(1, 100)]
    [int]$Iterations = 1,
    [string]$OutputPath,
    [ValidateSet('processes', 'performance', 'startup', 'services')]
    [string]$Page = 'processes',
    [switch]$ResourceMonitor
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

# Resolve defaults after parameter binding; some Windows PowerShell hosts do not
# initialize PSScriptRoot while evaluating parameter default expressions.
if ([string]::IsNullOrWhiteSpace($ExePath)) {
    $ExePath = Join-Path $PSScriptRoot '..\dist\FeatherTaskManager.exe'
}
if ([string]::IsNullOrWhiteSpace($OutputPath)) {
    $OutputPath = Join-Path $PSScriptRoot '..\dist\benchmark.json'
}

if ($env:OS -ne 'Windows_NT') {
    throw 'This benchmark requires Windows.'
}
if (-not (Test-Path -LiteralPath $ExePath -PathType Leaf)) {
    throw "Executable not found: $ExePath. Build the release binary first."
}

$resolvedExe = (Resolve-Path -LiteralPath $ExePath).ProviderPath
$resolvedOutput = $ExecutionContext.SessionState.Path.GetUnresolvedProviderPathFromPSPath($OutputPath)
$logicalProcessors = [Environment]::ProcessorCount
$results = New-Object 'System.Collections.Generic.List[object]'
$anyFailure = $false

for ($iteration = 1; $iteration -le $Iterations; $iteration++) {
    $launched = $null
    $startupClock = [Diagnostics.Stopwatch]::StartNew()
    $windowObservedMs = $null
    $inputIdleObservedMs = $null
    $canCheckInputIdle = $true
    $processIdForReport = $null

    try {
        $launchArguments = @('--page', $Page)
        if ($ResourceMonitor) { $launchArguments += '--resource-monitor' }
        $launched = Start-Process -FilePath $resolvedExe -ArgumentList $launchArguments -WorkingDirectory (Split-Path -Parent $resolvedExe) -WindowStyle Hidden -PassThru
        # Retain the process object/handle; never locate a cleanup target by name.
        $retainedProcessHandle = $launched.Handle
        $processIdForReport = $launched.Id

        while ($startupClock.Elapsed.TotalSeconds -lt 10) {
            $launched.Refresh()
            if ($launched.HasExited) {
                throw "Launched process exited during startup (exit code $($launched.ExitCode))."
            }
            if ($null -eq $inputIdleObservedMs -and $canCheckInputIdle) {
                try {
                    if ($launched.WaitForInputIdle(0)) {
                        $inputIdleObservedMs = [Math]::Round($startupClock.Elapsed.TotalMilliseconds, 2)
                    }
                }
                catch {
                    # A process without a GUI message loop cannot expose this signal.
                    $canCheckInputIdle = $false
                }
            }
            if ($launched.MainWindowHandle -ne [IntPtr]::Zero) {
                $windowObservedMs = [Math]::Round($startupClock.Elapsed.TotalMilliseconds, 2)
                break
            }
            Start-Sleep -Milliseconds 10
        }
        $startupClock.Stop()
        # A visible secondary window can be discovered sooner than the hidden
        # main window. Give optional providers the same 10-second settling
        # opportunity before recording Resource Monitor's steady-state cost.
        if ($ResourceMonitor -and $startupClock.Elapsed.TotalSeconds -lt 10) {
            Start-Sleep -Milliseconds ([int](10000 - $startupClock.Elapsed.TotalMilliseconds))
        }

        $launched.Refresh()
        if ($launched.HasExited) {
            throw 'Launched process exited before the sampling interval.'
        }
        $cpuStartMilliseconds = $launched.TotalProcessorTime.TotalMilliseconds
        $workingSetMaxBytes = $launched.WorkingSet64
        $privateMaxBytes = $launched.PrivateMemorySize64
        $handleCountAtStart = $launched.HandleCount
        $sampledMaxHandleCount = $handleCountAtStart
        $privateBytesAtStart = $launched.PrivateMemorySize64
        $sampleCount = 0
        $sampleClock = [Diagnostics.Stopwatch]::StartNew()

        do {
            Start-Sleep -Milliseconds 250
            $launched.Refresh()
            if ($launched.HasExited) {
                throw "Launched process exited during sampling (exit code $($launched.ExitCode))."
            }
            $sampleCount++
            $workingSetMaxBytes = [Math]::Max($workingSetMaxBytes, $launched.WorkingSet64)
            $privateMaxBytes = [Math]::Max($privateMaxBytes, $launched.PrivateMemorySize64)
            $sampledMaxHandleCount = [Math]::Max($sampledMaxHandleCount, $launched.HandleCount)
        } while ($sampleClock.Elapsed.TotalSeconds -lt $DurationSeconds)

        $cpuEndMilliseconds = $launched.TotalProcessorTime.TotalMilliseconds
        $sampleClock.Stop()
        $cpuMilliseconds = [Math]::Max(0, $cpuEndMilliseconds - $cpuStartMilliseconds)

        $results.Add([pscustomobject][ordered]@{
            iteration = $iteration
            status = 'ok'
            processId = $processIdForReport
            startupTimeToMainWindowMs = $windowObservedMs
            startupTimeToInputIdleMs = $inputIdleObservedMs
            startupObservationDurationMs = [Math]::Round($startupClock.Elapsed.TotalMilliseconds, 2)
            mainWindowDetected = ($null -ne $windowObservedMs)
            sampleDurationSeconds = [Math]::Round($sampleClock.Elapsed.TotalSeconds, 4)
            sampleCount = $sampleCount
            cpuTimeMilliseconds = [Math]::Round($cpuMilliseconds, 3)
            cpuPercentNormalized = [Math]::Round(100 * $cpuMilliseconds / $sampleClock.Elapsed.TotalMilliseconds / $logicalProcessors, 4)
            workingSetBytes = $launched.WorkingSet64
            workingSetMiB = [Math]::Round($launched.WorkingSet64 / 1MB, 3)
            privateBytes = $launched.PrivateMemorySize64
            privateMiB = [Math]::Round($launched.PrivateMemorySize64 / 1MB, 3)
            sampledMaxWorkingSetBytes = $workingSetMaxBytes
            sampledMaxPrivateBytes = $privateMaxBytes
            privateBytesGrowth = $launched.PrivateMemorySize64 - $privateBytesAtStart
            handleCountAtStart = $handleCountAtStart
            handleCount = $launched.HandleCount
            handleCountGrowth = $launched.HandleCount - $handleCountAtStart
            sampledMaxHandleCount = $sampledMaxHandleCount
            threadCount = $launched.Threads.Count
        })
    }
    catch {
        $anyFailure = $true
        $results.Add([pscustomobject][ordered]@{
            iteration = $iteration
            status = 'error'
            processId = $processIdForReport
            error = $_.Exception.Message
        })
        Write-Warning "Iteration ${iteration}: $($_.Exception.Message)"
    }
    finally {
        if ($null -ne $launched) {
            try {
                if (-not $launched.HasExited) {
                    $null = $launched.CloseMainWindow()
                    if (-not $launched.WaitForExit(1000)) {
                        $launched.Kill()
                        if (-not $launched.WaitForExit(3000)) {
                            throw 'The launched process did not exit within the cleanup deadline.'
                        }
                    }
                }
            }
            catch {
                $anyFailure = $true
                $results[$results.Count - 1].status = 'error'
                $results[$results.Count - 1] | Add-Member -NotePropertyName cleanupError -NotePropertyValue $_.Exception.Message
                Write-Warning "Could not finish cleanup of our launched process ${processIdForReport}: $($_.Exception.Message)"
            }
            finally {
                $launched.Dispose()
            }
        }
    }
}

$report = [pscustomobject][ordered]@{
    schemaVersion = 1
    recordedAtUtc = [DateTime]::UtcNow.ToString('o')
    executable = $resolvedExe
    executableSizeBytes = (Get-Item -LiteralPath $resolvedExe).Length
    executableSha256 = (Get-FileHash -LiteralPath $resolvedExe -Algorithm SHA256).Hash.ToLowerInvariant()
    operatingSystem = [Environment]::OSVersion.VersionString
    logicalProcessors = $logicalProcessors
    requestedDurationSeconds = $DurationSeconds
    launchWindowStyle = 'Hidden'
    initialPage = $Page
    resourceMonitor = [bool]$ResourceMonitor
    notes = @(
        'Measurements apply only to the process launched by this script; helper/child processes are excluded.',
        'Startup includes PowerShell Start-Process overhead. Main-window detection is not first paint or full UI readiness.',
        'Hidden launches may have no MainWindowHandle; the metric is null after a bounded 10-second wait.',
        'ResourceMonitor opens a visible secondary window and samples only after at least 10 seconds from launch; tracing stays off.',
        'Input-idle detection means a GUI message loop became idle; it does not verify that initial data is loaded.',
        'CPU percent = process CPU time / measured wall time / logical processors * 100.',
        'Working set includes shared resident pages. Private bytes are committed private memory, not necessarily resident.',
        'Sampled maxima use 250 ms sampling and may miss shorter peaks. Repeated runs are not cold-start measurements.'
    )
    runs = @($results.ToArray())
}

$outputDirectory = Split-Path -Parent $resolvedOutput
if (-not (Test-Path -LiteralPath $outputDirectory -PathType Container)) {
    $null = New-Item -ItemType Directory -Path $outputDirectory -Force
}
$json = $report | ConvertTo-Json -Depth 6
[IO.File]::WriteAllText($resolvedOutput, $json + [Environment]::NewLine, (New-Object Text.UTF8Encoding($false)))
Write-Output "Benchmark saved to $resolvedOutput"
Write-Output $json

if ($anyFailure) {
    throw 'One or more benchmark iterations failed; inspect the JSON report.'
}
