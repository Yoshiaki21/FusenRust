# 実装中に決めたこと（decisions）

小さな判断の記録。大きな方針はユーザーに確認してから書く。

---

## D-001 gpui / gpui-component の固定方法（2026-11-23）

- gpui-component は現在、gpui を **git ではなく crates.io の `gpui-pre`（Zed の gpui のスナップショット）** から
  `=0.3.12` の厳密指定で使っている。gpui-component の git の rev を固定すれば gpui も自動的に一致する。
- そこで `Cargo.toml` では **`gpui-kit`（gpui + gpui-base + gpui-component + アイコン）を git の rev で固定**し、
  gpui は直接依存に書かない（`gpui_kit::*` から使う）。
  - gpui-kit rev: `48e53b11762a7ac37835acca889a924c569ff681`（2026-11-23）
  - これに対応する gpui: `gpui-pre =0.3.12`
- 理由: gpui-component が要求する gpui のバージョンを崩さないため。gpui を別に指定すると二重になり不整合の元になる。
- 更新するときは gpui-kit の rev だけを変え、専用のコミットに分ける。

## D-002 トレイは Linux では `ksni`、Windows では `tray-icon`（2026-11-23）

- `tray-icon` は Linux では GTK のメインループが必要で、gpui のイベントループと別スレッドで GTK を回す必要がある。
- KDE のトレイは StatusNotifierItem（D-Bus）なので、D-Bus だけで動く `ksni` を Linux で使う。
  メニュー操作は ksni のスレッドから `async-channel` で gpui 側に送る。
- Windows は Phase 5 で `tray-icon` を使う（Windows では GTK 不要）。

---

## Phase 0 検証結果（CachyOS / KDE Plasma 6 / Wayland）

検証プログラム: `crates/fusen-desktop/examples/`

| 例 | 内容 |
|---|---|
| `p0_note_surface` | 付箋 1 枚 = layer-shell サーフェス 1 つ（仕様どおりの方式） |
| `p0_wall_overlay` | モニター全面の透明サーフェス ＋ 入力領域の制限（回避策の方式） |
| `p0_tray` | ksni のトレイと gpui のイベントループの共存 |
| `p0_kwin_window` | 付箋 1 枚 = 普通のウィンドウ 1 つ ＋ KWin スクリプトで常に背面/最前面（方式 D） |

「コード」= gpui / gpui-component のソースで API を確認した結果。
「起動」= Claude Code で実際に起動してエラーが出ないことを確認した結果。
見た目と操作はユーザーの手動確認が必要。

| 項目 | 結果 | 備考 |
|---|---|---|
| Bottom 層に枠なし・透明・角丸の四角 | **できる**（コード・起動） | `WindowKind::LayerShell` ＋ `WindowBackgroundAppearance::Transparent`。gpui-component の `Root` は背景を塗らない |
| 左上 anchor ＋ margin で好きな位置に置く | **できる**（コード・起動） | ただし margin は**作成時にしか指定できない** |
| ドラッグで移動 | **そのままではできない → 回避策あり** | gpui に layer surface の `set_margin` を後から呼ぶ API がない（`set_size` だけある）。下記「移動の問題」参照 |
| Bottom ⇔ Top の切り替え | **作り直しが必要** | gpui に層を変える API がない。方式 A は作り直し、方式 B は別のサーフェスへ移す |
| サイズ変更 | **できる**（コード） | `Window::resize` → `layer_surface.set_size` |
| 2 台目のモニターに表示 | **できる**（コード・起動） | `WindowOptions::display_id` → 出力（wl_output）を指定。2 台を認識した |
| TextView で Markdown（画像を含む） | **できる**（コード） | `TextView::markdown(...).image_source(...)` で `attachment://` を解決できる。`on_task_toggle` でチェックボックス、`on_link_click` でリンクを扱える |
| ダブルクリックで複数行入力 ⇔ 整形表示 | **できる**（コード） | `MouseDownEvent::click_count == 2` で `EditorState`（Markdown の色分けあり）に切り替え、`cx.on_blur` で戻す |
| layer-shell 上で fcitx5 の日本語入力 | **要手動確認** | gpui は text-input-v3 を使う。KWin が layer-shell サーフェスに IME を出すかは実機で確認する |
| JXL を jxl-oxide でデコードして表示 | **できる**（起動） | `jxl_oxide::integration::JxlDecoder` → `image::DynamicImage` → BGRA に並べ替えて `RenderImage`。起動時にデコードのエラーなし |
| ファイルのドラッグ＆ドロップ | **できる**（コード） | `.on_drop(\|paths: &ExternalPaths, ..\|)`。layer-shell サーフェスで DnD が届くかは手動確認 |
| トレイアイコン（イベントループと共存） | **できる**（起動） | D-002 のとおり ksni を使用 |

