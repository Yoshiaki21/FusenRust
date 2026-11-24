//! Windows 11 固有の処理（docs/spec.md 5.6、docs/decisions.md D-011）。
//!
//! ## 付箋ウィンドウの制御
//! gpui が作った付箋ウィンドウの HWND をタイトル（[`super::note_caption`]）で探し、Win32 API で制御する。
//! 付箋ウィンドウは gpui のメインスレッドが持つので、ここの関数はすべてメインスレッドから呼ぶこと
//! （ウィンドウのサブクラス化は持ち主のスレッドでしかできないため）。
//!
//! - タスクバー・Alt+Tab から外す: `WS_EX_TOOLWINDOW`（表示中に変えたときは一度隠して出し直す）
//! - 壁モード: `HWND_BOTTOM`。クリックなどで前に出ようとしたら、サブクラスの `WM_WINDOWPOSCHANGING` で最背面に戻す
//! - 最前面モード: `HWND_TOPMOST`
//! - 位置が決まるまで見せない（D-008）: DWM のクローク（`DWMWA_CLOAK`）
//! - バーのドラッグ移動: gpui の `window_control_area(Drag)`（`HTCAPTION`）。右下のつまみは `WM_NCHITTEST` で `HTBOTTOMRIGHT`
//!   （gpui の Windows 版は `start_window_move` / `start_window_resize` が未実装のため）
//! - 位置の通知: 移動・サイズ変更が終わったとき（`WM_EXITSIZEMOVE`）にアプリへ送る
//!
//! ## 座標（高 DPI）
//! 位置（x, y）は仮想スクリーンの物理ピクセル（モニターをまたいでも一意な絶対座標）、
//! 大きさ（幅・高さ）は論理ピクセル（そのモニターの拡大率で割った値）で保存する。
//! 拡大率の違うモニターに置き直すときは、先に位置だけ動かし（ここで `WM_DPICHANGED` が起きる）、
//! 移動先の拡大率で大きさを決め直す。
//!
//! ## 単一インスタンスとアプリへの命令
//! 名前付きミューテックスを作れたプロセスが 1 つ目。1 つ目は専用スレッドで隠しウィンドウとトレイを持ち、
//! 2 つ目のプロセスは隠しウィンドウを探して `WM_COPYDATA` で引数を渡して終了する。

use std::cell::RefCell;
use std::collections::HashMap;
use std::ffi::c_void;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use anyhow::Context as _;
use async_channel::Sender;
use windows_sys::Win32::Foundation::{
    CloseHandle, ERROR_ALREADY_EXISTS, ERROR_FILE_NOT_FOUND, ERROR_SUCCESS, GetLastError, HWND,
    LPARAM, LRESULT, POINT, RECT, WPARAM,
};
use windows_sys::Win32::Graphics::Dwm::{
    DWMWA_CLOAK, DWMWA_EXTENDED_FRAME_BOUNDS, DwmGetWindowAttribute, DwmSetWindowAttribute,
};
use windows_sys::Win32::Graphics::Gdi::{
    GetMonitorInfoW, MONITOR_DEFAULTTONULL, MONITOR_DEFAULTTOPRIMARY, MONITORINFO, MONITORINFOEXW,
    MonitorFromRect, MonitorFromWindow, ScreenToClient,
};
use windows_sys::Win32::System::DataExchange::COPYDATASTRUCT;
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::System::Registry::{
    HKEY_CURRENT_USER, REG_SZ, RegDeleteKeyValueW, RegSetKeyValueW,
};
use windows_sys::Win32::System::Threading::{CreateMutexW, GetCurrentProcessId};
use windows_sys::Win32::UI::HiDpi::{
    DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, GetDpiForWindow, SetProcessDpiAwarenessContext,
};
use windows_sys::Win32::UI::Shell::{DefSubclassProc, RemoveWindowSubclass, SetWindowSubclass};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    ASFW_ANY, AllowSetForegroundWindow, CreateWindowExW, DefWindowProcW, DispatchMessageW,
    EnumWindows, FindWindowW, GWL_EXSTYLE, GWL_STYLE, GetClientRect, GetForegroundWindow,
    GetMessageW, GetWindowLongPtrW, GetWindowRect, GetWindowTextW, GetWindowThreadProcessId,
    HTBOTTOMRIGHT, HTCLIENT, HWND_BOTTOM, HWND_MESSAGE, HWND_TOP, HWND_TOPMOST, IsIconic,
    MB_ICONERROR, MB_OK, MSG, MessageBoxW, PostMessageW, RegisterClassW, SC_MAXIMIZE,
    SMTO_ABORTIFHUNG, SW_HIDE, SW_RESTORE, SW_SHOW, SW_SHOWNOACTIVATE, SWP_NOACTIVATE, SWP_NOMOVE,
    SWP_NOSIZE, SWP_NOZORDER, SendMessageTimeoutW, SetForegroundWindow, SetWindowLongPtrW,
    SetWindowPos, ShowWindow, TranslateMessage, WINDOWPOS, WM_APP, WM_COPYDATA, WM_ENDSESSION,
    WM_EXITSIZEMOVE, WM_NCDESTROY, WM_NCHITTEST, WM_NCLBUTTONDBLCLK, WM_QUERYENDSESSION,
    WM_SYSCOMMAND, WM_WINDOWPOSCHANGING, WNDCLASSW, WS_EX_APPWINDOW, WS_EX_TOOLWINDOW,
    WS_MAXIMIZEBOX, WS_OVERLAPPED,
};

