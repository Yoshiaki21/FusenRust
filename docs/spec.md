# FusenRust 仕様書

## 1. 概要

複数のPC間で文字情報とファイルを共有するための付箋アプリ。
デスクトップに付箋を「貼り」、内容は自宅サーバー経由で全端末に同期される。

### 1.1 用語

| 用語 | 意味 |
|---|---|
| 付箋（note） | 1枚の付箋。UUIDv7 の `note_id` を持つ |
| 版（revision） | 付箋のある時点の状態。保存のたびに1つ追加され、上書き・削除はしない |
| 添付（attachment） | 付箋に付けたファイル。実体（blob）は SHA-256 で管理 |
| 端末（device） | アプリを動かすPC。UUIDv7 の `device_id` を持つ |
| 配置（placement） | 端末ごとの付箋の位置・サイズ・表示層 |
| 壁モード | 付箋を通常ウィンドウより下の層に表示する状態（既定） |
| 最前面モード | 付箋を通常ウィンドウより上に表示する状態 |

## 2. リポジトリ構成

```
FusenRust/
├── Cargo.toml              # ワークスペース定義（gpui / gpui-component の rev 固定もここ）
├── CLAUDE.md
├── docs/
│   ├── spec.md             # この仕様書
│   ├── phases.md           # 作業フェーズ
│   └── decisions.md        # 実装中に決めた細かい判断の記録
├── crates/
│   ├── fusen-proto/        # サーバーとクライアントで共有する API の型（serde）。gpui に依存しない
│   ├── fusen-core/         # データモデル、端末側 SQLite、同期エンジン、添付の管理。gpui に依存しない
│   ├── fusen-server/       # 同期サーバー（axum）
│   └── fusen-desktop/      # デスクトップアプリ（gpui + gpui-component）
│       └── src/platform/
│           ├── linux_kde.rs   # layer-shell 関連
│           └── windows.rs     # Win32 API 関連
└── deploy/
    ├── Containerfile        # サーバーのイメージ（podman / Docker 共用）
    ├── compose.yaml         # 本番（Proxmox 上の Docker）用
    ├── fusen-server.container  # テスト（CachyOS 上の podman Quadlet）用
    └── Caddyfile.example    # リバースプロキシの設定例
```

`fusen-core` は将来 Android 版から UniFFI 経由で使う想定。gpui や OS 固有の依存を入れないこと。

## 3. データモデル

### 3.1 端末側・サーバー側で共通の論理モデル

```sql
-- 付箋（最新状態の索引）
CREATE TABLE notes (
  note_id      TEXT PRIMARY KEY,      -- UUIDv7
  latest_rev   INTEGER NOT NULL,      -- 最新の版番号
  deleted      INTEGER NOT NULL DEFAULT 0,  -- ごみ箱に入っているか（論理削除）
  created_at   TEXT NOT NULL,         -- RFC3339
  updated_at   TEXT NOT NULL
);

-- 版（追記のみ。UPDATE / DELETE しない）
CREATE TABLE revisions (
  note_id      TEXT NOT NULL,
  rev          INTEGER NOT NULL,      -- 付箋ごとの連番（1 から）
  server_seq   INTEGER,               -- サーバーの受付番号。未同期の間は NULL
  device_id    TEXT NOT NULL,         -- 保存した端末
  edited_at    TEXT NOT NULL,         -- 端末の時刻（表示用。順序判定には使わない）
  kind         TEXT NOT NULL,         -- 'edit' | 'color' | 'title' | 'attach' | 'detach' | 'delete' | 'restore' | 'revert'
  title        TEXT NOT NULL DEFAULT '',  -- タイトル（空なら付けていない。D-013）
  content      TEXT NOT NULL,         -- Markdown 本文（その版の全文）
  color        TEXT NOT NULL,         -- 付箋の色（パレットのキー）
  attachments  TEXT NOT NULL,         -- その版で有効な attachment_id の JSON 配列
  deleted      INTEGER NOT NULL,      -- その版の時点でごみ箱にあるか
  base_rev     INTEGER,               -- 編集を始めたときの版（競合の検出用）
  conflict     INTEGER NOT NULL DEFAULT 0,  -- 競合ありの印（サーバーが設定）
  PRIMARY KEY (note_id, rev)
);

-- 添付のメタデータ（不変）
CREATE TABLE attachments (
  attachment_id TEXT PRIMARY KEY,     -- UUIDv7
  note_id       TEXT NOT NULL,        -- 最初に添付された付箋
  sha256        TEXT NOT NULL,        -- 実体のハッシュ
  file_name     TEXT NOT NULL,        -- 元のファイル名
  mime          TEXT NOT NULL,
  size          INTEGER NOT NULL,
  created_at    TEXT NOT NULL
);
```

