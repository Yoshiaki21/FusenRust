//! KWin スクリプトを D-Bus 経由で実行し、普通のウィンドウ（xdg-toplevel）の扱いを変える。
//!
//! Wayland では、アプリが自分のウィンドウを「常に背面」「常に最前面」にしたり、
//! タスクバーから外したり、位置を指定したりするプロトコルがない。
//! KDE Plasma では KWin のスクリプト（JavaScript）からならこれらを変えられるので、
//! `org.kde.KWin /Scripting` の `loadScript` → `run` で一時的なスクリプトを実行する。
//!
//! ウィンドウはタイトル（caption）で見分ける。付箋のウィンドウは枠なしなので、
//! タイトルは画面には出ない。

use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::Context as _;

/// 付箋ウィンドウのタイトル（KWin スクリプトで見分けるための印）
pub fn note_caption(id: usize) -> String {
    format!("fusen-note:{id}")
}

/// 付箋ウィンドウの層
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NoteLayer {
    /// 壁モード: 常に背面（他のウィンドウより下、デスクトップのアイコンより上）
    Wall,
    /// 最前面モード: 常に最前面
    Top,
}

/// 付箋ウィンドウの層を設定し、タスクバー・ページャー・Alt+Tab から外し、全仮想デスクトップに表示する。
///
/// `geometry` を渡すと、その位置・サイズ（論理座標）に置く。
pub fn apply_note_window(
    caption: &str,
    layer: NoteLayer,
    geometry: Option<(f64, f64, f64, f64)>,
) -> anyhow::Result<()> {
    let (below, above) = match layer {
        NoteLayer::Wall => (true, false),
        NoteLayer::Top => (false, true),
    };
    let geometry_js = match geometry {
        Some((x, y, w, h)) => {
            format!("w.frameGeometry = {{x: {x}, y: {y}, width: {w}, height: {h}}};")
        }
        None => String::new(),
    };
    // caption は自分で作った文字列（英数字と記号のみ）なので、そのまま埋め込んでよい
    let script = format!(
        r#"
for (const w of workspace.windowList()) {{
    if (w.caption !== "{caption}") continue;
    w.keepBelow = {below};
    w.keepAbove = {above};
    w.skipTaskbar = true;
    w.skipPager = true;
    w.skipSwitcher = true;
    w.onAllDesktops = true;
    {geometry_js}
}}
"#
    );
    run_script(&script, "fusen-window-control", false)
}

/// 付箋ウィンドウの最大化・タイル化（画面の端へドラッグしたときの全画面・半分表示）を止める
/// 常駐スクリプトを KWin に読み込む。アプリの終了時に [`uninstall_guard`] で解除する。
///
/// Wayland のウィンドウにはアプリ側から「最大化させない」と伝える手段がなく、
/// KWin のウィンドウルール（maximizehoriz/maximizevert）はユーザー全体の設定を書き換えてしまう。
/// そこでアプリが動いている間だけ、付箋ウィンドウがタイル化・最大化されたら即座に元へ戻す。
/// 取り消すと KWin はドラッグ前の位置に戻すので、ドラッグ中に覚えた位置（画面内に補正）へ置き直す。
pub fn install_guard() -> anyhow::Result<()> {
    let script = r#"
// 付箋を画面（パネルを除く領域）の中に収める
function clampToArea(w, g) {
    const a = workspace.clientArea(KWin.MaximizeArea, w);
    const x = Math.min(Math.max(g.x, a.x), a.x + a.width - g.width);
    const y = Math.min(Math.max(g.y, a.y), a.y + a.height - g.height);
    return {x: x, y: y, width: g.width, height: g.height};
}
function guard(w) {
    if (w.__fusenGuard || w.caption.indexOf("fusen-note:") !== 0) return;
    w.__fusenGuard = true;
    w.__fusenLast = null;
    // ドラッグ中の位置を覚えておく（タイル化を取り消すと KWin はドラッグ前の位置に戻すため）
    w.interactiveMoveResizeStarted.connect(function () { w.__fusenLast = null; });
    w.interactiveMoveResizeStepped.connect(function (g) {
        if (!w.tile && w.maximizeMode === 0) {
            w.__fusenLast = {x: g.x, y: g.y, width: g.width, height: g.height};
        }
    });
    // 取り消した後、ドラッグを離した位置に置き直す
    function restoreDropPosition() {
        if (w.__fusenLast) w.frameGeometry = clampToArea(w, w.__fusenLast);
    }
    w.tileChanged.connect(function () {
        if (w.tile) { w.tile = null; restoreDropPosition(); }
    });
    w.maximizedChanged.connect(function () {
        if (w.maximizeMode !== 0) { w.setMaximize(false, false); restoreDropPosition(); }
    });
}
function watch(w) {
    guard(w);
    // タイトルは表示後に設定されることがあるので、変わったときにも確認する
    w.captionChanged.connect(function () { guard(w); });
}
for (const w of workspace.windowList()) watch(w);
workspace.windowAdded.connect(watch);
"#;
    run_script(script, GUARD_PLUGIN, true)
}