use super::{AppEvent, Command, caption_prefix, note_id_from_caption};

/// 位置とサイズ（x, y は物理ピクセル、幅・高さは論理ピクセル。モジュールの説明を参照）
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Geometry {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

/// `WM_COPYDATA` の種類の印（他のアプリからの誤った送信を無視するため）
const COPYDATA_COMMAND: usize = 0x4655_5345; // "FUSE"
/// 隠しウィンドウへの通知: トレイのツールチップを更新する
const WM_APP_STATUS: u32 = WM_APP + 1;
/// 付箋ウィンドウのサブクラスの ID
const NOTE_SUBCLASS_ID: usize = 1;
/// 右下のサイズ変更つまみの大きさ（論理ピクセル。note_window.rs の grip と同じ）
const GRIP_SIZE: f64 = 14.0;

/// OS からアプリへの通知の送り口（ウィンドウプロシージャから使う）
static EVENTS: OnceLock<Sender<AppEvent>> = OnceLock::new();
/// このプロセスのプロファイル（付箋ウィンドウのタイトルから ID を取り出すため）
static PROFILE: OnceLock<Option<String>> = OnceLock::new();
/// トレイのツールチップに出す同期の状態
static TRAY_STATUS: Mutex<String> = Mutex::new(String::new());
/// 終了時の保存が終わったか（ログオフ時に、保存が終わるまで終了を待つため）
static SHUTDOWN_DONE: AtomicBool = AtomicBool::new(false);

fn send(ev: AppEvent) {
    if let Some(tx) = EVENTS.get() {
        let _ = tx.send_blocking(ev);
    }
}

/// NUL 終端の UTF-16
fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// プロファイル名を、ミューテックス・ウィンドウクラスの名前に使える文字にする
fn safe_profile(profile: Option<&str>) -> String {
    match profile {
        Some(p) => p
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
            .collect(),
        None => "default".into(),
    }
}

fn mutex_name(profile: Option<&str>) -> String {
    format!("Local\\FusenRust.{}", safe_profile(profile))
}

/// 1 つ目のプロセスの隠しウィンドウのクラス名（2 つ目のプロセスはこれで探す）
fn ipc_class(profile: Option<&str>) -> String {
    format!("FusenRust.Ipc.{}", safe_profile(profile))
}

// ----------------------------------------------------------------------
// 起動・単一インスタンス
// ----------------------------------------------------------------------

/// 1 つ目のプロセスが持つ OS 側の資源
pub struct PlatformHandle {
    /// 単一インスタンスのミューテックス（プロセスの終了で OS が解放する）
    _mutex: isize,
    /// 隠しウィンドウ（トレイの更新の通知先）
    ipc_hwnd: isize,
}

/// ミューテックスを取り、命令の受け口とトレイを始める。
///
/// 既に別のプロセスが動いていれば `Ok(None)` を返す（呼び出し側は [`send_to_running`] で命令を渡して終了する）。
pub fn start(
    profile: Option<&str>,
    tx: Sender<AppEvent>,
) -> anyhow::Result<Option<PlatformHandle>> {
    // gpui のマニフェストは実行ファイルに埋め込まれないので、モニターごとの拡大率への対応をここで宣言する
    // （既に設定済みなら失敗するだけなので結果は見ない）
    unsafe { SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };

    let name = wide(&mutex_name(profile));
    let mutex = unsafe { CreateMutexW(std::ptr::null(), 0, name.as_ptr()) };
    if mutex.is_null() {
        anyhow::bail!(
            "ミューテックスを作れません（エラー {}）",
            unsafe { GetLastError() }
        );
    }
    if unsafe { GetLastError() } == ERROR_ALREADY_EXISTS {
        unsafe { CloseHandle(mutex) };
        return Ok(None);
    }

    let _ = EVENTS.set(tx);
    let _ = PROFILE.set(profile.map(str::to_string));

    // 隠しウィンドウとトレイは専用のスレッドで持つ（gpui のメッセージループとは別に回す）
    let (ready_tx, ready_rx) = std::sync::mpsc::channel::<anyhow::Result<isize>>();
    let profile_owned = profile.map(str::to_string);
    std::thread::Builder::new()
        .name("fusen-platform".into())
        .spawn(move || platform_thread(profile_owned, ready_tx))
        .context("スレッドを作れません")?;
    let ipc_hwnd = ready_rx
        .recv()
        .context("命令の受け口を作れません")?
        .context("命令の受け口を作れません")?;

    // メインスレッド（この関数を呼んだスレッド。gpui もここで動く）に、後で実行する処理の受け口を作る
    create_main_runner()?;

    Ok(Some(PlatformHandle {
        _mutex: mutex as isize,
        ipc_hwnd,
    }))
}

/// トレイのツールチップに同期の状態を出す
pub fn set_tray_status(handle: &PlatformHandle, status: &str) {
    if let Ok(mut s) = TRAY_STATUS.lock() {
        *s = status.to_string();
    }
    unsafe { PostMessageW(handle.ipc_hwnd as HWND, WM_APP_STATUS, 0, 0) };
}

/// 動いている 1 つ目のプロセスに命令を渡す
pub fn send_to_running(profile: Option<&str>, commands: &[Command]) -> anyhow::Result<()> {
    // 1 つ目のプロセスが管理画面などを前面に出せるようにする（フォアグラウンドの権利を譲る）
    unsafe { AllowSetForegroundWindow(ASFW_ANY) };
    let class = wide(&ipc_class(profile));
    // 1 つ目が起動した直後で、まだ隠しウィンドウがないことがあるので少し待つ
    let deadline = Instant::now() + Duration::from_secs(5);
    let hwnd = loop {
        let h = unsafe { FindWindowW(class.as_ptr(), std::ptr::null()) };
        if !h.is_null() {
            break h;
        }
        anyhow::ensure!(
            Instant::now() < deadline,
            "動いている FusenRust が見つかりません"
        );
        std::thread::sleep(Duration::from_millis(100));
    };
    let payload: String = commands
        .iter()
        .map(|c| c.as_arg())
        .collect::<Vec<_>>()
        .join("\n");
    let cds = COPYDATASTRUCT {
        dwData: COPYDATA_COMMAND,
        cbData: payload.len() as u32,
        lpData: payload.as_ptr() as *mut c_void,
    };
    let mut result = 0usize;
    let ok = unsafe {
        SendMessageTimeoutW(
            hwnd,
            WM_COPYDATA,
            0,
            &cds as *const COPYDATASTRUCT as LPARAM,
            SMTO_ABORTIFHUNG,
            5000,
            &mut result,
        )
    };
    anyhow::ensure!(ok != 0, "動いている FusenRust に命令を渡せません");
    Ok(())
}

/// 隠しウィンドウとトレイのスレッド
fn platform_thread(profile: Option<String>, ready: std::sync::mpsc::Sender<anyhow::Result<isize>>) {
    let class = wide(&ipc_class(profile.as_deref()));
    let hwnd = unsafe {
        let hinstance = GetModuleHandleW(std::ptr::null());
        let wc = WNDCLASSW {
            lpfnWndProc: Some(ipc_wndproc),
            hInstance: hinstance,
            lpszClassName: class.as_ptr(),
            ..std::mem::zeroed()
        };
        RegisterClassW(&wc);
        // ログオフの通知（WM_QUERYENDSESSION / WM_ENDSESSION）を受けるため、メッセージ専用ではなく
        // 表示しないトップレベルのウィンドウにする
        CreateWindowExW(
            0,
            class.as_ptr(),
            class.as_ptr(),
            WS_OVERLAPPED,
            0,
            0,
            0,
            0,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            hinstance,
            std::ptr::null(),
        )
    };
    if hwnd.is_null() {
        let _ = ready.send(Err(anyhow::anyhow!(
            "隠しウィンドウを作れません（エラー {}）",
            unsafe { GetLastError() }
        )));
        return;
    }
    let _ = ready.send(Ok(hwnd as isize));

    // トレイ（このスレッドのメッセージループで動く）
    let tray = match create_tray(profile.as_deref()) {
        Ok(t) => Some(t),
        Err(e) => {
            // トレイがなくても付箋は使えるので、警告だけにする
            tracing::warn!("トレイを作れません: {e:#}");
            None
        }
    };
    TRAY.with(|t| *t.borrow_mut() = tray);

    unsafe {
        let mut msg: MSG = std::mem::zeroed();
        while GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0) > 0 {
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}

thread_local! {
    /// トレイ（`TrayIcon` は Send ではないので、作ったスレッドに置く）
    static TRAY: RefCell<Option<tray_icon::TrayIcon>> = const { RefCell::new(None) };
}

unsafe extern "system" fn ipc_wndproc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match msg {
        WM_COPYDATA => {
            // SAFETY: WM_COPYDATA の lparam は送り手が用意した COPYDATASTRUCT を指す（このメッセージの処理中だけ有効）
            let cds = unsafe { &*(lparam as *const COPYDATASTRUCT) };
            if cds.dwData != COPYDATA_COMMAND {
                return 0;
            }
            let bytes = if cds.cbData == 0 || cds.lpData.is_null() {
                &[][..]
            } else {
                unsafe { std::slice::from_raw_parts(cds.lpData as *const u8, cds.cbData as usize) }
            };
            let args: Vec<String> = String::from_utf8_lossy(bytes)
                .split('\n')
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .collect();
            let mut cmds = Command::from_args(&args);
            if cmds.is_empty() {
                cmds.push(Command::OpenManager);
            }
            for c in cmds {
                send(AppEvent::Command(c));
            }
            1
        }
        WM_APP_STATUS => {
            let status = TRAY_STATUS.lock().map(|s| s.clone()).unwrap_or_default();
            TRAY.with(|t| {
                if let Some(tray) = t.borrow().as_ref() {
                    let tip = if status.is_empty() {
                        tray_title()
                    } else {
                        format!("{}\n{status}", tray_title())
                    };
                    let _ = tray.set_tooltip(Some(tip));
                }
            });
            0
        }
        WM_QUERYENDSESSION => 1,
        WM_ENDSESSION => {
            if wparam != 0 {
                // ログオフ・シャットダウン: 編集中の内容を保存してから終了する。
                // このメッセージから戻るとプロセスが終了させられるので、保存が終わるまで（最大 5 秒）待つ
                tracing::info!("ログオフ・シャットダウンのため終了します");
                send(AppEvent::Command(Command::Quit));
                let deadline = Instant::now() + Duration::from_secs(5);
                while !SHUTDOWN_DONE.load(Ordering::SeqCst) && Instant::now() < deadline {
                    std::thread::sleep(Duration::from_millis(50));
                }
            }
            0
        }
        _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    }
}

// ----------------------------------------------------------------------
// トレイ
// ----------------------------------------------------------------------

fn tray_title() -> String {
    match PROFILE.get().and_then(|p| p.as_deref()) {
        Some(p) => format!("FusenRust ({p})"),
        None => "FusenRust".into(),
    }
}

fn create_tray(_profile: Option<&str>) -> anyhow::Result<tray_icon::TrayIcon> {
    use tray_icon::menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem};
    use tray_icon::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};

    let items = [
        ("新しい付箋", Command::NewNote),
        ("管理画面を開く", Command::OpenManager),
        ("全付箋を一時的に前面に出す / 戻す", Command::ShowAll),
    ];
    let menu = Menu::new();
    let mut ids = Vec::new();
    for (label, c) in items {
        let item = MenuItem::new(label, true, None);
        ids.push((item.id().clone(), c));
        menu.append(&item)?;
    }
    menu.append(&PredefinedMenuItem::separator())?;
    let quit = MenuItem::new("終了", true, None);
    ids.push((quit.id().clone(), Command::Quit));
    menu.append(&quit)?;

    MenuEvent::set_event_handler(Some(move |e: MenuEvent| {
        if let Some((_, c)) = ids.iter().find(|(id, _)| *id == e.id) {
            send(AppEvent::Command(*c));
        }
    }));
    // 左クリックで管理画面を開く（右クリックはメニュー）
    TrayIconEvent::set_event_handler(Some(|e: TrayIconEvent| {
        if let TrayIconEvent::Click {
            button: MouseButton::Left,
            button_state: MouseButtonState::Up,
            ..
        } = e
        {
            send(AppEvent::Command(Command::OpenManager));
        }
    }));

    let tray = TrayIconBuilder::new()
        .with_menu(Box::new(menu))
        .with_menu_on_left_click(false)
        .with_tooltip(tray_title())
        .with_icon(tray_icon_image()?)
        .build()?;
    Ok(tray)
}

