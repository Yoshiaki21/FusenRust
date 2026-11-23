//! KDE Plasma 6（Wayland）固有の処理。
//!
//! ## 付箋ウィンドウの制御（docs/decisions.md D-003）
//! Wayland には、アプリが自分のウィンドウを「常に背面 / 常に最前面」にしたり、位置を指定したり、
//! タスクバーから外したりするプロトコルがない。KDE では KWin のスクリプト（JavaScript）からなら
//! できるので、D-Bus（`org.kde.KWin /Scripting`）でスクリプトを読み込んで実行する。
//!
//! - 1 回きりの操作（層・位置の設定）: [`apply_note_window`] など。専用のスレッドで 1 つずつ実行する
//!   （同じプラグイン名を使い回すため、同時に実行すると競合する）
//! - 常駐の制御スクリプト（[`install_window_control`]）:
//!   - 画面端へのドラッグによる最大化・タイル化を取り消し、離した位置に置き直す
//!   - 移動・サイズ変更が終わったら、位置をアプリに D-Bus で知らせる（Wayland ではアプリが自分の位置を知れないため）
//!
//! ## 単一インスタンスとアプリへの命令
//! D-Bus のセッションバスで名前（[`bus_name`]）を所有できたプロセスが 1 つ目。
//! 2 つ目のプロセスは 1 つ目の `Command` メソッドを呼んで終了する。

use std::path::PathBuf;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc;

use anyhow::Context as _;
use async_channel::Sender;
use ksni::blocking::TrayMethods as _;

use super::{AppEvent, Command, caption_prefix, note_id_from_caption};

/// D-Bus のオブジェクトのパスとインターフェース名
const OBJECT_PATH: &str = "/io/github/yoshiaki21/FusenRust";
const INTERFACE: &str = "io.github.yoshiaki21.FusenRust";

/// D-Bus の名前（プロファイルごとに分ける）
pub fn bus_name(profile: Option<&str>) -> String {
    match profile {
        // D-Bus の名前の要素に使える文字は英数字と _ だけ
        Some(p) => {
            let safe: String = p
                .chars()
                .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
                .collect();
            format!("{INTERFACE}.p_{safe}")
        }
        None => INTERFACE.to_string(),
    }
}