- 版は「その時点の全状態」を持つ（差分ではない）。テキスト中心なので容量は問題にしない。
- 添付の実体（blob）は `sha256` をキーにファイルとして保存し、同じ中身は1つだけ持つ。
- 版の `attachments` が参照を持つので、過去の版に戻すと添付も当時の状態に戻る。
- **blob はどの版からも参照される可能性があるため、削除しない**（完全削除機能は今回の範囲外）。

### 3.2 端末側のみのテーブル

```sql
-- 端末ごとの配置（同期しない）
CREATE TABLE placements (
  note_id      TEXT PRIMARY KEY,
  monitor_key  TEXT,                  -- どのモニターか（名前や識別子）
  x            REAL NOT NULL,
  y            REAL NOT NULL,
  width        REAL NOT NULL,
  height       REAL NOT NULL,
  always_on_top INTEGER NOT NULL DEFAULT 0,  -- 最前面モードか（付箋ごと・端末ごと）
  visible      INTEGER NOT NULL DEFAULT 1    -- デスクトップに表示するか
);

-- 未送信の版（オフライン中の保存を貯める）
CREATE TABLE outbox (
  note_id TEXT NOT NULL,
  rev     INTEGER NOT NULL,
  PRIMARY KEY (note_id, rev)
);

-- 未送信の添付 blob
CREATE TABLE blob_outbox (sha256 TEXT PRIMARY KEY);

-- 編集中の下書き（クラッシュ対策。版ではなく、同期もしない）
CREATE TABLE drafts (
  note_id    TEXT PRIMARY KEY,
  content    TEXT NOT NULL,
  saved_at   TEXT NOT NULL
);

-- 同期の状態
CREATE TABLE sync_state (
  key   TEXT PRIMARY KEY,             -- 'last_server_seq' など
  value TEXT NOT NULL
);
```

- **付箋を貼るかどうかは端末ごと**（D-012）。配置がない付箋はこの端末には貼っていない。
  他の端末で新しく作られた付箋は自動では貼らず、管理画面の一覧から選んだときに貼る。
  ごみ箱の状態は全端末で共通（他の端末でごみ箱から戻された付箋は、この端末に貼ってあった場合だけ貼り直す）。
- `always_on_top` は**端末ごと**に持つ（画面構成が端末ごとに違うため）。

## 4. 保存と同期のタイミング

### 4.1 版が作られる操作

| 操作 | 版の kind | タイミング |
|---|---|---|
| 本文の編集 | `edit` | ダブルクリックで編集開始 → **フォーカスが外れて整形表示に戻った時点**で保存。内容が変わっていなければ版を作らない |
| 色の変更 | `color` | 変更した時点 |
| タイトルの変更 | `title` | 確定した時点（Enter・フォーカスが外れたとき）。同じなら版を作らない |
| 添付の追加・削除（整形表示中） | `attach` / `detach` | 操作した時点 |
| 添付の追加・削除（編集中） | ― | 編集終了時の `edit` 版にまとめて含める |
| 整形表示中のチェックボックス操作 | `edit` | クリックした時点 |
| ごみ箱へ移動 / 復元 | `delete` / `restore` | 操作した時点 |
| 履歴から過去の版に戻す | `revert` | 操作した時点（過去の版の内容で新しい版を作る） |