/// トレイのアイコン（仮。黄色い付箋の形を描く。デザインは未決事項 D-005）
fn tray_icon_image() -> anyhow::Result<tray_icon::Icon> {
    const N: u32 = 32;
    let mut rgba = Vec::with_capacity((N * N * 4) as usize);
    for y in 0..N {
        for x in 0..N {
            let inside = (3..29).contains(&x) && (3..29).contains(&y);
            // 右下の角を折った形
            let fold = x + y >= 48;
            let px: [u8; 4] = if !inside || x + y > 50 {
                [0, 0, 0, 0]
            } else if fold {
                [0xd9, 0xb8, 0x3a, 0xff]
            } else if y < 8 {
                [0xf2, 0xc9, 0x4c, 0xff]
            } else {
                [0xff, 0xe0, 0x7a, 0xff]
            };
            rgba.extend_from_slice(&px);
        }
    }
    Ok(tray_icon::Icon::from_rgba(rgba, N, N)?)
}

// ----------------------------------------------------------------------
// メインスレッドで後から実行する
// ----------------------------------------------------------------------
//
// アプリは gpui の更新処理（App を借用中）の中から付箋ウィンドウの操作を呼ぶ。そこで ShowWindow や SetWindowPos を
// 直接呼ぶと、その場で gpui のウィンドウプロシージャにメッセージが届き、借用中のため gpui がメッセージを捨てる
// （"RefCell already borrowed"）。そこで操作をキューに積み、メインスレッドのメッセージ専用ウィンドウへの
// メッセージで、gpui の更新処理の外から実行する（メッセージループは gpui が回しており、順番も保たれる）。

