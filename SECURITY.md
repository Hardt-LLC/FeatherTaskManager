# Security review — 2026.9.2, 2026.9.3

## Reporting a vulnerability / 취약점 제보

Please report security issues privately through GitHub's **Report a vulnerability** form on this repository's Security tab (GitHub Security Advisories), not in a public issue. Include the Feather version, Windows version and reproduction steps. Only the latest release receives fixes.

보안 문제는 공개 이슈 대신 이 저장소 Security 탭의 **Report a vulnerability**(GitHub 보안 권고)로 비공개 제보해 주세요. Feather 버전, Windows 버전과 재현 절차를 함께 알려 주세요. 수정은 최신 릴리스에만 제공합니다.

## 2026.9.3 출시 전 검토

검토일: 2026-09-27. 기준 소스: 2026.9.3 출시 전 작업 트리. 새 기능(메모리 정리 도우미, 하드웨어·메모리 진단 출력, 프로세스별 네트워크 ETW)과 실행 중 DLL 로드를 검토했습니다.

| ID | 위험도 | 조건 및 영향 | 수정 |
| --- | --- | --- | --- |
| FTM-2026-05 | 높음 | FTM-2026-02의 수정은 EXE의 **정적** 가져오기만 System32로 제한했습니다. Windows 구성 요소가 실행 중 이름으로 불러오는 DLL(예: `gdi32full.dll`의 `opengl32.dll`)과 DLL 초기화 중 불러오는 DLL(`powrprof.dll`이 지연 로드하는 `umpdc.dll`)은 여전히 EXE 폴더부터 검색되었습니다. 공격자가 포터블 EXE 옆(예: 다운로드 폴더)에 DLL을 놓으면 Feather 안에서 실행되고, 사용자가 포터블 복사본을 관리자 권한으로 다시 실행하면 같은 권한을 얻습니다. | `main()`의 첫 동작으로 `SetDefaultDllDirectories(LOAD_LIBRARY_SEARCH_SYSTEM32)`를 호출하고, 실패하면 다른 코드를 실행하지 않고 종료 코드 3으로 끝납니다. 이어서 이미지 로드 정책 `PreferSystem32Images`·`NoRemoteImages`를 설정합니다(지원하지 않는 Windows에서는 무시). `main()`보다 먼저 `umpdc.dll`을 부르는 `powrprof.dll`은 `/DELAYLOAD`로 첫 사용 시점(제한 이후)에 불러옵니다. |
| FTM-2026-06 | 중간 | 관리자 권한 UI에서 메모리 목록 비우기를 한 번 실행하면 `SeProfileSingleProcessPrivilege`가 프로세스가 끝날 때까지 켜진 채로 남았습니다. | `AdjustTokenPrivileges`가 바꾼 이전 상태를 저장하고 `NtSetSystemInformation` 직후 복원합니다. |
| FTM-2026-07 | 중간 | `--purge-memory-lists`가 목록 이름 뒤의 인수를 검사하지 않았습니다. | `<목록>`(쉼표로 구분한 `workingsets\|systemworkingset\|modified\|lowstandby\|standby`, 또는 `all`) 뒤에는 `--language ko\|en`만 허용하고, 그 밖의 인수는 사용법 오류(종료 코드 1)입니다. |
| FTM-2026-08 | 중간 | 진단 출력(`--self-test`, `--render-previews`, 새 `--dump-hardware`, `--memory-cleanup-dry-run`)을 다른 사용자가 쓸 수 있는 폴더에서 관리자 권한으로 실행하면, 미리 만든 심볼릭 링크·하드 링크·정션이 쓰기를 관리자 권한으로 다른 파일에 돌릴 수 있었습니다. | 두 새 인수는 출력 파일 경로가 필수입니다. 네 출력 모두 파일을 `FILE_FLAG_OPEN_REPARSE_POINT`로 열고, 경로의 폴더가 정션·심볼릭 링크·마운트 지점이거나 파일이 재분석 지점이거나 하드 링크가 둘 이상이면 쓰지 않습니다. 열린 파일과 폴더의 최종 경로가 검사한 폴더 체인과 일치할 때만 기존 파일을 비우며, 검사에 실패하면 새로 만든 빈 파일은 지웁니다. 일반 권한의 기본 파일 이름(`self-test.txt`, `previews`)은 그대로 동작합니다. |
| FTM-2026-09 | 중간 | 프로세스별 네트워크 ETW 콜백이 이벤트의 공급자를 확인하지 않아, Feather의 세션을 제어할 수 있는 사용자가 다른 공급자를 세션에 추가하면 같은 이벤트 ID로 바이트 수를 주입할 수 있었습니다. | `EventHeader.ProviderId`가 Microsoft-Windows-Kernel-Network인 이벤트만 집계합니다. |

