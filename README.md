<p align="center">
  <img src="assets/app.png" width="112" height="112" alt="Feather Task Manager">
</p>

# Feather Task Manager

가볍고 빠른 Windows 작업 관리자. Rust와 Win32로 만든 네이티브 앱입니다.

[English](README.en.md) · [다운로드](https://github.com/Hardt-LLC/FeatherTaskManager/releases/latest)

## 장점

- **가벼운 실행** — 브라우저 엔진 없이 동작하며, 별도 런타임 설치가 필요 없습니다.
- **편리한 프로세스 관리** — 목록·트리 전환, 검색·정렬, 개별 프로세스 및 트리 전체 종료를 지원합니다.
- **시스템 상태를 한눈에** — CPU·메모리·디스크·네트워크 성능 확인부터 시작 앱과 서비스 관리까지 제공합니다.
- **한국어·영어 지원** — 설정에서 즉시 전환하고, 주요 기능을 키보드로 사용할 수 있습니다.
- **서명된 배포 파일** — 실행 파일과 설치 프로그램에 Azure Artifact Signing의 HARDT 서명을 적용했습니다.

## 설치 및 사용법

**Windows 10 1607 이상 / Windows 11 x64**에서 사용할 수 있습니다. [최신 릴리스](https://github.com/Hardt-LLC/FeatherTaskManager/releases/latest)에서 원하는 파일을 받으세요.

| 파일 | 용도 |
| --- | --- |
| `Setup-x64.exe` | 설치 후 시작 메뉴에서 실행 |
| `Portable-x64.exe` | 설치 없이 바로 실행 |
| `Portable-x64.zip` | 포터블 앱, 설명서, 복구 스크립트 묶음 |

- **프로세스** 화면에서 목록·트리 보기를 선택하고, 작업을 선택해 개별 또는 트리 전체를 종료합니다.
- **성능 · 시작 앱 · 서비스** 탭에서 시스템 상태와 실행 항목을 관리합니다.
- **설정 → 한국어 / English**에서 언어를 변경합니다.

| 단축키 | 동작 |
| --- | --- |
| `Ctrl+1…4` | 화면 이동 |
| `Ctrl+F` | 검색 |
| `F5` | 새로고침 |
| `Shift+Delete` | 선택한 프로세스 트리 종료 확인 |

**Windows 작업 관리자 대체:** Setup으로 먼저 설치한 뒤, 설정에서 **Feather를 작업 관리자로 설정…**을 선택하고 UAC를 승인하세요. 이후 `Ctrl+Shift+Esc`나 `Ctrl+Alt+Delete → 작업 관리자`로 Feather를 실행할 수 있습니다. 원복은 **Windows 기본 작업 관리자로 복원…**에서 합니다. 설치된 파일을 수동 삭제하기 전에는 먼저 연결을 복원하세요.

업데이트·제거 전에는 Feather 창을 모두 닫으세요. 자세한 내용은 [설치 안내](INSTALLER.md)와 [기능 안내](FEATURES-v2.md)를 참고하세요.

## 소스에서 빌드

Windows에서 Rust의 `x86_64-pc-windows-msvc` 도구 모음, Visual Studio C++ Build Tools, Windows SDK가 필요합니다.

```powershell
git clone https://github.com/Hardt-LLC/FeatherTaskManager.git
cd FeatherTaskManager
cargo build --release --locked
```

실행 파일은 `target\release\FeatherTaskManager.exe`에 생성됩니다. 설치 파일 제작과 코드 서명은 [배포 안내](SIGNING.md)를 참고하세요.

## 라이선스

[MIT License](LICENSE). 사용한 라이브러리의 고지는 [THIRD_PARTY_NOTICES.txt](THIRD_PARTY_NOTICES.txt)에 있습니다.
