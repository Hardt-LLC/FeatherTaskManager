#ifndef AppVersion
  #error AppVersion must be supplied by scripts/build-installer.ps1
#endif
#ifndef WindowsVersion
  #error WindowsVersion must be supplied by scripts/build-installer.ps1
#endif
#define AppName "Feather Task Manager"
#define AppExe "FeatherTaskManager.exe"

[Setup]
AppId={{E5C76533-366A-4865-A845-F912B79413B6}
AppName={#AppName}
AppVersion={#AppVersion}
AppVerName={#AppName} {#AppVersion}
AppPublisher=HARDT
AppPublisherURL=https://github.com/Hardt-LLC/FeatherTaskManager
AppSupportURL=https://github.com/Hardt-LLC/FeatherTaskManager/issues
AppUpdatesURL=https://github.com/Hardt-LLC/FeatherTaskManager/releases
DefaultDirName={autopf}\Feather Task Manager
DefaultGroupName={#AppName}
DisableDirPage=yes
DisableProgramGroupPage=yes
UsePreviousAppDir=no
PrivilegesRequired=admin
ArchitecturesAllowed=x64os
ArchitecturesInstallIn64BitMode=x64os
MinVersion=10.0.14393
CloseApplications=no
RestartApplications=no
UninstallDisplayIcon={app}\{#AppExe}
SetupIconFile=..\assets\app.ico
WizardStyle=modern
WizardSizePercent=110
WizardResizable=yes
Compression=lzma2/max
SolidCompression=yes
OutputDir=..\dist
VersionInfoVersion={#WindowsVersion}
VersionInfoProductVersion={#AppVersion}
VersionInfoDescription=Feather Task Manager Setup
LicenseFile=..\LICENSE
SetupLogging=yes
#ifdef UnsignedDevelopment
OutputBaseFilename=FeatherTaskManager-{#AppVersion}-Setup-x64-UNSIGNED-DEVELOPMENT
SignedUninstaller=no
#else
OutputBaseFilename=FeatherTaskManager-{#AppVersion}-Setup-x64
SignTool=ArtifactSigning
SignedUninstaller=yes
SignedUninstallerDir=..\target\signed-uninstallers
#endif

[Languages]
Name: "english"; MessagesFile: "compiler:Default.isl"
Name: "korean"; MessagesFile: "compiler:Languages\Korean.isl"

[CustomMessages]
english.DesktopShortcut=Create a desktop shortcut
korean.DesktopShortcut=바탕 화면에 바로가기 만들기
english.RunApp=Open Feather Task Manager
korean.RunApp=Feather Task Manager 실행
english.CloseApp=Close all Feather Task Manager windows before installing, updating, or uninstalling.
korean.CloseApp=설치, 업데이트 또는 제거하기 전에 Feather Task Manager 창을 모두 닫으세요.
english.FixedLocation=Feather Task Manager must be installed in its fixed Program Files folder.
korean.FixedLocation=Feather Task Manager는 지정된 Program Files 폴더에 설치해야 합니다.
english.InstallSafetyFailed=The installation folder could not be prepared or verified. No Task Manager association was changed.
korean.InstallSafetyFailed=설치 폴더를 준비하거나 검증하지 못했습니다. 작업 관리자 연결은 변경하지 않았습니다.
english.RestoreFailed=Windows Task Manager could not be restored. Uninstall has been stopped to preserve the application. Use Settings to restore Windows Task Manager, then try again. If the application file is missing, run Restore-WindowsTaskManager.ps1 as administrator.
korean.RestoreFailed=Windows 작업 관리자를 복원하지 못해 앱을 보존하고 제거를 중단했습니다. 설정에서 Windows 기본 작업 관리자로 복원한 뒤 다시 시도하세요. 실행 파일이 없다면 Restore-WindowsTaskManager.ps1을 관리자 권한으로 실행하세요.
english.RegistryFailed=The Task Manager association could not be read safely. Uninstall has been stopped.
korean.RegistryFailed=작업 관리자 연결을 안전하게 읽을 수 없어 제거를 중단했습니다.

[Tasks]
Name: "desktopicon"; Description: "{cm:DesktopShortcut}"; Flags: unchecked

[Files]
Source: "..\dist\FeatherTaskManager.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\scripts\Restore-WindowsTaskManager.ps1"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\LICENSE"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\THIRD_PARTY_NOTICES.txt"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\RUST_LIBRARY_NOTICES.html"; DestDir: "{app}"; Flags: ignoreversion

[Icons]
Name: "{autoprograms}\Feather Task Manager"; Filename: "{app}\{#AppExe}"
Name: "{autodesktop}\Feather Task Manager"; Filename: "{app}\{#AppExe}"; Tasks: desktopicon

[Run]
Filename: "{app}\{#AppExe}"; Parameters: "--language {code:AppLanguage}"; Description: "{cm:RunApp}"; Flags: nowait postinstall skipifsilent runasoriginaluser

[Code]
type
  TByteArray = array of Byte;
const
  IfeoPath = 'SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\taskmgr.exe';

function AppLanguage(Param: String): String;
begin
  if ActiveLanguage = 'korean' then Result := 'ko' else Result := 'en';
end;

function RegOpenKeyExW(Root: LongWord; SubKey: String; Options, Access: LongWord; var Key: LongWord): Longint;
  external 'RegOpenKeyExW@advapi32.dll stdcall';
function RegQueryValueSize(Key: LongWord; Name: String; Reserved: LongWord; var Kind: LongWord; Data: LongWord; var Size: LongWord): Longint;
  external 'RegQueryValueExW@advapi32.dll stdcall';
function RegQueryValueBytes(Key: LongWord; Name: String; Reserved: LongWord; var Kind: LongWord; var Data: Byte; var Size: LongWord): Longint;
  external 'RegQueryValueExW@advapi32.dll stdcall';
function RegCloseKey(Key: LongWord): Longint;
  external 'RegCloseKey@advapi32.dll stdcall';
function CreateFileW(Name: String; Access, ShareMode, Security, Creation, Flags, Template: LongWord): LongWord;
  external 'CreateFileW@kernel32.dll stdcall';
function CloseHandle(Handle: LongWord): Boolean;
  external 'CloseHandle@kernel32.dll stdcall';

function IsAppBusy: Boolean;
var
  Handle: LongWord;
begin
  Result := False;
  if not FileExists(ExpandConstant('{app}\{#AppExe}')) then exit;
  { Trust the installed image's kernel-enforced sharing state, not a public named
    object that any local user could create to block installation. Opening an
    image mapped by Windows for writing fails; no bytes are changed here. }
  Handle := CreateFileW(ExpandConstant('{app}\{#AppExe}'), $40000000, 7, 0, 3, 0, 0);
  Result := Handle = $FFFFFFFF;
  if not Result then CloseHandle(Handle);
end;

function HasOwnedConnection: Boolean;
var
  Key, Kind, Size, ActualSize: LongWord;
  Code: Longint;
  Data: TByteArray;
  Expected, Decoded, InstalledPath: String;
  I, WordValue: Integer;
begin
  Result := False;
  Code := RegOpenKeyExW($80000002, IfeoPath, 0, $0101, Key); { QUERY_VALUE | WOW64_64KEY }
  if (Code = 2) or (Code = 3) then exit;
  if Code <> 0 then RaiseException(CustomMessage('RegistryFailed'));
  try
    Size := 0;
    Code := RegQueryValueSize(Key, 'Debugger', 0, Kind, 0, Size);
    if Code = 2 then exit;
    if (Code <> 0) or (Size > 65536) then RaiseException(CustomMessage('RegistryFailed'));
    InstalledPath := ExpandConstant('{autopf}\Feather Task Manager\{#AppExe}');
    Expected := '"' + InstalledPath + '" --task-manager' + #0;
    if Size = 0 then exit;
    SetArrayLength(Data, Size);
    ActualSize := Size;
    Code := RegQueryValueBytes(Key, 'Debugger', 0, Kind, Data[0], ActualSize);
    if (Code <> 0) or (ActualSize <> Size) then RaiseException(CustomMessage('RegistryFailed'));
    Result := (Kind = 1) and (Size = LongWord(Length(Expected) * 2));
    if Result then begin
      for I := 1 to Length(Expected) do begin
        WordValue := Ord(Expected[I]);
        if (Data[(I - 1) * 2] <> (WordValue and $FF)) or
           (Data[(I - 1) * 2 + 1] <> (WordValue shr 8)) then Result := False;
      end;
    end;
    if Result then exit;
    { A manually edited Feather command belongs to the user. Preserve it, but do
      not remove the EXE it still references. Only exact owned bytes are removed. }
    Decoded := '';
    for I := 0 to (Integer(Size) div 2) - 1 do
      Decoded := Decoded + Chr(Integer(Data[I * 2]) + Integer(Data[I * 2 + 1]) * 256);
    if Pos(LowerCase(InstalledPath), LowerCase(Decoded)) > 0 then
      RaiseException(CustomMessage('RestoreFailed'));
  finally
    RegCloseKey(Key);
  end;
end;

function PrepareToInstall(var NeedsRestart: Boolean): String;
begin
  Result := '';
  if CompareText(ExpandConstant('{app}'), ExpandConstant('{autopf}\Feather Task Manager')) <> 0 then
    Result := CustomMessage('FixedLocation')
  else if IsAppBusy then Result := CustomMessage('CloseApp');
end;

procedure CurStepChanged(CurStep: TSetupStep);
var
  Code: Integer;
begin
  if CurStep = ssInstall then begin
    if IsAppBusy then RaiseException(CustomMessage('CloseApp'));
    ExtractTemporaryFile('{#AppExe}');
    if not Exec(ExpandConstant('{tmp}\{#AppExe}'), '--prepare-install-directory', '', SW_HIDE, ewWaitUntilTerminated, Code) or (Code <> 0) then
      RaiseException(CustomMessage('InstallSafetyFailed'));
  end;
  if CurStep = ssPostInstall then begin
    if not Exec(ExpandConstant('{app}\{#AppExe}'), '--validate-installation', '', SW_HIDE, ewWaitUntilTerminated, Code) or (Code <> 0) then
      RaiseException(CustomMessage('InstallSafetyFailed'));
  end;
end;

function InitializeUninstall: Boolean;
begin
  Result := not IsAppBusy;
  if not Result then MsgBox(CustomMessage('CloseApp'), mbError, MB_OK);
end;

procedure CurUninstallStepChanged(CurUninstallStep: TUninstallStep);
var
  Code: Integer;
begin
  if CurUninstallStep = usUninstall then begin
    if IsAppBusy then begin
      MsgBox(CustomMessage('CloseApp'), mbError, MB_OK);
      Abort;
    end;
    { Run before Inno deletes any installed file. Never delete the IFEO key or
      a foreign Debugger, and never continue after a failed restoration. }
    try
      if HasOwnedConnection then begin
        if not FileExists(ExpandConstant('{app}\{#AppExe}')) or
           not Exec(ExpandConstant('{app}\{#AppExe}'), '--restore-task-manager', '', SW_HIDE, ewWaitUntilTerminated, Code) or
           (Code <> 0) or HasOwnedConnection then
          RaiseException(CustomMessage('RestoreFailed'));
      end;
    except
      MsgBox(GetExceptionMessage, mbError, MB_OK);
      Abort;
    end;
    { Only after the Task Manager check above, and while the installed helper
      still exists: delete HKCU\Software\FeatherTask (preferences, language).
      This elevated uninstaller's HKCU is the account that runs it; when a
      standard user approves UAC with an administrator's credentials, that is
      the administrator's HKCU. Other accounts keep their preferences. The
      helper never follows a registry link. A failure does not stop removal. }
    if not FileExists(ExpandConstant('{app}\{#AppExe}')) then
      Log('Feather preferences not removed: the application file is missing.')
    else if not Exec(ExpandConstant('{app}\{#AppExe}'), '--remove-user-preferences', '', SW_HIDE, ewWaitUntilTerminated, Code) then
      Log('Feather preferences not removed: the helper could not start.')
    else if Code <> 0 then
      Log(Format('Feather preferences not removed completely (helper exit code %d).', [Code]));
  end;
end;