FTM-2026-05도 **공격자에게 포터블 위치의 쓰기 권한이 있고 사용자가 실행·권한 상승을 승인하는 조건**에 해당합니다. Program Files 설치본은 관리자만 쓸 수 있는 폴더에 있어 같은 조건이 성립하지 않습니다.

## 2026.9.2 검토

검토일: 2026-09-26. 기준 소스: `c66666c` 및 2026.9.1 배포 파일. 수정 대상: 2026.9.2.

프로세스 제어·트리 구성, Windows 네이티브 버퍼 파싱, 시작 앱·서비스, 관리자 권한 전환, IFEO 등록·복원, 설치·제거, DLL 로더 정책, 서명·배포 경로를 검토했습니다. 외부 보안 인증이나 모든 Windows 환경에 대한 침투 테스트를 뜻하지는 않습니다.

### 확인하고 수정한 문제

| ID | 위험도 | 조건 및 영향 | 수정 |
| --- | --- | --- | --- |
| FTM-2026-01 | 높음 | 공격자가 포터블 폴더에 쓸 수 있고 사용자가 대체 설정의 관리자 실행을 승인한 경우, 실행 중인 원본 EXE를 이름 변경하고 같은 경로에 다른 파일을 놓아 관리자 복사 대상의 바이트를 바꿀 수 있었습니다. | 관리자 권한 포터블 복사를 제거했습니다. Setup으로 설치한 고정 Program Files 경로의 소유자·ACL·재분석 지점을 확인하고 경로 및 파일 핸들을 유지한 채 연결합니다. |
| FTM-2026-02 | 높음 | 공격자가 포터블 EXE 옆에 DLL을 놓을 수 있고 사용자가 앱을 실행하면 정적 DLL 검색이 그 파일을 선택할 수 있었습니다. 사용자가 앱을 관리자 실행하면 같은 권한으로 로드될 수 있습니다. | `/DEPENDENTLOADFLAG:0x800`으로 정적 DLL 검색을 System32로 제한했습니다. 배포 검사에서 해당 PE 값과 ASLR·DEP·고엔트로피 VA를 확인합니다. 실행 중·DLL 초기화 중 로드는 2026.9.3의 FTM-2026-05에서 제한했습니다. |
| FTM-2026-03 | 중간 | 일반 권한 프로그램이 HKCU의 시작 앱 승인 경로나 언어 설정 경로에 레지스트리 링크를 만들면 Feather가 다른 키를 읽거나 쓸 수 있었습니다. 앱이 관리자 실행 중이면 제한된 형식의 쓰기가 그 권한으로 수행될 수 있습니다. | 고정한 부모 핸들을 기준으로 경로 각 구성 요소를 `OBJ_DONT_REPARSE`로 열거나 생성합니다. 중간·말단·미완성 링크와 생성 직전 삽입된 링크를 거부합니다. |
| FTM-2026-04 | 중간 | 다른 로컬 사용자가 예측 가능한 전역 객체 이름을 선점하면 앱 실행이 실패하거나 설치·제거가 차단될 수 있었습니다. | 전역 실행 마커와 설치 프로그램의 이름 기반 뮤텍스 검사를 제거했습니다. 설치된 이미지의 사용 여부는 Windows 파일 공유 상태로 확인합니다. |

F01/F02의 높은 위험도는 **공격자에게 포터블 위치의 쓰기 권한이 있고 사용자가 실행·권한 상승을 승인하는 조건**에 해당합니다. 원격 무인 공격이나 UAC를 무조건 우회하는 취약점으로 판단한 것은 아닙니다. F03에서 쓸 수 있던 데이터는 임의 코드가 아니라 시작 승인 상태의 제한된 REG_BINARY 또는 `ko`/`en` REG_SZ입니다.