/// メインスレッドで後から実行する処理があることの通知
const WM_APP_RUN: u32 = WM_APP + 2;

thread_local! {
    static MAIN_RUNNER: std::cell::Cell<isize> = const { std::cell::Cell::new(0) };
    static MAIN_QUEUE: RefCell<std::collections::VecDeque<Box<dyn FnOnce()>>> =
        RefCell::new(std::collections::VecDeque::new());
}

fn create_main_runner() -> anyhow::Result<()> {
    let class = wide("FusenRust.MainRunner");
    let hwnd = unsafe {
        let hinstance = GetModuleHandleW(std::ptr::null());
        let wc = WNDCLASSW {
            lpfnWndProc: Some(main_runner_wndproc),
            hInstance: hinstance,
            lpszClassName: class.as_ptr(),
            ..std::mem::zeroed()
        };
        RegisterClassW(&wc);
        CreateWindowExW(
            0,
            class.as_ptr(),
            class.as_ptr(),
            0,
            0,
            0,
            0,
            0,
            HWND_MESSAGE,
            std::ptr::null_mut(),
            hinstance,
            std::ptr::null(),
        )
    };
    anyhow::ensure!(
        !hwnd.is_null(),
        "メッセージ専用ウィンドウを作れません（エラー {}）",
        unsafe { GetLastError() }
    );
    MAIN_RUNNER.with(|r| r.set(hwnd as isize));
    Ok(())
}