/// 位置とサイズ（論理座標）
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Geometry {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

// ----------------------------------------------------------------------
// 起動・単一インスタンス
// ----------------------------------------------------------------------

/// 1 つ目のプロセスが持つ OS 側の資源（drop すると D-Bus の名前やトレイを手放す）
pub struct PlatformHandle {
    _conn: zbus::blocking::Connection,
    _tray: Option<ksni::blocking::Handle<FusenTray>>,
}

/// D-Bus の名前を取り、命令の受け口・トレイ・シグナルの監視を始める。
///
/// 既に別のプロセスが動いていれば `Ok(None)` を返す（呼び出し側は [`send_to_running`] で命令を渡して終了する）。
pub fn start(
    profile: Option<&str>,
    tx: Sender<AppEvent>,
) -> anyhow::Result<Option<PlatformHandle>> {
    let service = Service {
        tx: tx.clone(),
        profile: profile.map(str::to_string),
    };
    let conn = zbus::blocking::connection::Builder::session()
        .context("セッションバスに接続できません")?
        .serve_at(OBJECT_PATH, service)?
        .build()
        .context("D-Bus に接続できません")?;
    // 名前の取得: 既定の Builder::name は後から来たプロセスに名前を奪われる設定（AllowReplacement）なので使わない。
    // 置き換えを許さず、順番待ちもしない（DoNotQueue のみ）。既に所有者がいれば 2 つ目のプロセス。
    let reply = match conn.request_name_with_flags(
        bus_name(profile),
        zbus::fdo::RequestNameFlags::DoNotQueue.into(),
    ) {
        Ok(r) => r,
        // zbus は「既に所有者がいる」をエラーで返す
        Err(zbus::Error::NameTaken) => return Ok(None),
        Err(e) => return Err(e).context("D-Bus の名前を取得できません"),
    };
    match reply {
        zbus::fdo::RequestNameReply::PrimaryOwner | zbus::fdo::RequestNameReply::AlreadyOwner => {}
        zbus::fdo::RequestNameReply::Exists | zbus::fdo::RequestNameReply::InQueue => {
            return Ok(None);
        }
    }

    let tray = match (FusenTray {
        tx: tx.clone(),
        profile: profile.map(str::to_string),
    })
    .spawn()
    {
        Ok(h) => Some(h),
        Err(e) => {
            // トレイがなくても付箋は使えるので、警告だけにする
            tracing::warn!("トレイを作れません: {e}");
            None
        }
    };

    watch_signals(tx);
    Ok(Some(PlatformHandle {
        _conn: conn,
        _tray: tray,
    }))
}

/// 動いている 1 つ目のプロセスに命令を渡す
pub fn send_to_running(profile: Option<&str>, commands: &[Command]) -> anyhow::Result<()> {
    let conn = zbus::blocking::Connection::session().context("セッションバスに接続できません")?;
    let args: Vec<String> = commands.iter().map(|c| c.as_arg().to_string()).collect();
    conn.call_method(
        Some(bus_name(profile).as_str()),
        OBJECT_PATH,
        Some(INTERFACE),
        "Command",
        &(args,),
    )
    .context("動いている FusenRust に命令を渡せません")?;
    Ok(())
}

/// D-Bus で公開するオブジェクト
struct Service {
    tx: Sender<AppEvent>,
    profile: Option<String>,
}

#[zbus::interface(name = "io.github.yoshiaki21.FusenRust")]
impl Service {
    /// 2 つ目のプロセスからの命令（コマンドライン引数）。命令がなければ管理画面を開く
    fn command(&self, args: Vec<String>) {
        let mut cmds = Command::from_args(&args);
        if cmds.is_empty() {
            cmds.push(Command::OpenManager);
        }
        for c in cmds {
            let _ = self.tx.send_blocking(AppEvent::Command(c));
        }
    }

    /// KWin の制御スクリプトからの通知: 付箋ウィンドウの位置・サイズ（"x,y,幅,高さ"）
    fn window_geometry(&self, caption: String, output: String, geometry: String) {
        let Some(note_id) = note_id_from_caption(self.profile.as_deref(), &caption) else {
            return;
        };
        let v: Vec<f64> = geometry
            .split(',')
            .filter_map(|s| s.trim().parse().ok())
            .collect();
        let [x, y, width, height] = v[..] else {
            tracing::warn!("KWin からの位置が読めません: {geometry}");
            return;
        };
        let _ = self.tx.send_blocking(AppEvent::WindowGeometry {
            note_id,
            monitor: (!output.is_empty()).then_some(output),
            x,
            y,
            width,
            height,
        });
    }
}

/// SIGTERM / SIGINT / SIGHUP（ログアウト・シャットダウン・Ctrl+C）で、編集中の内容を保存してから終了する
fn watch_signals(tx: Sender<AppEvent>) {
    use signal_hook::consts::{SIGHUP, SIGINT, SIGTERM};
    match signal_hook::iterator::Signals::new([SIGTERM, SIGINT, SIGHUP]) {
        Ok(mut signals) => {
            std::thread::spawn(move || {
                if let Some(sig) = signals.forever().next() {
                    tracing::info!("シグナル {sig} を受けたので終了します");
                    let _ = tx.send_blocking(AppEvent::Command(Command::Quit));
                }
            });
        }
        Err(e) => tracing::warn!("シグナルを監視できません: {e}"),
    }
}

// ----------------------------------------------------------------------
// トレイ
// ----------------------------------------------------------------------

/// トレイ（StatusNotifierItem）。メニュー操作は ksni のスレッドから呼ばれるので、チャンネルで送る
pub struct FusenTray {
    tx: Sender<AppEvent>,
    profile: Option<String>,
}

impl FusenTray {
    fn send(&self, c: Command) {
        let _ = self.tx.send_blocking(AppEvent::Command(c));
    }
}

impl ksni::Tray for FusenTray {
    fn id(&self) -> String {
        match &self.profile {
            Some(p) => format!("fusen-rust-{p}"),
            None => "fusen-rust".into(),
        }
    }
    fn title(&self) -> String {
        match &self.profile {
            Some(p) => format!("FusenRust ({p})"),
            None => "FusenRust".into(),
        }
    }
    fn icon_name(&self) -> String {
        // 仮のアイコン（KDE テーマのアイコン）。デザインは未決事項（docs/decisions.md D-005）
        "knotes".into()
    }
    fn activate(&mut self, _x: i32, _y: i32) {
        self.send(Command::OpenManager);
    }
    fn menu(&self) -> Vec<ksni::MenuItem<Self>> {
        use ksni::menu::*;
        let item = |label: &str, c: Command| -> ksni::MenuItem<Self> {
            StandardItem {
                label: label.into(),
                activate: Box::new(move |t: &mut Self| t.send(c)),
                ..Default::default()
            }
            .into()
        };
        vec![
            item("新しい付箋", Command::NewNote),
            item("管理画面を開く", Command::OpenManager),
            item("全付箋を一時的に前面に出す / 戻す", Command::ShowAll),
            MenuItem::Separator,
            item("終了", Command::Quit),
        ]
    }
}

// ----------------------------------------------------------------------
// KWin スクリプト
// ----------------------------------------------------------------------

enum KwinJob {
    /// スクリプトを読み込んで実行する。`keep` が false なら実行後に解除する
    Run {
        source: String,
        plugin: String,
        keep: bool,
    },
}

/// KWin への操作を 1 つずつ実行するスレッドへの送り口
fn kwin_queue() -> &'static mpsc::Sender<KwinJob> {
    static QUEUE: OnceLock<mpsc::Sender<KwinJob>> = OnceLock::new();
    QUEUE.get_or_init(|| {
        let (tx, rx) = mpsc::channel::<KwinJob>();
        std::thread::Builder::new()
            .name("kwin-script".into())
            .spawn(move || {
                let conn = match zbus::blocking::Connection::session() {
                    Ok(c) => c,
                    Err(e) => {
                        tracing::error!("KWin 用にセッションバスへ接続できません: {e}");
                        return;
                    }
                };
                for job in rx {
                    let KwinJob::Run {
                        source,
                        plugin,
                        keep,
                    } = job;
                    if let Err(e) = run_script(&conn, &source, &plugin, keep) {
                        tracing::warn!("KWin スクリプトの実行に失敗: {e:#}");
                    }
                }
            })
            .ok();
        tx
    })
}