프로세스 종료는 PID·생성 시각을 같은 열린 핸들에서 검증하고, 트리는 모든 대상을 사전 검사한 후 자식부터 종료합니다. 검사한 코드와 테스트에서 이 경로의 추가 취약점을 확인하지 못했습니다. 서비스 제어는 Windows가 서비스 핸들의 권한을 검사하며, 종료·설정 변경은 사용자 동작으로 시작됩니다.

## 메모리 정리(Nuclear Zombie)의 권한 경계

- 관리자 도우미 인수 `--purge-memory-lists <목록>`가 추가되었습니다. 목록은 `workingsets`, `systemworkingset`, `modified`, `lowstandby`, `standby`를 쉼표로 이은 것(각각 한 번, 알 수 없는 이름·중복은 사용법 오류) 또는 `all`(`modified,standby`)이며, 정해진 순서로 실행합니다. UI는 Feather가 이미 관리자 권한이면 같은 프로세스에서 처리하고, 아니면 연결 설정 도우미와 같은 방식으로 **설치된 Program Files 이미지만** UAC로 실행합니다. 경로 구성 요소와 설치 파일을 잠그고, 실행 중인 EXE와 **바이트 단위로 같은 빌드**일 때만 시작합니다. 포터블 EXE의 `current_exe()` 경로는 관리자 실행에 쓰지 않습니다. 다른 빌드나 확인할 수 없는 설치는 거부하고 그 이유를 결과에 표시합니다.
- 도우미는 정확히 `<목록 이름>` 또는 `<목록 이름> --language ko|en` 형식만 받고(UI가 언어를 덧붙임), 그 밖의 인수는 사용법 오류(종료 코드 1)로 끝냅니다(FTM-2026-07). 선택한 단계에 필요한 권한만 켭니다: 메모리 목록 명령(`NtSetSystemInformation`: 작업 집합·수정된 목록·대기 목록·우선순위 0 대기 목록)에는 `SeProfileSingleProcessPrivilege`, 시스템 작업 집합(`SetSystemFileCacheSize(-1, -1)`)에는 `SeIncreaseQuotaPrivilege`를 켜고, 끝나면 이전 상태로 되돌립니다. 관리자 권한 UI가 같은 프로세스에서 처리할 때도 마찬가지입니다(FTM-2026-06). 파일·레지스트리를 쓰지 않고 UI도 열지 않으며, 결과는 종료 코드로만 전달합니다.
- 도우미를 포함한 모든 실행은 `main()`의 첫 동작으로 DLL 검색을 System32로 제한합니다(FTM-2026-05). 설치된 이미지는 관리자만 쓸 수 있는 폴더에 있지만, 같은 제한을 둡니다.
- 좀비 프로세스 검사는 **읽기 전용**입니다. 시스템 핸들 표를 읽고, 다른 프로세스의 프로세스 핸들을 Feather 안으로 제한된 권한(`SYNCHRONIZE | PROCESS_QUERY_LIMITED_INFORMATION`)으로 복제해 종료 여부를 확인합니다. `DUPLICATE_CLOSE_SOURCE`는 쓰지 않으며 Feather가 만든 복제본만 닫습니다. 다른 프로그램의 핸들을 닫거나 바꾸지 않습니다. 핸들 표 버퍼는 256 MB로 제한하고 항목 수를 버퍼 크기와 대조합니다.
- 작업 집합 정리는 일반 권한에서는 Feather가 열 수 있는 프로세스에만 `EmptyWorkingSet`을 호출하고, 관리자 권한에서는 RAMMap의 Empty Working Sets와 같은 커널 명령으로 모든 프로세스를 한 번에 정리합니다. 어느 쪽도 프로세스를 종료하지 않고, 일반 권한의 정리는 UAC를 요청하지 않습니다. 보유 프로세스의 작업 끝내기는 자동으로 실행되지 않고 기존 확인 대화 상자와 PID·생성 시각 검증을 거칩니다.
- 미리보기·테스트는 작업 집합 정리나 메모리 목록 비우기를 실행하지 않습니다. `--memory-cleanup-dry-run <파일>`도 읽기만 하며, 결과는 FTM-2026-08의 링크 검사를 거쳐 지정한 파일에만 씁니다.

