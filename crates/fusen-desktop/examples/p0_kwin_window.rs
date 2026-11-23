//! Phase 0 検証 D: 「付箋 1 枚 = 普通のウィンドウ（xdg-toplevel）1 つ ＋ KWin スクリプト」方式。
//!
//! layer-shell では位置と層を後から変えられない（gpui の制約）ため、普通のウィンドウで付箋を作り、
//! KWin スクリプト（D-Bus）で次を設定する:
//! - 壁モード = 常に背面（keepBelow）、最前面モード = 常に最前面（keepAbove）
//! - タスクバー・ページャー・Alt+Tab に出さない、全仮想デスクトップに表示
//!
//! 移動とサイズ変更は OS（KWin）の機能を使う（`start_window_move` / `start_window_resize`）。
//! 本文の表示・編集は方式 A と同じ（common の NoteBody）。
//! 画像などのファイルを付箋にドロップすると、付箋の一番下に添付として表示する。
//!
//! 使い方:
//!   cargo run -p fusen-desktop --example p0_kwin_window

#[cfg(target_os = "linux")]
mod common;

#[cfg(target_os = "linux")]
fn main() {
    linux::main();
}

#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("この検証は Linux (KDE Plasma) 専用です");
}

#[cfg(target_os = "linux")]
mod linux {
    use std::path::PathBuf;
    use std::time::Duration;

    use gpui_kit::prelude::FluentBuilder as _;
    use gpui_kit::*;

    use crate::common::kwin::{self, NoteLayer};
    use crate::common::{self, NOTE_YELLOW, NoteBody};

    struct NoteWindow {
        id: usize,
        layer: NoteLayer,
        body: Entity<NoteBody>,
    }

    impl NoteWindow {
        fn new(
            id: usize,
            text: &str,
            tmp_dir: PathBuf,
            window: &mut Window,
            cx: &mut Context<Self>,
        ) -> Self {
            // KWin スクリプトで見分けるためのタイトル（枠なしなので画面には出ない）
            window.set_window_title(&kwin::note_caption(id));
            let images = common::sample_images(&tmp_dir);
            let body = cx.new(|cx| NoteBody::new(text, images, window, cx));

            // ウィンドウが表示（map）されてから KWin に設定を送る
            cx.spawn(async move |this, cx| {
                cx.background_executor()
                    .timer(Duration::from_millis(300))
                    .await;
                let _ = this.update(cx, |this, cx| this.apply_layer(cx));
            })
            .detach();

            Self {
                id,
                layer: NoteLayer::Wall,
                body,
            }
        }

        /// 現在の層を KWin に反映する（D-Bus の呼び出しはバックグラウンドで行う）
        fn apply_layer(&self, cx: &mut Context<Self>) {
            let caption = kwin::note_caption(self.id);
            let layer = self.layer;
            cx.background_executor()
                .spawn(async move {
                    match kwin::apply_note_window(&caption, layer, None) {
                        Ok(()) => tracing::info!("{caption}: {layer:?} を設定"),
                        Err(e) => tracing::error!("{caption}: KWin の設定に失敗: {e:#}"),
                    }
                })
                .detach();
        }
    }

    impl Render for NoteWindow {
        fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            let pinned = self.layer == NoteLayer::Top;

            let bar = div()
                .id("bar")
                .flex()
                .flex_row()
                .items_center()
                .gap_2()
                .h(px(28.))
                .px_2()
                .bg(rgba(0x00000014))
                .cursor_move()
                // バーを押したら KWin の移動操作を始める（滑らかに追従し、モニターもまたげる）
                .on_mouse_down(MouseButton::Left, |_, window, _| window.start_window_move())
                .child(
                    div()
                        .id("pin")
                        .px_1()
                        .rounded_sm()
                        .cursor_pointer()
                        .when(pinned, |d| d.bg(rgba(0x00000033)))
                        .child(if pinned { "📌 最前面" } else { "📍 壁" })
                        // ボタンの押下でウィンドウ移動が始まらないようにする
                        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.layer = match this.layer {
                                NoteLayer::Wall => NoteLayer::Top,
                                NoteLayer::Top => NoteLayer::Wall,
                            };
                            this.apply_layer(cx);
                            cx.notify();
                        })),
                )
                .child(div().text_xs().child(format!("#{}", self.id)))
                .child(div().flex_1())
                .child(
                    div()
                        .id("close")
                        .cursor_pointer()
                        .child("✕")
                        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                        .on_click(|_, window, _| window.remove_window()),
                );

            // 右下のつまみで KWin のサイズ変更を始める
            let grip = div()
                .id("grip")
                .absolute()
                .right_0()
                .bottom_0()
                .size(px(16.))
                .cursor_nwse_resize()
                .child(div().text_xs().child("◢"))
                .on_mouse_down(MouseButton::Left, |_, window, cx| {
                    cx.stop_propagation();
                    window.start_window_resize(ResizeEdge::BottomRight);
                });

            div()
                .relative()
                .size_full()
                .flex()
                .flex_col()
                .rounded_lg()
                .overflow_hidden()
                .bg(rgb(NOTE_YELLOW))
                .child(bar)
                .child(self.body.clone())
                .child(grip)
        }
    }

    fn open_note(id: usize, text: &str, origin: Point<Pixels>, tmp_dir: PathBuf, cx: &mut App) {
        let text = text.to_string();
        let options = WindowOptions {
            // タイトルバーなし・枠なし（クライアント側で描く＝何も描かない）・透明背景
            titlebar: None,
            window_decorations: Some(WindowDecorations::Client),
            window_background: WindowBackgroundAppearance::Transparent,
            window_bounds: Some(WindowBounds::Windowed(Bounds {
                origin,
                size: size(px(340.), px(420.)),
            })),
            window_min_size: Some(size(px(160.), px(120.))),
            app_id: Some("fusen-p0".to_string()),
            kind: WindowKind::Normal,
            ..Default::default()
        };
        if let Err(e) = gpui_kit::open_window(options, cx, |window, cx| {
            cx.new(|cx| NoteWindow::new(id, &text, tmp_dir, window, cx))
        }) {
            tracing::error!("付箋ウィンドウを作れません: {e:#}");
        }
    }

    pub fn main() {
        common::init_tracing();
        let tmp_dir = std::env::temp_dir().join("fusen-p0");

        gpui_kit::application()
            .with_assets(gpui_kit::assets::Assets)
            .run(move |cx| {
                gpui_kit::init(cx);
                open_note(
                    1,
                    common::SAMPLE_MARKDOWN,
                    point(px(200.), px(150.)),
                    tmp_dir.clone(),
                    cx,
                );
                open_note(
                    2,
                    "# 2 枚目\n\nここに画像やファイルをドロップすると、一番下に添付されます。\n",
                    point(px(600.), px(150.)),
                    tmp_dir,
                    cx,
                );
                // 全部の付箋を閉じたら終了する
                cx.on_window_closed(|cx, _| {
                    if cx.windows().is_empty() {
                        cx.quit();
                    }
                })
                .detach();
            });
    }
}
