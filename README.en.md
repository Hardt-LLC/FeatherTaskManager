<p align="center"><img src="assets/app.png" width="112" height="112" alt="Feather Task Manager feather icon"></p>

# Feather Task Manager

[한국어](README.md) · [Download](https://github.com/Hardt-LLC/FeatherTaskManager/releases/latest)

A lightweight Windows task manager built with Rust and Win32. Supports Windows 10 version 1607 or later and Windows 11, x64.

## Benefits

- **Native and lightweight** — no browser engine or separate runtime to install. Collection pauses when minimized.
- **Flexible process views** — switch between a list and a process tree, search processes, and end a task or an entire tree.
- **System overview** — monitor CPU, memory, disk and network, manage startup apps, and start or stop services.
- **English and Korean** — switch languages immediately from Settings.
- **Optional Task Manager replacement** — launch Feather through the familiar Windows shortcuts.

## Install and use

Download the latest version from [Releases](https://github.com/Hardt-LLC/FeatherTaskManager/releases/latest):

| Download | Use |
| --- | --- |
| **Setup-x64.exe** | Install with a Start menu shortcut and an uninstaller. |
| **Portable-x64.exe** | Run directly without installation. |
| **Portable-x64.zip** | Portable app, documentation and recovery script. |

Release executables and installers are signed by **HARDT** through Azure Artifact Signing.

- Open **Processes** to choose **List** or **Process tree**. Select a process to end it or its tree after confirmation.
- Open **Performance**, **Startup apps** or **Services** for monitoring and management.
- Select **Settings → English / 한국어** to save your preferred language.
- To replace Windows Task Manager, install with Setup first, then select **Settings → Use Feather as Task Manager…** and approve the administrator prompt. This enables `Ctrl+Shift+Esc` and `Ctrl+Alt+Delete → Task Manager`. Select **Restore Windows Task Manager…** before manually deleting the app.

| Shortcut | Action |
| --- | --- |
| `Ctrl+1…4` | Switch pages |
| `Ctrl+F` | Search |
| `F5` | Refresh |
| `Shift+Delete` | Confirm ending the selected process tree |

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

[MIT](LICENSE). Third-party licenses are listed in [THIRD_PARTY_NOTICES.txt](THIRD_PARTY_NOTICES.txt) and [RUST_LIBRARY_NOTICES.html](RUST_LIBRARY_NOTICES.html).
