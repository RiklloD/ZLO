# ZLO

[English](README.md) | 简体中文 | [Русский](README.ru.md) | [日本語](README.ja.md) | [한국어](README.ko.md)

> 本文是 [`README.md`](README.md) 的翻译。如有出入，以英文版为准。

我的个人 AI 桌面应用。用 Rust 为 macOS 构建，采用原生的 [Rune](https://github.com/anomalyco/rune) 界面和 [IronClaw](https://github.com/nearai/ironclaw) 后端。

我是 [Lorik](https://github.com/RiklloD)，一名喜欢折腾技术的商科学生。我的想法比工程经验多，所以我和 AI 智能体一起工作，把这些想法变成自己能用的软件。ZLO 就是我围绕这种工作方式构建的应用。

## 为什么做它

对我来说，桌面应用和它背后的智能体一样重要。我想要一个轻量、在 Mac 上用起来很自然的东西。内存占用和电池续航是产品的一部分，而不是在加完所有功能之后才去修的问题。

这是我自己的工作环境，也是尝试想法的地方。它不承诺成为打磨好的正式版本，也不承诺支持所有机器。

## 它能做什么

- **原生界面。** 基于 Rune 的 GPU 渲染 Rust 界面。没有 Electron，也没有常驻的内嵌浏览器；浏览器和文档视图只在需要时才创建。
- **对话、配置文件和模型。** 按任务选择模型，保留不同的配置文件，并通过编排器把工作委派给其他智能体，包括 Codex 和 Claude Code。
- 通过 IronClaw 后端提供的**工具、技能、记忆和 MCP**。
- 通过我 fork 的 [computer-use 引擎](https://github.com/RiklloD/hermes-plugin-computer-use-plus)实现的**电脑操控**。
- **审批。** 每个配置文件要么先询问，要么给予完全访问权限。自动审批默认关闭。
- **在其他电脑上工作。** 后端可以运行在另一台机器上，比如 Mac mini 或 Linux 服务器，而笔记本上的 ZLO 只是一个窗口。机器之间通过 Tailscale 或 SSH 连接。参见[使用另一台电脑](tools/ironclaw_rune/docs/remote-computers.md)。

我在使用中不断修改这个应用。要了解它的当前状态，代码和开发笔记比一份固定的功能清单更可靠。

## 运行方式

ZLO 分为两部分：**窗口**（Rune 应用）和**后端**（IronClaw，负责运行智能体、它的工具和记忆）。两者通过经过认证的 HTTP 通信。后端在哪里运行由你决定：

| 配置 | 什么在哪里运行 | 适用场景 |
| --- | --- | --- |
| **全部在这台 Mac 上**（默认） | ZLO 把自己的后端作为子进程启动，退出时将其停止。 | 一台机器，无需任何设置。 |
| **后端在另一台电脑上** | 后端运行在服务器或另一台 Mac 上。ZLO 通过你的 tailnet 或 SSH 连接它。 | 让智能体在笔记本休眠时继续运行，或在性能更强、有你的代码仓库或网络更好的机器上工作。 |
| **仅应用**（ZLO Light） | 不内置后端的 ZLO 版本。它只会连接到其他电脑。 | 不应自己运行智能体的笔记本。 |

你可以在 **Settings → Computers** 中切换电脑。ZLO 一次只在一台电脑上工作。每台电脑有自己的线程、记忆、设置和提供商密钥，因为它们都保存在该电脑的后端里。

状态保存在运行后端的那台机器的 `~/.zlo` 下（设置 `ZLO_HOME` 可放到别处），使用 libSQL，而不是单独的 PostgreSQL 服务器。旧安装的 `~/.ironclaw/reborn` 会在首次启动时迁移过去，旧路径保留为链接，所以记着旧路径的东西不会出问题。

本地存储并不意味着每个任务都留在本机。使用远程模型或外部集成时，请求所需的数据会发送给相应的提供商。电脑操控需要在运行后端的机器上授予相关的 macOS 权限。

## 安装

在 Apple Silicon Mac 上，把下面这行粘贴到终端：

```sh
curl -fsSL https://raw.githubusercontent.com/RiklloD/ZLO/main/install.sh | sh
```

它会从源码构建 ZLO 并安装到 `~/Applications/ZLO.app`。如果缺少 Xcode 命令行工具，它会打开安装程序并请你重新运行命令。如果没有 Rust，它会安装 Rust。首次构建需要 10 到 20 分钟。再次运行同一命令即可更新。

要构建 `main` 以外的分支，设置 `ZLO_REF`：

```sh
curl -fsSL https://raw.githubusercontent.com/RiklloD/ZLO/main/install.sh | ZLO_REF=my-branch sh
```

如果没有 Apple Development 签名身份，应用会使用临时（ad-hoc）签名，因此每次更新后 macOS 都会再次请求辅助功能和屏幕录制权限。

## 使用另一台电脑

以 tailnet 上的服务器为例，简要步骤如下：

```sh
# 在服务器上
zlo onboard            # 提供商和密钥
zlo service install    # 开机启动
zlo remote enable      # 发布到你的 tailnet（绝不发布到互联网）
zlo service restart
```

然后在 Mac 上打开 ZLO，进入 **Settings → Computers**，在 “Found on your tailnet” 下点击服务器旁边的 **Connect**。

[**使用另一台电脑**](tools/ironclaw_rune/docs/remote-computers.md)涵盖完整的设置：

- 设置 Linux 服务器或第二台 Mac；
- 一键配对、用配对码配对或通过 SSH 配对；
- 管理已配对的设备；
- 用 `zlo transfer user@host`，或 `zlo export` / `zlo import`，把你的环境（线程、记忆、配置文件、设置、提供商密钥）迁移到服务器；
- 哪些功能可以远程使用；
- 连接如何受到保护；
- 故障排除。

[**保持机器更新**](tools/ironclaw_rune/docs/updates.md)介绍了 “Update all machines” 和 `zlo update`。

## 从源码构建

这是 macOS 开发环境，不是跨平台安装程序。它需要 Xcode 命令行工具、通过 rustup 安装的 Rust，以及 Apple Development 签名身份。仓库在 [`rust-toolchain.toml`](rust-toolchain.toml) 中固定了 Rust 工具链版本。内置的 computer-use 引擎目前面向 Apple Silicon。

```sh
git clone https://github.com/RiklloD/ZLO.git
cd zlo
sh tools/ironclaw_rune/bundle.sh
```

该脚本会构建后端和桌面应用，下载并校验固定版本的 computer-use 引擎，为应用包签名，并安装到 `~/Applications/ZLO.app`。它会替换已有的安装：如果应用正在运行，会先退出它；如果之前是打开的，会重新打开。

| 命令 | 结果 |
| --- | --- |
| `sh tools/ironclaw_rune/bundle.sh` | 构建，安装到 `~/Applications/ZLO.app`，重新打开 |
| `sh tools/ironclaw_rune/bundle.sh --no-install` | 只构建，输出到 `tools/ironclaw_rune/target/ZLO.app` |
| `sh tools/ironclaw_rune/bundle.sh --light` | 构建并安装 **ZLO Light.app**，它没有后端，也没有 computer-use 引擎。它是一个独立的应用，所以不会替换 ZLO。 |

单独构建后端（用于服务器）：

```sh
cargo build --release -p ironclaw     # 二进制文件位于 target/release/zlo
```

修改桌面工作区时：

```sh
cd tools/ironclaw_rune
cargo check
cargo clippy --all-targets -- -D warnings
cargo test
```

修改后端代码前，请先阅读 [`AGENTS.md`](AGENTS.md) 和所属 crate 的说明。

## 仓库导览

| 路径 | 内容 |
| --- | --- |
| [`tools/ironclaw_rune/`](tools/ironclaw_rune/) | 桌面应用及其独立的 Cargo 工作区 |
| [`tools/ironclaw_rune/app/`](tools/ironclaw_rune/app/) | Rune 界面、原生视图、偏好设置、后端监管，以及电脑列表（`src/remote*`、`src/pages/computers*`） |
| [`tools/ironclaw_rune/client/`](tools/ironclaw_rune/client/) | 桌面端的类型化 HTTP 客户端，包括配对、Tailscale 发现、SSH 和钥匙串 |
| [`tools/ironclaw_rune/docs/`](tools/ironclaw_rune/docs/) | 用户指南：其他电脑、更新 |
| [`crates/`](crates/) | IronClaw 后端以及我对它的扩展 |
| [`crates/product/ironclaw_webui/`](crates/product/ironclaw_webui/) | ZLO 与之通信的 HTTP 网关，包括配对、设备和 Tailscale 身份监听器 |
| [`crates/app/ironclaw_cli/`](crates/app/ironclaw_cli/) | `zlo` 二进制：`serve`、`remote`、`pair`、`devices`、`update`、`service` |
| [`docs/internal/plans/2026-11-08-zlo-remote-environments.md`](docs/internal/plans/2026-11-08-zlo-remote-environments.md) | 远程电脑的设计和通信协议 |
| [`AGENTS.md`](AGENTS.md) | 与编程智能体协作的仓库规则 |

从 IronClaw 继承来的 React 前端不是 ZLO 的桌面界面。它背后的网关仍在使用。

你输入的命令（`zlo`）和数据文件夹（`~/.zlo`）属于 ZLO。内部 crate 保留 IronClaw 的名称（`ironclaw_*`，以及构建 `zlo` 的 Cargo 包 `ironclaw`），`~/.ironclaw/` 下 CLI 智能体的工作文件夹和一些较旧的 Numoh 签名标识符也是如此。保持这些名称不变，可以让与上游合并以及维护 macOS 权限更容易。

## 来源

ZLO 是我对 [NEAR AI](https://github.com/nearai) 所构建的 [IronClaw](https://github.com/nearai/ironclaw) 的个人 fork。我在这个基础上构建了自己的桌面应用，并继续按自己的需要修改后端。智能体运行时、安全机制以及这个仓库的大部分内容都来自他们的工作。

界面使用 [Rune](https://github.com/anomalyco/rune)。应用的早期版本基于 [GPUI](https://www.gpui.rs/)（为 Zed 开发的 Rust UI 框架），已归档在 `archive/gpui` 分支（标签 `gpui-final`）。computer-use 辅助程序来自 [hermes-plugin-computer-use-plus](https://github.com/RiklloD/hermes-plugin-computer-use-plus)。ZLO 连接其他电脑的方式参照了 [T3 Code](https://github.com/pingdotgg/t3code) 的远程环境，用 Tailscale 代替了它的托管中继。

这个仓库是我的个人项目，不是 IronClaw 或 NEAR AI 的官方发布。

## 许可证

你可以任选以下其一：

- [Apache License, Version 2.0](LICENSE-APACHE)
- [MIT License](LICENSE-MIT)

上游的许可证和署名声明均已保留。内置的依赖项和 computer-use 辅助程序各有自己的许可证。
