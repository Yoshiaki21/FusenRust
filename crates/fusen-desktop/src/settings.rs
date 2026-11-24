//! 設定と保存場所。
//!
//! - 設定: `~/.config/fusen-rust/settings.json`（Windows は `%APPDATA%\fusen-rust\`）
//! - データ: `~/.local/share/fusen-rust/`（`fusen.db` と `blobs/`）
//!
//! `--profile <名前>` を付けると、別の設定・データで起動する（同じ PC で 2 つ動かして同期を試すため）。

use std::path::{Path, PathBuf};

use anyhow::Context as _;
use serde::{Deserialize, Serialize};

/// 既定のフォント（日本語を正しく表示するため）
#[cfg(not(windows))]
pub const DEFAULT_FONT: &str = "Noto Sans CJK JP";
#[cfg(windows)]
pub const DEFAULT_FONT: &str = "Yu Gothic UI";
/// 新しい付箋の既定のサイズ（論理ピクセル。docs/decisions.md D-005）
pub const DEFAULT_NOTE_WIDTH: f64 = 280.0;
pub const DEFAULT_NOTE_HEIGHT: f64 = 320.0;

/// ユーザーが変えられる設定
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    /// 同期サーバーの URL（Phase 4 で使う）
    pub server_url: String,
    /// サーバーのトークン（Phase 4 で使う）
    pub token: String,
    /// 端末名（表示用）
    pub device_name: String,
    /// 付箋のフォント
    pub font_family: String,
    /// 1 ファイルの添付の上限（MB）
    pub max_attachment_mb: u64,
    /// ログイン時に自動起動するか
    pub autostart: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            server_url: String::new(),
            token: String::new(),
            device_name: default_device_name(),
            font_family: DEFAULT_FONT.to_string(),
            max_attachment_mb: fusen_core::DEFAULT_MAX_ATTACHMENT_BYTES / 1024 / 1024,
            autostart: false,
        }
    }
}

impl Settings {
    pub fn max_attachment_bytes(&self) -> u64 {
        self.max_attachment_mb.saturating_mul(1024 * 1024)
    }

    /// 読み込む。なければ既定値。壊れていれば警告して既定値を使う。
    pub fn load(path: &Path) -> Self {
        match std::fs::read_to_string(path) {
            Ok(s) => serde_json::from_str(&s).unwrap_or_else(|e| {
                tracing::warn!("設定ファイルを読めません（既定値を使います）: {e}");
                Self::default()
            }),
            Err(_) => Self::default(),
        }
    }

    pub fn save(&self, path: &Path) -> anyhow::Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let json = serde_json::to_string_pretty(self)?;
        // 書きかけで落ちても壊れないよう、一時ファイルに書いてから置き換える
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, json).with_context(|| format!("書き込めません: {tmp:?}"))?;
        std::fs::rename(&tmp, path)?;
        Ok(())
    }
}

fn default_device_name() -> String {
    std::fs::read_to_string("/etc/hostname")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .or_else(|| std::env::var("COMPUTERNAME").ok())
        .unwrap_or_else(|| "この端末".to_string())
}

/// 保存場所
#[derive(Clone, Debug)]
pub struct AppPaths {
    /// プロファイル名（既定は None）
    pub profile: Option<String>,
    pub data_dir: PathBuf,
    pub settings_file: PathBuf,
}

impl AppPaths {
    pub fn new(profile: Option<String>) -> anyhow::Result<Self> {
        let base = "fusen-rust";
        let sub = match &profile {
            Some(p) => format!("{base}/profiles/{p}"),
            None => base.to_string(),
        };
        let data_dir = dirs::data_dir()
            .context("データディレクトリが分かりません")?
            .join(&sub);
        let settings_file = dirs::config_dir()
            .context("設定ディレクトリが分かりません")?
            .join(&sub)
            .join("settings.json");
        Ok(Self {
            profile,
            data_dir,
            settings_file,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_roundtrip_and_defaults() {
        let dir = tempfile_dir();
        let path = dir.join("settings.json");
        assert_eq!(Settings::load(&path), Settings::default());
        let s = Settings {
            font_family: "IPAexGothic".into(),
            max_attachment_mb: 10,
            ..Default::default()
        };
        s.save(&path).unwrap();
        assert_eq!(Settings::load(&path), s);
        assert_eq!(s.max_attachment_bytes(), 10 * 1024 * 1024);
        // 足りない項目は既定値で補う
        std::fs::write(&path, r#"{"font_family":"X"}"#).unwrap();
        let partial = Settings::load(&path);
        assert_eq!(partial.font_family, "X");
        assert_eq!(partial.max_attachment_mb, 500);
        let _ = std::fs::remove_dir_all(dir);
    }

    fn tempfile_dir() -> PathBuf {
        let d = std::env::temp_dir().join(format!("fusen-settings-test-{}", uuid::Uuid::now_v7()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }
}
