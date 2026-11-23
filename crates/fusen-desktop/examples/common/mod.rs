//! Phase 0 の検証用プログラムで共有する部品。
//!
//! - 画像（PNG / JPEG / JXL）を gpui の `RenderImage` に変換する
//! - 付箋の本文ビュー（Markdown の整形表示 ⇔ ダブルクリックで編集）
//!
//! 本実装ではなく検証用なので、エラーはログに出すだけにしている。
#![allow(dead_code)]

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::Arc,
};

use anyhow::Context as _;
use gpui_kit::component::{
    ActiveTheme as _,
    input::{Editor, EditorState},
    text::TextView,
};
use gpui_kit::*;

/// 付箋の色（検証用の仮パレット。本番の色はユーザーと決める）
pub const NOTE_YELLOW: u32 = 0xfff3a8;
pub const NOTE_TEXT: u32 = 0x2b2b2b;

/// 画像ファイルを読み込み、gpui で描画できる `RenderImage` に変換する。
///
/// gpui の `RenderImage` は BGRA の並びを要求するため、RGBA から R と B を入れ替える。
pub fn load_render_image(path: &Path) -> anyhow::Result<Arc<RenderImage>> {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
        .unwrap_or_default();

    let rgba = if ext == "jxl" {
        // JXL は image クレートが対応していないので jxl-oxide の image 連携でデコードする
        let file = std::fs::File::open(path).with_context(|| format!("open {path:?}"))?;
        let decoder = jxl_oxide::integration::JxlDecoder::new(std::io::BufReader::new(file))
            .context("JXL のデコーダー作成に失敗")?;
        image::DynamicImage::from_decoder(decoder)
            .context("JXL のデコードに失敗")?
            .to_rgba8()
    } else {
        image::open(path)
            .with_context(|| format!("画像の読み込みに失敗: {path:?}"))?
            .to_rgba8()
    };

    let mut bgra = rgba;
    for px in bgra.pixels_mut() {
        px.0.swap(0, 2);
    }
    let frame = image::Frame::new(bgra);
    Ok(Arc::new(RenderImage::new([frame])))
}

/// 付箋の本文。整形表示と編集状態を切り替える。
pub struct NoteBody {
    text: String,
    editing: bool,
    editor: Entity<EditorState>,
    /// `attachment://<名前>` → デコード済み画像
    images: Arc<HashMap<String, Arc<RenderImage>>>,
    /// ドロップされたファイル（検証表示用）
    dropped: Vec<PathBuf>,
    _subscriptions: Vec<Subscription>,
}

impl NoteBody {
    pub fn new(
        text: impl Into<String>,
        images: HashMap<String, Arc<RenderImage>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let text = text.into();
        let editor = cx.new(|cx| {
            EditorState::new(window, cx)
                .language("markdown")
                .line_number(false)
                .soft_wrap(true)
                .default_value(text.clone())
        });

        // フォーカスが外れたら整形表示に戻す（本実装ではここで版を保存する）
        let focus = editor.read(cx).focus_handle(cx);
        let sub = cx.on_blur(&focus, window, |this, _window, cx| {
            if this.editing {
                this.text = this.editor.read(cx).value().to_string();
                this.editing = false;
                tracing::info!("編集終了: {} 文字", this.text.chars().count());
                cx.notify();
            }
        });

        Self {
            text,
            editing: false,
            editor,
            images: Arc::new(images),
            dropped: Vec::new(),
            _subscriptions: vec![sub],
        }
    }

    pub fn is_editing(&self) -> bool {
        self.editing
    }

    fn start_editing(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.editing = true;
        let text = self.text.clone();
        self.editor.update(cx, |state, cx| {
            state.set_value(text, window, cx);
            state.focus(window, cx);
        });
        cx.notify();
    }

    fn on_drop_paths(&mut self, paths: &ExternalPaths, cx: &mut Context<Self>) {
        for p in paths.paths() {
            tracing::info!("ドロップ: {p:?}");
            self.dropped.push(p.clone());
        }
        cx.notify();
    }
}

