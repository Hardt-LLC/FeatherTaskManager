# Feather Task Manager Privacy Policy

**Effective date:** 2026-09-28 · **Applies to:** Feather Task Manager 2026.9.5 and later · **Publisher:** HARDT (Wyoming, USA)

[English](#english) · [한국어](#한국어)

## English

### Summary

Feather Task Manager does not collect, send, sell or share any personal information. It has no telemetry, no analytics, no crash reporting, no ads and no account. The app does not connect to the internet. Everything it shows is read from your own PC and stays on your PC.

### What the app reads on your PC, only to show it to you

- Processes: names, process IDs, executable paths, and CPU, memory, disk, GPU and network usage. Executable paths can include your Windows user folder name.
- Process details, only for the columns and details you choose to show: the account name, session, command line, CPU time and status of each process. A command line can contain file paths or options that someone typed.
- Startup apps and services: names, commands, state, and which process a service runs in.
- Hardware: CPU, memory, disk, network adapter and GPU model names and properties, such as speed, capacity, driver, temperature, fan speed and power, and firmware thermal zone temperatures. Serial numbers and asset tags are **not** read.
- Per-process network (only when you run Feather as administrator): a private Event Tracing for Windows session on your PC that counts bytes per process. It reads IP addresses and ports only while network tracing is turned on in the Resource Monitor (see below).
- Resource Monitor (only while its window is open):
  - TCP and UDP connections and listening ports: local and remote IP addresses, ports, state and the owning process, from the Windows connection table.
  - The files (DLLs and other modules) loaded by the process you select, with their paths.
  - Disk volumes and memory state.
  - Only with administrator rights, and only while you turn on file or network tracing (off by default, stopped when you close, pause or minimize the window): the paths of files that processes read or write, the remote addresses and ports of their network traffic, and byte counts.
- Nuclear Zombie (only when you run it): memory list sizes and the system handle table, to find exited processes that another program still holds open. It reads process identities only and never closes handles in other programs.

None of this information is stored in a file or sent anywhere. It is kept in memory while the app is open.

### What the app saves on your PC

- In `HKEY_CURRENT_USER\Software\FeatherTask`:
  - Your preferences (theme, language, refresh rate, default page, always on top, tray, always run as administrator).
  - The process column layout (which columns are shown, their order and widths).
  - The order of devices on the Performance page. Devices are identified by disk number and drive letters, network adapter ID, and GPU PCI IDs and slot; not by serial number.
- Only when you choose these actions:
  - Turning on the "Open with Ctrl + Shift + Esc" setting (Replace the built-in Task Manager) writes a `Debugger` value for `taskmgr.exe` under `HKEY_LOCAL_MACHINE\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options`. Turning it off removes it, and the uninstaller also restores it.
  - Turning a startup app on or off changes that entry's Windows "StartupApproved" state.
  - "Copy info" and "Copy details" place the selected details on the clipboard.
- The installer writes its own standard setup log to your temporary folder.

### Actions that change your system

Feather ends processes, changes priority or efficiency mode, starts or stops services, trims working sets, clears memory lists, starts the program you enter in Run new task and restarts Windows Explorer only when you ask it to. Administrator actions use the standard Windows UAC prompt.

### Third parties

The app itself shares nothing with HARDT or anyone else. If you download Feather from the Microsoft Store, winget or GitHub, those services handle the download under their own privacy statements. Feather does not receive that data.

### Children

Feather is a general-purpose utility, and it does not collect information from anyone, including children.

### Your choices and control

Because Feather does not collect data, there is nothing for us to access, correct or delete. Uninstalling the app removes your Feather preferences for the Windows account that runs the uninstaller. You can also delete `HKEY_CURRENT_USER\Software\FeatherTask` yourself.

### Changes and contact

If this policy changes, we will update this page and its effective date before the new version ships. Questions: https://github.com/Hardt-LLC/FeatherTaskManager/issues

---

## 한국어

### 요약

Feather Task Manager는 개인정보를 수집·전송·판매·공유하지 않습니다. 원격 분석, 통계 수집, 충돌 보고, 광고, 계정이 모두 없습니다. 앱은 인터넷에 연결하지 않습니다. 화면에 보이는 정보는 모두 사용자 PC에서 읽은 것이고, PC 밖으로 나가지 않습니다.

### 화면에 보여 주려고 PC에서 읽는 정보

- 프로세스: 이름, 프로세스 ID, 실행 파일 경로, CPU·메모리·디스크·GPU·네트워크 사용량. 실행 파일 경로에 Windows 사용자 폴더 이름이 들어 있을 수 있습니다.
- 프로세스 세부 정보(표시하도록 고른 열과 세부 정보에 한함): 각 프로세스의 계정 이름, 세션, 명령줄, CPU 시간, 상태. 명령줄에는 누군가 입력한 파일 경로나 옵션이 들어 있을 수 있습니다.
- 시작 앱과 서비스: 이름, 명령, 상태, 서비스가 실행 중인 프로세스
- 하드웨어: CPU·메모리·디스크·네트워크 어댑터·GPU의 모델명과 속성(속도, 용량, 드라이버, 온도, 팬 속도, 전력)과 펌웨어 온도 영역의 온도. 일련번호와 자산 태그는 **읽지 않습니다.**
- 프로세스별 네트워크(관리자 권한으로 실행했을 때만): PC 안의 전용 ETW(Windows 이벤트 추적) 세션으로 프로세스별 바이트 수를 셉니다. IP 주소와 포트는 리소스 모니터에서 네트워크 추적을 켰을 때만 읽습니다(아래 참고).
- 리소스 모니터(창이 열려 있는 동안만):
  - TCP·UDP 연결과 수신 대기 포트: Windows 연결 표에서 읽은 로컬·원격 IP 주소, 포트, 상태, 해당 프로세스
  - 선택한 프로세스가 불러온 파일(DLL 등 모듈)과 그 경로
  - 디스크 볼륨과 메모리 상태
  - 관리자 권한이 있고 파일 또는 네트워크 추적을 켰을 때만(기본값 꺼짐, 창을 닫거나 일시정지·최소화하면 멈춤): 프로세스가 읽고 쓰는 파일의 경로, 네트워크 통신의 상대방 주소와 포트, 바이트 수
- Nuclear Zombie(실행했을 때만): 메모리 목록 크기와 시스템 핸들 표를 읽어, 종료됐지만 다른 프로그램이 붙잡고 있는 프로세스를 찾습니다. 프로세스 식별 정보만 읽고 다른 프로그램의 핸들은 닫지 않습니다.

이 정보는 파일로 저장하거나 어디로도 보내지 않습니다. 앱이 열려 있는 동안 메모리에만 둡니다.

### PC에 저장하는 정보

- `HKEY_CURRENT_USER\Software\FeatherTask`:
  - 환경설정(테마, 언어, 새로 고침 주기, 기본 페이지, 항상 위, 트레이, 항상 관리자 권한으로 실행)
  - 프로세스 열 배치(표시할 열, 순서, 너비)
  - 성능 페이지의 장치 순서. 장치는 디스크 번호와 드라이브 문자, 네트워크 어댑터 ID, GPU의 PCI ID와 슬롯으로 구분하며 일련번호는 쓰지 않습니다.
- 사용자가 직접 선택했을 때만 저장되는 것:
  - 설정의 "Ctrl + Shift + Esc로 열기"(Windows 기본 작업 관리자 대체)를 켜면 `HKEY_LOCAL_MACHINE\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options`의 `taskmgr.exe`에 `Debugger` 값을 씁니다. 이 설정을 끄거나 앱을 제거하면 되돌립니다.
  - 시작 앱을 켜거나 끄면 해당 항목의 Windows "StartupApproved" 상태를 바꿉니다.
  - "정보 복사"나 "세부 정보 복사"를 누르면 선택한 정보를 클립보드에 넣습니다.
- 설치 프로그램은 표준 설치 로그를 임시 폴더에 남깁니다.

### 시스템을 바꾸는 동작

프로세스 끝내기, 우선순위나 효율 모드 변경, 서비스 시작·중지, 작업 집합 정리, 메모리 목록 비우기, "새 작업 실행"에 입력한 프로그램 실행, Windows 탐색기 다시 시작은 사용자가 요청할 때만 실행합니다. 관리자 권한이 필요한 동작은 Windows 표준 UAC 창을 거칩니다.

### 제3자

앱 자체는 HARDT를 포함해 누구와도 정보를 공유하지 않습니다. Microsoft Store, winget, GitHub에서 내려받으면 다운로드 과정은 각 서비스의 개인정보 처리방침을 따릅니다. Feather는 그 데이터를 받지 않습니다.

### 아동

Feather는 누구나 쓰는 일반 유틸리티이며, 아동을 포함해 어떤 사람의 정보도 수집하지 않습니다.

### 사용자의 선택과 통제

Feather는 데이터를 수집하지 않으므로 열람·정정·삭제를 요청할 대상이 없습니다. 앱을 제거하면 제거 프로그램을 실행한 Windows 계정의 Feather 환경설정도 지워집니다. `HKEY_CURRENT_USER\Software\FeatherTask`를 직접 지워도 됩니다.

### 변경과 문의

방침이 바뀌면 새 버전을 배포하기 전에 이 페이지와 시행일을 먼저 고칩니다. 문의: https://github.com/Hardt-LLC/FeatherTaskManager/issues
