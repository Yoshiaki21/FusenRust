//! アプリ全体の状態（グローバル）と、命令の処理。
//!
//! - 付箋ウィンドウの一覧と開閉
//! - 管理画面の開閉
//! - OS からの通知（命令・ウィンドウの位置）の処理
//! - 終了時の保存

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use fusen_core::{Placement, Store};
use gpui_kit::*;
use uuid::Uuid;

use crate::manager::{ManagerTab, ManagerWindow};
use crate::note_window::NoteWindow;
use crate::platform::{self, AppEvent, Command, imp};
use crate::settings::{AppPaths, DEFAULT_NOTE_HEIGHT, DEFAULT_NOTE_WIDTH, Settings};
use crate::sync::{FetchResult, ServerConfig, SyncEvent, SyncService, SyncState, SyncStatus};

/// OS 側の資源（D-Bus の名前・トレイ）をアプリの終了まで保持する
pub struct PlatformGlobal(pub imp::PlatformHandle);
impl Global for PlatformGlobal {}

/// gpui-component の Root が、クライアント側で描くウィンドウの周りに付ける影の幅（Linux は 20px）。
/// ウィンドウマネージャーが扱う位置・サイズ（配置として保存する値）は影の内側なので、
/// ウィンドウを開くときは影の分だけ大きくする（しないと再起動のたびに付箋が縮む）。
const CSD_SHADOW: f64 = 20.0;

/// 開いている付箋ウィンドウ
struct OpenNote {
    window: AnyWindowHandle,
    view: Entity<NoteWindow>,
}

/// アプリ全体の状態
pub struct FusenApp {
    pub store: Arc<Store>,
    pub paths: AppPaths,
    pub settings: Settings,
    notes: HashMap<Uuid, OpenNote>,
    manager: Option<(AnyWindowHandle, Entity<ManagerWindow>)>,
    /// 「全付箋を一時的に前面に出す」中か
    show_all_active: bool,
    /// ウィンドウを開いたが、まだウィンドウマネージャーから最初の位置の通知が来ていない付箋。
    /// 最初の通知でウィンドウが表示されたと分かるので、そこで保存済みの配置・層を適用する。
    awaiting_map: HashSet<Uuid>,
    /// この端末で新しく作り、まだ配置を保存していない付箋（最前面で表示する。docs/decisions.md D-007）
    created_here: HashSet<Uuid>,
    sync: SyncService,
    sync_status: SyncStatus,
}

impl Global for FusenApp {}

/// アプリの状態を作り、保存されている付箋を開く
pub fn init(cx: &mut App, store: Arc<Store>, paths: AppPaths, settings: Settings) {
    // 同期サービス（別スレッド）。状態や他の端末からの変更はチャンネルで届く
    let (events_tx, events_rx) = async_channel::unbounded::<SyncEvent>();
    let sync = SyncService::start(
        store.clone(),
        server_config(&settings),
        settings.max_attachment_bytes(),
        events_tx,
    );
    cx.spawn(async move |cx| {
        while let Ok(ev) = events_rx.recv().await {
            cx.update(|cx| on_sync_event(cx, ev));
        }
    })
    .detach();

    cx.set_global(FusenApp {
        store,
        paths,
        settings,
        sync,
        sync_status: SyncStatus {
            state: SyncState::Connecting,
            pending: 0,
        },
        notes: HashMap::new(),
        manager: None,
        show_all_active: false,
        awaiting_map: HashSet::new(),
        created_here: HashSet::new(),
    });
    imp::install_window_control(profile(cx).as_deref());

    // 終了時: 編集中の付箋を保存し、KWin の常駐スクリプトを解除する
    cx.on_app_quit(|cx| {
        save_all_editing(cx);
        cx.global::<FusenApp>().sync.shutdown();
        imp::uninstall_window_control(profile(cx).as_deref());
        async {}
    })
    .detach();

    restore_notes(cx);
}

