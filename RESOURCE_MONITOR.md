# Built-in Resource Monitor

Open **Performance → Resource Monitor** or launch Feather with
`--resource-monitor`. The window runs inside Feather and follows the supplied
`resource-monitor.html` design: Overview, CPU, Memory, Disk and Network, with
collapsible tables and recent-history charts. No browser runtime is embedded.

Process checkboxes filter related rows across tabs. Search, sortable columns,
independent pause/refresh rate, selected-process modules, TCP connections and
listening ports work with live local data. Ending checked processes uses the
same identity and critical-process protections as Task Manager, after confirmation.

## Collection and cost

- Task Manager and Resource Monitor share immutable process/performance samples.
  Paused windows retain their own frame. The monitor uses the fastest active
  window's interval; there is no second process sampler or UI polling timer.
- Endpoint tables, physical-memory lists and fixed-volume capacity are fetched
  only for expanded panels that need them, at most every two seconds. A single
  bounded background job keeps device queries off both the UI and main sampler.
  A selected process's modules refresh at most every five seconds.
- Closing, minimizing or pausing Resource Monitor stops optional collection.
  Default operation creates no file tracing session. Both windows being inactive
  pauses shared sampling. Drawing uses native virtual tables and bounded traces.
- **Tracing on** optionally enables file I/O requests and connection traffic for
  their visible expanded panels. These use bounded, in-memory Windows ETW data.
  Connection attribution reuses Feather's existing network session. Administrator
  rights may be required; turning tracing on never changes privileges or installs
  a driver. Turning it off stops its additional work.
- No DNS lookups, traffic probes, packet payload capture, file reads, trace files,
  external telemetry, or new runtime dependency are introduced.

## Reading the numbers

| Metric | Meaning / limitation |
| --- | --- |
| Process read/write | All process I/O, including files, network and devices; not physical disk traffic. |
| File I/O | Requested bytes, including cached I/O. Only requests with verified process identity and a name observed in the trace are attributed. Existing open files can be unavailable. |
| Disk graph | Actual aggregate Windows disk counters, shared with Performance. |
| Network activity | Observed TCP/UDP endpoint transfers; this is separate from the passive socket inventory. |
| Private working set | Resident private pages from the bulk process snapshot, distinct from private committed bytes. |
| Hard faults/sec | Delta of the bulk process hard-fault counter across a valid interval. A new/reused PID is unmeasured first. |
| Associated modules | One checked process, guarded by PID and creation time; protected processes can deny access. |
| Handle names | Unavailable. Arbitrary handle-name queries can block, so lightweight monitoring shows the measured handle count instead. |
| File response time, TCP latency/loss, firewall policy, per-volume queue | Unavailable where the chosen passive providers do not supply a trustworthy value. |

`—` means unmeasured, not zero. First intervals prime rate counters. Event loss,
tracking limits and access failures are surfaced; incomplete attribution is never
presented as complete physical disk or network coverage. No demo values from the
HTML are used.

## Verification

Use the existing focused tests, `cargo clippy --all-targets -- -D warnings`, and
`cargo build --release --locked`. `--render-previews <directory>` also renders the
resource pages using Feather's own drawing code. It does not capture other apps.

For the ordinary path, compare the same pages through `scripts/performance-loop.ps1`.
For additional cost with the monitor open:

```powershell
.\scripts\measure.ps1 -ExePath target\resource-build\release\FeatherTaskManager.exe `
  -ResourceMonitor -DurationSeconds 5 -Iterations 3 -OutputPath target\resource-open.json
```

Do not run builds or other benchmarks during measurements. The script closes only
the exact process it launched. Local results and limitations belong in VALIDATION.md;
do not claim performance improvement from an unmeasured or noisy result.
Disconnect UI inspection sessions before measuring ordinary idle cost: a connected
accessibility inspector can continuously traverse every process row. Measure that
workload separately if screen-reader/inspection performance is the subject.

Provider references: [IP Helper TCP tables](https://learn.microsoft.com/windows/win32/api/iphlpapi/nf-iphlpapi-getextendedtcptable),
[UDP tables](https://learn.microsoft.com/windows/win32/api/iphlpapi/nf-iphlpapi-getextendedudptable),
[Toolhelp modules](https://learn.microsoft.com/windows/win32/api/tlhelp32/nf-tlhelp32-createtoolhelp32snapshot),
[Microsoft Kernel-File parser](https://github.com/microsoft/perfview/blob/main/src/TraceEvent/Parsers/Microsoft-Windows-Kernel-File.cs).
