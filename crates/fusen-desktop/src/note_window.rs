//! 付箋 1 枚のウィンドウ（`docs/spec.md` 5.1, 5.2）。
//!
//! - 上部のバー: ドラッグで移動。最前面の切り替え・色・メニュー
//! - 本文: 整形表示（Markdown）⇔ ダブルクリックで編集。フォーカスが外れたら保存
//! - 本文の下: 添付の一覧（チップ）
//! - 右下のつまみ: サイズ変更
//!
//! 移動・サイズ変更はウィンドウマネージャーの機能を使う。位置は KWin の制御スクリプトから届く（app.rs）。

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use fusen_core::markdown::{self, SCHEME};
use fusen_core::{AttachmentMeta, NoteSnapshot, Placement};
use gpui_kit::assets::IconName as Lucide;
use gpui_kit::component::{
    ActiveTheme as _, Icon, Selectable as _, Sizable as _, WindowExt as _,
    button::{Button, ButtonVariants as _},
    input::{Editor, EditorState},
    menu::{DropdownMenu as _, PopupMenuItem},
    notification::Notification,
    text::TextView,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use uuid::Uuid;

use crate::settings::{DEFAULT_NOTE_HEIGHT, DEFAULT_NOTE_WIDTH};
use crate::{app, images, palette};

gpui_kit::actions!(fusen_note, [PasteAttachment]);

/// 編集中の下書きを保存する間隔
const DRAFT_INTERVAL: Duration = Duration::from_secs(3);

/// 編集の状態
struct EditSession {
    /// 編集を始めたときの版の操作 ID（競合の検出用）。
    /// 版の番号は同期で仮番号から確定番号に変わるので、番号ではなく操作 ID で覚える
    base_op: Uuid,
    /// 編集中に増減した添付（編集終了時の版にまとめる）
    attachments: Vec<Uuid>,
    /// 最後に保存した下書き
    last_draft: String,
}

pub struct NoteWindow {
    note_id: Uuid,
    snapshot: Option<NoteSnapshot>,
    attachments: Vec<AttachmentMeta>,
    always_on_top: bool,
    editing: Option<EditSession>,
    editor: Entity<EditorState>,
    focus_handle: FocusHandle,
    /// 管理画面から一時的に前面に出した（フォーカスが外れたら元の層に戻す）
    temporarily_raised: bool,
    _subscriptions: Vec<Subscription>,
}

impl NoteWindow {
    pub fn new(
        note_id: Uuid,
        draft: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let editor = cx.new(|cx| {
            EditorState::new(window, cx)
                .language("markdown")
                .line_number(false)
                .soft_wrap(true)
        });
        let editor_focus = editor.read(cx).focus_handle(cx);

        let subscriptions = vec![
            // 編集中にフォーカスが外れたら保存して整形表示に戻す
            cx.on_blur(&editor_focus, window, |this, _, cx| this.finish_editing(cx)),
            // 他のウィンドウをクリックしてこのウィンドウが非アクティブになったとき
            cx.observe_window_activation(window, |this, window, cx| {
                if window.is_window_active() {
                    return;
                }
                this.finish_editing(cx);
                if std::mem::take(&mut this.temporarily_raised) {
                    app::apply_layer(cx, this.note_id, this.always_on_top);
                }
            }),
            // ウィンドウが閉じられたら一覧から外す
            cx.on_release(|this, cx| app::forget_note_window(cx, this.note_id)),
        ];

        // 編集中は数秒ごとに下書きを保存する（クラッシュ対策）
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(DRAFT_INTERVAL).await;
                if this.update(cx, |this, cx| this.save_draft(cx)).is_err() {
                    break;
                }
            }
        })
        .detach();

        let mut this = Self {
            note_id,
            snapshot: None,
            attachments: Vec::new(),
            always_on_top: false,
            editing: None,
            editor,
            focus_handle: cx.focus_handle(),
            temporarily_raised: false,
            _subscriptions: subscriptions,
        };
        this.load(cx);

        if let Some(draft) = draft {
            // 下書きが残っていた（前回落ちた）か、新しい付箋: 編集状態で開く
            cx.defer_in(window, move |this, window, cx| {
                this.start_editing(Some(draft), window, cx)
            });
        }
        this
    }

    fn store(&self, cx: &App) -> Arc<fusen_core::Store> {
        app::store(cx)
    }

    fn load(&mut self, cx: &mut Context<Self>) {
        let store = self.store(cx);
        match store.note(self.note_id) {
            Ok(n) => {
                self.attachments = store.attachments_of(&n).unwrap_or_default();
                self.snapshot = Some(n);
            }
            Err(e) => tracing::error!("付箋を読めません: {e}"),
        }
        if let Ok(Some(p)) = store.placement(self.note_id) {
            self.always_on_top = p.always_on_top;
        }
    }

    /// 保存内容を読み直す（他の操作で変わったとき）
    pub fn reload(&mut self, cx: &mut Context<Self>) {
        self.load(cx);
        cx.notify();
    }

    pub fn always_on_top(&self) -> bool {
        self.always_on_top
    }

    pub fn set_always_on_top_state(&mut self, on_top: bool) {
        self.always_on_top = on_top;
    }

    pub fn mark_temporarily_raised(&mut self) {
        self.temporarily_raised = true;
    }

    pub fn is_temporarily_raised(&self) -> bool {
        self.temporarily_raised
    }

    fn notify_error(&self, msg: String, window: &mut Window, cx: &mut App) {
        tracing::warn!("{msg}");
        window.push_notification(Notification::error(msg), cx);
    }

    // ------------------------------------------------------------------
    // 編集
    // ------------------------------------------------------------------

    fn start_editing(&mut self, text: Option<String>, window: &mut Window, cx: &mut Context<Self>) {
        let Some(n) = &self.snapshot else { return };
        if self.editing.is_some() {
            return;
        }
        let text = text.unwrap_or_else(|| n.content.clone());
        self.editing = Some(EditSession {
            base_op: n.op_id,
            attachments: n.attachments.clone(),
            last_draft: text.clone(),
        });
        self.editor.update(cx, |state, cx| {
            state.set_value(text, window, cx);
            state.focus(window, cx);
        });
        cx.notify();
    }

    /// 編集を終えて保存する（内容が変わっていなければ版を作らない）
    pub fn finish_editing(&mut self, cx: &mut Context<Self>) {
        let Some(session) = self.editing.take() else {
            return;
        };
        let content = self.editor.read(cx).value().to_string();
        let store = self.store(cx);
        // 編集を始めた版の、今の番号（同期で確定番号に変わっていることがある）
        let base_rev = store
            .rev_of_op(self.note_id, session.base_op)
            .ok()
            .flatten();
        match store.save_edit(self.note_id, &content, Some(session.attachments), base_rev) {
            Ok(Some(r)) => tracing::info!("付箋 {} を保存しました（版 {}）", self.note_id, r.rev),
            Ok(None) => {}
            Err(e) => tracing::error!("付箋を保存できません: {e}"),
        }
        self.reload(cx);
        app::notes_changed(cx, self.note_id);
    }

    /// 終了時に呼ぶ（ウィンドウがなくても保存できる）
    pub fn finish_editing_without_window(&mut self, cx: &mut Context<Self>) {
        self.finish_editing(cx);
    }

    fn save_draft(&mut self, cx: &mut Context<Self>) {
        let content = self.editor.read(cx).value().to_string();
        let Some(session) = &mut self.editing else {
            return;
        };
        if session.last_draft == content {
            return;
        }
        session.last_draft = content.clone();
        if let Err(e) = app::store(cx).save_draft(self.note_id, &content) {
            tracing::warn!("下書きを保存できません: {e}");
        }
    }

    /// 整形表示中のチェックボックスのクリック: すぐに `edit` 版を作る
    fn toggle_task(
        &mut self,
        checked: bool,
        marker: std::ops::Range<usize>,
        cx: &mut Context<Self>,
    ) {
        let Some(n) = &self.snapshot else { return };
        let mut content = n.content.clone();
        if content.get(marker.clone()).is_none() {
            return;
        }
        content.replace_range(marker, if checked { "[x]" } else { "[ ]" });
        if let Err(e) = app::store(cx).save_edit(self.note_id, &content, None, Some(n.rev)) {
            tracing::error!("チェックボックスの変更を保存できません: {e}");
        }
        self.reload(cx);
        app::notes_changed(cx, self.note_id);
    }

    // ------------------------------------------------------------------
    // バーの操作
    // ------------------------------------------------------------------

    fn toggle_pin(&mut self, cx: &mut Context<Self>) {
        self.always_on_top = !self.always_on_top;
        let store = app::store(cx);
        let p = match store.placement(self.note_id) {
            Ok(Some(p)) => p,
            _ => Placement {
                note_id: self.note_id,
                monitor_key: None,
                x: 40.0,
                y: 40.0,
                width: DEFAULT_NOTE_WIDTH,
                height: DEFAULT_NOTE_HEIGHT,
                always_on_top: false,
                visible: true,
            },
        };
        if let Err(e) = store.set_placement(&Placement {
            always_on_top: self.always_on_top,
            ..p
        }) {
            tracing::error!("配置を保存できません: {e}");
        }
        app::apply_layer(cx, self.note_id, self.always_on_top);
        cx.notify();
    }

    fn set_color(&mut self, key: &'static str, cx: &mut Context<Self>) {
        if let Err(e) = app::store(cx).set_color(self.note_id, key) {
            tracing::error!("色を変えられません: {e}");
        }
        self.reload(cx);
        app::notes_changed(cx, self.note_id);
    }

    fn move_to_trash(&mut self, cx: &mut Context<Self>) {
        self.finish_editing(cx);
        if let Err(e) = app::store(cx).trash(self.note_id) {
            tracing::error!("ごみ箱へ移動できません: {e}");
            return;
        }
        let id = self.note_id;
        cx.defer(move |cx| {
            app::close_note_window(cx, id);
            app::notes_changed(cx, id);
        });
    }

    fn hide(&mut self, cx: &mut Context<Self>) {
        self.finish_editing(cx);
        let id = self.note_id;
        cx.defer(move |cx| app::hide_note(cx, id));
    }

    fn choose_files(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let rx = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: true,
            prompt: Some("添付".into()),
        });
        cx.spawn_in(window, async move |this, cx| {
            if let Ok(Ok(Some(paths))) = rx.await {
                let _ = this.update_in(cx, |this, window, cx| this.attach_paths(paths, window, cx));
            }
        })
        .detach();
    }

    // ------------------------------------------------------------------
    // 添付
    // ------------------------------------------------------------------

    /// ファイルを添付する（ドロップ・選択・貼り付け）。取り込みはバックグラウンドで行う。
    fn attach_paths(&mut self, paths: Vec<PathBuf>, window: &mut Window, cx: &mut Context<Self>) {
        let store = app::store(cx);
        let max = app::settings(cx).max_attachment_bytes();
        let note_id = self.note_id;
        let task = cx.background_executor().spawn(async move {
            paths
                .into_iter()
                .map(|p| {
                    let name = p.display().to_string();
                    store
                        .import_attachment_file(note_id, &p, max)
                        .map_err(|e| format!("{name}: {e}"))
                })
                .collect::<Vec<_>>()
        });
        cx.spawn_in(window, async move |this, cx| {
            let results = task.await;
            let _ = this.update_in(cx, |this, window, cx| this.on_imported(results, window, cx));
        })
        .detach();
    }

    fn attach_bytes(
        &mut self,
        file_name: String,
        bytes: Vec<u8>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let store = app::store(cx);
        let max = app::settings(cx).max_attachment_bytes();
        let note_id = self.note_id;
        let task = cx.background_executor().spawn(async move {
            vec![
                store
                    .import_attachment_bytes(note_id, &file_name, &bytes, max)
                    .map_err(|e| format!("{file_name}: {e}")),
            ]
        });
        cx.spawn_in(window, async move |this, cx| {
            let results = task.await;
            let _ = this.update_in(cx, |this, window, cx| this.on_imported(results, window, cx));
        })
        .detach();
    }

    /// 取り込んだ添付を付箋に付ける。
    /// - 編集中: カーソル位置に参照を挿入し、編集終了時の版にまとめる
    /// - 整形表示中: 本文の末尾に参照を追加して `attach` 版を作る（付箋の一番下に表示される）
    fn on_imported(
        &mut self,
        results: Vec<Result<AttachmentMeta, String>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let store = app::store(cx);
        for r in results {
            let meta = match r {
                Ok(m) => m,
                Err(msg) => {
                    self.notify_error(format!("添付できません: {msg}"), window, cx);
                    continue;
                }
            };
            let reference = markdown::reference_for(&meta);
            if let Some(session) = &mut self.editing {
                session.attachments.push(meta.attachment_id);
                self.editor.update(cx, |state, cx| {
                    state.insert(format!("{reference}\n"), window, cx);
                });
            } else {
                let Ok(n) = store.note(self.note_id) else {
                    continue;
                };
                let content = markdown::append_reference(&n.content, &reference);
                if let Err(e) =
                    store.add_attachment(self.note_id, meta.attachment_id, Some(&content))
                {
                    self.notify_error(format!("添付できません: {e}"), window, cx);
                }
            }
        }
        if self.editing.is_none() {
            self.reload(cx);
            app::notes_changed(cx, self.note_id);
        }
    }

    /// クリップボードの画像・ファイルを添付する。添付したら true
    fn paste_from_clipboard(
        &mut self,
        item: &ClipboardItem,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let mut handled = false;
        for entry in item.entries() {
            match entry {
                ClipboardEntry::Image(img) => {
                    let ext = match img.format {
                        ImageFormat::Png => "png",
                        ImageFormat::Jpeg => "jpg",
                        ImageFormat::Webp => "webp",
                        ImageFormat::Gif => "gif",
                        ImageFormat::Svg => "svg",
                        ImageFormat::Bmp => "bmp",
                        ImageFormat::Tiff => "tiff",
                        _ => "bin",
                    };
                    let name = format!(
                        "貼り付け-{}.{ext}",
                        chrono::Local::now().format("%Y%m%d-%H%M%S")
                    );
                    self.attach_bytes(name, img.bytes.clone(), window, cx);
                    handled = true;
                }
                ClipboardEntry::ExternalPaths(paths) => {
                    self.attach_paths(paths.paths().to_vec(), window, cx);
                    handled = true;
                }
                ClipboardEntry::String(_) => {}
            }
        }
        handled
    }

    fn on_paste_action(
        &mut self,
        _: &PasteAttachment,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(item) = cx.read_from_clipboard() {
            self.paste_from_clipboard(&item, window, cx);
        }
    }

    /// 添付の実体のパスを得てから `f` を呼ぶ。この端末にまだなければサーバーから取得する
    /// （画像以外の添付は、開く・保存するときに取得する方針。docs/spec.md 5.2）
    fn with_blob(
        &mut self,
        id: Uuid,
        window: &mut Window,
        cx: &mut Context<Self>,
        f: impl FnOnce(&mut Self, PathBuf, &mut Window, &mut Context<Self>) + 'static,
    ) {
        match app::store(cx).blob_path(id) {
            Ok(Some(path)) => f(self, path, window, cx),
            Ok(None) => {
                window
                    .push_notification(Notification::info("添付をサーバーから取得しています…"), cx);
                let rx = app::fetch_blob(cx, id);
                cx.spawn_in(window, async move |this, cx| {
                    let result = rx
                        .recv()
                        .await
                        .unwrap_or_else(|_| Err("取得を中断しました".into()));
                    let _ = this.update_in(cx, |this, window, cx| match result {
                        Ok(path) => f(this, path, window, cx),
                        Err(e) => {
                            this.notify_error(format!("添付を取得できません: {e}"), window, cx)
                        }
                    });
                })
                .detach();
            }
            Err(e) => self.notify_error(format!("添付を開けません: {e}"), window, cx),
        }
    }

    fn open_attachment(&mut self, id: Uuid, window: &mut Window, cx: &mut Context<Self>) {
        self.with_blob(id, window, cx, |_, path, _, cx| cx.open_with_system(&path));
    }

    fn save_attachment_as(
        &mut self,
        meta: AttachmentMeta,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.with_blob(meta.attachment_id, window, cx, move |_, src, window, cx| {
            let dir = dirs::download_dir()
                .or_else(dirs::home_dir)
                .unwrap_or_else(|| PathBuf::from("."));
            let rx = cx.prompt_for_new_path(&dir, Some(&meta.file_name));
            cx.spawn_in(window, async move |this, cx| {
                if let Ok(Ok(Some(dest))) = rx.await
                    && let Err(e) = std::fs::copy(&src, &dest)
                {
                    let _ = this.update_in(cx, |this, window, cx| {
                        this.notify_error(format!("保存できません: {e}"), window, cx)
                    });
                }
            })
            .detach();
        });
    }

    /// 編集中に、他の端末で付箋が更新されたか（docs/spec.md 4.4）
    fn updated_elsewhere(&self, cx: &App) -> bool {
        match (&self.editing, &self.snapshot) {
            (Some(session), Some(n)) => {
                n.op_id != session.base_op && n.edited_by != app::store(cx).device_id()
            }
            _ => false,
        }
    }

    fn remove_attachment(&mut self, id: Uuid, cx: &mut Context<Self>) {
        let Some(n) = &self.snapshot else { return };
        let content = markdown::remove_reference(&n.content, id);
        if let Err(e) = app::store(cx).remove_attachment(self.note_id, id, Some(&content)) {
            tracing::error!("添付を削除できません: {e}");
        }
        self.reload(cx);
        app::notes_changed(cx, self.note_id);
    }

    // ------------------------------------------------------------------
    // 描画
    // ------------------------------------------------------------------

    fn render_bar(&self, color: &str, cx: &mut Context<Self>) -> impl IntoElement {
        let pinned = self.always_on_top;
        let view = cx.entity();
        let id = self.note_id;
        let current = color.to_string();

        // ボタンの押下がバーのドラッグ（ウィンドウ移動）にならないようにする
        let no_drag = |e: Div| e.on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation());

        let pin = Button::new("pin")
            .ghost()
            .xsmall()
            .icon(Icon::from(if pinned {
                Lucide::Pin
            } else {
                Lucide::PinOff
            }))
            .when(pinned, |b| b.selected(true))
            .tooltip(if pinned {
                "最前面に表示中（クリックで壁に戻す）"
            } else {
                "壁に表示中（クリックで最前面にする）"
            })
            .on_click(cx.listener(|this, _, _, cx| this.toggle_pin(cx)));

        let colors = Button::new("color")
            .ghost()
            .xsmall()
            .icon(Icon::from(Lucide::Palette))
            .tooltip("色を変える")
            .dropdown_menu({
                let view = view.clone();
                move |menu, window, _| {
                    let mut menu = menu;
                    for (key, label, _) in palette::COLORS {
                        let key: &'static str = key;
                        menu =
                            menu.item(PopupMenuItem::new(*label).checked(current == key).on_click(
                                window.listener_for(&view, move |this, _, _, cx| {
                                    this.set_color(key, cx)
                                }),
                            ));
                    }
                    menu
                }
            });

        let menu = Button::new("menu")
            .ghost()
            .xsmall()
            .icon(Icon::from(Lucide::Ellipsis))
            .dropdown_menu(move |menu, window, _| {
                menu.item(PopupMenuItem::new("ファイルを添付…").on_click(
                    window.listener_for(&view, |this, _, window, cx| this.choose_files(window, cx)),
                ))
                .item(PopupMenuItem::new("新しい付箋").on_click(|_, _, cx| {
                    cx.defer(app::new_note);
                }))
                .separator()
                .item(PopupMenuItem::new("履歴を見る").on_click(move |_, _, cx| {
                    cx.defer(move |cx| {
                        app::open_manager(cx, Some(crate::manager::ManagerTab::History), Some(id))
                    });
                }))
                .item(PopupMenuItem::new("管理画面を開く").on_click(|_, _, cx| {
                    cx.defer(|cx| app::open_manager(cx, None, None));
                }))
                .separator()
                .item(
                    PopupMenuItem::new("デスクトップから外す")
                        .on_click(window.listener_for(&view, |this, _, _, cx| this.hide(cx))),
                )
                .item(
                    PopupMenuItem::new("ごみ箱へ移動").on_click(
                        window.listener_for(&view, |this, _, _, cx| this.move_to_trash(cx)),
                    ),
                )
            });

        div()
            .id("bar")
            .flex()
            .flex_row()
            .items_center()
            .flex_shrink_0()
            .h(px(26.))
            .px_1()
            .gap_0p5()
            .bg(palette::bar(color))
            .cursor_move()
            // バーを押したらウィンドウマネージャーの移動を始める（モニターもまたげる）
            .on_mouse_down(MouseButton::Left, |_, window, _| window.start_window_move())
            .child(no_drag(div()).child(pin))
            .child(div().flex_1())
            .child(no_drag(div()).child(colors))
            .child(no_drag(div()).child(menu))
    }

    fn render_body(&self, n: &NoteSnapshot, cx: &mut Context<Self>) -> AnyElement {
        if self.editing.is_some() {
            let view = cx.entity().downgrade();
            return Editor::new(&self.editor)
                .bordered(false)
                .appearance(false)
                .h_full()
                .on_paste(move |item, window, cx| {
                    // 画像・ファイルの貼り付けは添付にする。文字は通常どおり貼り付ける
                    view.update(cx, |this, cx| this.paste_from_clipboard(item, window, cx))
                        .unwrap_or(false)
                })
                .into_any_element();
        }

        // 本文中に表示する画像（PNG / JPEG / JXL）
        let store = app::store(cx);
        let mut images: HashMap<String, Arc<RenderImage>> = HashMap::new();
        for a in &self.attachments {
            if !markdown::is_inline_image(&a.mime) {
                continue;
            }
            if let Ok(Some(path)) = store.blob_path(a.attachment_id)
                && let Some(img) = images::get_or_load(cx, a.attachment_id, &path, &a.mime)
            {
                images.insert(a.attachment_id.to_string(), img);
            }
        }

        let toggle_view = cx.entity().downgrade();
        let link_view = cx.entity().downgrade();
        let text = if n.content.trim().is_empty() {
            "*ダブルクリックで編集*".to_string()
        } else {
            n.content.clone()
        };
        div()
            .id("note-view")
            .size_full()
            .overflow_y_scroll()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, ev: &MouseDownEvent, window, cx| {
                    if ev.click_count == 2 {
                        this.start_editing(None, window, cx);
                    }
                }),
            )
            .child(
                TextView::markdown(ElementId::Name(format!("md-{}", self.note_id).into()), text)
                    .selectable(false)
                    .text_color(rgb(palette::TEXT))
                    .image_source(move |uri| {
                        let key = uri.as_ref().strip_prefix(SCHEME).unwrap_or_default();
                        match images.get(key) {
                            Some(img) => ImageSource::Render(img.clone()),
                            // 読み込み中（デコードが終わると描き直される）
                            None => ImageSource::Custom(Arc::new(|_, _| None)),
                        }
                    })
                    .on_task_toggle(move |ev, _, cx| {
                        let (checked, marker) = (ev.checked(), ev.marker_range());
                        let _ = toggle_view
                            .update(cx, |this, cx| this.toggle_task(checked, marker, cx));
                    })
                    .on_link_click(move |url, _, window, cx| {
                        if let Some(id) = url
                            .strip_prefix(SCHEME)
                            .and_then(|s| Uuid::parse_str(s).ok())
                        {
                            let _ = link_view
                                .update(cx, |this, cx| this.open_attachment(id, window, cx));
                        } else {
                            cx.open_url(url);
                        }
                    }),
            )
            .into_any_element()
    }

    fn render_chips(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let view = cx.entity();
        div()
            .flex()
            .flex_row()
            .flex_wrap()
            .gap_1()
            .px_2()
            .pb_1()
            .children(self.attachments.iter().enumerate().map(|(i, a)| {
                let meta = a.clone();
                let id = a.attachment_id;
                let is_image = markdown::is_inline_image(&a.mime) || a.mime.starts_with("image/");
                let view = view.clone();
                div()
                    .id(("chip", i))
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_1()
                    .max_w_full()
                    .pl_1()
                    .rounded_md()
                    .bg(rgba(0x0000000f))
                    .text_xs()
                    .child(
                        Icon::from(if is_image {
                            Lucide::FileImage
                        } else {
                            Lucide::File
                        })
                        .xsmall(),
                    )
                    .child(
                        div()
                            .id(("chip-name", i))
                            .cursor_pointer()
                            .overflow_hidden()
                            .text_ellipsis()
                            .whitespace_nowrap()
                            .child(format!("{} ({})", a.file_name, human_size(a.size)))
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.open_attachment(id, window, cx)
                            })),
                    )
                    .child(
                        Button::new(("chip-menu", i))
                            .ghost()
                            .xsmall()
                            .icon(Icon::from(Lucide::ChevronDown))
                            .dropdown_menu(move |menu, window, _| {
                                let meta = meta.clone();
                                menu.item(PopupMenuItem::new("開く").on_click(
                                    window.listener_for(&view, move |this, _, window, cx| {
                                        this.open_attachment(id, window, cx)
                                    }),
                                ))
                                .item(PopupMenuItem::new("名前を付けて保存…").on_click(
                                    window.listener_for(&view, move |this, _, window, cx| {
                                        this.save_attachment_as(meta.clone(), window, cx)
                                    }),
                                ))
                                .separator()
                                .item(
                                    PopupMenuItem::new("添付を削除").on_click(
                                        window.listener_for(&view, move |this, _, _, cx| {
                                            this.remove_attachment(id, cx)
                                        }),
                                    ),
                                )
                            }),
                    )
            }))
    }
}