fn profile(cx: &App) -> Option<String> {
    cx.global::<FusenApp>().paths.profile.clone()
}

pub fn store(cx: &App) -> Arc<Store> {
    cx.global::<FusenApp>().store.clone()
}

pub fn settings(cx: &App) -> &Settings {
    &cx.global::<FusenApp>().settings
}

/// 設定から同期の接続先を作る（URL かトークンが空なら同期しない）
pub fn server_config(s: &Settings) -> Option<ServerConfig> {
    let url = s.server_url.trim();
    let token = s.token.trim();
    (!url.is_empty() && !token.is_empty()).then(|| ServerConfig {
        url: url.to_string(),
        token: token.to_string(),
    })
}

/// 設定を変えた: 同期の接続先を更新する
pub fn apply_settings(cx: &mut App) {
    let app = cx.global::<FusenApp>();
    app.sync.reconfigure(server_config(&app.settings));
}

pub fn sync_status(cx: &App) -> SyncStatus {
    cx.global::<FusenApp>().sync_status.clone()
}

/// 添付の実体をサーバーから取得する（この端末にまだないとき）
pub fn fetch_blob(cx: &App, attachment_id: Uuid) -> async_channel::Receiver<FetchResult> {
    cx.global::<FusenApp>().sync.fetch(attachment_id)
}

/// 同期サービスからの通知
fn on_sync_event(cx: &mut App, ev: SyncEvent) {
    match ev {
        SyncEvent::Status(status) => {
            let label = status.label();
            cx.global_mut::<FusenApp>().sync_status = status;
            if let Some(p) = cx.try_global::<PlatformGlobal>() {
                imp::set_tray_status(&p.0, &label);
            }
            refresh_manager(cx);
        }
        SyncEvent::RemoteChanges { changed, new } => on_remote_changes(cx, changed, new),
        // 取得した画像を描き直す
        SyncEvent::BlobsFetched => cx.refresh_windows(),
    }
}

/// 他の端末からの変更を画面に反映する（docs/spec.md 3.2, 4.4）
fn on_remote_changes(cx: &mut App, changed: Vec<Uuid>, new: Vec<Uuid>) {
    let store = store(cx);
    let mut ids = changed;
    for id in &new {
        if !ids.contains(id) {
            ids.push(*id);
        }
    }
    for id in ids {
        let Some(note) = log_err(store.note(id)) else {
            continue;
        };
        let is_open = cx.global::<FusenApp>().notes.contains_key(&id);
        if note.deleted {
            // 他の端末でごみ箱に入れられた
            close_note_window(cx, id);
            continue;
        }
        if is_open {
            // 編集中なら本文は上書きせず「他の端末で更新されました」を出す（note_window.rs）
            notes_changed(cx, id);
            continue;
        }
        let restored = log_err(store.revision(id, note.rev))
            .is_some_and(|r| r.kind == fusen_core::RevisionKind::Restore);
        if new.contains(&id) {
            // 他の端末で新しく作られた付箋: 左上付近に既定サイズで、壁モードで置く
            log_err(store.ensure_placement(id, DEFAULT_NOTE_WIDTH, DEFAULT_NOTE_HEIGHT));
            open_note(cx, id, None);
        } else if restored
            && log_err(store.placement(id))
                .flatten()
                .is_none_or(|p| p.visible)
        {
            // 他の端末でごみ箱から戻された
            open_note(cx, id, None);
        }
    }
    refresh_manager(cx);
}

/// 管理画面を読み直す（後で実行する）
pub fn refresh_manager(cx: &mut App) {
    cx.defer(|cx| {
        if let Some((_, manager)) = cx.global::<FusenApp>().manager.clone() {
            manager.update(cx, |m, cx| m.refresh(cx));
        }
    });
}

/// エラーをログに出す（UI に出せない場所で使う）
fn log_err<T>(r: fusen_core::Result<T>) -> Option<T> {
    r.map_err(|e| tracing::error!("{e}")).ok()
}