/// メインスレッドで、gpui の更新処理の外から実行する
fn run_later(f: impl FnOnce() + 'static) {
    let hwnd = MAIN_RUNNER.with(|r| r.get());
    if hwnd == 0 {
        // 受け口がない（メインスレッド以外から呼ばれた）ときはその場で実行する
        f();
        return;
    }
    MAIN_QUEUE.with(|q| q.borrow_mut().push_back(Box::new(f)));
    unsafe { PostMessageW(hwnd as HWND, WM_APP_RUN, 0, 0) };
}

unsafe extern "system" fn main_runner_wndproc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if msg == WM_APP_RUN {
        // 1 つずつ取り出して実行する（キューの借用を持ったまま実行しない。実行中に積まれたものもここで実行される）
        while let Some(f) = MAIN_QUEUE.with(|q| q.borrow_mut().pop_front()) {
            f();
        }
        return 0;
    }
    unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
}

// ----------------------------------------------------------------------
// 付箋ウィンドウ
// ----------------------------------------------------------------------

/// 付箋ウィンドウの層
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Layer {
    /// 壁モード（常に最背面に戻す）
    Bottom,
    /// 最前面モード
    Top,
    /// 一時的に前面に出している（管理画面から選んだとき。フォーカスが外れたらアプリが元に戻す）
    Free,
}

thread_local! {
    /// 付箋ウィンドウ（HWND）ごとの層。メインスレッドだけで使う（サブクラスのプロシージャもメインスレッドで動く）
    static LAYERS: RefCell<HashMap<isize, Layer>> = RefCell::new(HashMap::new());
}

fn layer_of(hwnd: HWND) -> Option<Layer> {
    LAYERS.with(|l| l.borrow().get(&(hwnd as isize)).copied())
}

fn set_layer_state(hwnd: HWND, layer: Layer) {
    LAYERS.with(|l| l.borrow_mut().insert(hwnd as isize, layer));
}

fn window_text(hwnd: HWND) -> String {
    let mut buf = [0u16; 256];
    let n = unsafe { GetWindowTextW(hwnd, buf.as_mut_ptr(), buf.len() as i32) };
    String::from_utf16_lossy(&buf[..n.max(0) as usize])
}

/// このプロセスのトップレベルのウィンドウとタイトル
fn own_windows() -> Vec<(HWND, String)> {
    unsafe extern "system" fn cb(hwnd: HWND, lparam: LPARAM) -> windows_sys::core::BOOL {
        // SAFETY: lparam は own_windows の中の Vec を指す（EnumWindows の実行中だけ使う）
        let out = unsafe { &mut *(lparam as *mut Vec<HWND>) };
        let mut pid = 0u32;
        unsafe { GetWindowThreadProcessId(hwnd, &mut pid) };
        if pid == unsafe { GetCurrentProcessId() } {
            out.push(hwnd);
        }
        1
    }
    let mut hwnds: Vec<HWND> = Vec::new();
    unsafe { EnumWindows(Some(cb), &mut hwnds as *mut Vec<HWND> as LPARAM) };
    hwnds.into_iter().map(|h| (h, window_text(h))).collect()
}

fn find_note_window(caption: &str) -> Option<HWND> {
    let found = own_windows()
        .into_iter()
        .find(|(_, t)| t == caption)
        .map(|(h, _)| h);
    if found.is_none() {
        tracing::debug!("付箋ウィンドウが見つかりません: {caption}");
    }
    found
}

fn cloak(hwnd: HWND, on: bool) {
    let v: i32 = on.into();
    unsafe {
        DwmSetWindowAttribute(
            hwnd,
            DWMWA_CLOAK as u32,
            &v as *const i32 as *const c_void,
            std::mem::size_of::<i32>() as u32,
        )
    };
}

