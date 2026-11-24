//! OS 依存の処理。OS ごとの実装は `cfg(target_os)` で分ける。
//!
//! - Linux（KDE Plasma 6 / Wayland）: [`linux_kde`]
//! - Windows 11: [`windows`]

use uuid::Uuid;

#[cfg(target_os = "linux")]
pub mod linux_kde;
#[cfg(target_os = "linux")]
pub use linux_kde as imp;
#[cfg(windows)]
pub mod windows;
#[cfg(windows)]
pub use windows as imp;

/// アプリへの命令（コマンドライン引数・2 つ目のプロセス・トレイから）
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Command {
    /// 新しい付箋を作る（`--new`）
    NewNote,
    /// 全付箋を一時的に前面に出す / 元に戻す（`--show-all`）
    ShowAll,
    /// 管理画面を開く（引数なしで 2 回目に起動したとき・トレイ）
    OpenManager,
    /// 終了する
    Quit,
}

impl Command {
    /// コマンドライン引数から命令を作る（`--profile` などの値付きの引数は呼び出し側で除く）
    pub fn from_args(args: &[String]) -> Vec<Command> {
        let mut out = Vec::new();
        for a in args {
            match a.as_str() {
                "--new" => out.push(Command::NewNote),
                "--show-all" => out.push(Command::ShowAll),
                "--manager" => out.push(Command::OpenManager),
                "--quit" => out.push(Command::Quit),
                _ => {}
            }
        }
        out
    }

    pub fn as_arg(self) -> &'static str {
        match self {
            Command::NewNote => "--new",
            Command::ShowAll => "--show-all",
            Command::OpenManager => "--manager",
            Command::Quit => "--quit",
        }
    }
}

/// OS からアプリへの通知
#[derive(Clone, Debug, PartialEq)]
pub enum AppEvent {
    Command(Command),
    /// 付箋ウィンドウの位置・サイズが変わった（論理座標。ウィンドウマネージャーから）
    WindowGeometry {
        note_id: Uuid,
        monitor: Option<String>,
        x: f64,
        y: f64,
        width: f64,
        height: f64,
    },
}

/// 付箋ウィンドウのタイトル（ウィンドウマネージャーで見分けるための印）の接頭辞。
/// プロファイルごとに分けて、同じ PC で動く別のプロファイルの付箋と混ざらないようにする。
pub fn caption_prefix(profile: Option<&str>) -> String {
    format!("fusen-note:{}:", profile.unwrap_or("default"))
}

/// 付箋ウィンドウのタイトル
pub fn note_caption(profile: Option<&str>, note_id: Uuid) -> String {
    format!("{}{note_id}", caption_prefix(profile))
}

/// タイトルから付箋の ID を取り出す
pub fn note_id_from_caption(profile: Option<&str>, caption: &str) -> Option<Uuid> {
    caption
        .strip_prefix(&caption_prefix(profile))
        .and_then(|s| Uuid::parse_str(s).ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_commands() {
        let args: Vec<String> = ["--profile", "x", "--new", "--show-all", "--unknown"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(
            Command::from_args(&args),
            vec![Command::NewNote, Command::ShowAll]
        );
        for c in [
            Command::NewNote,
            Command::ShowAll,
            Command::OpenManager,
            Command::Quit,
        ] {
            assert_eq!(Command::from_args(&[c.as_arg().to_string()]), vec![c]);
        }
    }

    #[test]
    fn caption_roundtrip() {
        let id = Uuid::now_v7();
        let c = note_caption(Some("test"), id);
        assert_eq!(note_id_from_caption(Some("test"), &c), Some(id));
        assert_eq!(
            note_id_from_caption(None, &c),
            None,
            "別のプロファイルは無視"
        );
        assert_eq!(note_id_from_caption(None, "konsole"), None);
    }
}