/// 起動時: 表示中の付箋と、下書きが残っている付箋（編集状態で開き直す）を開く
fn restore_notes(cx: &mut App) {
    let store = store(cx);
    let drafts: HashMap<Uuid, String> = log_err(store.drafts())
        .unwrap_or_default()
        .into_iter()
        .map(|d| (d.note_id, d.content))
        .collect();
    let placements: HashMap<Uuid, Placement> = log_err(store.placements())
        .unwrap_or_default()
        .into_iter()
        .map(|p| (p.note_id, p))
        .collect();

    for note in log_err(store.list_notes()).unwrap_or_default() {
        let draft = drafts.get(&note.note_id).cloned();
        // 配置がない付箋（他の端末で作られた付箋など）も表示する
        let visible = placements.get(&note.note_id).is_none_or(|p| p.visible);
        if visible || draft.is_some() {
            open_note(cx, note.note_id, draft);
        }
    }
    // ごみ箱に入った付箋の下書きは捨てる
    for id in drafts.keys() {
        if !cx.global::<FusenApp>().notes.contains_key(id) {
            log_err(store.clear_draft(*id));
        }
    }
}

/// 付箋のウィンドウを開く（開いていれば前面に出す）。`draft` があれば編集状態で開く。
pub fn open_note(cx: &mut App, note_id: Uuid, draft: Option<String>) {
    if cx.global::<FusenApp>().notes.contains_key(&note_id) {
        activate_note(cx, note_id);
        return;
    }
    let store = store(cx);
    let placement = log_err(store.placement(note_id)).flatten();
    if let Some(p) = &placement
        && !p.visible
    {
        // デスクトップから外していた付箋を表示に戻す
        log_err(store.set_placement(&Placement {
            visible: true,
            ..p.clone()
        }));
    }
    let (w, h) = placement
        .as_ref()
        .map(|p| (p.width, p.height))
        .unwrap_or((DEFAULT_NOTE_WIDTH, DEFAULT_NOTE_HEIGHT));

    let options = WindowOptions {
        // タイトルバーなし・枠なし（クライアント側で描く＝自分で描く）・透明背景（角丸の外側）
        titlebar: None,
        window_decorations: Some(WindowDecorations::Client),
        window_background: WindowBackgroundAppearance::Transparent,
        window_bounds: Some(WindowBounds::Windowed(Bounds {
            origin: point(px(0.), px(0.)),
            size: size(
                px((w + CSD_SHADOW * 2.0) as f32),
                px((h + CSD_SHADOW * 2.0) as f32),
            ),
        })),
        window_min_size: Some(size(
            px((160.0 + CSD_SHADOW * 2.0) as f32),
            px((120.0 + CSD_SHADOW * 2.0) as f32),
        )),
        app_id: Some("fusen-rust".to_string()),
        kind: WindowKind::Normal,
        focus: draft.is_some(),
        ..Default::default()
    };
    let caption = platform::note_caption(profile(cx).as_deref(), note_id);
    match gpui_kit::open_window(options, cx, |window, cx| {
        window.set_window_title(&caption);
        cx.new(|cx| NoteWindow::new(note_id, draft, window, cx))
    }) {
        Ok((window, view)) => {
            let app = cx.global_mut::<FusenApp>();
            app.notes.insert(note_id, OpenNote { window, view });
            app.awaiting_map.insert(note_id);
            // 保険: KWin から位置の通知が来ず、透明のまま残ってしまわないよう、少し後に必ず見えるようにする
            let profile = profile(cx);
            cx.spawn(async move |cx| {
                cx.background_executor()
                    .timer(std::time::Duration::from_secs(3))
                    .await;
                imp::reveal_note_window(profile.as_deref(), &caption);
            })
            .detach();
        }
        Err(e) => tracing::error!("付箋のウィンドウを開けません: {e:#}"),
    }
}

