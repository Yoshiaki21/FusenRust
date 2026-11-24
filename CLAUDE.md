# CLAUDE.md

> このファイルはプロジェクトの進行に合わせて Claude Code 自身が更新していく「生きたドキュメント」です。
> 新規プロジェクト開始時はほぼ空の状態で構いません。実装が進むにつれて、下記のルールに従って自動的に育てていってください。

---

## Claude Codeへの指示（自己更新ルール）

あなたはこのプロジェクトで作業するたびに、以下のルールに従って本ファイル（CLAUDE.md）を更新してください。

### 更新すべきタイミング
- 新しいファイル/モジュール/ディレクトリを作成したとき
- 既存のアーキテクチャや設計方針を変更したとき（例: ライブラリの乗り換え、レイヤー構成の変更）
- 「今後も同じ説明をユーザーにさせそうだ」と感じたとき
- ユーザーから明示的に「CLAUDE.mdに書いて」と言われたとき

### 更新時の振る舞い
- 該当する章（下記「## 構成

### 1. プロジェクト概要
（何を作るソフトか、1〜3行で。決まり次第すぐ埋める）

- 目的: 複数PC（CachyOS / Windows 11）間で文字・ファイルを共有するデスクトップ付箋アプリ。自宅サーバー経由で同期し、全履歴を保存
- 想定ユーザー: 作者本人（自宅の複数PC）
- 作業指示: `FusenRust指示書.md`（ルール）、`docs/spec.md`（仕様）、`docs/phases.md`（フェーズ順）を必ず読む。実装中の判断は `docs/decisions.md`（D-001〜）
- 仕様からの大きな変更: 付箋は layer-shell ではなく普通のウィンドウ＋KWin スクリプト（D-003。spec.md 5.5 を置き換え）

### 2. 技術スタック
（決定した技術要素を都度追記。バージョンも明記）

- 言語: Rust 1.99（edition 2024）、Cargo ワークスペース（proto / core / server / desktop）
- UI: `gpui-kit`（gpui + gpui-component）を git rev `48e53b1` に固定。gpui は gpui-kit が要求する `gpui-pre =0.3.12`（D-001）。編集時の色分けに `tree-sitter-markdown` を有効化
- DB: SQLite（rusqlite 0.37、bundled。端末・サーバーとも）
- core: uuid（v7）、chrono、sha2、mime_guess、thiserror。テストは tempfile・futures-lite
- server: axum 0.8（ws）、tokio、tokio-util、clap、getrandom。テストは reqwest 0.13（json, stream）・tokio-tungstenite
- desktop の同期: 専用スレッドの tokio、reqwest 0.13（rustls / aws-lc-rs）、tokio-tungstenite 0.30（rustls、OS の証明書）
- desktop の Linux 固有: zbus 5（KWin スクリプト・単一インスタンス・位置の通知）、ksni（トレイ。D-002）、signal-hook（SIGTERM で保存終了）
- desktop の Windows 固有: windows-sys 0.60（Win32 API。付箋ウィンドウ・単一インスタンス・自動起動）、tray-icon 0.26（トレイ。D-002, D-011）
- その他: jxl-oxide（JXL）、image（PNG/JPEG）、async-channel、dirs、tracing
- 配備: podman（テスト、Quadlet）/ Docker（本番、Proxmox VE の専用 LXC 内）、前段に Caddy

### 3. ディレクトリ構造 / Critical Paths（最重要）
（「〇〇をしたいときはここを見る」の対応表。ファイルが増えるたびに更新）

