//! Phase 0 検証 A: 「付箋 1 枚 = layer-shell のサーフェス 1 つ」方式。
//!
//! 確認すること:
//! - Bottom 層に枠なし・透明背景・角丸の付箋を出せるか
//! - 左上 anchor ＋ margin で好きな位置に置けるか
//! - Bottom ⇔ Top の切り替え（gpui には作成後に層を変える API がないので、作り直しで行う）
//! - 移動（gpui には作成後に margin を変える API がないので、ドラッグ終了時に作り直す）
//! - サイズ変更（`Window::resize` → layer_surface.set_size でその場で変えられる）
//! - 2 台目のモニターへの表示（`--display 1`）
//! - Markdown・画像・編集・日本語入力・ファイルのドロップ（common の NoteBody）
//!
//! 使い方:
//!   cargo run -p fusen-desktop --example p0_note_surface -- [--top] [--display N] [--x X] [--y Y]

#[cfg(target_os = "linux")]
mod common;

#[cfg(target_os = "linux")]
fn main() {
    linux::main();
}

#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("この検証は Linux (KDE Plasma / Wayland) 専用です");
}

#[cfg(target_os = "linux")]
mod linux {
    use std::path::PathBuf;

    use gpui_kit::layer_shell::{Anchor, KeyboardInteractivity, Layer, LayerShellOptions};
    use gpui_kit::prelude::FluentBuilder as _;
    use gpui_kit::*;

    use crate::common::{self, NOTE_YELLOW, NoteBody};

    /// 付箋サーフェスの作成パラメーター
    #[derive(Clone, Copy, Debug)]
    struct Placement {
        layer: Layer,
        display_index: usize,
        x: f32,
        y: f32,
        width: f32,
        height: f32,
    }

    struct NoteWindow {
        placement: Placement,
        body: Entity<NoteBody>,
        /// バーのドラッグ開始位置（ウィンドウ内座標）
        drag_start: Option<Point<Pixels>>,
        /// ドラッグ中の移動量（表示用）
        drag_delta: Point<Pixels>,
        /// 右下つまみでのサイズ変更の開始時の (ポインタ位置, サイズ)
        resize_start: Option<(Point<Pixels>, Size<Pixels>)>,
        tmp_dir: PathBuf,
    }

    impl NoteWindow {
        fn new(
            placement: Placement,
            tmp_dir: PathBuf,
            window: &mut Window,
            cx: &mut Context<Self>,
        ) -> Self {
            let images = common::sample_images(&tmp_dir);
            let body = cx.new(|cx| NoteBody::new(common::SAMPLE_MARKDOWN, images, window, cx));
            Self {
                placement,
                body,
                drag_start: None,
                drag_delta: point(px(0.), px(0.)),
                resize_start: None,
                tmp_dir,
            }
        }

        /// サーフェスを新しい配置で作り直し、今のウィンドウを閉じる。
        ///
        /// layer-shell の層と margin は gpui では作成時にしか指定できないため。
        fn recreate(&self, placement: Placement, window: &mut Window, cx: &mut App) {
            let tmp_dir = self.tmp_dir.clone();
            open_note(placement, tmp_dir, cx);
            window.remove_window();
        }
    }

    impl Render for NoteWindow {
        fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            let p = self.placement;
            let pinned = p.layer == Layer::Top;