## "항상 관리자 권한으로 실행" 설정의 권한 경계

대상: 2026.9.3 이후 작업 트리(미출시). 검토일: 2026-09-27.

- 설정은 HKCU `Software\FeatherTask\Preferences`의 `AlwaysRunAsAdministrator`(REG_DWORD, 정확히 1일 때만 켜짐)에 다른 설정과 같은 no-reparse 열기(FTM-2026-03)로 저장합니다. 각 창은 자기가 바꾼 값만 쓰므로, 열려 있는 다른 창(예: "관리자로 실행" 뒤에 남는 일반 권한 창)이 오래된 값으로 되돌리지 않습니다. 저장에 실패하면 스위치는 저장된 상태로 돌아갑니다.
- 켜져 있고 토큰이 상승되지 않았으면, 창을 여는 일반 실행(`--task-manager` 포함)이 창을 만들기 전에 **설치된 Program Files 이미지만** 관리자 도우미와 같은 방식으로 다시 시작합니다: 경로 구성 요소와 설치 파일을 잠그고, 실행 중인 EXE와 **바이트 단위로 같은 빌드**일 때만, 잠금을 유지한 채 `ShellExecuteExW` "runas"(`SEE_MASK_NOASYNC | SEE_MASK_NOCLOSEPROCESS`)로 시작합니다. 프로세스 핸들을 받은 경우에만 일반 권한 인스턴스가 종료합니다. 매번 Windows의 일반 UAC 동의 창을 거치며, 예약 작업·서비스·자동 상승 COM 등 UAC를 건너뛰는 방법은 쓰지 않습니다.
- 포터블·다른 빌드·확인할 수 없는 설치의 실행은 자동으로 상승하지 않습니다(FTM-2026-01/05의 조건: 실행 중 이름 변경·교체, EXE 폴더의 DLL). 거부·실패와 마찬가지로 재시도 없이 일반 권한으로 계속 실행하고 상태 표시줄에 이유를 알립니다. 한 번만 실행하는 **⋯ → 관리자로 실행**은 이전과 같이 실행 중인 EXE를 상승시키므로, 포터블 EXE는 다른 사용자가 쓸 수 있는 폴더에 두지 말고 UAC 창의 게시자(HARDT)를 확인하세요.
- 인수는 명령줄 텍스트를 복사하지 않고 고정 문자열로 다시 만듭니다: `--task-manager`만(IFEO가 붙인 taskmgr 인수는 버림), 또는 알려진 `--page` 값, 리소스 모니터 창 표식 `--resource-monitor`(한 번만), 표시 언어, 그리고 비공개 표식 `--elevated-relaunch`(**⋯ → 관리자로 실행**도 붙임). 표식이 있는 실행은 다시 시작하지 않으며, 상승되지 않았으면(UAC가 꺼진 표준 사용자처럼 "runas"가 상승되지 않은 경우) 그 사실을 상태 표시줄에 알립니다. 진단·도우미 모드(`--self-test`, `--purge-memory-lists` 등)와 알 수 없는·중복된 인수의 실행은 다시 시작하지 않습니다.
- 같은 사용자의 프로그램도 이 값을 켤 수 있지만, 결과는 설치된 Feather에 대한 UAC 확인 창이며 상승에는 여전히 사용자 동의가 필요합니다.
- 표준 사용자가 UAC 창에 다른 관리자 계정의 자격 증명을 입력하면, 상승된 인스턴스는 그 관리자 계정으로 실행되어 **그 계정의 HKCU**(이 설정, 테마, 트레이, 시작 페이지, 언어)를 읽고 씁니다. 그 창에서 이 설정을 끄면 관리자 계정의 값만 바뀌므로, 표준 사용자의 설정은 일반 권한 창(UAC 창을 거부해 연 창)에서 끄세요. `--task-manager` 실행은 언어 인수를 전달하지 않아 그 계정의 언어로 열립니다.
- 상승된 창은 UIPI 때문에 탐색기(보통 무결성)의 `TaskbarCreated` 브로드캐스트를 받지 못해, 탐색기가 다시 시작되면 트레이에 숨긴 창의 아이콘이 돌아오지 않을 수 있었습니다. 이 메시지만 `ChangeWindowMessageFilterEx`로 허용하며, 처리기는 트레이 아이콘을 다시 추가하기만 합니다.
- 제거 프로그램은 작업 관리자 복원 검사 뒤, 파일을 지우기 전에 설치된 도우미 `--remove-user-preferences`(정확히 이 인수 하나만, 그 밖에는 종료 코드 1)를 관리자 권한으로 실행해 **제거를 실행한 계정**의 HKCU `Software\FeatherTask`를 지웁니다. 일반 권한 프로그램이 이 트리에 만든 레지스트리 링크가 관리자 권한 삭제를 다른 키로 돌리지 못하도록, 상위 경로는 FTM-2026-03의 no-reparse 열기로(링크면 거부), 키와 모든 하위 키는 고정한 부모 기준으로 한 구성 요소씩 `OBJ_OPENLINK | OBJ_DONT_REPARSE`로 열어 링크는 링크 자체만 지우고 대상은 열거나 지우지 않습니다. 깊이 32단계·키 10,000개를 넘으면 중단합니다. 파일을 쓰지 않고 UI를 열지 않으며, 창 실행 전에 처리되어 "항상 관리자 권한으로 실행"의 다시 시작 대상이 아닙니다. 도우미가 실패해도 제거는 계속됩니다.

