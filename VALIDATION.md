# Feather Task Manager 0.3.0 검증 기록

2026-09-26, Windows 11 x64에서 Windows 작업 관리자 연결·복원 기능과 기존 기능을 검증했습니다.

## 자동 검사

- `cargo test --offline --locked`: **49개 통과, 2개 명시적 제외**.
- `cargo fmt --all -- --check`, `cargo clippy --offline --locked --all-targets -- -D warnings`: 통과.
- 전용 임시 HKCU 키에서 등록·복원, 반복 실행, 다른 프로그램의 명령 보존, 잘못된 형식 거부, IFEO 필터·하위 키 보존을 검증했습니다. 실제 `taskmgr.exe`의 IFEO 키는 변경하지 않았습니다.
- 설치 경로의 큰따옴표 처리·재귀 실행 방지, 파일과 폴더의 소유자·쓰기 권한 검사, 일반 사용자에게 쓰기를 허용하는 상속 규칙 거부를 검증했습니다. 실제 Program Files의 기존 권한은 읽기만 했습니다.
- 설정 메뉴의 등록·복원 활성화 상태와 IFEO가 전달하는 원래 작업 관리자 인수의 분리를 검증했습니다. 기존 네이티브 UI·가상 목록·아이콘 DPI·프로세스·시작 앱·서비스 검사도 통과했습니다.
- PowerShell 복구 스크립트의 값 검사를 임시 HKCU 키의 8가지 정상·비정상 값으로 확인했습니다. Windows PowerShell 5.1 구문 검사와 네이티브 API 선언 컴파일도 통과했습니다. 실제 복구 스크립트의 HKLM 변경 부분은 실행하지 않았습니다.

기본 제외된 검사 두 개는 실제 성능 공급자 조회와 관리자 권한이 필요한 파일 설치 통합 검사입니다. 성능 공급자는 아래 릴리스 자체 검사에서 별도로 확인했습니다. 파일 설치 통합 검사는 현재 셸이 관리자 토큰이 아니어서 실행하지 않았습니다.

관리자 PowerShell에서 아래 검사를 실행하면 작업 폴더의 `target/test-install-*` 아래에 만든 임시 설치 위치에서 파일 복사·권한·재설치·업데이트를 검증하고 생성한 파일과 폴더만 정리합니다. 이 검사도 실제 Program Files와 작업 관리자 연결을 변경하지 않습니다.

```powershell
cargo test --offline replacement::tests::disposable_installation_copies_secures_and_updates_the_binary -- --exact --ignored --nocapture
```

## 릴리스 실행 파일

`FeatherTaskManager.exe --self-test`가 통과했습니다. 실제 프로세스 554개, 서비스 337개, 시작 앱 17개를 조회했습니다. 직접 만든 임시 자식 프로세스 종료·오래된 PID 거부·자기 자신과 System 보호·메모리 범위·네이티브 성능 카운터를 확인했습니다. 사용자 서비스와 자동실행 설정은 변경하지 않았습니다.

첫 프로세스 스냅샷 11.698ms, 이후 수집 중앙값 8.993ms·최대 10.165ms였습니다. 이는 해당 시점의 수집 시간이며 전체 앱 시작 시간이나 다른 PC의 보장값은 아닙니다. 디스크·네트워크가 준비됐고 GPU 값이 제공됐으며 카운터 경고는 없었습니다. [검사 원본](measurements/self-test-v0.3.0.txt)

파일 크기는 **726,016 bytes / 709 KiB**, PE 제품 이름은 `Feather Task Manager`, 파일 버전은 `0.3.0.0`입니다. 최종 해시와 측정 조건은 [BENCHMARK.md](BENCHMARK.md)에 있습니다.

## 수동 확인 범위

이 PC에 실제 대체 연결을 적용하지 않았습니다. UAC 승인·취소 화면, 실제 Program Files 설치, Ctrl+Alt+Delete 보안 화면과 Ctrl+Shift+Esc를 통한 실행, 실제 HKLM 연결의 적용·복원은 별도 수동 확인이 필요합니다. 자동 검사의 임시 레지스트리 결과를 이 전체 경로의 실측으로 간주하지 않습니다.

설정 후에는 새로 작업 관리자를 실행해 Feather가 열리는지 확인하고, **설정 → Windows 기본 작업 관리자로 복원…** 후 Windows 작업 관리자가 다시 열리는지 확인하세요. 복구 방법은 [README.md](README.md)에 있습니다.

실제 사용자 서비스 제어·시작 앱 설정 변경·모든 Windows 빌드·여러 물리 모니터의 DPI 이동·장시간 누수·극단적 메모리 부족 상태는 별도 환경 검증 대상입니다. 숨김 창 벤치마크는 보이는 창의 조작 비용을 대표하지 않습니다. 이전 버전의 기록은 해당 버전의 파일·해시를 기준으로 보세요.