fn oneshot_plugin(profile: Option<&str>) -> String {
    format!("fusen-oneshot-{}", profile.unwrap_or("default"))
}

fn control_plugin(profile: Option<&str>) -> String {
    format!("fusen-control-{}", profile.unwrap_or("default"))
}

/// JavaScript の文字列リテラルにする（タイトルなどを埋め込むため）
fn js_str(s: &str) -> String {
    serde_json::to_string(s).unwrap_or_else(|_| "\"\"".into())
}

/// 付箋ウィンドウの層と表示を設定する。`geometry` があればその位置・サイズに置く。
///
/// 全仮想デスクトップに表示し、タスクバー・ページャー・Alt+Tab から外す。
pub fn apply_note_window(
    profile: Option<&str>,
    caption: &str,
    on_top: bool,
    geometry: Option<Geometry>,
) {
    let geometry_js = geometry
        .map(|g| {
            format!(
                "w.frameGeometry = {{x: {}, y: {}, width: {}, height: {}}};",
                g.x, g.y, g.width, g.height
            )
        })
        .unwrap_or_default();
    let source = format!(
        r#"
for (const w of workspace.windowList()) {{
    if (w.caption !== {caption}) continue;
    w.keepBelow = {below};
    w.keepAbove = {above};
    w.skipTaskbar = true;
    w.skipPager = true;
    w.skipSwitcher = true;
    w.onAllDesktops = true;
    {geometry_js}
}}
"#,
        caption = js_str(caption),
        below = !on_top,
        above = on_top,
    );
    submit_oneshot(profile, source);
}

