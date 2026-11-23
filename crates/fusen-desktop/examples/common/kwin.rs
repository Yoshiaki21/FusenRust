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
    run_script(&script)
}

/// 一時ファイルに書いた KWin スクリプトを読み込んで実行し、すぐに読み込みを解除する。
fn run_script(source: &str) -> anyhow::Result<()> {
    const PLUGIN: &str = "fusen-window-control";
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
    let _ = conn.call_method(
        Some("org.kde.KWin"),
        "/Scripting",
        Some("org.kde.kwin.Scripting"),
        "unloadScript",
        &(PLUGIN,),
    );

    let path_str = path.to_string_lossy().to_string();
    let reply = conn
        .call_method(
            Some("org.kde.KWin"),
            "/Scripting",
            Some("org.kde.kwin.Scripting"),
            "loadScript",
            &(path_str.as_str(), PLUGIN),
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

    let _ = conn.call_method(
        Some("org.kde.KWin"),
        "/Scripting",
        Some("org.kde.kwin.Scripting"),
        "unloadScript",
        &(PLUGIN,),
    );
    let _ = std::fs::remove_file(&path);
    Ok(())
}