/// サブクラス化とスタイルの設定（初回だけ）。初めて設定したら true
fn setup_note_window(hwnd: HWND) -> bool {
    if layer_of(hwnd).is_some() {
        return false;
    }
    set_layer_state(hwnd, Layer::Bottom);
    unsafe {
        SetWindowSubclass(hwnd, Some(note_subclass_proc), NOTE_SUBCLASS_ID, 0);
        let ex = GetWindowLongPtrW(hwnd, GWL_EXSTYLE);
        let ex = (ex | WS_EX_TOOLWINDOW as isize) & !(WS_EX_APPWINDOW as isize);
        SetWindowLongPtrW(hwnd, GWL_EXSTYLE, ex);
        // 最大化させない（画面上端へのドラッグ・ダブルクリックでの最大化を防ぐ）
        let style = GetWindowLongPtrW(hwnd, GWL_STYLE);
        SetWindowLongPtrW(hwnd, GWL_STYLE, style & !(WS_MAXIMIZEBOX as isize));
    }
    true
}

/// gpui のウィンドウを開いた直後に呼ぶ: タスクバーから外し、位置が決まるまで隠し、最初の位置をアプリに知らせる
/// （Linux で KWin の制御スクリプトが最初の位置を知らせるのと同じ流れにする）
pub fn note_window_opened(_profile: Option<&str>, caption: &str) {
    let Some(hwnd) = find_note_window(caption) else {
        return;
    };
    // クロークは DWM の属性だけでウィンドウにメッセージを送らないので、最初の描画より前にその場で隠す
    cloak(hwnd, true);
    let hwnd = hwnd as isize;
    run_later(move || note_window_opened_now(hwnd as HWND));
}

fn note_window_opened_now(hwnd: HWND) {
    let focused = unsafe { GetForegroundWindow() } == hwnd;
    if setup_note_window(hwnd) {
        // 表示中のウィンドウのタスクバーのボタンは、隠して出し直さないと消えない（クローク中なので見えない）
        unsafe {
            ShowWindow(hwnd, SW_HIDE);
            ShowWindow(hwnd, if focused { SW_SHOW } else { SW_SHOWNOACTIVATE });
            if focused {
                SetForegroundWindow(hwnd);
            }
        }
    }
    report_geometry(hwnd);
}

/// 見えている枠（DWM の拡張フレーム）。取れなければウィンドウの矩形
fn frame_rect(hwnd: HWND) -> RECT {
    let mut r = RECT {
        left: 0,
        top: 0,
        right: 0,
        bottom: 0,
    };
    let ok = unsafe {
        DwmGetWindowAttribute(
            hwnd,
            DWMWA_EXTENDED_FRAME_BOUNDS as u32,
            &mut r as *mut RECT as *mut c_void,
            std::mem::size_of::<RECT>() as u32,
        )
    } == 0;
    if !ok {
        unsafe { GetWindowRect(hwnd, &mut r) };
    }
    r
}

fn window_rect(hwnd: HWND) -> RECT {
    let mut r = RECT {
        left: 0,
        top: 0,
        right: 0,
        bottom: 0,
    };
    unsafe { GetWindowRect(hwnd, &mut r) };
    r
}

fn scale_of(hwnd: HWND) -> f64 {
    let dpi = unsafe { GetDpiForWindow(hwnd) };
    if dpi == 0 { 1.0 } else { dpi as f64 / 96.0 }
}

fn monitor_name(hwnd: HWND) -> Option<String> {
    unsafe {
        let mon = MonitorFromWindow(hwnd, MONITOR_DEFAULTTONULL);
        if mon.is_null() {
            return None;
        }
        let mut info: MONITORINFOEXW = std::mem::zeroed();
        info.monitorInfo.cbSize = std::mem::size_of::<MONITORINFOEXW>() as u32;
        if GetMonitorInfoW(mon, &mut info as *mut MONITORINFOEXW as *mut MONITORINFO) == 0 {
            return None;
        }
        let len = info
            .szDevice
            .iter()
            .position(|&c| c == 0)
            .unwrap_or(info.szDevice.len());
        Some(String::from_utf16_lossy(&info.szDevice[..len]))
    }
}

/// 付箋ウィンドウの今の位置・サイズをアプリに知らせる
fn report_geometry(hwnd: HWND) {
    let profile = PROFILE.get().and_then(|p| p.as_deref());
    let Some(note_id) = note_id_from_caption(profile, &window_text(hwnd)) else {
        return;
    };
    let r = frame_rect(hwnd);
    let scale = scale_of(hwnd);
    send(AppEvent::WindowGeometry {
        note_id,
        monitor: monitor_name(hwnd),
        x: r.left as f64,
        y: r.top as f64,
        width: (r.right - r.left) as f64 / scale,
        height: (r.bottom - r.top) as f64 / scale,
    });
}

