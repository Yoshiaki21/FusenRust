# CLAUDE.md — FusenRust

このファイルは Claude Code 向けの作業指示書です。作業を始める前に必ず読み、
あわせて `docs/spec.md`（仕様）と `docs/phases.md`（作業フェーズ）を読んでください。

## プロジェクトの目的

複数のPC（CachyOS / Windows 11）の間で、文字情報やファイルを**シンプルに共有する**ための付箋アプリ。
テキストファイルを手でやり取りする手間をなくすことが目的。

- デスクトップを「壁」に見立てて付箋を貼る（通常ウィンドウより下の層に表示）
- 付箋ごとに「最前面表示」を切り替えられる
- 本文は Markdown、任意のファイルを添付できる（画像 PNG/JPEG/JXL はインライン表示）
- 自宅サーバー経由で同期し、付箋ごとに全履歴を保存・閲覧・復元できる

## 対象プラットフォームと優先順位

1. **CachyOS（KDE Plasma 6 / Wayland）** … 主開発環境。最優先
2. **Windows 11** … CachyOS 版の完成後
3. Android … CachyOS / Windows 版が完成してから着手（現時点では実装しない）

GNOME など KDE 以外のデスクトップ環境には**対応しない**（考慮不要）。

## 技術スタック

| 用途 | 採用 |
|---|---|
| 言語 | Rust（stable 最新） |
| デスクトップ UI | gpui（zed-industries/zed）＋ gpui-component（longbridge/gpui-component） |
| 端末側の保存 | SQLite |
| 同期サーバー | Rust（axum）＋ SQLite ＋ ファイル保存（添付） |
| 通信 | REST（HTTPS）＋ WebSocket（変更の通知） |
| ID | UUIDv7 |
| 画像デコード | PNG/JPEG: `image` クレート、JXL: `jxl-oxide` |

## リポジトリ

- GitHub: https://github.com/Yoshiaki21/FusenRust
- Cargo ワークスペース構成（詳細は `docs/spec.md` の「リポジトリ構成」）

## 守ってほしいルール

### gpui / gpui-component の扱い（最重要）

- gpui は 1.0 前で、破壊的変更が頻繁にある。**学習済みの知識で API を推測して書かないこと。**
- コードを書く前に、必ず次の順に実物を確認する:
  1. `~/.cargo/git/checkouts/` に展開された gpui / gpui-component のソース
  2. gpui-component の `crates/story`（サンプル集）
  3. gpui の `examples/`（特に `layer_shell.rs`、ウィンドウ透過、画像表示の例）
- gpui と gpui-component は `Cargo.toml` で **git の特定コミット（`rev = "..."`）に固定**する。
  更新するときは専用のコミットに分け、何が変わったかをコミットメッセージに書く。
- gpui-component が要求する gpui のコミットに合わせること（両者の組み合わせを崩さない）。

### コード

- コメントは**日本語**で書く。特に OS 依存の処理（layer-shell、Win32 API）には理由を書く。
- OS 依存コードは `cfg(target_os = ...)` で分け、`platform/` モジュールにまとめる。
- `unsafe` を使う場合は、安全である理由をコメントに書く。
- エラー処理は `anyhow`（アプリ側）/ `thiserror`（ライブラリ側）。`unwrap()` はテスト以外で使わない。
- ログは `tracing`。

### 検証

- コミット前に `cargo fmt --all`、`cargo clippy --all-targets -- -D warnings`、`cargo test --all` を通す。
- GUI の見た目や操作は Claude Code では確認できないので、各フェーズの「手動確認項目」
  （`docs/phases.md`）をユーザーに提示して、確認を依頼すること。
- `fusen-core` と `fusen-server` はユニットテスト・結合テストを書く（同期・履歴・競合の判定は必須）。

### 作業の進め方

- `docs/phases.md` のフェーズ順に進める。フェーズをまたいで先回りしない。
- 各フェーズの完了条件を満たしたら、ユーザーに報告して次へ進む許可をもらう。
- 仕様に書かれていないことで判断が必要な場合は、勝手に決めずユーザーに質問する。
  小さな判断で済むものは、決めた内容を `docs/decisions.md` に追記する。
- コミットは小さく分け、Conventional Commits 形式（`feat:` `fix:` `docs:` など）で書く。
- push はユーザーの指示があるときだけ行う。

## 開発環境メモ

- CachyOS: KDE Plasma 6 / Wayland、日本語入力は fcitx5
- サーバーのテスト: CachyOS 上の **podman**
- サーバーの本番: Proxmox 上の **Docker** コンテナ、前段にリバースプロキシ（Caddy）
- コンテナイメージは podman と Docker の両方で使える `Containerfile` で作る
