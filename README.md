<p align="center">
  <img src="assets/app.png" width="112" height="112" alt="Feather Task Manager 깃털 앱 아이콘">
</p>

# Feather Task Manager 2026.9.1

[English](README.en.md) · [설치 파일과 포터블 다운로드](https://github.com/Hardt-LLC/FeatherTaskManager/releases/latest)

느려진 Windows에서도 빠르게 상태를 확인하고 작업을 정리하기 위한 네이티브 작업 관리자입니다. Rust와 Win32로 만들었으며 브라우저 엔진·WebView·WMI를 사용하지 않습니다. 0.2에는 **성능·시작 앱·서비스** 화면과 새 UI를 추가했습니다.

2026.9.1에서는 프로세스 목록·트리 전환, 트리 전체 종료, 한국어·영어 전환, 설치 프로그램을 추가했습니다. 단축키 안내는 키와 설명을 각각 고정된 열에 배치합니다. 깃털 로고·앱·탭 아이콘은 배율별 크기를 사용하며 새 그래픽 런타임을 추가하지 않았습니다.

## 실행

**Windows 10/11 x64**를 지원합니다. [GitHub Releases](https://github.com/Hardt-LLC/FeatherTaskManager/releases)에서 **Setup-x64.exe**는 설치용, **Portable-x64.exe**는 바로 실행하는 포터블 파일입니다. Rust·Visual C++ 런타임을 별도로 설치할 필요는 없습니다. 설치 파일은 Program Files에 앱을 배치하고 시작 메뉴 바로가기와 제거 프로그램을 제공합니다. 포터블 ZIP에는 복구 스크립트와 설명서도 포함됩니다.

언어는 처음 실행할 때 Windows 표시 언어를 따르며, **설정 → 한국어 / English**에서 즉시 바꿀 수 있습니다. 선택은 사용자별로 저장합니다. `--language en` 또는 `--language ko`는 저장된 설정을 바꾸지 않는 실행별 선택입니다.

Git 저장소에는 소스와 빌드에 필요한 아이콘 원본·생성 자산을 포함합니다. `dist`의 실행 파일·ZIP·미리보기는 로컬 빌드 결과이며 Git에는 포함하지 않습니다. 저장소를 처음 받았다면 아래 **소스에서 빌드** 절차로 생성하세요.

기본 권한으로 시작합니다. **설정 → 관리자로 실행**은 Windows UAC 확인을 거쳐 별도 관리자 창을 엽니다. 서비스 제어나 모든 사용자 시작 앱 변경에는 관리자 권한이 필요할 수 있습니다. Windows 작업 관리자 연결은 아래 설정을 선택할 때만 바뀝니다.

## Windows 작업 관리자로 설정

1. 왼쪽 아래 **설정 → Feather를 작업 관리자로 설정…**을 선택합니다.
2. 설치 위치와 변경 내용을 확인하고 Windows UAC를 승인합니다. 실행 파일을 `Program Files\Feather Task Manager\FeatherTaskManager.exe`에 복사한 뒤 이 PC의 모든 사용자에게 연결을 적용합니다.
3. 다음부터 `Ctrl+Alt+Delete → 작업 관리자`, `Ctrl+Shift+Esc` 또는 `taskmgr.exe` 실행 시 Feather가 열립니다. 이미 열린 작업 관리자 창은 바뀌지 않으며 재부팅은 필요하지 않습니다.

Windows의 [프로그램별 실행 설정(IFEO)](https://learn.microsoft.com/en-us/windows-hardware/drivers/debugger/gflags-details)을 사용합니다. [설정은 다음 실행부터 적용](https://learn.microsoft.com/en-us/windows-hardware/drivers/debugger/gflags-overview)되며 위 단축키는 [Windows의 작업 관리자 실행 경로](https://support.microsoft.com/en-us/windows/keyboard-shortcuts-in-windows-dcc61a57-8ff0-cffe-9796-cb9706c75eec)입니다. 조직의 작업 관리자 차단 정책은 변경하지 않습니다. 보안 화면에서 실제 `Ctrl+Alt+Delete` 동작은 자동 검증하지 않았으므로 적용 후 직접 확인하세요.

포터블 앱에서도 이 설정을 선택하면 연결할 실행 파일을 고정 위치에 설치할 수 있습니다. 설치 프로그램은 작업 관리자 연결을 자동으로 바꾸지 않습니다. 다른 작업 관리자 연결이나 지원하지 않는 IFEO 설정이 있으면 덮어쓰지 않고 오류를 알립니다.

**원복:** **설정 → Windows 기본 작업 관리자로 복원…**을 선택하고 UAC를 승인하세요. 설치된 실행 파일을 수동으로 삭제하기 전에 먼저 복원해야 합니다. 복원은 Feather의 연결만 해제하며 설치된 파일은 남겨 둡니다.

**업데이트:** Feather 창을 모두 닫고 새 설치 프로그램을 실행하세요. 기존의 정확한 Feather 연결은 유지합니다. 포터블 파일로 설치된 앱을 갱신하려면 먼저 연결을 복원하고 설치된 창을 닫은 뒤 새 파일에서 대체 설정을 다시 선택하세요. 제거 프로그램은 Feather 연결을 먼저 복원하며, 안전하게 복원할 수 없으면 앱 파일을 지우지 않습니다.

앱 메뉴를 사용할 수 없어도 실행 파일이 남아 있다면 64비트 PowerShell에서 다음 명령으로 복원할 수 있습니다.

```powershell
$featherExe = Join-Path ([Environment]::GetFolderPath('ProgramFiles')) 'Feather Task Manager\FeatherTaskManager.exe'
Start-Process -FilePath $featherExe -Verb RunAs -ArgumentList '--restore-task-manager' -Wait
```

실행 파일을 먼저 지웠다면 배포 ZIP에 포함된 `Restore-WindowsTaskManager.ps1`을 **관리자 권한의 64비트 PowerShell**에서 실행하세요. 이 스크립트는 정확히 일치하는 Feather의 `Debugger` 값 하나만 제거하며 다른 연결이나 잘못된 형식의 값은 그대로 둡니다. 소스 저장소에서는 `scripts\Restore-WindowsTaskManager.ps1`에 있습니다.

```powershell
powershell.exe -NoProfile -ExecutionPolicy Bypass -File .\Restore-WindowsTaskManager.ps1
```

## 네 가지 화면

| 화면 | 기능 |
| --- | --- |
| **프로세스** | 목록·트리 전환, 가지 접기·펼치기, 이름·PID 검색, 열 정렬, CPU·메모리·전용 메모리·I/O·스레드·핸들 표시, 작업·트리 끝내기, 파일 위치 열기 |
| **성능** | CPU·메모리·디스크·네트워크의 최근 60초 그래프, CPU 모델·논리 프로세서 수·가동 시간, GPU 사용량 요약 |
| **시작 앱** | 사용자·공용 Run 레지스트리와 시작프로그램 폴더 조회, 지원되는 항목 사용·사용 안 함 전환 |
| **서비스** | 이름·표시 이름·상태·PID·시작 유형 조회, 선택한 서비스 시작·중지 요청 |

화면은 어두운 탐색 영역과 밝은 작업 영역으로 구성했습니다. 시스템 글꼴, 일정한 간격, 정렬된 숫자 열, 키보드 탐색을 사용합니다. 요청한 [UI UX Pro Max skill](https://github.com/nextlevelbuilder/ui-ux-pro-max-skill)의 Fluent 2·접근성 지침을 네이티브 데스크톱에 맞게 적용했습니다. 자세한 기능 범위와 수치 해석은 [FEATURES-v2.md](FEATURES-v2.md)를 참고하세요.

## 조작

| 단축키 | 동작 |
| --- | --- |
| `Ctrl+1` / `Ctrl+2` / `Ctrl+3` / `Ctrl+4` | 프로세스 / 성능 / 시작 앱 / 서비스 |
| `Ctrl+F` | 현재 화면 검색창으로 이동 |
| `Esc` | 검색창의 검색어 지우기 |
| `F5` | 현재 화면 새로고침 |
| `Delete` | 프로세스 목록에서 선택한 작업 끝내기 확인 |
| `Shift+Delete` | 선택한 프로세스와 확인한 하위 트리 끝내기 확인 |
| `←` / `→` | 트리 화면에서 선택한 가지 접기 / 펼치기 |
| `Space` | 목록에 포커스가 있을 때 일시정지·계속 |
| `Ctrl+L` | 선택한 프로세스 파일 위치 열기 |

갱신 간격은 0.5초·1초·2초·5초 중 선택할 수 있으며 기본값은 1초입니다. 일시정지·항상 위 표시도 지원합니다. 최소화하면 주기적인 수집이 중단됩니다. 서비스 화면은 일시정지하지 않은 동안 5초마다 상태를 새로 읽습니다.

작업 끝내기는 저장하지 않은 작업을 잃을 수 있는 강제 종료이며 확인 대화상자를 표시합니다. PID와 생성 시각을 다시 확인하고 같은 프로세스 핸들로 종료하여 PID 재사용을 구분합니다. 시스템 필수 프로세스, PID 0·4, Feather Task Manager 자신과 신원을 확인할 수 없는 대상은 종료하지 않습니다.

**프로세스 트리:** 프로세스 화면의 보기 선택에서 목록과 트리를 전환합니다. 트리에서 숫자 열은 그대로 정렬되며 이름 열만 들여씁니다. 검색은 일치하는 프로세스의 상위 경로도 표시하고, 정렬은 같은 부모의 자식과 최상위 항목 사이에 적용합니다. 종료된 부모의 PID가 재사용된 경우 생성 시각으로 잘못된 연결을 배제합니다.

**트리 끝내기:** 확인할 때 표시한 프로세스와 하위 프로세스만 대상으로 합니다. 모든 대상의 신원·접근 권한·필수 프로세스 여부를 먼저 검사하고, 검증한 핸들을 유지한 채 자식부터 종료 요청을 보냅니다. 확인 이후 새로 생성된 자식까지 추적해 종료하지 않습니다. 사전 검사 실패 시 아무 대상도 종료하지 않고, 종료 도중 실패하면 요청·기종료·실패 수를 표시합니다. Windows나 다른 앱이 종료된 프로세스를 다시 실행하는 것까지 막지는 않습니다.

시작 앱 변경과 서비스 시작·중지는 사용자가 버튼을 누를 때만 수행합니다. 서비스 중지는 먼저 확인하며, 의존 서비스까지 연쇄 중지하거나 시작 유형을 바꾸지 않습니다. Windows의 접근 제한은 그대로 적용됩니다.

## 가볍게 동작하는 구조

프로세스는 `NtQuerySystemInformation`으로 묶어서 읽고 수집 버퍼를 재사용합니다. 모든 프로세스의 아이콘·실행 경로·핸들을 주기적으로 조회하지 않습니다. 데이터 수집과 제어 요청은 작업 스레드에서 처리하고, 목록은 필요한 셀만 그리는 Win32 가상 목록을 사용합니다.

추가 성능 카운터는 성능 화면에서만 수집합니다. 시작 앱·서비스는 화면을 열 때 조회하며 시작 유형 등 서비스 설정 정보는 캐시합니다. 그래프 때문에 별도의 애니메이션 타이머를 돌리지 않습니다. 실제 리소스 사용량은 열린 화면·프로세스 수·장치·갱신 간격에 따라 달라집니다.

## 소스에서 빌드

Rust의 `x86_64-pc-windows-msvc` 도구 모음, Visual Studio Build Tools의 C++ 빌드 도구와 Windows SDK가 필요합니다.

```powershell
cargo build --release --locked
```

의존성을 캐시한 상태에서는 `--offline`을 추가할 수 있습니다. 결과는 `target\release\FeatherTaskManager.exe`입니다. 배포 폴더·ZIP·SHA-256을 만들려면:

```powershell
powershell -NoProfile -ExecutionPolicy Bypass -File .\scripts\build.ps1 -Offline
```

처음 의존성을 내려받아야 한다면 `-Offline`을 빼세요.

## 검증과 측정

```powershell
cargo test --offline
Start-Process -FilePath .\dist\FeatherTaskManager.exe -ArgumentList '--self-test', '.\dist\self-test.txt' -WindowStyle Hidden -Wait
Get-Content .\dist\self-test.txt
powershell -NoProfile -ExecutionPolicy Bypass -File .\scripts\measure.ps1 -DurationSeconds 10 -Iterations 3
```

시작 앱 변경 테스트는 실제 자동실행 위치를 사용하지 않습니다. 전용 임시 레지스트리 키와 임시 파일로 원본 보존·상태 전환·오래된 항목 거부를 검증합니다. 서비스 검증은 조회와 입력 검사를 수행하며 사용자 서비스를 시작하거나 중지하지 않습니다. 프로세스 종료 검증은 직접 만든 임시 자식 프로세스만 대상으로 합니다. 제한된 샌드박스에서는 임시 레지스트리 쓰기 권한이 필요할 수 있습니다.

측정 스크립트는 자신이 실행한 프로세스만 종료하며, 결과를 `dist\benchmark.json`에 기록합니다. 실행 파일 크기·SHA-256과 CPU·작업 집합·전용 메모리·핸들·스레드 수를 함께 남깁니다. 숨김 실행의 창 감지·입력 대기는 첫 화면 완성 시간을 뜻하지 않으며 반복 실행은 콜드 스타트 측정이 아닙니다. 같은 컴퓨터·부하·화면·갱신 간격에서 비교하세요.

측정 조건과 결과는 [BENCHMARK.md](BENCHMARK.md), 검증 범위는 [VALIDATION.md](VALIDATION.md)를 참고하세요. 실행 파일 SHA-256이 일치하는 기록으로 확인해야 합니다. 이전 버전의 기록은 `dist/archive`에 별도로 보관합니다. 설치·서명·릴리스 방법은 [SIGNING.md](SIGNING.md)를 참고하세요. 버전은 `yyyy.m.x`이며 같은 달의 다음 릴리스는 `2026.9.2`처럼 증가합니다.

실제 데이터를 사용한 앱 내부 렌더링을 내보내려면 `FeatherTaskManager.exe --render-previews <폴더>`를 실행하세요. 약 1분간 읽기 전용으로 수집한 뒤 네 화면과 최소 크기·150% 배율 시뮬레이션의 BMP를 저장합니다. 다른 앱이나 바탕화면은 캡처하지 않습니다.

## 범위와 라이선스

Windows 작업 관리자의 모든 기능을 대체하지는 않습니다. Store 앱의 StartupTask, 예약 작업, RunOnce, 시작 영향도, 서비스 구성 편집, GPU별 상세 그래프는 포함하지 않습니다. 성능 카운터를 제공하지 않는 환경에서는 해당 항목의 경고나 확인 불가 상태를 표시합니다.

프로세스 수집의 NT 구조와 시작 앱의 `StartupApproved` 형식에는 Windows 변경에 따른 호환성 제한이 있습니다. 알 수 없는 시작 앱 상태는 읽기 전용으로 표시합니다. 세부 사항은 [기능 및 호환성 설명](FEATURES-v2.md)에 정리했습니다.

MIT 라이선스입니다. 전문은 [LICENSE](LICENSE), 의존성 고지는 [THIRD_PARTY_NOTICES.txt](THIRD_PARTY_NOTICES.txt)를 참고하세요.
