# Windows installer

`FeatherTaskManager-yyyy.m.x-Setup-x64.exe` is the administrator installer for Windows 10 version 1607 or later and Windows 11, x64. It supports English and Korean, installs to the fixed `Program Files\Feather Task Manager` directory, creates a Start menu shortcut, offers an optional desktop shortcut, and registers an uninstaller in Windows Settings. The portable EXE remains available separately.

Installation does not enable the Windows Task Manager replacement. Enable it explicitly from Feather's Settings after installation. The installer uses a fixed location so its payload and the Task Manager replacement always refer to the same executable. The folder and executable must be owned by Administrators or SYSTEM and must not allow ordinary users to modify them.

Since 2026.9.2, Task Manager registration requires this protected installation. The app never copies a portable executable into Program Files with elevated privileges. Both enabling and restoring through the menu use the verified installed helper; if it is missing, repair the installation or use the recovery script below.

An upgrade uses the same installation identity and location. Close all installed Feather windows first. The installer tests whether Windows permits opening the installed image for writing without changing its bytes; an image mapped by a running process is refused. It does not rely on public named mutexes or terminate processes for the user. Portable instances do not block installation.

Before removal deletes the app, it reads the 64-bit Task Manager `Debugger` value. Only a byte-for-byte match to Feather's canonical REG_SZ command is eligible for restoration. It invokes Feather's restore helper, checks the exit code, then reads the registry again. Failed restoration or an unreadable setting aborts removal and preserves the executable. A manually edited command that still references Feather also blocks removal; genuinely unrelated programs' settings are preserved. If the executable was manually deleted first, use `Restore-WindowsTaskManager.ps1` as administrator before retrying removal.

Normal uninstall removes only installer-owned files and shortcuts. It does not recursively delete the application directory or arbitrary user files. Language preferences under the user's profile are retained.

Build a signed installer through `scripts/build-release.ps1` (see [SIGNING.md](SIGNING.md)). For compiler/layout testing only, `scripts/build-installer.ps1 -UnsignedDevelopment` writes an explicitly named `-UNSIGNED-DEVELOPMENT.exe`; the release publishing script never selects it.

Compiler checks validate the installer script and its payload metadata. Real installation, update, Task Manager replacement, and uninstall should also be exercised in a disposable Windows VM before broad distribution. Compiling an installer does not exercise those system changes.