/// 見えている枠が `g` になるよう置く
fn apply_geometry(hwnd: HWND, g: Geometry) {
    let (mut x, mut y) = (g.x.round() as i32, g.y.round() as i32);
    // 保存した位置のモニターが今はない（外した）ときは、主モニターの左上付近に置く
    let scale = scale_of(hwnd);
    let probe = RECT {
        left: x,
        top: y,
        right: x + (g.width * scale) as i32,
        bottom: y + (g.height * scale) as i32,
    };
    if unsafe { MonitorFromRect(&probe, MONITOR_DEFAULTTONULL) }.is_null() {
        unsafe {
            let mon = MonitorFromRect(&probe, MONITOR_DEFAULTTOPRIMARY);
            let mut info: MONITORINFO = std::mem::zeroed();
            info.cbSize = std::mem::size_of::<MONITORINFO>() as u32;
            if GetMonitorInfoW(mon, &mut info) != 0 {
                x = info.rcWork.left + 40;
                y = info.rcWork.top + 40;
            }
        }
    }
    // ウィンドウの矩形と見えている枠の差（リサイズ用の見えない枠）
    let offsets = |hwnd: HWND| {
        let (w, f) = (window_rect(hwnd), frame_rect(hwnd));
        (
            f.left - w.left,
            f.top - w.top,
            w.right - f.right,
            w.bottom - f.bottom,
        )
    };
    let flags = SWP_NOZORDER | SWP_NOACTIVATE;
    unsafe {
        // 先に位置だけ動かす（拡大率の違うモニターなら、ここで WM_DPICHANGED が起きる）
        let (l, t, _, _) = offsets(hwnd);
        SetWindowPos(
            hwnd,
            std::ptr::null_mut(),
            x - l,
            y - t,
            0,
            0,
            flags | SWP_NOSIZE,
        );
        // 移動先の拡大率で大きさを決める
        let scale = scale_of(hwnd);
        let (l, t, r, b) = offsets(hwnd);
        let w = (g.width * scale).round() as i32 + l + r;
        let h = (g.height * scale).round() as i32 + t + b;
        SetWindowPos(hwnd, std::ptr::null_mut(), x - l, y - t, w, h, flags);
    }
}

/// 層を変える
fn set_layer(hwnd: HWND, layer: Layer) {
    set_layer_state(hwnd, layer);
    let after = match layer {
        // 最前面のウィンドウを HWND_BOTTOM に置くと最前面も外れる
        Layer::Bottom => HWND_BOTTOM,
        Layer::Top => HWND_TOPMOST,
        Layer::Free => return,
    };
    unsafe {
        SetWindowPos(
            hwnd,
            after,
            0,
            0,
            0,
            0,
            SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
        )
    };
}

/// 付箋ウィンドウの層を設定する。`geometry` があればその位置・サイズに置く。
/// `reveal` が true なら、隠しておいたウィンドウを見えるようにする（D-008）。
pub fn apply_note_window(
    _profile: Option<&str>,
    caption: &str,
    on_top: bool,
    geometry: Option<Geometry>,
    reveal: bool,
) {
    let caption = caption.to_string();
    run_later(move || {
        let Some(hwnd) = find_note_window(&caption) else {
            return;
        };
        setup_note_window(hwnd);
        if let Some(g) = geometry {
            apply_geometry(hwnd, g);
        }
        set_layer(hwnd, if on_top { Layer::Top } else { Layer::Bottom });
        if reveal {
            cloak(hwnd, false);
        }
    });
}

/// 隠しておいた付箋ウィンドウを見えるようにする（保険）
pub fn reveal_note_window(_profile: Option<&str>, caption: &str) {
    let caption = caption.to_string();
    run_later(move || {
        if let Some(hwnd) = find_note_window(&caption) {
            cloak(hwnd, false);
        }
    });
}

/// 付箋を前面に出してフォーカスする（管理画面の一覧から選んだとき）。
/// 壁モードの付箋は一時的に最背面の維持を外す。フォーカスが外れたらアプリが元の層に戻す。
pub fn activate_note_window(_profile: Option<&str>, caption: &str) {
    let caption = caption.to_string();
    run_later(move || activate_note_window_now(&caption));
}

fn activate_note_window_now(caption: &str) {
    let Some(hwnd) = find_note_window(caption) else {
        return;
    };
    setup_note_window(hwnd);
    if layer_of(hwnd) == Some(Layer::Bottom) {
        set_layer_state(hwnd, Layer::Free);
    }
    unsafe {
        if IsIconic(hwnd) != 0 {
            ShowWindow(hwnd, SW_RESTORE);
        }
        SetWindowPos(hwnd, HWND_TOP, 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE);
        SetForegroundWindow(hwnd);
    }
}

/// このプロファイルの全付箋を最前面にする（「全付箋を一時的に前面に出す」）。
/// 元に戻すときはアプリが付箋ごとに [`apply_note_window`] を呼ぶ。
pub fn raise_all_note_windows(profile: Option<&str>) {
    let prefix = caption_prefix(profile);
    run_later(move || {
        for (hwnd, title) in own_windows() {
            if title.starts_with(&prefix) {
                setup_note_window(hwnd);
                set_layer(hwnd, Layer::Top);
            }
        }
    });
}

/// Linux の KWin の常駐スクリプトに当たるものはない（付箋ごとにサブクラス化する）
pub fn install_window_control(_profile: Option<&str>) {}

/// 終了時の保存が終わったときに呼ばれる（ログオフ時の待ちを終える）
pub fn uninstall_window_control(_profile: Option<&str>) {
    SHUTDOWN_DONE.store(true, Ordering::SeqCst);
}

