# Windows installer

`FeatherTaskManager-yyyy.m.x-Setup-x64.exe` is the administrator installer for Windows 10/11 x64. It supports English and Korean, installs to the fixed `Program Files\Feather Task Manager` directory, creates a Start menu shortcut, offers an optional desktop shortcut, and registers an uninstaller in Windows Settings. The portable EXE remains available separately.

Installation does not enable the Windows Task Manager replacement. Enable it explicitly from Feather's Settings after installation. The installer uses a fixed location so its payload and the Task Manager replacement always refer to the same executable. The folder and executable must be owned by Administrators or SYSTEM and must not allow ordinary users to modify them.

An upgrade uses the same installation identity and location. Close all Feather windows first. A global application mutex and a read-only test for a mapped executable make installation and removal refuse an app that is still running, including older releases without the mutex. The installer does not terminate processes for the user.

Before removal deletes the app, it reads the 64-bit Task Manager `Debugger` value. Only a byte-for-byte match to Feather's canonical REG_SZ command is eligible for restoration. It invokes Feather's restore helper, checks the exit code, then reads the registry again. Failed restoration or an unreadable setting aborts removal and preserves the executable. A manually edited command that still references Feather also blocks removal; genuinely unrelated programs' settings are preserved. If the executable was manually deleted first, use `Restore-WindowsTaskManager.ps1` as administrator before retrying removal.

Normal uninstall removes only installer-owned files and shortcuts. It does not recursively delete the application directory or arbitrary user files. Language preferences under the user's profile are retained.

Build a signed installer through `scripts/build-release.ps1` (see [SIGNING.md](SIGNING.md)). For compiler/layout testing only, `scripts/build-installer.ps1 -UnsignedDevelopment` writes an explicitly named `-UNSIGNED-DEVELOPMENT.exe`; the release publishing script never selects it.

Compiler checks validate the installer script and its payload metadata. Real installation, update, Task Manager replacement, and uninstall should also be exercised in a disposable Windows VM before broad distribution. Compiling an installer does not exercise those system changes.