/// 新しい付箋を作って、編集状態で開く
pub fn new_note(cx: &mut App) {
    let store = store(cx);
    if let Some(r) = log_err(store.create_note("", fusen_core::color::DEFAULT)) {
        // 新しく作った付箋は他のウィンドウに埋もれないよう最前面で表示する
        cx.global_mut::<FusenApp>().created_here.insert(r.note_id);
        open_note(cx, r.note_id, Some(String::new()));
        notes_changed(cx, r.note_id);
    }
}

/// 付箋をデスクトップから外す（ウィンドウを閉じ、次回起動時も表示しない）
pub fn hide_note(cx: &mut App, note_id: Uuid) {
    let store = store(cx);
    if let Some(Some(p)) = log_err(store.placement(note_id)) {
        log_err(store.set_placement(&Placement {
            visible: false,
            ..p
        }));
    }
    close_note_window(cx, note_id);
}

/// 付箋のウィンドウを閉じる（ごみ箱に入れたときなど）
pub fn close_note_window(cx: &mut App, note_id: Uuid) {
    let app = cx.global_mut::<FusenApp>();
    app.awaiting_map.remove(&note_id);
    if let Some(open) = app.notes.remove(&note_id) {
        let _ = open
            .window
            .update(cx, |_, window, _| window.remove_window());
    }
}

/// 付箋のウィンドウが閉じられたときに一覧から外す（ウィンドウ側から呼ぶ）
pub fn forget_note_window(cx: &mut App, note_id: Uuid) {
    let app = cx.global_mut::<FusenApp>();
    app.notes.remove(&note_id);
    app.awaiting_map.remove(&note_id);
}

/// 付箋を前面に出す（管理画面の一覧から選んだとき）
pub fn activate_note(cx: &mut App, note_id: Uuid) {
    let caption = platform::note_caption(profile(cx).as_deref(), note_id);
    if let Some(open) = cx.global::<FusenApp>().notes.get(&note_id) {
        let view = open.view.clone();
        view.update(cx, |v, _| v.mark_temporarily_raised());
    }
    imp::activate_note_window(profile(cx).as_deref(), &caption);
}

/// 付箋の層（壁 / 最前面）を KWin に反映する
pub fn apply_layer(cx: &App, note_id: Uuid, on_top: bool) {
    let caption = platform::note_caption(profile(cx).as_deref(), note_id);
    imp::apply_note_window(profile(cx).as_deref(), &caption, on_top, None, true);
}

/// 付箋の内容が変わった: 開いているウィンドウと管理画面を更新する。
///
/// ビューの更新中（付箋ウィンドウ自身の操作の中など）から呼ばれても二重に借用しないよう、後で実行する。
pub fn notes_changed(cx: &mut App, note_id: Uuid) {
    // 付箋が変わったらすぐ同期する（未送信の版がなければ差分の取り込みだけ）
    cx.global::<FusenApp>().sync.kick();
    cx.defer(move |cx| {
        if let Some(open) = cx.global::<FusenApp>().notes.get(&note_id) {
            let view = open.view.clone();
            view.update(cx, |v, cx| v.reload(cx));
        }
        if let Some((_, manager)) = cx.global::<FusenApp>().manager.clone() {
            manager.update(cx, |m, cx| m.refresh(cx));
        }
    });
}

/// 付箋をデスクトップに表示して前面に出す（管理画面の一覧から選んだとき）
pub fn show_note(cx: &mut App, note_id: Uuid) {
    if cx.global::<FusenApp>().notes.contains_key(&note_id) {
        activate_note(cx, note_id);
        return;
    }
    open_note(cx, note_id, None);
    // ウィンドウが表示された（最初の位置の通知が来た）ときに前面に出す
    if let Some(open) = cx.global::<FusenApp>().notes.get(&note_id) {
        let view = open.view.clone();
        view.update(cx, |v, _| v.mark_temporarily_raised());
    }
}