- 版を保存したら、すぐに `outbox` に入れて同期を試みる。オフラインなら再接続時に送る。
- 配置（位置・サイズ・最前面）の変更は版を作らず、`placements` だけ更新する。

### 4.2 編集中の安全対策

- 編集中は数秒ごとに `drafts` へ下書きを保存する（版でも同期対象でもない）。
- 起動時に `drafts` が残っていたら、その付箋を編集状態で開き直し、下書きを復元する。
- アプリの終了時やOSのシャットダウン時に編集中なら、編集を終了したものとして保存する。

### 4.3 後勝ちと競合

- 順序は**サーバーの受付番号 `server_seq`** で決める。端末の時刻は表示にだけ使う。
- サーバーは受け取った版をすべて記録し、受付順に最新の版とする（後勝ち）。
- 受け取った版の `base_rev` がサーバー上の最新版と違う場合は `conflict = 1` を付ける。
  負けた側の内容も版として残るので、履歴画面から確認・復元できる。
- 版番号 `rev` はサーバーが採番する。端末は仮の番号で保存し、サーバーの応答で確定番号に置き換える。

### 4.4 編集中に他の端末で更新された場合

- 編集中の内容は上書きしない。付箋に「他の端末で更新されました」の表示を出す。
- 編集を終えて保存すると後勝ちで自分の版が最新になり、相手の版は履歴に残る（`conflict = 1`）。

## 5. デスクトップアプリ（fusen-desktop）

### 5.1 付箋ウィンドウ

- 1枚の付箋 = 1つのウィンドウ（サーフェス）。
- 枠なし・透明背景。見た目は角丸の四角＋影（付箋の色で塗る）。
- 上部に細いバーを置き、次のボタンを並べる:
  - **最前面の切り替え**（ピンのアイコン。オン/オフが見て分かること）
  - **タイトル**（ピンの右に 1 行で表示。バーのダブルクリックかメニューで編集。D-013）
  - 色の変更（パレット: 黄・ピンク・青・緑・紫・灰。将来増やせるようにする）
  - メニュー（タイトルを編集、ファイルを添付、ごみ箱へ移動、履歴を見る、デスクトップから外す）
- バーをドラッグすると移動、右下のつまみをドラッグするとサイズ変更。
- 本文の表示:
  - 通常は Markdown を整形して表示（gpui-component の TextView）
  - **ダブルクリック**で編集状態（gpui-component の複数行入力。Markdown の色分けあり）
  - フォーカスが外れたら整形表示に戻り、保存（4.1）
- Markdown は GFM 相当: 見出し、太字/斜体/取り消し線、リスト、チェックボックス、表、
  コード（ブロック・行内）、引用、リンク。リンクはクリックで既定のブラウザで開く。
- 付箋の文字は日本語が正しく表示されること（フォントは設定で変更可能。既定は Noto Sans CJK JP）。

### 5.2 添付

- **任意の形式のファイル**を添付できる（ファイルの受け渡しにも使うため）。
- 追加方法: ファイルのドラッグ＆ドロップ、クリップボードからの貼り付け（画像・ファイル）、メニューから選択。
- 本文中の参照は次の形式で書く（編集中に添付すると、カーソル位置に自動で挿入する）:
  - 画像: `![説明](attachment://<attachment_id>)`
  - その他: `[ファイル名](attachment://<attachment_id>)`
- 整形表示での扱い:
  - **PNG / JPEG / JXL** は本文中に画像として表示する（JXL は `jxl-oxide` で RGBA に変換して表示）
  - 上記以外（WebP や GIF を含む）は、ファイル名・サイズ・アイコン付きのチップとして表示する
- 本文の下に、その付箋の添付の一覧（チップ）を表示する。本文から参照されていない添付もここに出る。
- チップの操作: クリックで既定のアプリで開く、メニューから「名前を付けて保存」「添付を削除」。
- 1ファイルの上限サイズは設定で変更できる（既定 500 MB）。超えたら追加時にエラーを表示する。
- 画像は縮小せず、元のファイルのまま保存・同期する。
- 受信側の取得方針: PNG/JPEG/JXL は同期時にすぐ取得、それ以外は開く・保存するときに取得する。