## 재현과 회귀 검사

- FTM-2026-05: 격리된 임시 폴더에 배포 EXE 복사본과 **실행 코드가 없는 유효한 x64 DLL**(엔트리 포인트·가져오기·내보내기 없음, MSVC 링커로 생성)을 `opengl32.dll`, `umpdc.dll`, `wmiclnt.dll` 이름으로 두고, 화면이 없는 `--self-test`를 디버그 이벤트(`LOAD_DLL_DEBUG_EVENT`)로 관찰했습니다. `opengl32.dll`은 화면 없는 경로에서 로드되지 않았습니다. 이전 빌드는 `umpdc.dll`을 EXE 폴더에서 매핑했고, 로더 중단점과 EXE 진입점 사이(DLL 초기화 단계, `main()` 이전)에서 발생했습니다. `main()` 제한만 넣은 빌드도 같은 파일을 매핑했고, 지연 로드만 넣은 빌드는 `main()` 이후 같은 파일을 매핑했습니다. 두 수정을 모두 넣은 빌드는 `--test-child`, `--self-test`, `--dump-hardware`, `--memory-cleanup-dry-run`에서 EXE 폴더의 DLL을 하나도 매핑하지 않았고 자체 검사는 `PASS`였습니다. 매핑된 나머지 모듈(WinSxS의 comctl32 v6·GDI+, System32의 oleacc·uxtheme·dwmapi·pdh 등)은 이전 빌드와 같았습니다. 창을 여는 UI 경로는 이 검사에서 실행하지 않았습니다.
- FTM-2026-06: 테스트는 무해한 `SeTimeZonePrivilege`로 권한이 켜졌다가 이전 상태로 돌아오는지, 이미 켜진 권한은 그대로 두는지 확인합니다. 관리자 권한 메모리 목록 비우기는 실행하지 않았습니다.
- FTM-2026-07: 정확한 형식 2가지를 허용하고 인수 누락·추가 인수·알 수 없는 언어·반복된 `--language` 등 7가지를 거부하는 테스트를 추가했습니다. 배포 EXE에서도 추가 인수와 잘못된 언어가 종료 코드 1, 정상 형식이 일반 권한에서 권한 오류(종료 코드 2)로 끝남을 확인했습니다.
- FTM-2026-08: 임시 폴더의 하드 링크(대상 내용 유지), 정션(출력 폴더 및 상위 폴더), 심볼릭 링크(만들 수 있는 환경에서), 폴더 경로를 거부하고 일반 파일은 쓰고 비우는 테스트를 추가했습니다. 배포 EXE에서도 정션 아래 출력이 종료 코드 1이고 대상 폴더에 파일이 생기지 않으며, 출력 경로 없는 새 인수가 종료 코드 1, 기본 `self-test.txt`가 정상 동작함을 확인했습니다.
- FTM-2026-09: 같은 송수신 이벤트 ID라도 다른 공급자 GUID와 비어 있는 GUID의 이벤트는 집계하지 않는 테스트를 추가했습니다.
- F01: 격리된 `--test-child` EXE가 실행 중일 때 이름 변경과 원래 경로의 파일 교체가 가능함을 확인했습니다. 실제 Program Files 덮어쓰기나 UAC 공격은 실행하지 않았습니다. 수정 후 신뢰할 수 없거나 없는 설치를 거부하고 연결 키·디렉터리를 만들지 않는 테스트를 추가했습니다.
- F02: 격리된 배포 파일 옆에 **실행 가능한 코드가 없는 텍스트 파일**을 `pdh.dll` 이름으로 놓았을 때 이전 버전이 `0xC000012F`로 시작 전에 종료됐습니다. 수정 빌드는 같은 파일을 무시하고 테스트 진입점에 도달했습니다. 악성 DLL을 실행하지 않았습니다. PE 정책 검사기는 정상 빌드를 통과시키고 이전 빌드 및 14개 변형·손상 파일을 거부했습니다.
- F03: 전용 임시 HKCU 링크로 실제 시작 앱 변경 함수를 호출했을 때 다른 임시 키에 상태가 쓰이는 문제를 재현했습니다. 수정 후 링크 종류·경로 위치·생성 경합과 정상 설정 저장을 회귀 검사합니다. 실제 시작 앱, 사용자 언어 설정, HKLM 값을 바꾸지 않았습니다.
- F04: 관리자 권한이 없는 프로세스가 임의의 테스트용 Global 이름에 Event를 만들면 같은 이름의 Mutex 생성이 오류 6으로 실패함을 확인했습니다. 실제 앱의 전역 이름은 사용하지 않았습니다. 해당 실행 의존성을 제품에서 제거했습니다.
- 별도 격리 검사에서 네이티브 프로세스 레코드 50,000개 변형과 120개 노드 그래프 1,000개를 처리했습니다. 잘못된 버퍼의 거부, 유한 순회, 대상 중복 방지 및 자식 우선 순서를 확인했습니다. 이는 메모리 안전성의 수학적 증명이나 지속 퍼징을 대신하지 않습니다.

