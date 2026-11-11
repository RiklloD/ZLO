# ZLO

[English](README.md) | [简体中文](README.zh-CN.md) | [Русский](README.ru.md) | [日本語](README.ja.md) | 한국어

> 이 문서는 [`README.md`](README.md)의 번역입니다. 내용이 다르면 영어 원문이 기준입니다.

제 개인용 AI 데스크톱 앱입니다. macOS용으로 Rust로 만들었고, 네이티브 [Rune](https://github.com/anomalyco/rune) 인터페이스와 [IronClaw](https://github.com/nearai/ironclaw) 백엔드를 씁니다.

저는 [Lorik](https://github.com/RiklloD)입니다. 기술을 만지작거리기 좋아하는 경영학과 학생입니다. 엔지니어링 경험보다 아이디어가 더 많아서, AI 에이전트와 함께 일하며 그 아이디어를 제가 쓸 수 있는 소프트웨어로 만듭니다. ZLO는 그런 작업 방식을 중심으로 만들고 있는 앱입니다.

## 왜 만드는가

저에게는 데스크톱 앱이 그 뒤의 에이전트만큼 중요합니다. 가볍고 Mac에서 자연스럽게 느껴지는 것을 원합니다. 메모리 사용량과 배터리 수명은 제품의 일부이지, 기능을 모두 넣은 뒤에 고칠 일이 아닙니다.

이것은 제 작업 환경이자 아이디어를 시험해 보는 곳입니다. 완성된 릴리스나 모든 기기 지원을 약속하지 않습니다.

## 할 수 있는 일

- **네이티브 인터페이스.** Rune으로 GPU 렌더링하는 Rust입니다. Electron도, 항상 켜져 있는 내장 브라우저도 없습니다. 브라우저와 문서 뷰는 필요할 때만 만들어집니다.
- **채팅, 프로필, 모델.** 작업마다 모델을 고르고, 프로필을 따로 두고, 오케스트레이터를 통해 Codex와 Claude Code를 포함한 다른 에이전트에게 작업을 맡깁니다.
- IronClaw 백엔드를 통한 **도구, 스킬, 메모리, MCP**.
- 제가 포크한 [computer-use 엔진](https://github.com/RiklloD/hermes-plugin-computer-use-plus)을 통한 **컴퓨터 사용**.
- **승인.** 각 프로필은 "먼저 묻기" 또는 "전체 접근" 중 하나입니다. 자동 승인은 기본적으로 꺼져 있습니다.
- **다른 컴퓨터에서 작업.** 백엔드는 Mac mini나 Linux 서버 같은 다른 기기에서 돌리고, 노트북의 ZLO는 창 역할만 할 수 있습니다. 기기들은 Tailscale이나 SSH로 연결됩니다. [다른 컴퓨터 사용하기](tools/ironclaw_rune/docs/remote-computers.md)를 보세요.

앱은 쓰면서 계속 바꾸고 있습니다. 현재 상태는 고정된 기능 목록보다 코드와 개발 노트가 더 정확합니다.

## 동작 방식

ZLO는 두 부분으로 되어 있습니다. **창**(Rune 앱)과 **백엔드**(에이전트와 그 도구, 메모리를 돌리는 IronClaw)입니다. 둘은 인증된 HTTP로 통신합니다. 백엔드를 어디서 돌릴지는 사용자가 정합니다.

| 구성 | 무엇이 어디서 도는가 | 언제 쓰는가 |
| --- | --- | --- |
| **모두 이 Mac에서**(기본값) | ZLO가 자체 백엔드를 자식 프로세스로 시작하고, 종료할 때 멈춥니다. | 기기 한 대, 설정할 것 없음. |
| **다른 컴퓨터의 백엔드** | 백엔드는 서버나 다른 Mac에서 돕니다. ZLO는 tailnet이나 SSH로 연결합니다. | 노트북이 잠자는 동안에도 에이전트를 계속 돌리거나, 성능, 저장소, 연결이 더 나은 기기에서 작업할 때. |
| **앱만**(ZLO Light) | 백엔드가 들어 있지 않은 ZLO 빌드입니다. 항상 다른 컴퓨터에만 연결합니다. | 스스로 에이전트를 돌리면 안 되는 노트북. |

컴퓨터는 **Settings → Computers**에서 바꿀 수 있습니다. ZLO는 한 번에 한 컴퓨터에서 동작합니다. 스레드, 메모리, 설정, 프로바이더 키는 백엔드에 있으므로 컴퓨터마다 따로입니다.

상태는 백엔드를 돌리는 기기의 `~/.zlo`에 저장됩니다(다른 곳에 두려면 `ZLO_HOME`을 설정하세요). 별도의 PostgreSQL 서버가 아니라 libSQL을 씁니다. 예전 설치의 `~/.ironclaw/reborn`은 처음 시작할 때 그곳으로 옮겨지고, 옛 경로는 링크로 남아 그 경로를 기억하는 것이 깨지지 않습니다.

로컬 저장이 모든 작업이 기기 안에 머문다는 뜻은 아닙니다. 원격 모델이나 외부 연동을 쓰면 그 요청에 필요한 데이터가 해당 프로바이더로 전송됩니다. 컴퓨터 사용에는 백엔드가 도는 기기에서 해당 macOS 권한이 필요합니다.

## 설치

Apple Silicon Mac에서 터미널에 다음을 붙여 넣으세요.

```sh
curl -fsSL https://raw.githubusercontent.com/RiklloD/ZLO/main/install.sh | sh
```

ZLO를 소스에서 빌드해 `~/Applications/ZLO.app`에 설치합니다. Xcode 명령줄 도구가 없으면 설치 프로그램을 열고 명령을 다시 실행하라고 안내합니다. Rust가 없으면 설치합니다. 첫 빌드는 10~20분 걸립니다. 업데이트하려면 같은 명령을 다시 실행하세요.

`main`이 아닌 브랜치를 빌드하려면 `ZLO_REF`를 설정하세요.

```sh
curl -fsSL https://raw.githubusercontent.com/RiklloD/ZLO/main/install.sh | ZLO_REF=my-branch sh
```

Apple Development 서명 ID가 없으면 앱이 임시(ad-hoc) 서명되므로, 업데이트할 때마다 macOS가 손쉬운 사용과 화면 기록 권한을 다시 요청합니다.

## 다른 컴퓨터 사용하기

tailnet에 있는 서버를 위한 짧은 버전입니다.

```sh
# 서버에서
zlo onboard            # 프로바이더와 키
zlo service install    # 부팅 시 시작
zlo remote enable      # tailnet에 공개(인터넷에는 절대 공개하지 않음)
zlo service restart
```

그다음 Mac에서 ZLO를 열고 **Settings → Computers**로 가서, "Found on your tailnet" 아래 서버 옆의 **Connect**를 클릭하세요.

[**다른 컴퓨터 사용하기**](tools/ironclaw_rune/docs/remote-computers.md)에서 전체 설정을 다룹니다.

- Linux 서버나 두 번째 Mac 설정
- 원클릭, 코드, 또는 SSH로 페어링
- 페어링된 기기 관리
- `zlo transfer user@host` 또는 `zlo export` / `zlo import`로 설정(스레드, 메모리, 프로필, 설정, 프로바이더 키)을 서버로 옮기기
- 원격에서 되는 기능
- 연결을 보호하는 방식
- 문제 해결

[**기기를 최신으로 유지하기**](tools/ironclaw_rune/docs/updates.md)에서 "Update all machines"와 `zlo update`를 설명합니다.

## 소스에서 빌드하기

이것은 macOS 개발 환경이지 크로스플랫폼 설치 프로그램이 아닙니다. Xcode 명령줄 도구, rustup으로 설치한 Rust, Apple Development 서명 ID가 필요합니다. 저장소는 Rust 툴체인을 [`rust-toolchain.toml`](rust-toolchain.toml)에 고정합니다. 함께 들어 있는 computer-use 엔진은 현재 Apple Silicon을 대상으로 합니다.

```sh
git clone https://github.com/RiklloD/ZLO.git
cd zlo
sh tools/ironclaw_rune/bundle.sh
```

이 스크립트는 백엔드와 데스크톱을 빌드하고, 고정된 computer-use 엔진을 내려받아 검증하고, 번들에 서명한 뒤 `~/Applications/ZLO.app`에 설치합니다. 기존 설치를 교체하며, 앱이 실행 중이면 먼저 종료하고 열려 있었다면 다시 엽니다.

| 명령 | 결과 |
| --- | --- |
| `sh tools/ironclaw_rune/bundle.sh` | 빌드하고 `~/Applications/ZLO.app`에 설치한 뒤 다시 열기 |
| `sh tools/ironclaw_rune/bundle.sh --no-install` | 빌드만, 결과는 `tools/ironclaw_rune/target/ZLO.app` |
| `sh tools/ironclaw_rune/bundle.sh --light` | 백엔드와 computer-use 엔진이 없는 **ZLO Light.app**을 빌드하고 설치합니다. 별도의 앱이라 ZLO를 교체하지 않습니다. |

백엔드만(서버용) 빌드하려면:

```sh
cargo build --release -p ironclaw     # 바이너리는 target/release/zlo
```

데스크톱 워크스페이스를 바꿀 때:

```sh
cd tools/ironclaw_rune
cargo check
cargo clippy --all-targets -- -D warnings
cargo test
```

백엔드 코드를 바꾸기 전에 [`AGENTS.md`](AGENTS.md)와 담당 크레이트의 안내를 읽으세요.

## 저장소 둘러보기

| 경로 | 내용 |
| --- | --- |
| [`tools/ironclaw_rune/`](tools/ironclaw_rune/) | 데스크톱 앱과 그 별도 Cargo 워크스페이스 |
| [`tools/ironclaw_rune/app/`](tools/ironclaw_rune/app/) | Rune 인터페이스, 네이티브 뷰, 환경설정, 백엔드 감독, 컴퓨터 목록(`src/remote*`, `src/pages/computers*`) |
| [`tools/ironclaw_rune/client/`](tools/ironclaw_rune/client/) | 데스크톱용 타입이 있는 HTTP 클라이언트. 페어링, Tailscale 탐색, SSH, 키체인 포함 |
| [`tools/ironclaw_rune/docs/`](tools/ironclaw_rune/docs/) | 사용자 가이드: 다른 컴퓨터, 업데이트 |
| [`crates/`](crates/) | IronClaw 백엔드와 그에 대한 제 확장 |
| [`crates/product/ironclaw_webui/`](crates/product/ironclaw_webui/) | ZLO가 통신하는 HTTP 게이트웨이. 페어링, 기기, Tailscale ID 리스너 포함 |
| [`crates/app/ironclaw_cli/`](crates/app/ironclaw_cli/) | `zlo` 바이너리: `serve`, `remote`, `pair`, `devices`, `update`, `service` |
| [`docs/internal/plans/2026-11-08-zlo-remote-environments.md`](docs/internal/plans/2026-11-08-zlo-remote-environments.md) | 원격 컴퓨터의 설계와 와이어 계약 |
| [`AGENTS.md`](AGENTS.md) | 코딩 에이전트와 함께 일하기 위한 저장소 규칙 |

IronClaw에서 물려받은 React 프론트엔드는 ZLO의 데스크톱 인터페이스가 아닙니다. 그 뒤의 게이트웨이는 여전히 쓰입니다.

입력하는 명령(`zlo`)과 데이터 폴더(`~/.zlo`)는 ZLO의 것입니다. 내부 크레이트는 IronClaw 이름(`ironclaw_*`, 그리고 `zlo`를 빌드하는 Cargo 패키지 `ironclaw`)을 유지하며, `~/.ironclaw/` 아래 CLI 에이전트의 작업 폴더와 일부 오래된 Numoh 서명 식별자도 마찬가지입니다. 이것들을 그대로 두면 업스트림 병합과 macOS 권한을 관리하기가 쉬워집니다.

## 출처

ZLO는 [NEAR AI](https://github.com/nearai)가 만든 [IronClaw](https://github.com/nearai/ironclaw)의 제 개인 포크입니다. 그 기반 위에 데스크톱 앱을 만들었고, 제 용도에 맞게 백엔드도 계속 바꾸고 있습니다. 에이전트 런타임, 보안 장치, 그리고 이 저장소의 많은 부분은 그들의 작업에서 왔습니다.

인터페이스는 [Rune](https://github.com/anomalyco/rune)을 씁니다. 앱의 이전 버전은 Zed를 위해 개발된 Rust UI 프레임워크인 [GPUI](https://www.gpui.rs/)로 만들었고, `archive/gpui` 브랜치(태그 `gpui-final`)에 보관되어 있습니다. computer-use 헬퍼는 [hermes-plugin-computer-use-plus](https://github.com/RiklloD/hermes-plugin-computer-use-plus)에서 왔습니다. ZLO가 다른 컴퓨터에 연결하는 방식은 [T3 Code](https://github.com/pingdotgg/t3code)의 원격 환경을 따르며, 호스팅된 릴레이 대신 Tailscale을 씁니다.

이 저장소는 제 프로젝트이며, IronClaw나 NEAR AI의 공식 릴리스가 아닙니다.

## 라이선스

다음 중 하나를 선택해 사용할 수 있습니다.

- [Apache License, Version 2.0](LICENSE-APACHE)
- [MIT License](LICENSE-MIT)

업스트림 라이선스와 저작자 표시는 유지됩니다. 함께 들어 있는 의존성과 computer-use 헬퍼에는 각자의 라이선스가 있습니다.