/// 付箋を前面に出してフォーカスする（管理画面の一覧から選んだとき）。
///
/// 壁モードの付箋は一時的に常に背面を外す。フォーカスが外れたらアプリが元の層に戻す。
pub fn activate_note_window(profile: Option<&str>, caption: &str) {
    let source = format!(
        r#"
for (const w of workspace.windowList()) {{
    if (w.caption !== {caption}) continue;
    w.keepBelow = false;
    w.minimized = false;
    workspace.activeWindow = w;
}}
"#,
        caption = js_str(caption)
    );
    submit_oneshot(profile, source);
}

/// このプロファイルの全付箋を常に最前面にする（「全付箋を一時的に前面に出す」）。
/// 元に戻すときはアプリが付箋ごとに [`apply_note_window`] を呼ぶ。
pub fn raise_all_note_windows(profile: Option<&str>) {
    let source = format!(
        r#"
for (const w of workspace.windowList()) {{
    if (w.caption.indexOf({prefix}) !== 0) continue;
    w.keepBelow = false;
    w.keepAbove = true;
}}
"#,
        prefix = js_str(&caption_prefix(profile))
    );
    submit_oneshot(profile, source);
}

fn submit_oneshot(profile: Option<&str>, source: String) {
    let _ = kwin_queue().send(KwinJob::Run {
        source,
        plugin: oneshot_plugin(profile),
        keep: false,
    });
}

/// 常駐の制御スクリプトを読み込む（アプリの起動時）
pub fn install_window_control(profile: Option<&str>) {
    let source = format!(
        r#"
const PREFIX = {prefix};
function report(w) {{
    const g = w.frameGeometry;
    callDBus({bus}, {path}, {iface}, "WindowGeometry",
             w.caption, w.output ? w.output.name : "",
             g.x + "," + g.y + "," + g.width + "," + g.height);
}}
// 付箋を画面（パネルを除く領域）の中に収める
function clampToArea(w, g) {{
    const a = workspace.clientArea(KWin.MaximizeArea, w);
    const x = Math.min(Math.max(g.x, a.x), a.x + a.width - g.width);
    const y = Math.min(Math.max(g.y, a.y), a.y + a.height - g.height);
    return {{x: x, y: y, width: g.width, height: g.height}};
}}
function guard(w) {{
    if (w.__fusenGuard || w.caption.indexOf(PREFIX) !== 0) return;
    w.__fusenGuard = true;
    w.__fusenLast = null;
    // ドラッグ中の位置を覚えておく（タイル化を取り消すと KWin はドラッグ前の位置に戻すため）
    w.interactiveMoveResizeStarted.connect(function () {{ w.__fusenLast = null; }});
    w.interactiveMoveResizeStepped.connect(function (g) {{
        if (!w.tile && w.maximizeMode === 0) {{
            w.__fusenLast = {{x: g.x, y: g.y, width: g.width, height: g.height}};
        }}
    }});
    w.interactiveMoveResizeFinished.connect(function () {{ report(w); }});
    function restoreDropPosition() {{
        if (w.__fusenLast) w.frameGeometry = clampToArea(w, w.__fusenLast);
        report(w);
    }}
    // 画面端へのドラッグによる最大化・タイル化を取り消す
    w.tileChanged.connect(function () {{
        if (w.tile) {{ w.tile = null; restoreDropPosition(); }}
    }});
    w.maximizedChanged.connect(function () {{
        if (w.maximizeMode !== 0) {{ w.setMaximize(false, false); restoreDropPosition(); }}
    }});
    report(w);
}}
function watch(w) {{
    guard(w);
    // タイトルは表示後に設定されることがあるので、変わったときにも確認する
    w.captionChanged.connect(function () {{ guard(w); }});
}}
for (const w of workspace.windowList()) watch(w);
workspace.windowAdded.connect(watch);
"#,
        prefix = js_str(&caption_prefix(profile)),
        bus = js_str(&bus_name(profile)),
        path = js_str(OBJECT_PATH),
        iface = js_str(INTERFACE),
    );
    let _ = kwin_queue().send(KwinJob::Run {
        source,
        plugin: control_plugin(profile),
        keep: true,
    });
}