impl Render for NoteBody {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let body: AnyElement = if self.editing {
            Editor::new(&self.editor)
                .bordered(false)
                .appearance(false)
                .h_full()
                .into_any_element()
        } else {
            let images = self.images.clone();
            let view = cx.entity().downgrade();
            div()
                .id("note-view")
                .size_full()
                .overflow_y_scroll()
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, ev: &MouseDownEvent, window, cx| {
                        // ダブルクリックで編集状態へ
                        if ev.click_count == 2 {
                            this.start_editing(window, cx);
                        }
                    }),
                )
                .child(
                    TextView::markdown("note-md", self.text.clone())
                        .selectable(false)
                        .text_color(rgb(NOTE_TEXT))
                        .image_source(move |uri| {
                            // attachment://<名前> は事前にデコードした画像を使う
                            let key = uri.as_ref().trim_start_matches("attachment://");
                            match images.get(key) {
                                Some(img) => ImageSource::Render(img.clone()),
                                None => ImageSource::from(uri.clone()),
                            }
                        })
                        .on_task_toggle(move |event, _window, cx| {
                            // チェックボックスのクリック。本文の [ ] / [x] を書き換える
                            let _ = view.update(cx, |this, cx| {
                                let marker = if event.checked() { "[x]" } else { "[ ]" };
                                this.text.replace_range(event.marker_range(), marker);
                                cx.notify();
                            });
                        }),
                )
                .into_any_element()
        };

        let dropped = self.dropped.clone();
        div()
            .id("note-body")
            .flex()
            .flex_col()
            .flex_1()
            .min_h_0()
            .px_3()
            .py_2()
            .text_color(rgb(NOTE_TEXT))
            // ファイルのドラッグ＆ドロップ
            .drag_over::<ExternalPaths>(|s, _, _, cx| s.bg(cx.theme().drag_border.opacity(0.15)))
            .on_drop(
                cx.listener(|this, paths: &ExternalPaths, _, cx| this.on_drop_paths(paths, cx)),
            )
            .child(div().flex_1().min_h_0().child(body))
            .children(dropped.into_iter().map(|p| {
                div()
                    .text_xs()
                    .child(format!("📎 {}", p.display()))
                    .into_any_element()
            }))
    }
}

/// 検証用のサンプル画像を一時ディレクトリに作り、`attachment://` 名 → 画像 の表を返す。
///
/// JXL は `cjxl` コマンドがあれば PNG から変換して作る。
pub fn sample_images(dir: &Path) -> HashMap<String, Arc<RenderImage>> {
    let mut map = HashMap::new();
    if let Err(e) = std::fs::create_dir_all(dir) {
        tracing::warn!("一時ディレクトリを作れません: {e}");
        return map;
    }

    // グラデーションの PNG を作る
    let png = dir.join("sample.png");
    let img = image::RgbaImage::from_fn(160, 80, |x, y| {
        image::Rgba([(x * 255 / 160) as u8, (y * 255 / 80) as u8, 180, 255])
    });
    if let Err(e) = img.save(&png) {
        tracing::warn!("PNG の保存に失敗: {e}");
    }

    let jxl = dir.join("sample.jxl");
    if !jxl.exists() {
        let status = std::process::Command::new("cjxl")
            .arg(&png)
            .arg(&jxl)
            .arg("-q")
            .arg("90")
            .output();
        if let Err(e) = status {
            tracing::warn!("cjxl を実行できません: {e}");
        }
    }

    for (name, path) in [("sample.png", &png), ("sample.jxl", &jxl)] {
        match load_render_image(path) {
            Ok(img) => {
                map.insert(name.to_string(), img);
            }
            Err(e) => tracing::warn!("{name}: {e:#}"),
        }
    }
    map
}

pub const SAMPLE_MARKDOWN: &str = r#"# 付箋のテスト

**太字** と *斜体* と ~~取り消し線~~、`行内コード`。

- [ ] 未完了のタスク
- [x] 完了したタスク

| 列A | 列B |
|---|---|
| 日本語 | 表示 |

> 引用です

[リンク](https://example.com)

PNG:

![png](attachment://sample.png)

JXL:

![jxl](attachment://sample.jxl)
"#;

/// ログの初期化（`RUST_LOG` で調整可能）
pub fn init_tracing() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .try_init();
}