### 5.3 管理画面（1つのウィンドウ）

- トレイのアイコンやコマンドから開く通常のウィンドウ。
- タブ:
  - **一覧**: 全付箋の一覧（タイトル（なければ本文の 1 行目）・色・更新日時・更新した端末・この端末に貼ってあるか）。
    検索（タイトル・本文・添付ファイル名）。「この端末に貼ってある付箋だけ」の絞り込み。貼る / 外すボタン。
    クリックでその付箋をデスクトップに表示し、前面に出す。
  - **ごみ箱**: 削除した付箋の一覧と復元。
  - **履歴**: 選んだ付箋の版を時系列で表示（日時・端末・操作の種類・競合の印）。
    版を選ぶとその時点の内容を整形表示し、「この版に戻す」で `revert` 版を作る。
    直前の版との差分表示はあると望ましい（後回しでもよい）。
  - **設定**: サーバーURL、トークン、端末名、フォント、添付の上限サイズ、自動起動。
- 同期の状態（接続中 / オフライン / 未送信の件数）を管理画面とトレイのアイコンに表示する。

### 5.4 その他の操作

- トレイ常駐（gpui にはトレイ機能がないので `tray-icon` などのクレートを使う）。
  メニュー: 新しい付箋、管理画面を開く、全付箋を一時的に前面に出す、終了。
- コマンドライン引数:
  - `fusen-desktop --new` … 新しい付箋を作る（起動済みなら既存のプロセスに伝える）
  - `fusen-desktop --show-all` … 全付箋を一時的に前面に出す
  - 2つ目のプロセスは1つ目に命令を渡して終了する（単一インスタンス）
- 全体のショートカットキーは、アプリ自身では登録しない。
  KDE の「ショートカット」設定や Windows のショートカットから上記コマンドを呼び出す想定。
- ログイン時の自動起動（KDE: `~/.config/autostart/` に .desktop、Windows: スタートアップ）を設定から切り替えられる。

### 5.5 CachyOS（KDE Plasma 6 / Wayland）固有

- 付箋は **wlr-layer-shell** のサーフェスとして作る（gpui の layer_shell 機能を使う）。
  - 壁モード: `Layer::Bottom`
  - 最前面モード: `Layer::Top`
  - 位置: 左上に anchor し、margin で座標を表す
  - キーボード: 編集のために `OnDemand` のキーボード操作を有効にする
- layer-shell のサーフェスはOSのウィンドウ移動・サイズ変更が使えないので、
  ドラッグ中のポインタの動きに合わせて margin とサイズを自分で更新する。
- モニターをまたぐ移動は、移動先のモニター（output）でサーフェスを作り直す。
- 表示層の切り替えをその場でできない場合は、サーフェスを作り直して切り替える。
- 仮想デスクトップ: layer-shell のサーフェスは全仮想デスクトップに表示される。**今回はそれでよい**
  （仮想デスクトップごとの表示は、使うようになったら検討する）。
- 管理画面は通常のウィンドウ（xdg-toplevel）。
- 日本語入力（fcitx5）が付箋の編集中に使えること。

### 5.6 Windows 11 固有

- 付箋は枠なし・タスクバーに出さないウィンドウ（`WS_EX_TOOLWINDOW` 相当）。
- gpui のウィンドウから HWND を取り出し、Win32 API で表示層を制御する:
  - 壁モード: `SetWindowPos(HWND_BOTTOM, ...)`。他のウィンドウの操作で前に出てこないよう、
    `WM_WINDOWPOSCHANGING` などで最背面を保つ
  - 最前面モード: `HWND_TOPMOST`
- Win+D（デスクトップの表示）で付箋が隠れるのは**許容する**。
- 位置はモニターをまたいでも絶対座標で保存・復元できる。高DPI（モニターごとの拡大率）に対応する。

## 6. 同期サーバー（fusen-server）

