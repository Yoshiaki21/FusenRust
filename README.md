# FusenRust

複数の PC（CachyOS / Windows 11）の間で、文字やファイルを共有するデスクトップ付箋アプリです。
デスクトップに付箋を「貼る」と、内容が自宅のサーバーを通して全部の PC に同期されます。編集の履歴はすべて残ります。

- 付箋は Markdown で書けて、表示するときは整形されます（見出し・リスト・チェックボックス・表・コード・リンクなど）
- どんな形式のファイルでも添付できます（PC 間のファイルの受け渡しにも使えます）。PNG / JPEG / JXL の画像は本文の中に表示されます
- 付箋ごとに「壁モード」（他のウィンドウの下に貼り付く。既定）と「最前面モード」を切り替えられます
- 付箋にタイトルを付けられます（バーのピンの横に表示されます）
- どの付箋を貼るかは PC ごとに選べます。全部の付箋を全部の PC に貼る必要はありません
- 保存のたびに版が残り、管理画面の履歴から前の版に戻せます。消した付箋はごみ箱から戻せます
- オフラインでも使えます。編集はためておき、サーバーにつながったときに送ります
- 2 台で同時に編集したときは後から保存した方が最新になり、もう一方の版も履歴に残ります

Rust で書いています。UI は [gpui](https://www.gpui.rs/) と [gpui-component](https://github.com/longbridge/gpui-component) です。

## 動作環境

| | 状態 |
|---|---|
| CachyOS（KDE Plasma 6 / Wayland） | 使えます |
| Windows 11 | 実装済み・動作確認中 |
| 同期サーバー（Linux のコンテナ） | 使えます |
| Android | 将来の予定 |

- Linux 版は **KDE Plasma 6（Wayland）が必要**です。付箋を常に背面・最前面に置く処理や、付箋の位置の取得に KWin のスクリプトを使っているためです。
- 同期サーバーがなくても、1 台の PC の中で付箋アプリとして使えます。

## 使い方

### 起動と終了

```sh
fusen-desktop               # 起動する（起動済みなら管理画面を開く）
fusen-desktop --new         # 新しい付箋を作る
fusen-desktop --show-all    # 全部の付箋を一時的に前面に出す / 元に戻す
fusen-desktop --manager     # 管理画面を開く
fusen-desktop --quit        # 終了する
```

起動するとトレイにアイコンが出ます。左クリックで管理画面が開き、右クリックのメニューから新しい付箋・全付箋を前面に出す・終了を選べます。

アプリ自身は全体のショートカットキーを登録しません。次のように OS の機能からコマンドを呼んでください。

- **KDE**: システム設定 →「キーボード」→「ショートカット」で、コマンド `fusen-desktop --new` を追加します
- **Windows**: `fusen-desktop.exe` のショートカットを作り、プロパティの「リンク先」の後ろに ` --new` を付けて「ショートカット キー」を割り当てます

ログイン時の自動起動は、管理画面の「設定」で切り替えられます。

### 付箋の操作

| やりたいこと | 操作 |
|---|---|
| 移動 | 上のバーをドラッグ |
| 大きさを変える | 右下のつまみをドラッグ |
| 書く・直す | 本文をダブルクリックすると編集状態になります。付箋の外をクリックすると整形表示に戻り、保存されます |
| 最前面 / 壁の切り替え | バーのピンのボタン |
| タイトルを付ける・直す | バーをダブルクリック（またはメニューの「タイトルを編集…」）。Enter で確定、Esc で取り消し。空にするとタイトルを外します |
| 色を変える | バーの色のボタン（黄・ピンク・青・緑・紫・灰） |
| チェックボックス | 整形表示のままクリックで切り替え |
| ファイルを添付する | ドラッグ＆ドロップ、貼り付け（Ctrl+V）、メニューの「ファイルを添付…」 |
| 添付を開く・保存・削除 | 本文の下のチップをクリックで開きます。保存と削除はチップのメニューから |
| 履歴・ごみ箱・デスクトップから外す | バーのメニュー |

- 編集中に添付すると、カーソルの位置に参照（`![説明](attachment://…)`）が入ります。
- 編集中の内容は数秒ごとに下書きとして保存されます。アプリが強制終了しても、次の起動で編集中の状態に戻ります。
- 編集中に別の PC で同じ付箋が更新されると「他の端末で更新されました」と表示されます。自分の内容は上書きされません。
- 「デスクトップから外す」は、この PC から付箋を外すだけです（付箋は消えず、他の PC にも影響しません）。管理画面の一覧から、また貼れます。

### 管理画面

| タブ | 内容 |
|---|---|
| 一覧 | 全部の付箋。名前はタイトル（なければ本文の 1 行目）です。タイトル・本文・添付のファイル名で検索できます。この PC に貼ってある付箋には「貼ってある」の印が付き、「貼る」「外す」ボタンで切り替えられます。クリックするとその付箋を貼って前面に出します |
| ごみ箱 | 消した付箋と、その復元 |
| 履歴 | 選んだ付箋の版の一覧（日時・端末・操作・競合の印）。版を選んで「この版に戻す」 |
| 設定 | サーバーの URL・トークン・端末名・フォント・添付の上限（既定 500 MB）・自動起動 |

画面の下に同期の状態（接続済み / オフライン / 未送信の件数）が出ます。同じ内容がトレイのツールチップにも出ます。

### 同期のしかた

1. サーバーを用意します（下の「同期サーバー」）
2. PC ごとにトークンを発行します
   ```sh
   fusen-server token create --device cachyos-main   # トークンは 1 回だけ表示されます
   ```
3. 各 PC の管理画面の「設定」に、サーバーの URL（例: `https://fusen.example.com`）とトークンを入れます

付箋のタイトル・本文・色・添付・ごみ箱の状態が同期されます。**どの付箋を貼るか・位置・大きさ・最前面の設定は PC ごと**で、同期しません。
別の PC で作られた付箋は、この PC には自動では貼られず、管理画面の一覧に出ます。必要な付箋だけ「貼る」で貼ってください（新しい PC で初めて同期したときも、何も貼られません）。

### データの場所

| | Linux | Windows |
|---|---|---|
| データ（`fusen.db` と `blobs/`） | `~/.local/share/fusen-rust/` | `%APPDATA%\fusen-rust\` |
| 設定（`settings.json`） | `~/.config/fusen-rust/` | `%APPDATA%\fusen-rust\` |
| ログ | 標準エラー出力 | `%LOCALAPPDATA%\fusen-rust\fusen-desktop.log`（リリース版） |

`--profile <名前>` を付けて起動すると、別の設定・データ（各場所の `profiles/<名前>/`）を使います。
同じ PC で 2 つ起動して同期を試すときや、実際のデータを汚さずに試すときに使います。

## 同期サーバー

`fusen-server` は axum と SQLite で作った小さなサーバーです。データはすべて 1 つのディレクトリ（`fusen.db` と `blobs/`）に入ります。
TLS は前に置くリバースプロキシ（Caddy など）に任せ、サーバー自体は HTTP で待ち受けます。

| 環境変数 | 意味 | 既定 |
|---|---|---|
| `FUSEN_LISTEN` | 待ち受けるアドレス | `0.0.0.0:8080` |
| `FUSEN_DATA_DIR` | データのディレクトリ | `./data` |
| `FUSEN_MAX_UPLOAD_BYTES` | 1 ファイルのアップロードの上限 | `524288000`（500 MB） |

トークンの管理:

```sh
fusen-server token create --device <端末名>
fusen-server token list
fusen-server token revoke <ID>
```

### Docker（compose）で動かす

```sh
cp deploy/.env.example deploy/.env                     # 必要なら値を変える
mkdir -p deploy/data && sudo chown 10001:10001 deploy/data   # コンテナは uid 10001 で動く
docker compose -f deploy/compose.yaml up -d --build
docker compose -f deploy/compose.yaml exec fusen-server fusen-server token create --device cachyos-main
curl http://127.0.0.1:8080/healthz                     # → ok
```

- リバースプロキシの例は [deploy/Caddyfile.example](deploy/Caddyfile.example) にあります（WebSocket の中継・アップロードの上限）
- podman（Quadlet）で動かす例は [deploy/fusen-server.container](deploy/fusen-server.container) にあります
- バックアップは、サーバーを止めてデータのディレクトリごとコピーします（動かしたままなら `sqlite3` の `.backup` を使います）

## ビルド

Rust 1.99 以降（edition 2024）が必要です。

```sh
cargo build --release -p fusen-desktop     # デスクトップアプリ
cargo build --release -p fusen-server      # 同期サーバー
```

Windows の配布用 zip（`fusen-desktop.exe` と README.txt。インストールは不要）は次のスクリプトで作れます。

```powershell
powershell -ExecutionPolicy Bypass -File scripts\package-windows.ps1   # → target\dist\FusenRust-<版>-windows-x64.zip
```

開発時のチェック（コミット前に必ず通します）:

```sh
cargo fmt --all && cargo clippy --all-targets -- -D warnings && cargo test --all
```

## リポジトリの構成

```
crates/
  fusen-proto/     サーバーとアプリで共有する API の型
  fusen-core/      データモデル・端末側の SQLite・履歴・添付・同期エンジン（UI や OS に依存しない）
  fusen-server/    同期サーバー
  fusen-desktop/   デスクトップアプリ（OS ごとの処理は src/platform/）
deploy/            コンテナのイメージ・compose・Quadlet・Caddy の設定例
scripts/           配布用のスクリプト
docs/              仕様（spec.md）・作業フェーズ（phases.md）・設計の判断の記録（decisions.md）
```

詳しい仕様は [docs/spec.md](docs/spec.md) に、実装中に決めたことは [docs/decisions.md](docs/decisions.md) にあります。

## 既知の制約

- Linux 版は KDE Plasma 6（Wayland）でしか、付箋の背面・最前面の固定と位置の保存が動きません
- Windows では Win+D（デスクトップの表示）で付箋も隠れます
- 添付の実体は、過去の版から参照されている可能性があるため消しません（完全に消す機能はありません）
- トレイのアイコンは仮のものです

## ライセンス

MIT OR Apache-2.0
