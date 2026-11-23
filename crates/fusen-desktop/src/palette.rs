//! 付箋の色（パレットのキー → 表示色）。
//!
//! 版にはキーだけを保存する（`fusen_core::color`）。色味はユーザーと決めた淡いパステル（docs/decisions.md D-005）。

use gpui_kit::{Hsla, Rgba, rgb};

/// 付箋の文字色
pub const TEXT: u32 = 0x2b2b2b;

/// パレットのキーと表示名・色
pub const COLORS: &[(&str, &str, u32)] = &[
    (fusen_core::color::YELLOW, "黄", 0xfff3a8),
    (fusen_core::color::PINK, "ピンク", 0xffd1dc),
    (fusen_core::color::BLUE, "青", 0xcde7ff),
    (fusen_core::color::GREEN, "緑", 0xd4f5c9),
    (fusen_core::color::PURPLE, "紫", 0xe5d4ff),
    (fusen_core::color::GRAY, "灰", 0xe6e6e6),
];

/// キーの背景色。知らないキー（新しい端末で増えた色など）は黄にする
pub fn background(key: &str) -> Rgba {
    let hex = COLORS
        .iter()
        .find(|(k, _, _)| *k == key)
        .map(|(_, _, c)| *c)
        .unwrap_or(COLORS[0].2);
    rgb(hex)
}

/// キーの背景色を少し暗くした色（上部のバーに使う）
pub fn bar(key: &str) -> Hsla {
    let mut c: Hsla = background(key).into();
    c.l = (c.l - 0.06).max(0.0);
    c
}
