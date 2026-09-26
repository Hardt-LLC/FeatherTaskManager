# Feather Task Manager design

The canonical reference is `design/reference/feather-task-manager.html`, supplied by the user on 2026-09-26. It supersedes the earlier Stitch design. Its JavaScript uses simulated data and is reference material only; the application remains native Rust/Win32 with real Windows measurements.

## Layout and appearance

- Five views: Processes, Performance, Startup apps, Services, Settings.
- 220 dp navigation rail, 44 dp global search strip, 60 dp page heading, 30 dp status bar.
- Dense virtual native tables with a 48 dp resource header, 32 dp rows and aligned numeric text.
- Light, dark and Windows-following themes. Segoe UI/Malgun Gothic for interface text; Consolas for numeric data.
- Source OKLCH colors converted to sRGB: light canvas #F6F9FC, surface #FFFFFF, foreground #121C23, accent #299236; dark canvas #0F1519, surface #171E23, foreground #E7ECF0, accent #5BBE62.
- Green measured history charts, subtle resource heatmaps, monochrome navigation and existing feather branding.
- Native Windows title bar and window controls remain available. The browser reference's desktop shadow is not part of the app client area.

## Connected behavior

- Processes: app-window/background/system grouping, flat list or generation-safe process tree; search/sort, confirmed termination/tree termination, priority/EcoQoS, file location, name/PID copy, optional selected-process CPU/memory/all-I/O history.
- Performance: logical CPU charts, CPU topology/reported clock, memory commit/cache/pools, individual physical disks, physical network interfaces, GPU adapter/engine usage and GPU memory. Selected device survives counter warm-up. Histories cover up to 60 seconds at 250 ms–5 s rates and mark collection gaps.
- Startup: inline native toggle, pending/access-denied state, stale-source checks, optional CompanyName metadata, explicit unmeasured startup impact.
- Services: state/start-type filters, start/stop/restart, selected configuration/dependencies, Services console.
- Settings: system/light/dark, Korean/English, refresh, initial page, always on top, optional tray minimize, installed-app Task Manager replacement. Registry preferences use the shared no-reparse opener.
- Nuclear Zombie (Processes head button, replacing Efficiency mode, which stays in the ⋯ and context menus as "Toggle efficiency mode"; palette command "Nuclear Zombie", found by "memory" / "zombie"): one modal panel (scrim, 560 dp surface, radius 8, header / scrolling body / footer like the confirm dialog). It shows the measured memory strip (Available, In use, Standby, Modified, Free), options with switches (trim working sets; clear standby cache and flush modified pages, which need administrator/UAC; find zombie processes) and runs on the job worker with progress on the Run button. The results appear in the same panel: Available/Free before → after with the change, each step's outcome (done, needs administrator, declined, error text), trimmed/skipped counts, and zombie holders (busiest first, PID, count, most common exited names) with a partial-scan line when processes could not be inspected. A holder's End task closes the panel and uses the normal End task confirmation. Copy details copies the report. Esc closes, except while the UAC prompt is pending. If the panel is closed during a run, a toast reports the result.
- Ctrl+K command palette; Ctrl+F search; Ctrl+1…4 views; F5 refresh; Delete/Shift+Delete termination confirmation; Space pauses, or toggles a selected startup entry.
- Minimized and modal monitoring pauses; hidden tray state has a tested restoration path. No WebView, WMI, external telemetry or polling subprocess.

## Data boundaries