impl Render for NoteWindow {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let Some(n) = self.snapshot.clone() else {
            return div()
                .size_full()
                .child("付箋を読み込めません")
                .into_any_element();
        };
        let font = app::settings(cx).font_family.clone();
        let editing = self.editing.is_some();

        // 右下のサイズ変更つまみ
        let grip = div()
            .id("grip")
            .absolute()
            .right_0()
            .bottom_0()
            .size(px(14.))
            .cursor_nwse_resize()
            .on_mouse_down(MouseButton::Left, |_, window, cx| {
                cx.stop_propagation();
                window.start_window_resize(ResizeEdge::BottomRight);
            })
            .child(
                Icon::from(Lucide::GripHorizontal)
                    .xsmall()
                    .text_color(rgba(0x00000040)),
            );

        div()
            .id("note")
            .key_context("FusenNote")
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::on_paste_action))
            .relative()
            .size_full()
            .flex()
            .flex_col()
            .rounded_lg()
            .overflow_hidden()
            .border_1()
            .border_color(rgba(0x0000001a))
            .bg(palette::background(&n.color))
            .text_color(rgb(palette::TEXT))
            .font_family(font)
            .text_sm()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, window, cx| {
                    // 整形表示中にクリックしたら付箋にフォーカス（貼り付けのため）
                    if this.editing.is_none() {
                        this.focus_handle.focus(window, cx);
                    }
                }),
            )
            // ファイルのドラッグ＆ドロップ
            .drag_over::<ExternalPaths>(|s, _, _, cx| s.bg(cx.theme().drag_border.opacity(0.25)))
            .on_drop(cx.listener(|this, paths: &ExternalPaths, window, cx| {
                this.attach_paths(paths.paths().to_vec(), window, cx)
            }))
            .child(self.render_bar(&n.color, cx))
            .when(self.updated_elsewhere(cx), |d| {
                // 編集中の内容は上書きしない。保存すると後勝ちで自分の版が最新になり、相手の版は履歴に残る
                d.child(div().px_2().py_0p5().text_xs().bg(rgba(0xff99004d)).child(
                    "他の端末で更新されました（保存すると上書きし、相手の版は履歴に残ります）",
                ))
            })
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .px_2()
                    .py_1()
                    .when(editing, |d| d.bg(rgba(0xffffff66)))
                    .child(self.render_body(&n, cx)),
            )
            .when(!self.attachments.is_empty(), |d| {
                d.child(self.render_chips(cx))
            })
            .child(grip)
            .into_any_element()
    }
}

/// バイト数を読みやすく
pub fn human_size(n: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KB", "MB", "GB"];
    let mut v = n as f64;
    let mut u = 0;
    while v >= 1024.0 && u < UNITS.len() - 1 {
        v /= 1024.0;
        u += 1;
    }
    if u == 0 {
        format!("{n} B")
    } else {
        format!("{v:.1} {}", UNITS[u])
    }
}

#[cfg(test)]
mod tests {
    // gpui_kit::* には gpui の test 属性マクロが含まれ、#[test] と衝突するので必要なものだけ使う
    use super::human_size;

    #[test]
    fn human_sizes() {
        assert_eq!(human_size(10), "10 B");
        assert_eq!(human_size(1536), "1.5 KB");
        assert_eq!(human_size(5 * 1024 * 1024), "5.0 MB");
    }
}