/// 管理画面を開く（開いていれば前面に出す）
pub fn open_manager(cx: &mut App, tab: Option<ManagerTab>, note_id: Option<Uuid>) {
    if let Some((window, manager)) = cx.global::<FusenApp>().manager.clone() {
        manager.update(cx, |m, cx| {
            if let Some(t) = tab {
                m.select(t, note_id, cx);
            }
            m.refresh(cx);
        });
        let ok = window
            .update(cx, |_, window, _| window.activate_window())
            .is_ok();
        if ok {
            return;
        }
        cx.global_mut::<FusenApp>().manager = None;
    }
    let options = WindowOptions {
        titlebar: Some(TitlebarOptions {
            title: Some("FusenRust".into()),
            ..Default::default()
        }),
        window_bounds: Some(WindowBounds::centered(size(px(900.), px(640.)), cx)),
        window_min_size: Some(size(px(560.), px(400.))),
        app_id: Some("fusen-rust-manager".to_string()),
        ..Default::default()
    };
    match gpui_kit::open_window(options, cx, |window, cx| {
        cx.new(|cx| ManagerWindow::new(tab.unwrap_or(ManagerTab::List), note_id, window, cx))
    }) {
        Ok((window, view)) => cx.global_mut::<FusenApp>().manager = Some((window, view)),
        Err(e) => tracing::error!("管理画面を開けません: {e:#}"),
    }
}

/// 管理画面が閉じられた
pub fn forget_manager(cx: &mut App) {
    cx.global_mut::<FusenApp>().manager = None;
}

/// 全付箋を一時的に前面に出す / 元に戻す
pub fn toggle_show_all(cx: &mut App) {
    let active = !cx.global::<FusenApp>().show_all_active;
    cx.global_mut::<FusenApp>().show_all_active = active;
    if active {
        imp::raise_all_note_windows(profile(cx).as_deref());
    } else {
        let notes: Vec<(Uuid, Entity<NoteWindow>)> = cx
            .global::<FusenApp>()
            .notes
            .iter()
            .map(|(id, o)| (*id, o.view.clone()))
            .collect();
        for (id, view) in notes {
            let on_top = view.read(cx).always_on_top();
            apply_layer(cx, id, on_top);
        }
    }
}

/// 編集中の付箋をすべて保存する（終了時）
fn save_all_editing(cx: &mut App) {
    let views: Vec<Entity<NoteWindow>> = cx
        .global::<FusenApp>()
        .notes
        .values()
        .map(|o| o.view.clone())
        .collect();
    for v in views {
        v.update(cx, |v, cx| v.finish_editing_without_window(cx));
    }
}

/// OS からの通知を処理する
pub fn handle_event(cx: &mut App, event: AppEvent) {
    match event {
        AppEvent::Command(c) => handle_command(cx, c),
        AppEvent::WindowGeometry {
            note_id,
            monitor,
            x,
            y,
            width,
            height,
        } => on_window_geometry(cx, note_id, monitor, x, y, width, height),
    }
}

pub fn handle_command(cx: &mut App, c: Command) {
    tracing::info!("命令: {c:?}");
    match c {
        Command::NewNote => new_note(cx),
        Command::ShowAll => toggle_show_all(cx),
        Command::OpenManager => open_manager(cx, None, None),
        Command::Quit => cx.quit(),
    }
}

/// 他の付箋と左上が（ほぼ）同じ位置なら、重ならなくなるまで 32px ずつ右下へずらす
fn cascade_position(store: &Store, note_id: Uuid, x: f64, y: f64) -> (f64, f64) {
    let others: Vec<(f64, f64)> = store
        .placements()
        .unwrap_or_default()
        .into_iter()
        .filter(|p| p.note_id != note_id && p.visible)
        .map(|p| (p.x, p.y))
        .collect();
    let (mut nx, mut ny) = (x, y);
    // 無限に回らないよう上限を設ける
    for _ in 0..20 {
        let taken = others
            .iter()
            .any(|(ox, oy)| (ox - nx).abs() < 8.0 && (oy - ny).abs() < 8.0);
        if !taken {
            break;
        }
        nx += 32.0;
        ny += 32.0;
    }
    (nx, ny)
}