- Apps means processes with visible unowned top-level windows; an app row sums its windowless descendants (not the shell's sign-in children). Windows processes are PID 0/4, the minimal processes System starts, and processes whose executable is inside the Windows directory (the end-task warning's test, on the image path the kernel reports for every PID without elevation). Background is the rest. This does not guess Windows process ownership from executable names.
- Process I/O includes all transfer types and is never labeled disk-only. System/device counters are measured independently of process counters.
- Process GPU (data layer, `performance::ProcessGpuTracker`): utilization is the busiest engine after summing that engine's instances, the maximum over all engines and adapters (Task Manager's rule); memory is PDH "GPU Process Memory" Local Usage (dedicated) and Shared Usage. The unreliable "Dedicated Usage" counter is not used. A new or reused (PID, creation time) shows "-" until its first full interval.
- Process network (data layer, `netetw::NetworkMonitor`): send/receive bytes per second from a private Microsoft-Windows-Kernel-Network ETW session, which Windows allows only when Feather runs elevated. Otherwise, and for any interval where the trace stopped or ETW reported lost events, the value is "-" with the reason ("Requires administrator", "Network trace stopped", "Network events dropped"). Bytes from events that predate a process's creation time are not attributed to it. Addresses and ports are never read.
- The Processes table shows both per process: GPU as "12.3%" (heat at 12 %), Network as "6.8 Mbps" (bytes × 8 / 10⁶, heat at 8 Mbps), "—" for an unmeasured value. When network is unmeasured the page head names the reason (e.g. "Network per process requires administrator"); there is no note when it is measured. The column totals are system-wide: physical adapters' throughput and the busiest listed GPU.
- The Performance page lists no software (Basic Render Driver) or indirect-display GPU and no network adapter that is not present or not connected.
- Device identity is collected once and again on device change, never guessed from names:
  - GPU (D3DKMT): name, dedicated/shared memory, driver version/date, WDDM version, PCI IDs. Software (Basic Render) and indirect-display adapters are flagged, not listed as GPUs.
  - Disks (storage IOCTLs): model, bus, SSD/HDD from the seek-penalty property, TRIM, capacity, system and page-file disk. Serial numbers are never read. A disk that does not report seek penalty (e.g. USB flash) is neither SSD nor HDD.
  - Memory (SMBIOS types 16/17): slots used/total, type, form factor, module size, manufacturer and part number. Speeds are labeled MT/s only for SMBIOS 3.2+ tables. Older tables expose only the raw reported value, because their specification says MHz and firmware reported either the clock or the transfer rate.
  - Network adapters: adapter model and Wi-Fi/Ethernet kind. MAC and IP addresses are never read.
  - CPU: base speed (`MaxMhz`), L1/L2/L3 totals, firmware virtualization.
- Temperatures: GPU temperature, fan RPM, power as % of the power limit (not watts) and memory clock come from D3DKMT adapter performance data, the source Windows Task Manager uses. CPU package temperature has no supported user-mode source without a kernel driver or WMI, so it is not collected or shown. ACPI thermal zones are labeled only by zone name, never as a CPU temperature.
- Still not measured: startup impact, SSD health/wear, CPU temperature, GPU power in watts. They are not inferred from mock values, and the UI omits them or marks them unavailable.
- Memory cleanup (`memclean`): readings are `GlobalMemoryStatusEx` (Available, In use = total − available) and `SystemMemoryListInformation` (Standby over all priorities, Modified, Free + zeroed); "—" when Windows does not report the lists. Trimming calls `EmptyWorkingSet` only on processes Feather may open with quota rights. Pages move to the standby/modified lists and fault back in, so nothing stops running. Standby purge and modified flush use `NtSetSystemInformation` with `SeProfileSingleProcessPrivilege`. Feather does this itself when elevated. Otherwise it starts the installed, byte-identical Program Files image with `--purge-memory-lists` through UAC, and reports a declined prompt or a missing or different installed build as such. The zombie scan is read-only: it duplicates process handles into Feather with limited access, never with `DUPLICATE_CLOSE_SOURCE`, and closes only its own copies. It counts distinct exited processes (PID + creation time) per holder, and Windows gives no supported per-zombie memory figure, so no memory size is claimed.
- Publisher is unverified executable version metadata, not an Authenticode signer.
- Do not replace missing counters with zero or use random/sample values in the application.

## Delivery

Released in 2026.9.3. Development checks never publish, sign, upload a release or change the Task Manager association; releases follow [SIGNING.md](../../SIGNING.md). README previews are rendered from the design prototype's simulated data, never from captures of a real PC.
