//! FusenRust デスクトップアプリ。
//!
//! 使い方:
//!   fusen-desktop               起動（起動済みなら管理画面を開く）
//!   fusen-desktop --new         新しい付箋を作る
//!   fusen-desktop --show-all    全付箋を一時的に前面に出す / 戻す
//!   fusen-desktop --manager     管理画面を開く
//!   fusen-desktop --quit        終了する
//!   fusen-desktop --profile X   別の設定・データで起動する（同じ PC で同期を試すため）
//!
//! 2 つ目のプロセスは 1 つ目に命令を渡して終了する（単一インスタンス）。

// Windows のリリース版はコンソールを出さない（ログはファイルに書く）
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod app;
mod images;
mod manager;
mod note_window;
mod palette;
mod platform;
mod settings;
mod sync;

use std::sync::Arc;

use anyhow::Context as _;
use gpui_kit::component::{Theme, ThemeMode};
use gpui_kit::{App, KeyBinding};

use crate::platform::{AppEvent, Command, imp};
use crate::settings::{AppPaths, Settings};

fn main() {
    init_tracing();
    if let Err(e) = run() {
        tracing::error!("{e:#}");
        eprintln!("FusenRust: {e:#}");
        #[cfg(windows)]
        imp::show_error(&format!("{e:#}"));
        std::process::exit(1);
    }
}

fn init_tracing() {
    let filter = || {
        tracing_subscriber::EnvFilter::try_from_default_env()
            .unwrap_or_else(|_| "info,wgpu_hal=warn,wgpu_core=warn,naga=warn".into())
    };
    // Windows のリリース版はコンソールがないので、%LOCALAPPDATA%\fusen-rust\fusen-desktop.log に追記する。
    // 作り直すと、命令を渡すだけの 2 つ目のプロセスが 1 つ目のログを消してしまうので追記にし、
    // 大きくなったら（5 MB）起動時に .old へ移す
    #[cfg(all(windows, not(debug_assertions)))]
    if let Some(file) = dirs::data_local_dir()
        .map(|d| d.join("fusen-rust"))
        .and_then(|d| std::fs::create_dir_all(&d).ok().map(|_| d))
        .and_then(|d| {
            let path = d.join("fusen-desktop.log");
            if std::fs::metadata(&path).is_ok_and(|m| m.len() > 5 * 1024 * 1024) {
                let _ = std::fs::rename(&path, d.join("fusen-desktop.log.old"));
            }
            std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
                .ok()
        })
    {
        let _ = tracing_subscriber::fmt()
            .with_env_filter(filter())
            .with_ansi(false)
            .with_writer(std::sync::Mutex::new(file))
            .try_init();
        return;
    }
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter())
        .try_init();
}

/// コマンドライン引数から `--profile <名前>` を取り出し、残りを返す
fn parse_profile(args: Vec<String>) -> (Option<String>, Vec<String>) {
    let mut profile = None;
    let mut rest = Vec::new();
    let mut it = args.into_iter();
    while let Some(a) = it.next() {
        if a == "--profile" {
            profile = it.next().filter(|p| !p.is_empty());
        } else if let Some(p) = a.strip_prefix("--profile=") {
            profile = Some(p.to_string()).filter(|p| !p.is_empty());
        } else {
            rest.push(a);
        }
    }
    (profile, rest)
}

fn run() -> anyhow::Result<()> {
    // TLS（HTTPS / WSS）の暗号の実装を決めておく（reqwest と WebSocket で同じものを使う）
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    let (profile, args) = parse_profile(std::env::args().skip(1).collect());
    let commands = Command::from_args(&args);

    let (tx, rx) = async_channel::unbounded::<AppEvent>();
    let Some(handle) = imp::start(profile.as_deref(), tx)? else {
        // 既に動いている: 命令を渡して終了する（命令がなければ管理画面を開く）
        imp::send_to_running(profile.as_deref(), &commands)?;
        return Ok(());
    };

    let paths = AppPaths::new(profile)?;
    let settings = Settings::load(&paths.settings_file);
    let store = Arc::new(
        fusen_core::Store::open(&paths.data_dir)
            .with_context(|| format!("データを開けません: {:?}", paths.data_dir))?,
    );
    tracing::info!(
        "データ: {:?}（端末 ID {}）",
        paths.data_dir,
        store.device_id()
    );

    gpui_kit::application()
        // 付箋や管理画面で Lucide のアイコンを広く使うので、全アイコンを入れる
        .with_assets(gpui_kit::assets::AllAssets)
        .run(move |cx: &mut App| {
            gpui_kit::init(cx);
            // 付箋は淡い色なので、システムが暗いテーマでも明るいテーマで描く
            Theme::change(ThemeMode::Light, None, cx);
            cx.bind_keys([KeyBinding::new(
                "ctrl-v",
                note_window::PasteAttachment,
                Some("FusenNote"),
            )]);
            cx.set_global(app::PlatformGlobal(handle));

            app::init(cx, store, paths, settings);
            for c in commands {
                app::handle_command(cx, c);
            }

            // OS からの通知（命令・ウィンドウの位置）を受け取る
            cx.spawn(async move |cx| {
                while let Ok(ev) = rx.recv().await {
                    cx.update(|cx| app::handle_event(cx, ev));
                }
            })
            .detach();
        });
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| x.to_string()).collect()
    }

    #[test]
    fn profile_args() {
        assert_eq!(
            parse_profile(s(&["--profile", "b", "--new"])),
            (Some("b".into()), s(&["--new"]))
        );
        assert_eq!(
            parse_profile(s(&["--profile=c"])),
            (Some("c".into()), vec![])
        );
        assert_eq!(
            parse_profile(s(&["--show-all"])),
            (None, s(&["--show-all"]))
        );
    }
}