### 移動の問題と回避策（ユーザーと相談が必要）

gpui（`gpui-pre 0.3.12`）では、layer-shell サーフェスの **margin（位置）と層を作成後に変えられない**。

- **方式 A: 付箋 1 枚 = サーフェス 1 つ（仕様どおり）**
  - 移動はドラッグ終了時にサーフェスを作り直す。ドラッグ中は付箋が動かない（移動量だけ表示）。
  - 層の切り替えも作り直し。作り直しの瞬間にちらつく可能性がある。
- **方式 B: モニター × 層 ごとに全面の透明サーフェス（推奨）**
  - 付箋はサーフェス内の要素。`Window::set_input_region` で付箋の矩形だけ入力を受け、それ以外のクリックは下に抜ける。
  - ドラッグに滑らかに追従する。層の切り替えは Top 側のサーフェスへ移すだけ。
  - モニターをまたぐ移動は、ドラッグ終了時に移動先のモニターのサーフェスへ移す。
  - 注意: サーフェスの数は「モニター数 × 2」で固定。全面の透明サーフェスなので GPU メモリを少し多く使う。
- **方式 C: gpui をフォークして `set_margin` / `set_layer` を足す**
  - 変更自体は小さいが、gpui の更新のたびにフォークを追従する必要がある。

### ユーザーの手動確認の結果（2026-11-23）

- 方式 A: 最前面の切り替えと本文の編集はできた。ドラッグでの移動はうまくできない。
- 方式 B: 透明になるはずの全面サーフェスが**白く**表示され、最前面に出て、**画面全体を覆わない**。
  白い範囲の中では移動・編集はできた。→ gpui の layer-shell は四辺 anchor（コンポジターがサイズを決める）の扱いに問題がある模様。不採用候補。
- トレイの例（普通のウィンドウ）: KDE のウィンドウメニューに「常に最前面に表示」があり、使えることを確認。

### 方式 D: 普通のウィンドウ ＋ KWin スクリプト（推奨・`p0_kwin_window`）

- 付箋は普通のウィンドウ（xdg-toplevel、枠なし・透明背景）。
- Wayland にはアプリが自分のウィンドウを常に背面/最前面にするプロトコルがないので、
  **KWin スクリプトを D-Bus（`org.kde.KWin /Scripting` の `loadScript` → `run`）で実行**して設定する。
  - 壁モード = `keepBelow`、最前面モード = `keepAbove`
  - `skipTaskbar` / `skipPager` / `skipSwitcher` / `onAllDesktops` も設定する
  - ウィンドウはタイトル（`fusen-note:<id>`。枠なしなので画面に出ない）で見分ける
- 移動・サイズ変更は KWin の機能（`Window::start_window_move` / `start_window_resize`）を使う。
- 起動して KWin から読み出し、keepBelow・skipTaskbar・onAllDesktops が設定されたことを確認した。
- 位置の復元: Wayland ではアプリが自分の位置を指定できないが、KWin スクリプトで `frameGeometry` を設定できる。
  位置の保存は、KWin スクリプトから `callDBus` でアプリに知らせる方式を Phase 2 で検証する。
- KWin スクリプトは同じプラグイン名を使い回すため、呼び出しは 1 つずつ行う（同時に呼ぶと競合した）。
- 注意: 「デスクトップを表示」（Meta+D）では隠れる（Windows 版と同じく許容の想定）。

### ドロップした画像の扱い（検証の実装）

- 整形表示中にドロップ → 本文の末尾に `![名前](attachment://id)` を追加（＝付箋の一番下に表示）。
- 編集中にドロップ → カーソル位置に挿入（仕様 5.2）。
- PNG / JPEG / JXL 以外はリンク `[📎 名前](attachment://id)` として追加する。

### 気づいた点

- `cx.displays()` の `bounds()` で、2 台目の原点が x=1324.5 なのに 1 台目の幅が 1920 と出た。
  1 台目に拡大率（約 1.45）がかかっていて、原点は論理座標・サイズは物理ピクセルのように見える。
  モニターをまたぐ移動の座標計算は、実機の構成で確かめてから本実装する。
- 既定の UI フォントは "Adwaita Sans" に解決された。日本語用の既定フォント（Noto Sans CJK JP）は Phase 2 で設定する。
