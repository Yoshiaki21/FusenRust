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
- 該当する章（下記「## 構成」参照）に、簡潔な箇条書きで追記する。長文の説明文は書かない。
- 「Critical Paths（ファイル所在表）」は特に優先して最新化する。何かを追加/変更したら、まずここを疑う。
- 迷ったら、ユーザーに聞かずにいったん追記し、後で「CLAUDE.mdのここを追記しました」と一言報告する。
- 冗長な履歴は残さない。古い情報は書き換える（変更履歴セクション以外は追記ではなく上書き優先）。
- コード規約・設計判断の「理由」も一言添える（例: 「認証は express-session を採用（JWT不要な単一サーバー構成のため）」）。

### 書かないこと
- 実装の詳細なロジック説明（コード自体やdocstringに書くべきもの）
- 一時的なタスクの進捗（TODOリストや作業ログはここに書かない）
- 憶測・未確定の設計（決まったことだけを書く）

---

## 構成

### 1. プロジェクト概要
（何を作るソフトか、1〜3行で。決まり次第すぐ埋める）

- 目的: 複数PC（CachyOS / Windows 11）間で文字・ファイルを共有するデスクトップ付箋アプリ。自宅サーバー経由で同期し、全履歴を保存
- 想定ユーザー: 作者本人（自宅の複数PC）
- 作業指示: `FusenRust指示書.md`（ルール）、`docs/spec.md`（仕様）、`docs/phases.md`（フェーズ順）を必ず読む

### 2. 技術スタック
（決定した技術要素を都度追記。バージョンも明記）

- 言語/フレームワーク: Rust 1.99（edition 2024）、Cargo ワークスペース
- UI: `gpui-kit`（gpui + gpui-component）を git rev 固定。gpui は `gpui-pre =0.3.12`（gpui-kit 経由。D-001）
- DB: SQLite（端末・サーバー）
- core: rusqlite 0.37（bundled）、uuid（v7）、chrono、sha2、thiserror。テストは tempfile・futures-lite
- 主要ライブラリ: jxl-oxide（JXL）、image（PNG/JPEG）、ksni（Linux トレイ。D-002）、zbus（KWin 制御）、async-channel、tracing

### 3. ディレクトリ構造 / Critical Paths（最重要）
（「〇〇をしたいときはここを見る」の対応表。ファイルが増えるたびに更新）

| やりたいこと | 場所 |
|---|---|
| gpui-kit の rev を変える | `Cargo.toml`（workspace.dependencies） |
| API の型（版・添付・差分・WebSocket） | `crates/fusen-proto/src/lib.rs` |
| 端末側 DB のスキーマ・マイグレーション | `crates/fusen-core/src/db.rs`（追加は MIGRATIONS の末尾に足すだけ） |
| 付箋・版・ごみ箱・revert・下書き・配置・同期の記録 | `crates/fusen-core/src/store.rs`（`Store`） |
| 添付の実体（SHA-256・重複排除・上限） | `crates/fusen-core/src/blob.rs` |
| `attachment://` 参照の抽出・作成 | `crates/fusen-core/src/markdown.rs` |
| 同期エンジン・通信のトレイト | `crates/fusen-core/src/sync.rs`（`Transport`） |
| core のテスト（仮サーバーでの同期を含む） | `crates/fusen-core/tests/` |
| デスクトップアプリ本体 | `crates/fusen-desktop/src/` |
| OS 依存コード（KWin スクリプト / Win32） | `crates/fusen-desktop/src/platform/` |
| Phase 0 の検証プログラム | `crates/fusen-desktop/examples/p0_*.rs`（共通部品は `examples/common/mod.rs`） |
| 細かい判断の記録・Phase 0 の結果 | `docs/decisions.md` |
| gpui のソース（API 確認用） | `~/.cargo/registry/src/*/gpui-pre-0.3.12/`（examples/layer_shell.rs など） |
| gpui-component のソース | `~/.cargo/git/checkouts/gpui-component-*/48e53b1/`（examples/, crates/story/） |

### 4. 設計方針・規約
（命名規則、レイヤー分離の考え方、避けているパターンなど）

- gpui の API は推測で書かず、上記ソースを確認してから書く（1.0 前で破壊的変更が多いため）
- gpui は `gpui_kit::*` 経由で使い、直接依存に書かない（バージョンの二重指定を避けるため）
- コメントは日本語。OS 依存は `cfg(target_os)` で分けて `platform/` に置く
- fusen-proto / fusen-core に gpui・OS 固有の依存を入れない（Android 版で UniFFI から使うため）
- 版は追記のみ。UPDATE は仮番号→確定番号の置き換え（`store.rs` の `confirm`）だけ。未同期の版は負の仮番号（D-004）
- エラーは anyhow（アプリ）/ thiserror（ライブラリ）。`unwrap()` はテスト以外禁止。ログは tracing

### 5. よく使うコマンド
（ビルド、テスト、起動などの決まり文句）

```bash
cargo fmt --all && cargo clippy --all-targets -- -D warnings && cargo test --all   # コミット前に必須
cargo run -p fusen-desktop --example p0_kwin_window     # Phase 0 検証（他に p0_note_surface, p0_wall_overlay, p0_tray）
```

### 6. 既知の制約・注意点
（ハマりやすい点、あえてやっていないことなど）

- 付箋は layer-shell ではなく普通のウィンドウ＋KWin スクリプト（D-003）。gpui では layer-shell の位置・層を後から変えられないため
- gpui の `RenderImage` は BGRA。RGBA の画像は R と B を入れ替えて渡す
- Linux のトレイは `tray-icon` ではなく `ksni`（GTK ループ不要のため）
- Wayland で普通のウィンドウを常に背面/最前面・タスクバー非表示・位置指定するには KWin スクリプトを D-Bus で実行する（`examples/common/kwin.rs`）。呼び出しは直列化すること

### 7. 変更履歴（任意）
（大きな方針転換があった場合のみ、日付と概要を1行で）

- 2026-11-23: 付箋ウィンドウを layer-shell から普通のウィンドウ＋KWin スクリプトに変更（D-003）

---

**運用メモ（ユーザー向け）**
- ある程度育ってきたら、章ごとに `docs/` 配下へ分割してこのファイルからリンクする構成に移行することを検討してください（tududiのCLAUDE.mdのような形）。
- 定期的に「CLAUDE.mdを見直して、実態と合っていない箇所を修正して」とClaude Codeに依頼すると、陳腐化を防げます。