### 6.1 構成

- Rust（axum）＋ SQLite。添付の実体はファイルとして保存する。
- データはすべて1つのディレクトリ（コンテナ内 `/data`）に置き、バックアップしやすくする:
  - `/data/fusen.db`
  - `/data/blobs/<sha256の先頭2文字>/<sha256>`
- 設定は環境変数（`FUSEN_LISTEN`、`FUSEN_DATA_DIR`、`FUSEN_MAX_UPLOAD_BYTES` など）。
- TLS はリバースプロキシ（Caddy）が担当し、サーバー自体は HTTP で待ち受ける。

### 6.2 認証

- 端末ごとのトークンによる Bearer 認証（`Authorization: Bearer <token>`）。
- トークンの発行・一覧・無効化はサーバーのコマンドで行う:
  - `fusen-server token create --device "cachyos-main"` … 新しいトークンを表示（表示は1回だけ）
  - `fusen-server token list`
  - `fusen-server token revoke <id>`
- サーバーにはトークンのハッシュ（SHA-256）だけを保存する。
- WebSocket も同じトークンで認証する。

### 6.3 API（案）

すべて `/api/v1` の下。JSON の型は `fusen-proto` に定義する。

| メソッド | パス | 内容 |
|---|---|---|
| GET | `/healthz` | 死活確認（認証不要） |
| GET | `/changes?since=<server_seq>&limit=<n>` | 指定の受付番号より後の版・添付メタデータを返す |
| POST | `/notes/{note_id}/revisions` | 版を送る（`base_rev` を含む）。確定した `rev`・`server_seq`・`conflict` を返す |
| GET | `/notes/{note_id}/revisions` | その付箋の全版 |
| POST | `/attachments` | 添付のメタデータを登録 |
| HEAD | `/blobs/{sha256}` | 実体がサーバーにあるか確認 |
| PUT | `/blobs/{sha256}` | 実体を送る（ストリーミングで受け取り、ハッシュを検証してから保存） |
| GET | `/blobs/{sha256}` | 実体を取得 |
| GET | `/ws` | WebSocket。新しい版を受け付けたら `{"type":"changed","server_seq":N}` を全端末に送る |

- 同じ版を2回送っても重複しないようにする（`note_id` ＋ 端末の仮番号などで冪等にする）。
- 版を送る前に、その版が参照する添付の blob を送っておく（blob がない版は受け付けない）。
- 上限を超える blob は 413 を返す。

### 6.4 同期の流れ（クライアント側）

1. 起動時・再接続時: `GET /changes?since=<last_server_seq>` で差分を取り込み、
   `outbox` に貯まった版を順に送る
2. 接続中: WebSocket を張り、`changed` を受けたら差分を取りに行く
3. 版を保存したら: 必要な blob を送る → 版を送る → 応答で確定番号に更新
4. 切断されたら: 間隔を少しずつ延ばしながら再接続する

### 6.5 デプロイ

- **テスト**: CachyOS 上の podman（Quadlet の `deploy/fusen-server.container`）
- **本番**: Proxmox 上の Docker（`deploy/compose.yaml`）
- イメージは `deploy/Containerfile` で、podman でも Docker でもビルドできること（マルチステージビルド）。
- リバースプロキシは Caddy。`deploy/Caddyfile.example` に例を置く:
  - WebSocket の中継
  - アップロードの上限（`request_body max_size`）をサーバーの上限に合わせる

## 7. 範囲外（今回は作らない）

- Android 版（CachyOS / Windows 版の完成後に別途計画する）
- GNOME など KDE 以外のデスクトップ環境への対応
- 仮想デスクトップごとの付箋の表示
- 付箋の完全削除・blob の掃除
- 複数ユーザー・共有権限
- 付箋の紙の質感（透明度付きPNGの見た目）。まずは角丸の四角＋影

## 8. 未決事項（実装中に必要になったらユーザーに確認）

- 付箋の既定サイズ、パレットの具体的な色
- 履歴の差分表示の方式
- トレイアイコンのデザイン
