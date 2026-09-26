# Feather Task Manager 2026.9.2 기능 및 수치 해석

0.2는 프로세스 중심의 첫 버전에 성능·시작 앱·서비스 화면을 추가합니다. Rust/Win32 구조를 유지하면서 네이티브 목록, 시스템 글꼴, 직접 그린 성능 그래프를 사용합니다.

0.3은 설정 메뉴에 Windows 작업 관리자 연결·복원을 추가합니다. 포터블 실행은 계속 지원합니다.

2026.9.1은 목록·트리 전환, 확인한 프로세스 트리 전체 종료, 한국어·영어 전환과 서명된 설치 프로그램을 추가합니다. 버전은 `yyyy.m.x` 형식을 사용합니다.

## UI와 디자인 출처

요청한 [nextlevelbuilder/ui-ux-pro-max-skill](https://github.com/nextlevelbuilder/ui-ux-pro-max-skill)의 `SKILL.md`, `pro-rules.md`, `quick-reference.md`와 디자인 검색 결과를 검토했습니다. 모니터링 대시보드·데스크톱 유틸리티 검색을 거쳐 Fluent 2 지침을 적용했습니다. 적용 기준은 소스 저장소의 [design-system/feather-task/MASTER.md](design-system/feather-task/MASTER.md)에 있습니다.

- 어두운 탐색 영역에 프로세스·성능·시작 앱·서비스를 항상 표시합니다.
- 밝은 작업 영역에 제목·설명·검색·현재 상태·주요 동작을 배치합니다.
- 일정한 여백, 시스템 글꼴, 우측 정렬된 숫자 열과 선택 표시를 사용합니다.
- 그래프는 색상에 이름과 현재 값을 함께 표시합니다. 조작은 키보드와 기본 포커스 표시를 지원합니다.
- 장식용 애니메이션·흐림 효과·웹 런타임을 추가하지 않았습니다.

## 프로세스

이름 또는 PID로 검색하고 각 열을 정렬할 수 있습니다. CPU, 작업 집합, 전용 메모리, I/O 속도, 스레드 수, 핸들 수를 표시합니다. 파일 위치 조회와 작업 끝내기는 선택한 프로세스에만 적용됩니다.

보기 선택에서 목록과 프로세스 트리를 전환합니다. 트리는 동일 스냅샷의 부모 PID와 생성 시각으로 구성하며, 부모 PID 재사용·알 수 없는 생성 시각·순환 관계를 유효한 부모 연결로 취급하지 않습니다. 이름 열만 들여쓰고 숫자 열은 동일하게 정렬합니다. 정렬은 부모·자식 관계를 유지하며 같은 부모의 자식 사이와 최상위 항목 사이에 적용합니다. 검색은 일치한 프로세스와 상위 경로를 보여 줍니다.

**트리 끝내기 / Shift+Delete**는 확인 전 캡처한 대상만 자식부터 종료 요청합니다. 접힌 가지의 자식도 포함하며, 이후 생성된 자식은 포함하지 않습니다. 모든 대상의 PID·생성 시각·접근 권한·필수 프로세스 여부를 먼저 확인하고 핸들을 유지합니다. 사전 검사 실패 시 전체 동작을 취소하고, 종료 도중 상태가 바뀌거나 실패하면 부분 결과를 표시합니다. 프로세스 종료는 비동기 요청이며 앱·서비스의 자동 재시작을 막지 않습니다.

| 표시 | 의미와 제한 |
| --- | --- |
| CPU | 지난 수집 이후 CPU 시간 차이를 전체 논리 프로세서 용량 100% 기준으로 정규화합니다. Windows 작업 관리자의 보정 방식과 다를 수 있습니다. |
| 메모리 | 현재 물리 메모리에 상주한 작업 집합입니다. 공유 페이지가 포함되어 프로세스별 합계는 시스템 사용량과 다릅니다. |
| 전용 메모리 | 전용 커밋입니다. 현재 RAM에 상주한 전용 페이지와 같은 수치가 아닙니다. |
| I/O / 초 | 프로세스의 읽기·쓰기·기타 I/O 전송량 합계입니다. 디스크나 네트워크만을 분리한 값이 아닙니다. |
| 첫 표본 | 이전 값이 없는 프로세스의 CPU·I/O는 다음 표본부터 구간 속도를 계산합니다. |

짧게 실행되어 두 번의 수집 사이에 종료된 프로세스는 보이지 않을 수 있습니다. 일시정지 중에는 마지막 수집값을 유지합니다. 재개 직후의 프로세스 CPU·I/O는 정지 기간을 포함한 평균이며 다음 갱신부터 선택한 간격의 평균으로 돌아옵니다.

종료 전에 PID와 생성 시각을 확인하고 동일한 핸들로 종료합니다. 시스템 필수 프로세스, PID 0·4, 자기 자신과 신원을 확인하지 못한 프로세스는 종료하지 않습니다. 접근 거부는 오류로 알립니다.

## 성능

CPU·메모리·디스크·네트워크의 최근 60초 추이를 보여 주며 CPU 모델, 논리 프로세서 수, Windows 가동 시간과 GPU 사용량 요약을 제공합니다. 화면을 열어 수집한 구간부터 그래프가 채워집니다.

| 항목 | 집계 기준 |
| --- | --- |
| CPU·메모리 | 기본 프로세스 수집기의 전체 CPU·물리 메모리 정보 |
| 디스크 읽기·쓰기 | 물리 디스크 전체의 읽기·쓰기 초당 바이트 합계 |
| 디스크 사용률 | 개별 물리 디스크 중 가장 바쁜 디스크의 활성 비율. 디스크별 비율을 더하지 않습니다. |
| 네트워크 수신·송신 | 활성 물리 네트워크 인터페이스의 바이트 증가량 합계. 루프백·VPN·필터 인터페이스는 중복 집계를 피하기 위해 제외합니다. |
| GPU | 같은 물리 GPU 엔진의 프로세스별 사용률을 합친 뒤 가장 바쁜 엔진의 비율을 표시합니다. 모든 GPU의 전체 용량을 합한 비율은 아닙니다. |

디스크·GPU에는 Windows PDH 카운터, 네트워크에는 IP Helper 정보를 사용합니다. 드라이버·Windows 구성에 따라 일부 카운터가 없거나 읽기에 실패할 수 있으며 화면에 경고를 표시합니다. 첫 표본에는 이전 카운터가 없어 속도 계산을 준비하는 시간이 필요합니다. GPU 장치별 상세 정보·전용 메모리·엔진별 그래프는 포함하지 않습니다.

성능 화면을 벗어나면 추가 성능 수집을 멈추고, 최소화하거나 일시정지해도 주기적인 수집을 중단합니다. 프로세스 I/O와 성능 화면의 디스크·네트워크 값은 집계 대상이 달라 직접 일치하지 않습니다.

## 시작 앱

현재 사용자와 모든 사용자의 `Run` 레지스트리를 32/64비트 보기에서 조회하며, 현재 사용자와 공용 시작프로그램 폴더도 표시합니다. 공유되는 HKCU Run 항목은 중복 표시하지 않습니다. 레지스트리 항목의 명령을 그대로 보여 주고, 폴더 항목은 원본 파일 경로를 보여 줍니다. 바로가기 대상을 실행하지 않습니다.

사용·사용 안 함 전환은 Windows Explorer의 해당 `StartupApproved` 값만 변경합니다. 원래 Run 값의 형식·내용이나 시작프로그램 파일은 수정·이동·삭제하지 않습니다. 변경 직전에 원본 값, 이전 승인 데이터, 파일 식별정보를 다시 확인하며 변경된 항목은 새로고침을 요청합니다.

`StartupApproved`는 Microsoft가 보장하는 공개 설정 API가 아닙니다. [Task Manager의 상태 저장 형식을 직접 조사한 자료](https://frendguo.com/how-to-disable-or-enable-startup-app-in-taskmgr/)에서 확인된 DWORD 상태와 FILETIME 형식을 사용합니다. 확인된 사용/사용 안 함 상태 2/3만 변경하며 추가 바이트는 보존합니다. 상태가 없으면 기본 사용 상태로 취급하고 전환 시 표준 12바이트 값을 만듭니다. 알 수 없는 상태, 잘못된 데이터, 재분석 지점 파일은 읽기 전용입니다. 다른 프로그램이 동시에 변경하면 재검사와 저장 후 확인으로 오류를 알리지만 Windows 레지스트리가 원자적인 비교 후 쓰기 기능을 제공하는 것은 아닙니다.

Store 앱의 StartupTask, 예약 작업, RunOnce, 정책 자동실행, 부팅 시작 영향도는 이 화면의 범위에 포함하지 않습니다. 모든 사용자 항목을 변경할 때는 Windows 권한이 필요합니다. 항목 변경을 실패했을 때 사용자 권한을 자동으로 우회하지 않습니다.

## 서비스

Windows Service Control Manager에서 실행 중·중지된 Win32 서비스를 조회합니다. 서비스 이름·표시 이름·현재 상태·PID·시작 유형을 표시하며 검색과 정렬을 지원합니다. 커널 드라이버 목록은 포함하지 않습니다.

서비스 화면은 진입 시 조회하고 일시정지하지 않은 동안 5초마다 상태를 갱신합니다. 시작 유형은 조회 비용을 줄이기 위해 최대 60초 캐시하므로 외부에서 바꾼 설정이 즉시 반영되지 않을 수 있습니다. 설정 조회 권한이 없어도 서비스 자체를 숨기지 않고 시작 유형을 확인 불가로 표시합니다.

**시작**과 **중지**는 선택한 서비스에 요청을 전달합니다. 요청 성공이 곧 시작·중지 완료를 의미하지 않으며 다음 조회에서 시작 중·중지 중·실행 중·중지됨 상태를 확인합니다. 중지는 확인 대화상자를 거칩니다. Windows가 허용하지 않거나 의존 서비스가 실행 중이면 오류를 표시합니다.

서비스 시작 유형·구성은 변경하지 않습니다. 의존 서비스를 자동으로 중지하거나, 서비스 프로세스를 강제로 종료하거나, 비활성화된 서비스를 임의로 활성화하지 않습니다.

## Windows 작업 관리자 연결과 복원

**설정 → Feather를 작업 관리자로 설정…**은 Setup으로 먼저 설치한 보호된 실행 파일을 Windows 작업 관리자로 연결합니다. `Program Files\Feather Task Manager\FeatherTaskManager.exe` 및 상위 경로의 소유자·쓰기 권한·재분석 지점을 검증하고 경로 핸들을 유지한 채 설치된 도우미를 관리자 권한으로 실행합니다. 포터블 파일을 관리자 권한으로 복사하는 기능은 2026.9.2에서 제거했습니다. 검증 후 `HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\taskmgr.exe`의 `Debugger`에 큰따옴표로 감싼 설치 경로와 `--task-manager`를 등록하며 이 PC의 모든 사용자에게 적용합니다. 설치 파일이 없거나 신뢰할 수 없으면 연결을 변경하지 않습니다.

다음 `taskmgr.exe` 실행부터 Feather가 열리므로 `Ctrl+Alt+Delete → 작업 관리자`와 `Ctrl+Shift+Esc`에도 적용됩니다. 기존 창은 유지되며 재부팅은 필요하지 않습니다. 보안 화면의 실제 실행 경로는 수동 확인 대상으로 남아 있습니다. 조직의 작업 관리자 차단 정책은 변경하지 않습니다.

다른 프로그램의 `Debugger`, 잘못된 형식의 값, 지원하지 않는 IFEO 필터 설정은 덮어쓰지 않습니다. Windows 10/11에서는 IFEO 키가 32·64비트 레지스트리 보기 사이에서 공유되므로 별도의 `Wow6432Node` 연결을 만들지 않습니다.

**설정 → Windows 기본 작업 관리자로 복원…**은 Feather가 등록하는 정확한 명령인지 확인한 뒤 `Debugger` 값만 제거합니다. IFEO 키 전체나 다른 값은 삭제하지 않습니다. 복원 후 파일은 남아 있으며 수동으로 삭제할 수 있습니다. 연결된 실행 파일을 먼저 이동하거나 삭제하면 작업 관리자를 실행할 수 없으므로 삭제 전에 복원하세요.

실행 파일이 남아 있으면 관리자 권한으로 `FeatherTaskManager.exe --restore-task-manager`를 실행할 수 있습니다. 파일을 삭제한 경우에는 배포 ZIP의 `Restore-WindowsTaskManager.ps1`을 관리자 권한의 64비트 PowerShell에서 실행합니다. 이 스크립트도 정확한 Feather 명령과 형식이 일치할 때만 `Debugger`를 제거합니다. 복구 절차는 [설치 안내](INSTALLER.md)를 참고하세요. 외부 관리자 도구와 동시에 같은 레지스트리 설정을 변경하지 마세요.

## 검증 범위

- 시작 앱: 상태 해석·알 수 없는 상태 거부·추가 바이트 보존·명령 원본 보존·파일 식별정보·오래된 항목 거부를 검증합니다. 실제 변경은 `HKCU\Software\FeatherTask\Tests` 아래 고유 임시 키와 임시 폴더에서만 수행합니다.
- 서비스: 실환경 목록 조회, 상태 표시, 이름 검사, 네이티브 버퍼 경계를 검증합니다. 사용자 서비스를 시작·중지하는 테스트는 실행하지 않습니다.
- 성능: 집계·초당 속도·카운터 재설정·옵션 카운터 처리와 읽기 전용 실환경 수집을 검증합니다.
- 프로세스: 직접 만든 임시 자식 프로세스를 사용해 신원 검사와 종료를 검증합니다.

현재 버전의 측정 조건과 결과는 `BENCHMARK.md`에 기록합니다. 이전 배포 결과는 `dist/archive`에 별도 보관합니다. 수치는 현재 실행 파일의 SHA-256과 측정 조건이 일치하는 결과로 확인하세요.

## 참고 문서

- [Microsoft: Run 및 RunOnce 레지스트리](https://learn.microsoft.com/en-us/windows/win32/setupapi/run-and-runonce-registry-keys)
- [Microsoft: EnumServicesStatusExW](https://learn.microsoft.com/en-us/windows/win32/api/winsvc/nf-winsvc-enumservicesstatusexw)
- [Microsoft: StartServiceW](https://learn.microsoft.com/en-us/windows/win32/api/winsvc/nf-winsvc-startservicew), [ControlService](https://learn.microsoft.com/en-us/windows/win32/api/winsvc/nf-winsvc-controlservice)
- [Microsoft: PDH 성능 데이터 수집](https://learn.microsoft.com/en-us/windows/win32/perfctrs/using-the-pdh-functions-to-consume-counter-data)
- [Microsoft: GetIfTable2](https://learn.microsoft.com/en-us/windows/win32/api/netioapi/nf-netioapi-getiftable2)
- [Microsoft: NtQuerySystemInformation](https://learn.microsoft.com/en-us/windows/win32/api/winternl/nf-winternl-ntquerysysteminformation)
- [Microsoft: 프로그램별 IFEO 설정 위치](https://learn.microsoft.com/en-us/windows-hardware/drivers/debugger/gflags-details), [설정 적용 시점](https://learn.microsoft.com/en-us/windows-hardware/drivers/debugger/gflags-overview)
- [Microsoft: WOW64에서 공유되는 레지스트리 키](https://learn.microsoft.com/en-us/windows/win32/winprog64/shared-registry-keys)
- [Microsoft: Windows 단축키](https://support.microsoft.com/en-us/windows/keyboard-shortcuts-in-windows-dcc61a57-8ff0-cffe-9796-cb9706c75eec), [작업 관리자 차단 정책](https://learn.microsoft.com/en-us/windows/client-management/mdm/policy-csp-admx-ctrlaltdel#disabletaskmgr)

프로세스 수집은 NT API와 x64 구조에 의존하며 향후 Windows 변경에 대응이 필요할 수 있습니다. 배포 대상은 Windows 10 1607 이상 및 Windows 11 x64이며 x86·ARM64 네이티브 빌드와 모든 Windows 빌드의 호환성을 보장하지 않습니다.
