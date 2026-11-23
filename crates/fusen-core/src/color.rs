//! 付箋の色（パレットのキー）。
//!
//! 版にはキーの文字列だけを保存し、実際の色は表示側（デスクトップアプリ）が決める。
//! 将来キーを増やしても、古い端末は知らないキーを既定色で表示すればよい。

/// 黄（既定）
pub const YELLOW: &str = "yellow";
/// ピンク
pub const PINK: &str = "pink";
/// 青
pub const BLUE: &str = "blue";
/// 緑
pub const GREEN: &str = "green";
/// 紫
pub const PURPLE: &str = "purple";
/// 灰
pub const GRAY: &str = "gray";

/// 新しい付箋の既定の色
pub const DEFAULT: &str = YELLOW;

/// パレットのキー（表示順）
pub const PALETTE: &[&str] = &[YELLOW, PINK, BLUE, GREEN, PURPLE, GRAY];

/// パレットにあるキーか
pub fn is_known(key: &str) -> bool {
    PALETTE.contains(&key)
}