최종 전체 검사와 서명된 배포 파일의 실행 결과는 [VALIDATION.md](VALIDATION.md)에 기록합니다. 진단 파일과 테스트 도구는 무시되는 `target` 경로에 두며 릴리스에 포함하지 않습니다.

## 소스·배포 및 비밀정보

- Gitleaks **8.30.1**로 기준 Git 이력 5개 커밋과 추적 파일 111개를 검사했으며 비밀정보가 탐지되지 않았습니다. 도구 다운로드는 공식 배포의 SHA-256과 대조했고 출력은 전체 마스킹을 사용했습니다. 패턴 검사로 탐지되지 않는 비밀정보까지 없다고 보증하지는 않습니다.
- 최신 RustSec 데이터베이스 커밋 `e2111519ba6d14a5da59a7b2e5c8083ae8a37c01`에서 의존성 `windows-sys 0.61.2`, `windows-link 0.2.1`의 등록된 권고를 찾지 못했습니다. Cargo.lock에는 두 패키지의 버전·체크섬이 고정되어 있습니다. `cargo-audit`가 설치되어 있지 않아 공식 데이터베이스의 해당 패키지 항목을 직접 대조했습니다.
- GitHub Actions는 main에서 실행한 워크플로만 허용하고, 태그 소스의 스크립트를 실행하기 전에 신뢰된 인라인 Git 검사로 태그·HEAD·main 이력을 대조합니다. 체크아웃 인증은 저장하지 않으며 빌드 작업은 저장소 읽기 권한만 가집니다. Azure 자격증명은 컴파일 이후 서명 단계에만 환경변수로 전달합니다.
- 게시 작업은 별도 러너에서 수행합니다. 로컬 게시에서도 정확한 태그, 깨끗한 추적 소스, main 이력, 원격의 주석 태그를 포함한 최종 커밋을 대조합니다. 두 소스 검증기의 독립 테스트는 정상 상태 4개를 허용하고 잘못된 상태 12개를 거부했습니다.
- 서명 계정 비밀은 GitHub나 저장소에 추가하지 않았습니다. 실제 서명은 승인된 로컬 자격증명을 프로세스 환경에서만 사용합니다.

