# Feather Task Manager 검증 기록

## 2026.9.4

2026-09-27, Windows 11 x64(빌드 26200).

- `cargo test --locked`: **305개 통과, 3개 제외, 실패 없음**. `cargo fmt --all -- --check`, `cargo clippy --locked --all-targets -- -D warnings`, `cargo build --release --locked`, 배포 스크립트 검사(정상 2개 허용·잘못된 상태 6개 거부 ×2): 통과.
- 새 검사: 모든 화면에서 성능 기록이 끊기지 않고 최소화는 여전히 끊김으로 남는지, 실제 모니터 스레드가 화면 전환 중에도 성능 샘플러를 유지하는지, 관리자 자동 실행 판단(설정·권한·표시 인수·진단 모드·인수 허용 목록)과 포터블 사본을 대상으로 삼지 않는지, 창마다 바꾼 설정만 저장하는지, 제거용 환경설정 삭제가 하위 키를 지우고 레지스트리 링크는 링크만 지우며 대상 키를 그대로 두는지(일반 `RegDeleteTreeW`는 대상을 지워 이 검사에 실패함을 확인).
- 앱·설치 프로그램은 서명 `Valid`, 게시자 `CN=HARDT, O=HARDT, L=Casper, S=Wyoming, C=US`, 타임스탬프, 버전 `2026.9.4.0`, 회사명 `HARDT`를 확인했습니다([signatures-2026.9.4.json](measurements/signatures-2026.9.4.json)). 서명된 포터블 앱의 [자체 검사](measurements/self-test-2026.9.4.txt)는 `PASS`입니다.
- 실제 UAC 승인·거절, 설치된 사본의 관리자 자동 실행, 관리자 권한 창의 트레이 동작, 실제 제거 시 환경설정 삭제는 이 환경에서 실행하지 않았으며 별도 VM 확인이 필요합니다.

## 2026.9.3

2026-09-27, Windows 11 x64(빌드 26200)에서 새 디자인, 하드웨어 정보, Nuclear Zombie와 보안 수정의 회귀 검사를 수행했습니다. 보안 검토 결과와 수정 내용은 [SECURITY.md](SECURITY.md)에 정리했습니다.

## 자동 검사

- `cargo test --locked`: **290개 통과, 3개 제외, 실패 없음**.
- `cargo fmt --all -- --check`, `cargo clippy --locked --all-targets -- -D warnings`, `cargo build --release --locked`: 통과.
- UI: 제목 표시줄 적중 판정·최대화·DPI 크기, 커스텀 표의 스크롤·키보드·선택 유지·트리/앱 그룹, 선택 상자·메뉴·확인 창의 키보드와 포커스 복원, 애니메이션 타이머가 멈춘 상태에서 남지 않는지, 반복 그리기·테마·DPI·언어 전환의 GDI/USER 개체 누수(자식 프로세스에서 격리 실행)를 검사합니다.
- 하드웨어·메모리: SMBIOS·스토리지·D3DKMT 파서의 경계 검사, 프로세스별 GPU의 PID 재사용 방지, ETW 네트워크 이벤트 파싱·손실 감지·제공자 확인, 핸들 표 파싱, 권한 복원, 도우미 인수 형식, 진단 출력의 링크 거부(정션·하드 링크·심볼릭 링크)를 검사합니다.
- 배포 스크립트: 로컬 소스·태그 검사와 원격 태그 검사가 각각 정상 2개를 허용하고 잘못된 상태 6개를 거부했습니다.

제외된 3개는 부모 테스트가 따로 실행하는 자식 진입점, 실환경 성능 공급자 검사, 실제 ETW 세션 수명 검사입니다. 성능 공급자는 최종 배포 앱의 `--self-test`로 확인합니다.

## 배포 검사

- 앱·설치·제거 프로그램은 Azure Artifact Signing으로 서명했고, 배포 EXE 2개 모두 `Valid`, 게시자 `CN=HARDT, O=HARDT, L=Casper, S=Wyoming, C=US`, 타임스탬프와 버전 `2026.9.3.0`을 확인했습니다. 서명·해시는 [signatures-2026.9.3.json](measurements/signatures-2026.9.3.json), 배포의 `SHA256SUMS.txt`와 `release-manifest.json`에 있습니다.
- PE 로더 정책: 정적 DLL 검색 `0x0800`, ASLR·DEP·고엔트로피 VA를 확인했습니다. 정적 가져오기 22개는 모두 System32 또는 WinSxS에 있습니다.
- 실행 중 DLL 검색(FTM-2026-05): 코드가 없는 올바른 DLL(`opengl32.dll`, `umpdc.dll`, `wmiclnt.dll`)을 EXE 사본 옆에 두었을 때 이전 빌드는 `umpdc.dll`을 EXE 폴더에서 불러왔고, 수정한 빌드는 `--self-test`, `--dump-hardware`, `--memory-cleanup-dry-run`, `--test-child`에서 EXE 폴더의 DLL을 하나도 불러오지 않았습니다.
- 공개 전 검사: Git 이력 전체와 커밋할 파일에서 비밀정보·개인 경로·기기 식별자를 찾지 못했습니다. README 미리보기는 [디자인 프로토타입](design/reference/feather-task-manager.html)의 시뮬레이션 데이터로 렌더링했고 이미지 메타데이터가 없습니다.

서명된 최종 포터블 앱의 [자체 검사](measurements/self-test-2026.9.3.txt)는 `PASS`입니다. 프로세스 554개·서비스 337개·시작 앱 34개를 조회했고, GPU 모델·온도, SMBIOS 메모리, 디스크 모델, 프로세스별 GPU를 읽었습니다. 프로세스별 네트워크는 일반 권한에서 `Requires administrator`로 정직하게 측정 불가를 보고했습니다. 최초 프로세스 수집 5.274ms, 반복 수집 중앙값 8.087ms이며 앱 전체 시작 시간을 뜻하지 않습니다.

## 실제 환경과 한계

- 관리자 권한 경로는 이 검증 환경에서 권한을 올릴 수 없어 실제로 실행하지 않았습니다: 프로세스별 네트워크(ETW), 메모리 목록 비우기(설치된 도우미·UAC), 관리자 권한 Feather의 좀비 전체 검사.
- 실제 관리자 설치·업데이트·제거, UAC 전체 흐름, `Ctrl+Alt+Delete` 보안 화면 실행, Windows 10 1607과 다른 물리 환경은 별도 확인이 필요합니다. Windows 10용 창 테두리 경로는 단위 테스트만 거쳤습니다.
- 기본 DLL 디렉터리를 System32로 제한했으므로, 셸 대화 상자가 불러오는 제3자 확장의 호환성은 수동으로 확인해야 합니다.
- UI 미리보기는 실제 사용자 프로세스를 담기 때문에 Git/Release에 올리지 않고 로컬 `dist/`에만 둡니다.

성능 기록은 [BENCHMARK.md](BENCHMARK.md)의 버전·SHA-256·측정 조건을 함께 확인하세요. 숨김 창 측정은 첫 화면 완성, 보이는 UI 조작, 장시간 누수 또는 극단적 자원 부족 상태를 대표하지 않습니다.
