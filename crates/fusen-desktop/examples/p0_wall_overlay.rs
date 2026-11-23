//! Phase 0 検証 B: 「モニター全面の透明な layer-shell サーフェス ＋ 入力領域の制限」方式。
//!
//! gpui には layer-shell サーフェスの margin（位置）や層を作成後に変える API がない。
//! そこで、モニターごと・層（Bottom / Top）ごとに画面全体を覆う透明なサーフェスを 1 つずつ作り、
//! 付箋はその中の要素として描く。`Window::set_input_region` で付箋の矩形だけを入力領域にするので、
//! それ以外の場所のクリックは下（デスクトップのアイコンや他のウィンドウ）に届く。
//!
//! - 移動: サーフェス内の要素の座標を変えるだけなので、ドラッグに滑らかに追従する
//! - 最前面の切り替え: 付箋を Bottom のサーフェスから Top のサーフェスへ移す
//! - モニターをまたぐ移動: ドラッグ終了時に、移動先のモニターのサーフェスへ移す
//!
//! 使い方:
//!   cargo run -p fusen-desktop --example p0_wall_overlay

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
    use std::collections::HashMap;
    use std::path::PathBuf;

    use gpui_kit::layer_shell::{Anchor, KeyboardInteractivity, Layer, LayerShellOptions};
    use gpui_kit::prelude::FluentBuilder as _;
    use gpui_kit::*;

    use crate::common::{self, NOTE_YELLOW, NoteBody};

    /// 付箋 1 枚の配置
    #[derive(Clone, Debug)]
    struct Note {
        id: usize,
        text: String,
        display_index: usize,
        layer: Layer,
        /// サーフェス（＝モニター）内の座標
        origin: Point<Pixels>,
        size: Size<Pixels>,
    }

    /// 全付箋の状態（全サーフェスで共有）
    struct Wall {
        notes: Vec<Note>,
        /// display_index → そのモニターの全体座標での原点
        display_origins: Vec<Point<Pixels>>,
    }

    /// 1 つのモニター・1 つの層を覆うサーフェス
    struct WallSurface {
        wall: Entity<Wall>,
        display_index: usize,
        layer: Layer,
        bodies: HashMap<usize, Entity<NoteBody>>,
        /// ドラッグ中の付箋: (id, ポインタと付箋原点の差)
        dragging: Option<(usize, Point<Pixels>)>,
        /// サイズ変更中の付箋: (id, 開始ポインタ位置, 開始サイズ)
        resizing: Option<(usize, Point<Pixels>, Size<Pixels>)>,
        tmp_dir: PathBuf,
        _observe: Subscription,
    }

    impl WallSurface {
        fn new(
            wall: Entity<Wall>,
            display_index: usize,
            layer: Layer,
            tmp_dir: PathBuf,
            window: &mut Window,
            cx: &mut Context<Self>,
        ) -> Self {
            let observe = cx.observe_in(&wall, window, |_, _, _, cx| cx.notify());
            Self {
                wall,
                display_index,
                layer,
                bodies: HashMap::new(),
                dragging: None,
                resizing: None,
                tmp_dir,
                _observe: observe,
            }
        }

        fn body_for(
            &mut self,
            note: &Note,
            window: &mut Window,
            cx: &mut Context<Self>,
        ) -> Entity<NoteBody> {
            if let Some(b) = self.bodies.get(&note.id) {
                return b.clone();
            }
            let images = common::sample_images(&self.tmp_dir);
            let text = note.text.clone();
            let b = cx.new(|cx| NoteBody::new(text, images, window, cx));
            self.bodies.insert(note.id, b.clone());
            b
        }

        /// ドラッグ終了時: はみ出していれば移動先のモニターへ移す
        fn finish_drag(&mut self, id: usize, cx: &mut Context<Self>) {
            let display_index = self.display_index;
            self.wall.update(cx, |wall, cx| {
                let origins = wall.display_origins.clone();
                if let Some(n) = wall.notes.iter_mut().find(|n| n.id == id) {
                    let here = origins.get(display_index).copied().unwrap_or_default();
                    // 付箋の中心の全体座標
                    let center = here + n.origin + point(n.size.width / 2., n.size.height / 2.);
                    // 中心が含まれるモニターを探す（各モニターの原点の右下側で最も近いもの）
                    let target = origins
                        .iter()
                        .enumerate()
                        .filter(|(_, o)| center.x >= o.x && center.y >= o.y)
                        .max_by(|a, b| {
                            (a.1.x + a.1.y)
                                .partial_cmp(&(b.1.x + b.1.y))
                                .unwrap_or(std::cmp::Ordering::Equal)
                        })
                        .map(|(i, _)| i)
                        .unwrap_or(display_index);
                    if target != n.display_index {
                        let to = origins[target];
                        n.origin = here + n.origin - to;
                        tracing::info!("モニター {} → {} へ移動", n.display_index, target);
                        n.display_index = target;
                    }
                    cx.notify();
                }
            });
        }
    }

    impl Render for WallSurface {
        fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            let notes: Vec<Note> = self
                .wall
                .read(cx)
                .notes
                .iter()
                .filter(|n| n.display_index == self.display_index && n.layer == self.layer)
                .cloned()
                .collect();

            // 付箋の矩形だけを入力領域にする（それ以外のクリックは下に抜ける）。
            // ドラッグ・サイズ変更中は全面で受けて、ポインタが付箋から外れても追従できるようにする。
            if self.dragging.is_some() || self.resizing.is_some() {
                window.set_input_region(None);
            } else {
                let rects: Vec<Bounds<Pixels>> = notes
                    .iter()
                    .map(|n| Bounds::new(n.origin, n.size))
                    .collect();
                window.set_input_region(Some(&rects));
            }

            // 使わなくなった本文ビューを捨てる
            self.bodies
                .retain(|id, _| notes.iter().any(|n| n.id == *id));

            let mut children = Vec::new();
            for n in &notes {
                let body = self.body_for(n, window, cx);
                let id = n.id;
                let pinned = n.layer == Layer::Top;
                let bar = div()
                    .id(("bar", id))
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
                        cx.listener(move |this, ev: &MouseDownEvent, _, cx| {
                            let origin = this
                                .wall
                                .read(cx)
                                .notes
                                .iter()
                                .find(|n| n.id == id)
                                .map(|n| n.origin)
                                .unwrap_or_default();
                            this.dragging = Some((id, ev.position - origin));
                            cx.notify();
                        }),
                    )
                    .child(
                        div()
                            .id(("pin", id))
                            .px_1()
                            .rounded_sm()
                            .cursor_pointer()
                            .when(pinned, |d| d.bg(rgba(0x00000033)))
                            .child(if pinned { "📌 最前面" } else { "📍 壁" })
                            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.wall.update(cx, |wall, cx| {
                                    if let Some(n) = wall.notes.iter_mut().find(|n| n.id == id) {
                                        n.layer = if n.layer == Layer::Top {
                                            Layer::Bottom
                                        } else {
                                            Layer::Top
                                        };
                                        tracing::info!("付箋 {id} の層 → {:?}", n.layer);
                                    }
                                    cx.notify();
                                });
                            })),
                    )
                    .child(div().text_xs().child(format!(
                        "#{id} display {} ({:.0},{:.0})",
                        n.display_index,
                        f32::from(n.origin.x),
                        f32::from(n.origin.y)
                    )));

                let grip = div()
                    .id(("grip", id))
                    .absolute()
                    .right_0()
                    .bottom_0()
                    .size(px(16.))
                    .cursor_nwse_resize()
                    .child(div().text_xs().child("◢"))
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, ev: &MouseDownEvent, _, cx| {
                            let size = this
                                .wall
                                .read(cx)
                                .notes
                                .iter()
                                .find(|n| n.id == id)
                                .map(|n| n.size)
                                .unwrap_or_default();
                            this.resizing = Some((id, ev.position, size));
                            cx.stop_propagation();
                            cx.notify();
                        }),
                    );

                children.push(
                    div()
                        .absolute()
                        .left(n.origin.x)
                        .top(n.origin.y)
                        .w(n.size.width)
                        .h(n.size.height)
                        .flex()
                        .flex_col()
                        .rounded_lg()
                        .overflow_hidden()
                        .shadow_lg()
                        .bg(rgb(NOTE_YELLOW))
                        .child(bar)
                        .child(body)
                        .child(grip),
                );
            }

            div()
                .id("wall")
                .size_full()
                .relative()
                .on_mouse_move(cx.listener(|this, ev: &MouseMoveEvent, _, cx| {
                    if ev.pressed_button != Some(MouseButton::Left) {
                        return;
                    }
                    if let Some((id, grab)) = this.dragging {
                        this.wall.update(cx, |wall, cx| {
                            if let Some(n) = wall.notes.iter_mut().find(|n| n.id == id) {
                                n.origin = ev.position - grab;
                            }
                            cx.notify();
                        });
                    } else if let Some((id, start, size)) = this.resizing {
                        let d = ev.position - start;
                        this.wall.update(cx, |wall, cx| {
                            if let Some(n) = wall.notes.iter_mut().find(|n| n.id == id) {
                                n.size = Size::new(
                                    (size.width + d.x).max(px(160.)),
                                    (size.height + d.y).max(px(120.)),
                                );
                            }
                            cx.notify();
                        });
                    }
                }))
                .on_mouse_up(
                    MouseButton::Left,
                    cx.listener(|this, _: &MouseUpEvent, _, cx| {
                        if let Some((id, _)) = this.dragging.take() {
                            this.finish_drag(id, cx);
                        }
                        this.resizing = None;
                        cx.notify();
                    }),
                )
                .children(children)
        }
    }

    fn open_surface(
        wall: Entity<Wall>,
        display_index: usize,
        display_id: DisplayId,
        size: Size<Pixels>,
        layer: Layer,
        tmp_dir: PathBuf,
        cx: &mut App,
    ) {
        let options = WindowOptions {
            titlebar: None,
            window_bounds: Some(WindowBounds::Windowed(Bounds {
                origin: point(px(0.), px(0.)),
                size,
            })),
            app_id: Some("fusen-p0".to_string()),
            window_background: WindowBackgroundAppearance::Transparent,
            display_id: Some(display_id),
            kind: WindowKind::LayerShell(LayerShellOptions {
                namespace: "fusen".to_string(),
                layer,
                // 四辺に anchor して画面全体を覆う（サイズはコンポジターが決める）
                anchor: Anchor::TOP | Anchor::BOTTOM | Anchor::LEFT | Anchor::RIGHT,
                // パネルの領域（exclusive zone）は避ける
                exclusive_zone: None,
                keyboard_interactivity: KeyboardInteractivity::OnDemand,
                ..Default::default()
            }),
            ..Default::default()
        };
        if let Err(e) = gpui_kit::open_window(options, cx, |window, cx| {
            cx.new(|cx| WallSurface::new(wall, display_index, layer, tmp_dir, window, cx))
        }) {
            tracing::error!("サーフェスを作れません: {e:#}");
        }
    }

    pub fn main() {
        common::init_tracing();
        let tmp_dir = std::env::temp_dir().join("fusen-p0");

        gpui_kit::application()
            .with_assets(gpui_kit::assets::Assets)
            .run(move |cx| {
                gpui_kit::init(cx);

                let displays = cx.displays();
                let display_origins: Vec<_> = displays.iter().map(|d| d.bounds().origin).collect();
                for (i, d) in displays.iter().enumerate() {
                    tracing::info!("display {i}: {:?} bounds={:?}", d.id(), d.bounds());
                }

                let notes = vec![
                    Note {
                        id: 1,
                        text: common::SAMPLE_MARKDOWN.to_string(),
                        display_index: 0,
                        layer: Layer::Bottom,
                        origin: point(px(200.), px(150.)),
                        size: Size::new(px(360.), px(480.)),
                    },
                    Note {
                        id: 2,
                        text: "# 2 枚目\n\nダブルクリックで編集。\n日本語入力のテスト: "
                            .to_string(),
                        display_index: 0,
                        layer: Layer::Top,
                        origin: point(px(620.), px(150.)),
                        size: Size::new(px(300.), px(240.)),
                    },
                    Note {
                        id: 3,
                        text: "# 2 台目のモニター\n\nここに表示されていれば成功".to_string(),
                        display_index: 1.min(displays.len().saturating_sub(1)),
                        layer: Layer::Bottom,
                        origin: point(px(200.), px(200.)),
                        size: Size::new(px(300.), px(200.)),
                    },
                ];

                let wall = cx.new(|_| Wall {
                    notes,
                    display_origins,
                });

                for (i, d) in displays.iter().enumerate() {
                    let size = d.bounds().size;
                    for layer in [Layer::Bottom, Layer::Top] {
                        open_surface(wall.clone(), i, d.id(), size, layer, tmp_dir.clone(), cx);
                    }
                }
            });
    }
}