## 남은 검증 범위와 운영 설정

- 실제 관리자 설치·업그레이드·제거, UAC 전체 흐름과 `Ctrl+Alt+Delete` 보안 화면은 별도 Windows VM에서 확인해야 합니다. 컴파일·서명·모듈 검사만으로 이 전체 흐름을 검증했다고 보지 않습니다.
- "항상 관리자 권한으로 실행"의 실제 흐름도 VM에서 확인해야 합니다: 설치본에서 UAC 승인·거부, IFEO를 거친 `Ctrl+Shift+Esc`와 `Ctrl+Alt+Delete → 작업 관리자`, 포터블·다른 빌드의 거부 알림, UAC가 꺼진 표준 사용자, 표준 사용자가 관리자 자격 증명을 입력하는 경우, 상승된 창을 트레이에 숨긴 채 탐색기를 다시 시작하는 경우(아이콘 복원과 트레이 클릭). 테스트는 결정 함수·인수 목록·포터블 거부 검사만 다루며 UAC를 띄우지 않습니다.
- DLL 검색 플래그의 보안 효과는 **Windows 10 1607 이상**에서 적용됩니다. 설치 프로그램의 최소 버전을 이에 맞췄습니다. `main()` 이후의 로드는 `SetDefaultDllDirectories`로 System32에 제한되지만, `main()` 이전 DLL 초기화 중의 로드는 Windows 버전·드라이버·입력기 구성에 따라 다를 수 있습니다. 이 검토의 PC에서 관찰된 `powrprof.dll`→`umpdc.dll` 경로만 지연 로드로 막았습니다.
- 기본 DLL 디렉터리가 System32뿐이므로, 셸 대화 상자 등이 불러오는 제3자 확장이 자기 폴더의 종속 DLL을 이름으로만 찾는 경우 로드되지 않을 수 있습니다. 창을 여는 UI 경로(파일 속성, OpenGL을 쓰는 GDI 경로 등)는 별도로 수동 확인해야 합니다.
- GitHub `release` 환경은 아직 구성되지 않았고 저장소·환경 비밀도 없습니다. 호스팅 서명을 사용하기 전 main 전용 환경 배포 규칙·검토자를 구성해야 합니다. 저장소 작성자가 다른 워크플로를 만드는 상황까지 YAML만으로 제한할 수는 없습니다. [설정 안내](SIGNING.md)를 참고하세요.
- main·버전 태그 보호 규칙, 변경 불가 릴리스, 비밀 검사·푸시 보호, Dependabot 경고는 저장소 설정으로 켜야 하며 이 검토에서 변경하지 않았습니다.
- 관리자/SYSTEM이 이미 악성 코드에 장악된 경우, 서명 계정·개발 도구 자체가 침해된 경우, 장기간 자원 고갈과 모든 Windows 빌드의 호환성은 이번 검토의 보장 범위가 아닙니다.

## 근거 문서

- [Microsoft: DEPENDENTLOADFLAG](https://learn.microsoft.com/en-us/cpp/build/reference/dependentloadflag?view=msvc-170)
- [Microsoft: SetDefaultDllDirectories](https://learn.microsoft.com/en-us/windows/win32/api/libloaderapi/nf-libloaderapi-setdefaultdlldirectories), [PROCESS_MITIGATION_IMAGE_LOAD_POLICY](https://learn.microsoft.com/en-us/windows/win32/api/winnt/ns-winnt-process_mitigation_image_load_policy), [/DELAYLOAD](https://learn.microsoft.com/en-us/cpp/build/reference/delayload-delay-load-import)
- [Microsoft: OBJECT_ATTRIBUTES / OBJ_DONT_REPARSE](https://learn.microsoft.com/en-us/windows/win32/api/ntdef/ns-ntdef-_object_attributes)
- [Microsoft: ZwCreateKey](https://learn.microsoft.com/en-us/windows-hardware/drivers/ddi/wdm/nf-wdm-zwcreatekey)
- [GitHub Actions 보안 권고](https://docs.github.com/en/actions/reference/security/secure-use)
- [RustSec Advisory Database](https://github.com/rustsec/advisory-db), [Gitleaks](https://github.com/gitleaks/gitleaks)
