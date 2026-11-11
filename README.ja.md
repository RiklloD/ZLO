# ZLO

[English](README.md) | [简体中文](README.zh-CN.md) | [Русский](README.ru.md) | 日本語 | [한국어](README.ko.md)

> この文書は [`README.md`](README.md) の翻訳です。内容が食い違う場合は英語版が正です。

私の個人用 AI デスクトップアプリです。macOS 向けに Rust で作っており、ネイティブの [Rune](https://github.com/anomalyco/rune) インターフェースと [IronClaw](https://github.com/nearai/ironclaw) バックエンドを使っています。

私は [Lorik](https://github.com/RiklloD) です。テクノロジーをいじるのが好きな経営学部の学生です。エンジニアリングの経験よりもアイデアのほうが多いので、AI エージェントと一緒に作業して、アイデアを自分で使えるソフトウェアにしています。ZLO は、そうした働き方を中心に作っているアプリです。

## なぜ作っているのか

デスクトップアプリは、その裏にいるエージェントと同じくらい大事です。軽くて、Mac に自然になじむものが欲しいのです。メモリ使用量とバッテリー持ちは製品の一部であり、機能を全部足したあとで直すものではありません。

これは私自身の作業環境であり、アイデアを試す場所です。完成したリリースや、あらゆるマシンへの対応を約束するものではありません。

## できること

- **ネイティブのインターフェース。** Rune による GPU 描画の Rust。Electron も常駐する組み込みブラウザもありません。ブラウザやドキュメントのビューは必要なときだけ作られます。
- **チャット、プロファイル、モデル。** タスクごとにモデルを選び、プロファイルを分けて保ち、オーケストレーターを通じて Codex や Claude Code などほかのエージェントに作業を任せられます。
- IronClaw バックエンドによる**ツール、スキル、メモリ、MCP**。
- 私がフォークした [computer-use エンジン](https://github.com/RiklloD/hermes-plugin-computer-use-plus)による**コンピューター操作**。
- **承認。** 各プロファイルは「先に確認する」か「フルアクセス」のどちらかです。自動承認はデフォルトでオフです。
- **ほかのコンピューターで作業。** バックエンドは Mac mini や Linux サーバーなど別のマシンで動かせて、ノートパソコンの ZLO はウィンドウだけになります。マシン同士は Tailscale か SSH でつながります。[別のコンピューターを使う](tools/ironclaw_rune/docs/remote-computers.md)を参照してください。

アプリは使いながら変えています。現在の状態を知るには、固定の機能一覧よりもコードと開発メモのほうが確かです。

## 動作の仕組み

ZLO は 2 つの部分からなります。**ウィンドウ**(Rune アプリ)と**バックエンド**(エージェント、そのツールとメモリを動かす IronClaw)です。両者は認証付きの HTTP で通信します。バックエンドをどこで動かすかは自由です。

| 構成 | どこで何が動くか | 使いどころ |
| --- | --- | --- |
| **すべてこの Mac で**(デフォルト) | ZLO が自分のバックエンドを子プロセスとして起動し、終了時に止めます。 | マシン 1 台で、設定不要。 |
| **バックエンドを別のコンピューターで** | バックエンドはサーバーや別の Mac で動きます。ZLO は tailnet か SSH で接続します。 | ノートパソコンがスリープ中もエージェントを動かし続けたいとき、または性能、リポジトリ、回線がより良いマシンで作業したいとき。 |
| **アプリのみ**(ZLO Light) | バックエンドを内蔵しない ZLO のビルド。常にほかのコンピューターにだけ接続します。 | 自分ではエージェントを動かすべきでないノートパソコン。 |

コンピューターは **Settings → Computers** で切り替えられます。ZLO は一度に 1 台のコンピューターで動作します。スレッド、メモリ、設定、プロバイダーキーはバックエンド側にあるので、コンピューターごとに別々です。

状態は、バックエンドを動かしているマシンの `~/.zlo` に保存されます(別の場所にするには `ZLO_HOME` を設定します)。別の PostgreSQL サーバーではなく libSQL を使います。古いインストールの `~/.ironclaw/reborn` は最初の起動時にそこへ移され、古いパスはリンクとして残るので、それを覚えているものが壊れることはありません。

ローカルに保存するからといって、すべての処理がマシン内にとどまるわけではありません。リモートのモデルや外部連携を使うと、そのリクエストに必要なデータがそのプロバイダーに送られます。コンピューター操作には、バックエンドが動くマシンで該当する macOS の権限が必要です。

## インストール

Apple Silicon の Mac で、これをターミナルに貼り付けます。

```sh
curl -fsSL https://raw.githubusercontent.com/RiklloD/ZLO/main/install.sh | sh
```

ZLO をソースからビルドし、`~/Applications/ZLO.app` にインストールします。Xcode コマンドラインツールがない場合はインストーラーを開き、コマンドをもう一度実行するよう求めます。Rust がなければインストールします。最初のビルドには 10〜20 分かかります。更新するには同じコマンドをもう一度実行します。

`main` 以外のブランチをビルドするには `ZLO_REF` を設定します。

```sh
curl -fsSL https://raw.githubusercontent.com/RiklloD/ZLO/main/install.sh | ZLO_REF=my-branch sh
```

Apple Development の署名 ID がない場合、アプリはアドホック署名になるため、更新のたびに macOS がアクセシビリティと画面収録の許可をもう一度求めます。

## 別のコンピューターを使う

tailnet 上のサーバーの場合の要約です。

```sh
# サーバー側で
zlo onboard            # プロバイダーとキー
zlo service install    # 起動時に開始
zlo remote enable      # tailnet に公開(インターネットには公開しない)
zlo service restart
```

次に Mac で ZLO を開き、**Settings → Computers** に進んで、「Found on your tailnet」の下にあるサーバーの横の **Connect** をクリックします。

[**別のコンピューターを使う**](tools/ironclaw_rune/docs/remote-computers.md)で設定全体を説明しています。

- Linux サーバーや 2 台目の Mac のセットアップ
- ワンクリック、コード、または SSH によるペアリング
- ペアリング済みデバイスの管理
- `zlo transfer user@host`、または `zlo export` / `zlo import` による、設定(スレッド、メモリ、プロファイル、設定、プロバイダーキー)のサーバーへの移行
- リモートで使える機能
- 接続の保護方法
- トラブルシューティング

[**マシンを最新に保つ**](tools/ironclaw_rune/docs/updates.md)では「Update all machines」と `zlo update` を説明しています。

## ソースからのビルド

これは macOS 向けの開発環境であり、クロスプラットフォームのインストーラーではありません。Xcode コマンドラインツール、rustup 経由の Rust、Apple Development の署名 ID が必要です。リポジトリは Rust ツールチェーンを [`rust-toolchain.toml`](rust-toolchain.toml) で固定しています。同梱の computer-use エンジンは現在 Apple Silicon 向けです。

```sh
git clone https://github.com/RiklloD/ZLO.git
cd zlo
sh tools/ironclaw_rune/bundle.sh
```

このスクリプトはバックエンドとデスクトップをビルドし、固定された computer-use エンジンをダウンロードして検証し、バンドルに署名して `~/Applications/ZLO.app` にインストールします。既存のインストールは置き換えます。アプリが動いていれば先に終了させ、開いていた場合は再度開きます。

| コマンド | 結果 |
| --- | --- |
| `sh tools/ironclaw_rune/bundle.sh` | ビルドし、`~/Applications/ZLO.app` にインストールして再度開く |
| `sh tools/ironclaw_rune/bundle.sh --no-install` | ビルドのみ、出力先は `tools/ironclaw_rune/target/ZLO.app` |
| `sh tools/ironclaw_rune/bundle.sh --light` | バックエンドも computer-use エンジンも持たない **ZLO Light.app** をビルドしてインストール。別のアプリなので ZLO を置き換えることはありません。 |

バックエンド単体(サーバー用)は次のようにビルドします。

```sh
cargo build --release -p ironclaw     # バイナリは target/release/zlo
```

デスクトップのワークスペースを変更する場合:

```sh
cd tools/ironclaw_rune
cargo check
cargo clippy --all-targets -- -D warnings
cargo test
```

バックエンドのコードを変更する前に、[`AGENTS.md`](AGENTS.md) と、担当するクレートのガイダンスを読んでください。

## リポジトリの歩き方

| パス | 内容 |
| --- | --- |
| [`tools/ironclaw_rune/`](tools/ironclaw_rune/) | デスクトップアプリと、その独立した Cargo ワークスペース |
| [`tools/ironclaw_rune/app/`](tools/ironclaw_rune/app/) | Rune インターフェース、ネイティブビュー、環境設定、バックエンドの監視、コンピューター一覧(`src/remote*`、`src/pages/computers*`) |
| [`tools/ironclaw_rune/client/`](tools/ironclaw_rune/client/) | デスクトップ用の型付き HTTP クライアント。ペアリング、Tailscale 検出、SSH、キーチェーンを含む |
| [`tools/ironclaw_rune/docs/`](tools/ironclaw_rune/docs/) | ユーザーガイド: ほかのコンピューター、更新 |
| [`crates/`](crates/) | IronClaw バックエンドと、それに対する私の拡張 |
| [`crates/product/ironclaw_webui/`](crates/product/ironclaw_webui/) | ZLO が通信する HTTP ゲートウェイ。ペアリング、デバイス、Tailscale ID リスナーを含む |
| [`crates/app/ironclaw_cli/`](crates/app/ironclaw_cli/) | `zlo` バイナリ: `serve`、`remote`、`pair`、`devices`、`update`、`service` |
| [`docs/internal/plans/2026-11-08-zlo-remote-environments.md`](docs/internal/plans/2026-11-08-zlo-remote-environments.md) | リモートコンピューターの設計とワイヤー契約 |
| [`AGENTS.md`](AGENTS.md) | コーディングエージェントと作業するためのリポジトリのルール |

IronClaw から引き継いだ React フロントエンドは ZLO のデスクトップインターフェースではありません。その裏にあるゲートウェイは今も使われています。

入力するコマンド(`zlo`)とデータフォルダー(`~/.zlo`)は ZLO のものです。内部のクレートは IronClaw の名前(`ironclaw_*`、および `zlo` をビルドする Cargo パッケージ `ironclaw`)を保っており、`~/.ironclaw/` 以下の CLI エージェントの作業フォルダーや、一部の古い Numoh 署名識別子も同様です。これらを安定させておくと、上流のマージと macOS の権限を保守しやすくなります。

## 由来

ZLO は、[NEAR AI](https://github.com/nearai) が作った [IronClaw](https://github.com/nearai/ironclaw) の私の個人フォークです。その土台の上にデスクトップアプリを作り、自分用にバックエンドも変え続けています。エージェントランタイム、セキュリティの仕組み、そしてこのリポジトリの多くは彼らの仕事によるものです。

インターフェースは [Rune](https://github.com/anomalyco/rune) を使っています。以前のバージョンのアプリは、Zed のために開発された Rust の UI フレームワーク [GPUI](https://www.gpui.rs/) で作られていました。それは `archive/gpui` ブランチ(タグ `gpui-final`)にアーカイブされています。computer-use ヘルパーは [hermes-plugin-computer-use-plus](https://github.com/RiklloD/hermes-plugin-computer-use-plus) から来ています。ZLO がほかのコンピューターに接続する方法は [T3 Code](https://github.com/pingdotgg/t3code) のリモート環境にならっており、ホスト型リレーの代わりに Tailscale を使います。

このリポジトリは私のプロジェクトであり、IronClaw や NEAR AI の公式リリースではありません。

## ライセンス

次のいずれかを選んで利用できます。

- [Apache License, Version 2.0](LICENSE-APACHE)
- [MIT License](LICENSE-MIT)

上流のライセンス表示と帰属表示は保持しています。同梱の依存関係と computer-use ヘルパーには、それぞれ独自のライセンスがあります。