/// 付箋ウィンドウのサブクラスのプロシージャ（メインスレッドで動く）
unsafe extern "system" fn note_subclass_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
    _id: usize,
    _data: usize,
) -> LRESULT {
    match msg {
        WM_WINDOWPOSCHANGING => {
            // 壁モード: 前に出ようとしたら最背面に戻す
            // SAFETY: WM_WINDOWPOSCHANGING の lparam は WINDOWPOS を指す
            let pos = unsafe { &mut *(lparam as *mut WINDOWPOS) };
            if layer_of(hwnd) == Some(Layer::Bottom) && pos.flags & SWP_NOZORDER == 0 {
                pos.hwndInsertAfter = HWND_BOTTOM;
            }
        }
        WM_NCHITTEST => {
            let r = unsafe { DefSubclassProc(hwnd, msg, wparam, lparam) };
            if r == HTCLIENT as LRESULT && in_grip(hwnd, lparam) {
                return HTBOTTOMRIGHT as LRESULT;
            }
            return r;
        }
        // バー（HTCAPTION）のダブルクリックで最大化させない
        WM_NCLBUTTONDBLCLK => return 0,
        WM_SYSCOMMAND if (wparam & 0xFFF0) == SC_MAXIMIZE as usize => return 0,
        WM_EXITSIZEMOVE => {
            let r = unsafe { DefSubclassProc(hwnd, msg, wparam, lparam) };
            report_geometry(hwnd);
            return r;
        }
        WM_NCDESTROY => {
            LAYERS.with(|l| l.borrow_mut().remove(&(hwnd as isize)));
            unsafe { RemoveWindowSubclass(hwnd, Some(note_subclass_proc), NOTE_SUBCLASS_ID) };
        }
        _ => {}
    }
    unsafe { DefSubclassProc(hwnd, msg, wparam, lparam) }
}

/// 画面座標（WM_NCHITTEST の lparam）が右下のサイズ変更つまみの上か
fn in_grip(hwnd: HWND, lparam: LPARAM) -> bool {
    let mut p = POINT {
        x: (lparam & 0xFFFF) as i16 as i32,
        y: ((lparam >> 16) & 0xFFFF) as i16 as i32,
    };
    let mut c = RECT {
        left: 0,
        top: 0,
        right: 0,
        bottom: 0,
    };
    unsafe {
        ScreenToClient(hwnd, &mut p);
        GetClientRect(hwnd, &mut c);
    }
    let grip = (GRIP_SIZE * scale_of(hwnd)).round() as i32;
    p.x >= c.right - grip && p.y >= c.bottom - grip && p.x < c.right && p.y < c.bottom
}

// ----------------------------------------------------------------------
// 自動起動・エラー表示
// ----------------------------------------------------------------------

const RUN_KEY: &str = "Software\\Microsoft\\Windows\\CurrentVersion\\Run";

fn autostart_value_name(profile: Option<&str>) -> String {
    match profile {
        Some(p) => format!("FusenRust ({p})"),
        None => "FusenRust".into(),
    }
}

/// ログイン時の自動起動を切り替える（`HKCU\...\Run` の値）
pub fn set_autostart(profile: Option<&str>, enabled: bool) -> anyhow::Result<()> {
    let key = wide(RUN_KEY);
    let name = wide(&autostart_value_name(profile));
    if !enabled {
        let r = unsafe { RegDeleteKeyValueW(HKEY_CURRENT_USER, key.as_ptr(), name.as_ptr()) };
        anyhow::ensure!(
            r == ERROR_SUCCESS || r == ERROR_FILE_NOT_FOUND,
            "自動起動を解除できません（エラー {r}）"
        );
        return Ok(());
    }
    let exe: PathBuf = std::env::current_exe().context("実行ファイルの場所が分かりません")?;
    let mut cmd = format!("\"{}\"", exe.display());
    if let Some(p) = profile {
        cmd.push_str(&format!(" --profile \"{p}\""));
    }
    let data = wide(&cmd);
    let r = unsafe {
        RegSetKeyValueW(
            HKEY_CURRENT_USER,
            key.as_ptr(),
            name.as_ptr(),
            REG_SZ,
            data.as_ptr() as *const c_void,
            (data.len() * 2) as u32,
        )
    };
    anyhow::ensure!(r == ERROR_SUCCESS, "自動起動を設定できません（エラー {r}）");
    Ok(())
}

/// 起動できなかったときのエラーを表示する（リリース版はコンソールがないため）
pub fn show_error(message: &str) {
    let text = wide(message);
    let title = wide("FusenRust");
    unsafe {
        MessageBoxW(
            std::ptr::null_mut(),
            text.as_ptr(),
            title.as_ptr(),
            MB_OK | MB_ICONERROR,
        )
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_safe() {
        assert_eq!(mutex_name(None), "Local\\FusenRust.default");
        assert_eq!(ipc_class(Some("a\\b c")), "FusenRust.Ipc.a_b_c");
        assert_eq!(autostart_value_name(Some("t")), "FusenRust (t)");
    }

    #[test]
    fn wide_is_nul_terminated() {
        assert_eq!(wide("あ"), vec![0x3042, 0]);
    }
}
