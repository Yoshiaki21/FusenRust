//! 本文中に表示する画像（PNG / JPEG / JXL）のデコードとキャッシュ。
//!
//! デコードはバックグラウンドで行い、終わったら全ウィンドウを描き直す。
//! gpui の `RenderImage` は BGRA の並びを要求するので、RGBA の R と B を入れ替える。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Context as _;
use gpui_kit::{App, Global, RenderImage};
use uuid::Uuid;

enum ImageState {
    Loading,
    Ready(Arc<RenderImage>),
    Failed,
}

/// attachment_id → デコード済みの画像
#[derive(Default)]
pub struct ImageCache {
    states: HashMap<Uuid, ImageState>,
}

impl Global for ImageCache {}

/// デコード済みの画像を返す。まだなら読み込みを始めて `None` を返す（終わると描き直される）。
pub fn get_or_load(
    cx: &mut App,
    attachment_id: Uuid,
    path: &Path,
    mime: &str,
) -> Option<Arc<RenderImage>> {
    let cache = cx.default_global::<ImageCache>();
    match cache.states.get(&attachment_id) {
        Some(ImageState::Ready(img)) => return Some(img.clone()),
        Some(ImageState::Loading | ImageState::Failed) => return None,
        None => {}
    }
    cache.states.insert(attachment_id, ImageState::Loading);

    let path: PathBuf = path.to_path_buf();
    let is_jxl = mime == "image/jxl";
    let task = cx
        .background_executor()
        .spawn(async move { decode(&path, is_jxl) });
    cx.spawn(async move |cx| {
        let result = task.await;
        cx.update(|cx| {
            let state = match result {
                Ok(img) => ImageState::Ready(img),
                Err(e) => {
                    tracing::warn!("画像をデコードできません: {e:#}");
                    ImageState::Failed
                }
            };
            cx.default_global::<ImageCache>()
                .states
                .insert(attachment_id, state);
            cx.refresh_windows();
        });
    })
    .detach();
    None
}

/// 画像ファイルを `RenderImage` に変換する
pub fn decode(path: &Path, is_jxl: bool) -> anyhow::Result<Arc<RenderImage>> {
    let rgba = if is_jxl {
        // JXL は image クレートが対応していないので jxl-oxide の image 連携でデコードする
        let file = std::fs::File::open(path).with_context(|| format!("開けません: {path:?}"))?;
        let decoder = jxl_oxide::integration::JxlDecoder::new(std::io::BufReader::new(file))
            .context("JXL のデコーダーを作れません")?;
        image::DynamicImage::from_decoder(decoder)
            .context("JXL のデコードに失敗")?
            .to_rgba8()
    } else {
        image::ImageReader::open(path)
            .with_context(|| format!("開けません: {path:?}"))?
            .with_guessed_format()?
            .decode()
            .context("画像のデコードに失敗")?
            .to_rgba8()
    };
    let mut bgra = rgba;
    for px in bgra.pixels_mut() {
        px.0.swap(0, 2);
    }
    Ok(Arc::new(RenderImage::new([image::Frame::new(bgra)])))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_png_as_bgra() {
        let dir = std::env::temp_dir().join(format!("fusen-img-{}", Uuid::now_v7()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("red.png");
        image::RgbaImage::from_pixel(2, 2, image::Rgba([255, 0, 0, 255]))
            .save(&p)
            .unwrap();
        let img = decode(&p, false).unwrap();
        // BGRA なので先頭のバイトは青（0）、3 番目が赤（255）
        let bytes = img.as_bytes(0).unwrap();
        assert_eq!(&bytes[..4], &[0, 0, 255, 255]);
        let _ = std::fs::remove_dir_all(dir);
    }
}