            // 付箋の外側に影のための余白を取る（サーフェス自体は透明）
            let shadow_pad = px(10.);

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
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, ev: &MouseDownEvent, _, _| {
                        this.drag_start = Some(ev.position);
                    }),
                )
                .on_mouse_move(cx.listener(|this, ev: &MouseMoveEvent, _, cx| {
                    // ボタンを押している間は Wayland の暗黙のグラブにより、
                    // サーフェスの外に出てもこのサーフェスに移動イベントが届く
                    if let Some(start) = this.drag_start
                        && ev.pressed_button == Some(MouseButton::Left)
                    {
                        this.drag_delta = ev.position - start;
                        cx.notify();
                    }
                }))
                .on_mouse_up(
                    MouseButton::Left,
                    cx.listener(|this, ev: &MouseUpEvent, window, cx| {
                        if let Some(start) = this.drag_start.take() {
                            let d = ev.position - start;
                            this.drag_delta = point(px(0.), px(0.));
                            if f32::from(d.x).abs() > 2. || f32::from(d.y).abs() > 2. {
                                let mut np = this.placement;
                                np.x = (np.x + f32::from(d.x)).max(0.);
                                np.y = (np.y + f32::from(d.y)).max(0.);
                                tracing::info!("移動 → 作り直し: ({}, {})", np.x, np.y);
                                this.recreate(np, window, cx);
                            }
                        }
                    }),
                )
                .child(
                    // 最前面の切り替え（作り直しで層を変える）
                    div()
                        .id("pin")
                        .px_1()
                        .rounded_sm()
                        .cursor_pointer()
                        .when(pinned, |d| d.bg(rgba(0x00000033)))
                        .child(if pinned { "📌 最前面" } else { "📍 壁" })
                        .on_click(cx.listener(|this, _, window, cx| {
                            let mut np = this.placement;
                            np.layer = if np.layer == Layer::Top {
                                Layer::Bottom
                            } else {
                                Layer::Top
                            };
                            tracing::info!("層の切り替え → {:?}", np.layer);
                            this.recreate(np, window, cx);
                        })),
                )
                .child(div().text_xs().child(format!(
                    "display {} / ({:.0},{:.0}) Δ({:.0},{:.0})",
                    p.display_index,
                    p.x,
                    p.y,
                    f32::from(self.drag_delta.x),
                    f32::from(self.drag_delta.y)
                )))
                .child(div().flex_1())
                .child(
                    div()
                        .id("close")
                        .cursor_pointer()
                        .child("✕")
                        .on_click(cx.listener(|_, _, window, _| window.remove_window())),
                );

            // 右下のサイズ変更つまみ
            let grip = div()
                .id("grip")
                .absolute()
                .right_0()
                .bottom_0()
                .size(px(16.))
                .cursor_nwse_resize()
                .child(div().text_xs().child("◢"))
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, ev: &MouseDownEvent, window, _| {
                        this.resize_start = Some((ev.position, window.viewport_size()));
                    }),
                )
                .on_mouse_move(cx.listener(|this, ev: &MouseMoveEvent, window, _| {
                    if let Some((start, size)) = this.resize_start
                        && ev.pressed_button == Some(MouseButton::Left)
                    {
                        let d = ev.position - start;
                        let new_size = Size::new(
                            (size.width + d.x).max(px(160.)),
                            (size.height + d.y).max(px(120.)),
                        );
                        window.resize(new_size);
                    }
                }))
                .on_mouse_up(
                    MouseButton::Left,
                    cx.listener(|this, _: &MouseUpEvent, window, _| {
                        if this.resize_start.take().is_some() {
                            let s = window.viewport_size();
                            this.placement.width = f32::from(s.width);
                            this.placement.height = f32::from(s.height);
                        }
                    }),
                );

            let _ = window;
            div().size_full().p(shadow_pad).child(
                div()
                    .relative()
                    .size_full()
                    .flex()
                    .flex_col()
                    .rounded_lg()
                    .overflow_hidden()
                    .shadow_lg()
                    .bg(rgb(NOTE_YELLOW))
                    .child(bar)
                    .child(self.body.clone())
                    .child(grip),
            )
        }
    }

    fn open_note(p: Placement, tmp_dir: PathBuf, cx: &mut App) {
        let displays = cx.displays();
        let display_id = displays.get(p.display_index).map(|d| d.id());
        if display_id.is_none() {
            tracing::warn!(
                "display {} がありません（{} 台）",
                p.display_index,
                displays.len()
            );
        }
        for (i, d) in displays.iter().enumerate() {
            tracing::info!("display {i}: {:?} bounds={:?}", d.id(), d.bounds());
        }

        let options = WindowOptions {
            titlebar: None,
            window_bounds: Some(WindowBounds::Windowed(Bounds {
                origin: point(px(0.), px(0.)),
                size: Size::new(px(p.width), px(p.height)),
            })),
            app_id: Some("fusen-p0".to_string()),
            window_background: WindowBackgroundAppearance::Transparent,
            display_id,
            kind: WindowKind::LayerShell(LayerShellOptions {
                namespace: "fusen".to_string(),
                layer: p.layer,
                // 左上に anchor し、margin（上, 右, 下, 左）で座標を表す
                anchor: Anchor::TOP | Anchor::LEFT,
                margin: Some((px(p.y), px(0.), px(0.), px(p.x))),
                // 編集のためにキーボード入力を受け付ける（クリックでフォーカス）
                keyboard_interactivity: KeyboardInteractivity::OnDemand,
                ..Default::default()
            }),
            ..Default::default()
        };

        if let Err(e) = gpui_kit::open_window(options, cx, |window, cx| {
            cx.new(|cx| NoteWindow::new(p, tmp_dir, window, cx))
        }) {
            tracing::error!("付箋サーフェスを作れません: {e:#}");
        }
    }

    fn parse_args() -> Placement {
        let mut p = Placement {
            layer: Layer::Bottom,
            display_index: 0,
            x: 200.,
            y: 200.,
            width: 380.,
            height: 520.,
        };
        let mut args = std::env::args().skip(1);
        while let Some(a) = args.next() {
            match a.as_str() {
                "--top" => p.layer = Layer::Top,
                "--display" => {
                    p.display_index = args.next().and_then(|v| v.parse().ok()).unwrap_or(0)
                }
                "--x" => p.x = args.next().and_then(|v| v.parse().ok()).unwrap_or(p.x),
                "--y" => p.y = args.next().and_then(|v| v.parse().ok()).unwrap_or(p.y),
                _ => eprintln!("不明な引数: {a}"),
            }
        }
        p
    }

    pub fn main() {
        common::init_tracing();
        let placement = parse_args();
        let tmp_dir = std::env::temp_dir().join("fusen-p0");

        gpui_kit::application()
            .with_assets(gpui_kit::assets::Assets)
            .run(move |cx| {
                gpui_kit::init(cx);
                open_note(placement, tmp_dir, cx);
            });
    }
}