/// ウィンドウマネージャーから付箋ウィンドウの位置が届いた
fn on_window_geometry(
    cx: &mut App,
    note_id: Uuid,
    monitor: Option<String>,
    x: f64,
    y: f64,
    width: f64,
    height: f64,
) {
    let Some(view) = cx
        .global::<FusenApp>()
        .notes
        .get(&note_id)
        .map(|o| o.view.clone())
    else {
        return;
    };
    let store = store(cx);
    let saved = log_err(store.placement(note_id)).flatten();

    // 最初の通知 = ウィンドウが表示された。保存済みの配置と層を適用する
    let created_here = cx.global_mut::<FusenApp>().created_here.remove(&note_id);
    if cx.global_mut::<FusenApp>().awaiting_map.remove(&note_id) {
        // 保存済みの配置があればその設定、なければ「この端末で新しく作った付箋」だけ最前面
        let on_top = saved.as_ref().map_or(created_here, |p| p.always_on_top);
        view.update(cx, |v, _| v.set_always_on_top_state(on_top));
        let caption = platform::note_caption(profile(cx).as_deref(), note_id);
        let show_all = cx.global::<FusenApp>().show_all_active;
        let geometry = saved.as_ref().map(|p| imp::Geometry {
            x: p.x,
            y: p.y,
            width: p.width,
            height: p.height,
        });
        // 配置がない（この端末で新しく作った）付箋: ウィンドウマネージャーが決めた位置が
        // 他の付箋と重なっていれば、少しずつずらす
        let geometry = geometry.or_else(|| {
            let (nx, ny) = cascade_position(&store, note_id, x, y);
            ((nx, ny) != (x, y)).then_some(imp::Geometry {
                x: nx,
                y: ny,
                width,
                height,
            })
        });
        // 動かさないならすぐ見せる。動かすなら、置き直した後（0.4 秒後）に見せる
        imp::apply_note_window(
            profile(cx).as_deref(),
            &caption,
            on_top || show_all,
            geometry,
            geometry.is_none(),
        );
        if let Some(g) = geometry {
            // 表示直後は KWin の初期配置が後から上書きすることがあるので、少し待ってもう一度置く
            let profile = profile(cx);
            let caption = caption.clone();
            cx.spawn(async move |cx| {
                for delay in [400u64, 1200] {
                    cx.background_executor()
                        .timer(std::time::Duration::from_millis(delay))
                        .await;
                    imp::apply_note_window(
                        profile.as_deref(),
                        &caption,
                        on_top || show_all,
                        Some(g),
                        true,
                    );
                }
            })
            .detach();
        }
        if view.read(cx).is_temporarily_raised() {
            imp::activate_note_window(profile(cx).as_deref(), &caption);
        }
        match (saved.is_some(), geometry) {
            // 保存済みの位置に動かしたので、今回の（ウィンドウマネージャーが決めた）位置は保存しない
            (true, _) => return,
            // ずらした位置を保存する
            (false, Some(g)) => {
                log_err(store.set_placement(&Placement {
                    note_id,
                    monitor_key: monitor,
                    x: g.x,
                    y: g.y,
                    width: g.width,
                    height: g.height,
                    always_on_top: on_top,
                    visible: true,
                }));
                return;
            }
            (false, None) => {}
        }
    }

    let p = Placement {
        note_id,
        monitor_key: monitor,
        x,
        y,
        width,
        height,
        always_on_top: saved.as_ref().map_or(created_here, |p| p.always_on_top),
        visible: true,
    };
    if saved.as_ref() != Some(&p) {
        log_err(store.set_placement(&p));
    }
}