/// 常駐の制御スクリプトを解除する（アプリの終了時。終了前に確実に実行するため、この場で実行する）
pub fn uninstall_window_control(profile: Option<&str>) {
    match zbus::blocking::Connection::session() {
        Ok(conn) => unload(&conn, &control_plugin(profile)),
        Err(e) => tracing::warn!("制御スクリプトを解除できません: {e}"),
    }
}

fn unload(conn: &zbus::blocking::Connection, plugin: &str) {
    let _ = conn.call_method(
        Some("org.kde.KWin"),
        "/Scripting",
        Some("org.kde.kwin.Scripting"),
        "unloadScript",
        &(plugin,),
    );
}

/// 一時ファイルに書いた KWin スクリプトを読み込んで実行する
fn run_script(
    conn: &zbus::blocking::Connection,
    source: &str,
    plugin: &str,
    keep: bool,
) -> anyhow::Result<()> {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    let path: PathBuf =
        std::env::temp_dir().join(format!("fusen-kwin-{}-{seq}.js", std::process::id()));
    std::fs::write(&path, source).with_context(|| format!("スクリプトを書けません: {path:?}"))?;

    // 同じ名前のスクリプトが残っていると loadScript が失敗するので、先に解除しておく
    // （前回のアプリが異常終了して常駐スクリプトが残っている場合もここで入れ替わる）
    unload(conn, plugin);

    let result = (|| {
        let path_str = path.to_string_lossy().to_string();
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
        unload(conn, plugin);
    }
    // KWin は run の時点でスクリプトを読み終えているので、ファイルは消してよい
    let _ = std::fs::remove_file(&path);
    result
}

// ----------------------------------------------------------------------
// 自動起動
// ----------------------------------------------------------------------

fn autostart_file(profile: Option<&str>) -> anyhow::Result<PathBuf> {
    let name = match profile {
        Some(p) => format!("fusen-rust-{p}.desktop"),
        None => "fusen-rust.desktop".to_string(),
    };
    Ok(dirs::config_dir()
        .context("設定ディレクトリが分かりません")?
        .join("autostart")
        .join(name))
}

/// ログイン時の自動起動を切り替える（`~/.config/autostart/` の .desktop ファイル）
pub fn set_autostart(profile: Option<&str>, enabled: bool) -> anyhow::Result<()> {
    let file = autostart_file(profile)?;
    if !enabled {
        if file.exists() {
            std::fs::remove_file(&file)?;
        }
        return Ok(());
    }
    let exe = std::env::current_exe().context("実行ファイルの場所が分かりません")?;
    let mut exec = format!("\"{}\"", exe.display());
    if let Some(p) = profile {
        exec.push_str(&format!(" --profile {p}"));
    }
    let content = format!(
        "[Desktop Entry]\nType=Application\nName=FusenRust\nComment=デスクトップの付箋\nExec={exec}\nIcon=knotes\nTerminal=false\nX-KDE-autostart-after=panel\n"
    );
    if let Some(dir) = file.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(&file, content)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bus_names_are_valid() {
        assert_eq!(bus_name(None), "io.github.yoshiaki21.FusenRust");
        assert_eq!(
            bus_name(Some("test-2")),
            "io.github.yoshiaki21.FusenRust.p_test_2"
        );
    }

    #[test]
    fn js_strings_are_escaped() {
        assert_eq!(js_str(r#"a"b\c"#), r#""a\"b\\c""#);
    }
}
