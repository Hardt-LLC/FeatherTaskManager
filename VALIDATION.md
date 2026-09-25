# Feather Task Manager 0.2.1 검증 기록

2026-09-26, Windows 11 x64에서 새 로고·탭 아이콘·제품 이름을 적용한 릴리스를 검증했습니다.

## 이번 변경 검증

- `cargo test --offline --locked ui::`: 네이티브 UI·캡처 테스트 **15개 통과**.
- 새 아이콘 테스트는 실제 숨김 Windows 창의 제목이 `Feather Task Manager`인지 확인하고, 100%·125%·150%·200% DPI에서 큰 아이콘과 작은 아이콘의 실제 비트맵 크기를 검사합니다.
- `cargo fmt --all -- --check`, `cargo clippy --offline --locked --all-targets -- -D warnings`: 통과.
- 릴리스 PE 메타데이터: ProductName·FileDescription 모두 `Feather Task Manager`, OriginalFilename `FeatherTaskManager.exe`, FileVersion `0.2.1.0`.
- Windows ICO는 16·20·24·32·40·48·64·128·256px 이미지를 포함합니다. 창 아이콘은 DPI 변경 시 다시 로드합니다.
- 앱 자신의 실제 네이티브 컨트롤을 렌더한 네 화면과 최소 크기·150% DPI 시뮬레이션을 확인했습니다. 미리보기는 `dist/previews-v0.2.1`에 있습니다. 다른 앱이나 바탕화면을 캡처하지 않았습니다.
- 탭 아이콘은 정사각형 SVG 원본과 배율별 커버리지 마스크를 사용합니다. 알파·종횡비·GDI 객체 해제·24개 캐시 상한을 검토했습니다. 별도 SVG 엔진이나 이미지 라이브러리를 실행 파일에 추가하지 않았습니다.

## 최종 실행 파일

`FeatherTaskManager.exe --self-test`가 통과했습니다. 실제 프로세스 557개, 서비스 337개, 시작 앱 17개를 조회했습니다. 직접 만든 임시 자식 프로세스 종료·오래된 PID 거부·자기 자신과 System 보호·메모리 범위·네이티브 성능 카운터를 확인했습니다. 사용자 서비스나 자동실행 설정은 변경하지 않았습니다.

첫 프로세스 스냅샷 13.139ms, 이후 수집 중앙값 9.821ms·최대 12.972ms였습니다. 이는 해당 시점의 수집 시간이며 전체 앱 시작 시간이나 다른 PC의 보장값은 아닙니다. 디스크·네트워크가 준비됐고 GPU 값이 제공됐으며 카운터 경고는 없었습니다. [실제 검사 결과](measurements/self-test-v0.2.1.txt)

파일 크기는 **691,712 bytes / 675.5 KiB**입니다. 현재 해시와 측정 조건은 [BENCHMARK.md](BENCHMARK.md)에 있습니다. 이전 0.2에서는 전체 40개 테스트를 통과했으며 이번 변경은 수집·시작 앱·서비스 백엔드를 수정하지 않았습니다.

## 검증 범위

고배율 렌더링은 DPI 메시지 시뮬레이션이며 여러 물리 모니터 사이 이동 검증은 아닙니다. 실제 사용자 서비스 제어·시작 앱 설정 변경·관리자 UAC·모든 Windows 빌드·장시간 누수·극단적 메모리 부족 상태는 별도 환경 검증이 필요합니다. 숨김 창 벤치마크는 보이는 창을 조작하는 비용을 대표하지 않습니다.

디자인 원본과 재생성 방법은 [디자인 기준](design-system/feather-task/MASTER.md)에 기록했습니다. SVG 원본은 `assets/source`, Windows 아이콘은 `assets/app.ico`에 있습니다.
