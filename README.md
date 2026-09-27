<p align="center"><img src="assets/app.png" width="112" height="112" alt="Feather Task Manager feather icon"></p>

# Feather Task Manager

**English** · [한국어](README.ko.md) · [Download](https://github.com/Hardt-LLC/FeatherTaskManager/releases/latest)

A lightweight Windows task manager built with Rust and Win32. Supports Windows 10 version 1607 or later and Windows 11, x64.

<p align="center"><img src="assets/preview/processes-light.png" width="900" alt="Feather Task Manager processes page, light theme"></p>

## Preview

| Dark theme | Performance |
| --- | --- |
| <img src="assets/preview/processes-dark.png" width="440" alt="Processes page, dark theme"> | <img src="assets/preview/performance-dark.png" width="440" alt="Performance page with GPU details, dark theme"> |
| **Nuclear Zombie** | **Settings** |
| <img src="assets/preview/nuclear-zombie-light.png" width="440" alt="Nuclear Zombie memory cleanup panel with results"> | <img src="assets/preview/settings-light.png" width="440" alt="Settings page, light theme"> |

These images come from the [interactive design prototype](design/reference/feather-task-manager.html), a single HTML file with simulated data that mirrors the app. Download it and open it in a browser to try every page, light and dark themes, sorting, the context menu, the command palette (`Ctrl+K`) and a Nuclear Zombie run.

## Benefits

- **Native and lightweight** — no browser engine or separate runtime to install. Collection pauses when minimized, and animations run only while something moves.
- **Designed, not default** — one title bar with the logo, search and window buttons (Snap Layouts supported), custom-drawn tables, menus and dialogs, pixel-smooth scrolling and matching light/dark themes.
- **Flexible process views** — switch between app groups, a list and a process tree; inspect live history, control efficiency mode, and end a task or an entire tree.
- **System overview** — CPU cores, memory, disks, network adapters and GPUs with their model names, GPU temperature, memory speed and slots; toggle startup apps; start, stop or restart services.
- **Nuclear Zombie** — tidy memory on a long-running PC without closing apps (trim working sets, clear the standby cache with administrator approval) and find "zombie" processes that another program still holds open.
- **English and Korean** — switch languages and light/dark themes from Settings; use command search and optional tray minimize.
- **Optional Task Manager replacement** — launch Feather through the familiar Windows shortcuts.

## Install and use

Download the latest version from [Releases](https://github.com/Hardt-LLC/FeatherTaskManager/releases/latest):

| Download | Use |
| --- | --- |
| **Setup-x64.exe** | Install with a Start menu shortcut and an uninstaller. |
| **Portable-x64.exe** | Run directly without installation. |
| **Portable-x64.zip** | Portable app, documentation and recovery script. |

Release executables and installers are signed by **HARDT** through Azure Artifact Signing.

- Open **Processes** to choose **App groups**, **List view** or **Tree view**. Select a process to end it or its tree after confirmation.
- Open **Performance**, **Startup apps** or **Services** for monitoring and management.
- Open **Settings** to change language, theme, refresh rate, start page and window behavior.
- Use **⋯ → Show / hide live telemetry** for selected-process CPU, memory and all-I/O history.
- Select **Processes → Nuclear Zombie** (or `Ctrl+K` → "memory") to clean up memory and list zombie processes. Efficiency mode is in the **⋯** and right-click menus.
- `—` means unmeasured. Per-process GPU uses Windows Task Manager's rule (the busiest engine); per-process network is measured only when Feather runs as administrator. Startup impact and CPU temperature are not collected.
- Turn on **Settings → Always run as administrator** to start elevated every time (per-process network, full zombie scans). It applies to the installed Feather only: every start, including each `Ctrl+Shift+Esc` even while an elevated window is open, shows the Windows UAC prompt, and Feather keeps running with standard rights if you decline. A portable copy is never elevated automatically; **Processes → ⋯ → Run as administrator** elevates the running copy once.
- To replace Windows Task Manager, install with Setup first, then select **Settings → Replace Windows Task Manager** and approve the administrator prompt. This enables `Ctrl+Shift+Esc` and `Ctrl+Alt+Delete → Task Manager`. Turn the same setting off to restore Windows Task Manager before manually deleting the app.

| Shortcut | Action |
| --- | --- |
| `Ctrl+1…4` | Switch pages |
| `Ctrl+F` | Search |
| `Ctrl+K` | Find a command or process |
| `F5` | Refresh |
| `Space` | Pause or resume updates |
| `Delete` / `Shift+Delete` | Confirm ending the selected process / process tree |

Close all Feather windows before updating or uninstalling. See the [installation guide](INSTALLER.md) and [feature guide](FEATURES-v2.md) for details.

## Build from source

Requires Windows, Rust with the `x86_64-pc-windows-msvc` toolchain, Visual Studio **C++ Build Tools**, and the **Windows SDK**.

```powershell
git clone https://github.com/Hardt-LLC/FeatherTaskManager.git
cd FeatherTaskManager
cargo build --release --locked
```

Run `target\release\FeatherTaskManager.exe`. For signed installers and release packaging, see [SIGNING.md](SIGNING.md).

## License

[MIT](LICENSE). Third-party licenses are listed in [THIRD_PARTY_NOTICES.txt](THIRD_PARTY_NOTICES.txt) and [RUST_LIBRARY_NOTICES.html](RUST_LIBRARY_NOTICES.html). Feather collects no personal data; see the [privacy policy](PRIVACY.md). Report security issues privately as described in [SECURITY.md](SECURITY.md).
