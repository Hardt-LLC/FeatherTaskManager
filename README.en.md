<p align="center"><img src="assets/app.png" width="112" height="112" alt="Feather Task Manager feather icon"></p>

# Feather Task Manager 2026.9.1

[한국어](README.md) · [Download](https://github.com/Hardt-LLC/FeatherTaskManager/releases/latest)

A small native Windows task manager built with Rust and Win32. No browser engine, WebView or WMI. Supports Windows 10/11 x64 without a separate Rust or Visual C++ runtime installation.

## Install or run portable

Choose **Setup-x64.exe** to install into Program Files with a Start menu shortcut and an uninstaller, or **Portable-x64.exe** to run directly. The optional portable ZIP includes recovery instructions and scripts. Release executables use Azure Artifact Signing; compare downloaded checksums with `SHA256SUMS.txt`.

Select **Settings → English / 한국어** to switch languages immediately. Your choice is saved per user; the initial default follows the Windows display language. `--language en` and `--language ko` override it for one launch without changing the saved preference.

## Processes and performance

- Switch the Processes view between **List** and **Process tree**. Expand or collapse branches with their arrows or the left/right arrow keys. Search keeps the matching process and its ancestor path visible; sorting applies within each sibling group.
- **End task** ends one process after confirmation. **End tree** (`Shift+Delete`) targets the selected process and descendants captured before confirmation, children first. PID and creation time are verified and handles retained. System, critical and Feather's own processes are protected. Preflight failures abort before any termination; later failures are reported with counts.
- New descendants created after confirmation are excluded. Restarting services or applications may launch replacement processes. Termination can discard unsaved work.
- Performance shows CPU, memory, disk and network history with GPU summaries where Windows provides counters. Startup apps and Services offer explicit supported management actions.

`Ctrl+1…4` switches pages, `Ctrl+F` focuses search, `F5` refreshes, `Space` pauses/resumes while the list is focused, and `Ctrl+L` reveals a process executable. Collection pauses when minimized. The default refresh interval is one second.

## Replace Windows Task Manager

Choose **Settings → Use Feather as Task Manager…** and approve UAC. This copies the executable into the fixed Program Files directory and registers it for all users. Subsequent `Ctrl+Alt+Delete → Task Manager`, `Ctrl+Shift+Esc` and `taskmgr.exe` launches use Feather. Installation alone does not enable this setting, and other applications' associations are preserved.

Restore with **Settings → Restore Windows Task Manager…** before manually deleting installed files. The uninstaller restores an exact Feather association before deleting the app and stops if recovery cannot be completed safely. Close all Feather windows before updating or uninstalling.

If the executable has already been deleted, run the included `Restore-WindowsTaskManager.ps1` from an administrator 64-bit PowerShell window. The script removes only Feather's exact registration and preserves unrelated settings.

## Build, validation and limitations

Use Rust `x86_64-pc-windows-msvc`, Visual Studio C++ Build Tools and Windows SDK: `cargo build --release --locked`. `scripts/build.ps1` packages a local build. Signed installer/release instructions are in [SIGNING.md](SIGNING.md).

Versions use `yyyy.m.x`; `x` increments for each release in the same month. See [VALIDATION.md](VALIDATION.md) and [BENCHMARK.md](BENCHMARK.md) for checks and measurement limits. A short hidden-window benchmark does not measure first paint, worst-case load or a direct comparison with Windows Task Manager.

This app does not implement every Windows Task Manager feature: packaged StartupTask entries, scheduled tasks, startup impact, service configuration and detailed per-GPU graphs are outside the current scope. NT process structures and StartupApproved formats may change with Windows releases. Unknown states remain read-only.

MIT license. See [LICENSE](LICENSE) and [THIRD_PARTY_NOTICES.txt](THIRD_PARTY_NOTICES.txt).