/// [`install_guard`] で読み込んだ常駐スクリプトを解除する。
pub fn uninstall_guard() -> anyhow::Result<()> {
    let conn = zbus::blocking::Connection::session().context("セッションバスに接続できません")?;
    unload(&conn, GUARD_PLUGIN);
    Ok(())
}

const GUARD_PLUGIN: &str = "fusen-guard";

fn unload(conn: &zbus::blocking::Connection, plugin: &str) {
    let _ = conn.call_method(
        Some("org.kde.KWin"),
        "/Scripting",
        Some("org.kde.kwin.Scripting"),
        "unloadScript",
        &(plugin,),
    );
}

/// 一時ファイルに書いた KWin スクリプトを読み込んで実行する。
/// `keep` が false なら実行後すぐに読み込みを解除する（1 回きりの操作）。
fn run_script(source: &str, plugin: &str, keep: bool) -> anyhow::Result<()> {
    // 同じプラグイン名を使い回すので、複数の付箋から同時に呼ばれても 1 つずつ実行する
    static LOCK: Mutex<()> = Mutex::new(());
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let _guard = LOCK.lock().unwrap_or_else(|e| e.into_inner());

    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    let path: PathBuf =
        std::env::temp_dir().join(format!("fusen-kwin-{}-{seq}.js", std::process::id()));
    std::fs::write(&path, source).with_context(|| format!("スクリプトを書けません: {path:?}"))?;

    let conn = zbus::blocking::Connection::session().context("セッションバスに接続できません")?;

    // 同じ名前のスクリプトが残っていると loadScript が失敗するので、先に解除しておく
    // （前回のアプリが異常終了して常駐スクリプトが残っている場合もここで入れ替わる）
    unload(&conn, plugin);

    let path_str = path.to_string_lossy().to_string();
    let result = (|| {
        let reply = conn
            .call_method(
                Some("org.kde.KWin"),
                "/Scripting",
                Some("org.kde.kwin.Scripting"),
                "loadScript",
                &(path_str.as_str(), plugin),
            )
            .context("KWin の loadScript に失敗")?;
        let id: i32 = reply
            .body()
            .deserialize()
            .context("loadScript の応答が不正")?;
        anyhow::ensure!(id >= 0, "KWin がスクリプトを読み込めませんでした (id={id})");

        conn.call_method(
            Some("org.kde.KWin"),
            format!("/Scripting/Script{id}").as_str(),
            Some("org.kde.kwin.Script"),
            "run",
            &(),
        )
        .context("KWin スクリプトの実行に失敗")?;
        Ok(())
    })();

    if !keep {
        unload(&conn, plugin);
    }
    // KWin は run の時点でスクリプトを読み終えているので、ファイルは消してよい
    let _ = std::fs::remove_file(&path);
    result
}