| やりたいこと | 場所 |
|---|---|
| gpui-kit の rev・共通の依存を変える | `Cargo.toml`（workspace.dependencies） |
| API の型（版・添付・差分・WebSocket） | `crates/fusen-proto/src/lib.rs` |
| 端末側 DB のスキーマ・マイグレーション | `crates/fusen-core/src/db.rs`（追加は MIGRATIONS の末尾に足すだけ） |
| 付箋・版・ごみ箱・revert・下書き・配置・同期の記録 | `crates/fusen-core/src/store.rs`（`Store`） |
| 添付の実体（SHA-256・重複排除・上限） | `crates/fusen-core/src/blob.rs` |
| `attachment://` 参照の抽出・作成、一覧に出す付箋の名前（`display_title`） | `crates/fusen-core/src/markdown.rs` |
| 同期エンジン・通信のトレイト | `crates/fusen-core/src/sync.rs`（`Transport`） |
| 付箋の色のキー | `crates/fusen-core/src/color.rs`（表示色は desktop の `palette.rs`） |
| core のテスト（仮サーバーでの同期を含む） | `crates/fusen-core/tests/` |
| サーバーの API（ルーティング・認証・blob・WebSocket） | `crates/fusen-server/src/routes.rs` |
| サーバーの DB（版の受け付け・競合・冪等・トークン） | `crates/fusen-server/src/db.rs` |
| サーバーの起動・設定・トークン管理コマンド | `crates/fusen-server/src/lib.rs`, `src/main.rs` |
| サーバーの結合テスト（テスト用の HTTP の Transport を含む） | `crates/fusen-server/tests/api.rs` |
| コンテナのイメージ | `deploy/Containerfile`（ビルド時はメンバーを proto/core/server に絞る） |
| テスト用サーバー（podman Quadlet、127.0.0.1:9090） | `deploy/fusen-server.container` |
| 本番用サーバー（Docker compose）とその設定例 | `deploy/compose.yaml`, `deploy/.env.example`（実際の設定は `deploy/.env`、git 管理外） |
| リバースプロキシの設定例 | `deploy/Caddyfile.example` |
| デスクトップの起動・引数・単一インスタンス | `crates/fusen-desktop/src/main.rs` |
| アプリ全体の状態・命令・付箋ウィンドウの開閉・位置の保存と復元・同期イベントの反映 | `crates/fusen-desktop/src/app.rs` |
| 付箋ウィンドウ（バー・本文・編集・添付チップ・貼り付け・他端末更新の表示） | `crates/fusen-desktop/src/note_window.rs` |
| 管理画面（一覧・ごみ箱・履歴・設定・同期状態） | `crates/fusen-desktop/src/manager.rs` |
| 設定・保存場所・既定サイズ・プロファイル | `crates/fusen-desktop/src/settings.rs` |
| 付箋の表示色 | `crates/fusen-desktop/src/palette.rs` |
| 画像のデコードとキャッシュ | `crates/fusen-desktop/src/images.rs` |
| 同期サービス（専用スレッドの tokio・WebSocket・再接続・状態） | `crates/fusen-desktop/src/sync/mod.rs` |
| サーバーとの HTTP 通信（Transport の実装・URL の正規化） | `crates/fusen-desktop/src/sync/http.rs` |
| OS 依存の共通型（命令・通知・付箋のタイトル） | `crates/fusen-desktop/src/platform/mod.rs` |
| KDE: KWin スクリプト・D-Bus（単一インスタンス・位置の通知）・トレイ・自動起動・シグナル | `crates/fusen-desktop/src/platform/linux_kde.rs` |
| Windows: 付箋ウィンドウの制御（サブクラス・層・DPI・クローク）・単一インスタンス・トレイ・自動起動 | `crates/fusen-desktop/src/platform/windows.rs` |
| Windows 版の配布用 zip を作る | `scripts/package-windows.ps1`（`target/dist/` にできる） |
| Phase 0 の検証プログラム（参考用。本体からは使わない） | `crates/fusen-desktop/examples/p0_*.rs`, `examples/common/` |
| 細かい判断の記録・Phase 0 の結果 | `docs/decisions.md` |
| 端末のデータ・設定（Linux） | `~/.local/share/fusen-rust/`（fusen.db, blobs/）、`~/.config/fusen-rust/settings.json`。`--profile X` は各 `profiles/X/` |
| 端末のデータ・設定・ログ（Windows） | `%APPDATA%\fusen-rust\`（fusen.db, blobs\, settings.json）。リリース版のログは `%LOCALAPPDATA%\fusen-rust\fusen-desktop.log` |
| gpui のソース（API 確認用） | `~/.cargo/registry/src/*/gpui-pre-0.3.12/`（examples/, src/window.rs など） |
| gpui-component のソース | `~/.cargo/git/checkouts/gpui-component-*/48e53b1/`（examples/, crates/story/, crates/component/src/） |

### 4. 設計方針・規約
（命名規則、レイヤー分離の考え方、避けているパターンなど）

- gpui の API は推測で書かず、上記ソースを確認してから書く（1.0 前で破壊的変更が多いため）
- gpui は `gpui_kit::*` 経由で使い、直接依存に書かない（バージョンの二重指定を避けるため）
- コメントは日本語。OS 依存は `cfg(target_os)` で分けて `platform/` に置く（`platform::imp` が OS ごとの実装を指す）
- fusen-proto / fusen-core に gpui・OS 固有の依存を入れない（Android 版で UniFFI から使うため）
- 版は追記のみ。UPDATE は仮番号→確定番号の置き換え（`store.rs` の `confirm`）だけ。未同期の版は負の仮番号（D-004）
- 同じ版の再送は op_id（UUIDv7）で冪等にする。版を後から指すときも番号ではなく op_id で覚える（番号は同期で変わる。D-004, D-010）
- 付箋を貼るかどうかは端末ごと。配置（placements、同期しない）がない付箋はこの端末には貼っていない。他の端末の付箋を自動で貼らない（D-012）
- 版に項目を足すときは、proto に `#[serde(default)]`、端末とサーバーの両方の DB にマイグレーションを足す（D-013 の title が例）
- ビューの更新中からアプリ全体の処理（`app::notes_changed` など）を呼ぶときは `cx.defer` を通す（同じエンティティの二重借用を避けるため）
- エラーは anyhow（アプリ）/ thiserror（ライブラリ）。`unwrap()` はテスト以外禁止。ログは tracing
- コミットは小さく Conventional Commits。push はユーザーの指示があるときだけ。フェーズの完了ごとに報告して許可をもらう

### 5. よく使うコマンド
（ビルド、テスト、起動などの決まり文句）

```bash
cargo fmt --all && cargo clippy --all-targets -- -D warnings && cargo test --all   # コミット前に必須
cargo run -p fusen-desktop                       # 起動（--new / --show-all / --manager / --quit）
cargo run -p fusen-desktop -- --profile test     # 別の設定・データで起動（実データを汚さずに試す）
# サーバーをローカルで起動（開発 PC の 8080 は使用中なので別のポートで）
cargo run -p fusen-server -- --listen 127.0.0.1:19191 --data-dir /tmp/fusen-data
cargo run -p fusen-server -- --data-dir /tmp/fusen-data token create --device test
# テスト用サーバー（podman Quadlet。ユーザーが常用中）
podman build -f deploy/Containerfile -t localhost/fusen-server:latest .
systemctl --user restart fusen-server
podman exec fusen-server fusen-server token create --device <端末名>
```

### 6. 既知の制約・注意点
（ハマりやすい点、あえてやっていないことなど）

- 付箋は layer-shell ではなく普通のウィンドウ＋KWin スクリプト（D-003）。gpui では layer-shell の位置・層を後から変えられないため
- Wayland で普通のウィンドウを常に背面/最前面・タスクバー非表示・位置指定するには KWin スクリプトを D-Bus で実行する（`platform/linux_kde.rs`）。呼び出しは専用スレッドで直列化すること
- 付箋の位置はアプリから読めないので、KWin の常駐スクリプトが `callDBus` で通知する。保存済みの位置は表示後に適用し、KWin の初期配置に上書きされるため 0.4 秒後・1.2 秒後にも再適用する（D-006）
- 新しく開いた付箋は位置が決まるまで KWin で透明（opacity 0）にし、動く様子を見せない（D-008）。この端末で作った付箋は最前面で始める（D-007）
- gpui-component の Root は CSD ウィンドウに 20px の影を付ける。KWin の位置・サイズは影の内側なので、ウィンドウは影の分大きく開く（`app.rs` の `CSD_SHADOW`）
- gpui の `RenderImage` は BGRA。RGBA の画像は R と B を入れ替えて渡す
- `gpui_kit::*` を glob import したモジュールのテストで `use super::*` すると gpui の `test` マクロと衝突する。必要なものだけ import する
- zbus の `Builder::name` は名前を奪われる設定。単一インスタンスには `request_name_with_flags(DoNotQueue)` を使う
- 同期は gpui の実行器ではなく専用スレッドの tokio で動く（reqwest / WebSocket が tokio を必要とするため。D-010）
- 開発 PC: 8080 は llama サーバーが使用中。テスト用の fusen-server は podman の `fusen-server` コンテナ（127.0.0.1:9090）で常用中なので、試験で同名のコンテナを起動・削除しない
- 試験でアプリを起動するときは `--profile` を使い、`~/.local/share/fusen-rust` 本体（ユーザーの実データ）は消さない
- Windows: gpui の更新処理の中から ShowWindow / SetWindowPos を呼ぶとメッセージが捨てられる。付箋ウィンドウの操作は `platform/windows.rs` の `run_later` を通す（D-011）
- Windows: gpui は `start_window_move` / `start_window_resize` が未実装。バーは `window_control_area(Drag)`、右下のつまみはサブクラスの `WM_NCHITTEST`（D-011）
- Windows: PowerShell 5.1 用の .ps1 に日本語を書くときは UTF-8（BOM 付き）で保存する（BOM なしだと壊れて読まれる）
- コンテナのビルドは `.dockerignore` / `.containerignore` で target/ を除く（target は数十 GB になる）
- サーバーのコンテナは uid 10001 で動く。本番の bind mount のデータディレクトリは事前に chown が必要

### 7. 変更履歴（任意）
（大きな方針転換があった場合のみ、日付と概要を1行で）

- 2026-11-23: 付箋ウィンドウを layer-shell から普通のウィンドウ＋KWin スクリプトに変更（D-003）
- 2026-11-23: gpui の固定を git rev から gpui-kit の rev 固定（gpui-pre =0.3.12）に変更（D-001。gpui-component 側の変更に合わせる）
- 2026-11-24: Windows 11 版を追加（`platform/windows.rs`。D-011）

---

**運用メモ（ユーザー向け）**
- ある程度育ってきたら、章ごとに `docs/` 配下へ分割してこのファイルからリンクする構成に移行することを検討してください（tududiのCLAUDE.mdのような形）。
- 定期的に「CLAUDE.mdを見直して、実態と合っていない箇所を修正して」とClaude Codeに依頼すると、陳腐化を防げます。
