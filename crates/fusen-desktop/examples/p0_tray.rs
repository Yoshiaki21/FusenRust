//! Phase 0 検証 C: トレイアイコンと gpui のイベントループの共存。
//!
//! KDE では StatusNotifierItem（D-Bus）でトレイを出す。`ksni` は D-Bus だけで動き、
//! GTK のイベントループを必要としないので、gpui のイベントループとぶつからない。
//! （`tray-icon` クレートは Linux では GTK のメインループが必要なため、Linux では使わない）
//!
//! トレイのメニュー操作は ksni のスレッドで呼ばれるので、チャンネルで gpui 側に送る。
//!
//! 使い方:
//!   cargo run -p fusen-desktop --example p0_tray

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
    use gpui_kit::*;
    use ksni::blocking::TrayMethods as _;

    use crate::common;

    /// トレイから gpui 側への命令
    #[derive(Debug, Clone, Copy)]
    enum TrayCommand {
        NewNote,
        Quit,
    }

    struct FusenTray {
        tx: async_channel::Sender<TrayCommand>,
    }

    impl ksni::Tray for FusenTray {
        fn id(&self) -> String {
            "fusen-p0".into()
        }
        fn title(&self) -> String {
            "FusenRust (Phase 0)".into()
        }
        fn icon_name(&self) -> String {
            // 仮のアイコン（テーマのアイコン名）。本番のデザインは未決事項
            "knotes".into()
        }
        fn menu(&self) -> Vec<ksni::MenuItem<Self>> {
            use ksni::menu::*;
            vec![
                StandardItem {
                    label: "新しい付箋".into(),
                    activate: Box::new(|t: &mut Self| {
                        let _ = t.tx.send_blocking(TrayCommand::NewNote);
                    }),
                    ..Default::default()
                }
                .into(),
                MenuItem::Separator,
                StandardItem {
                    label: "終了".into(),
                    activate: Box::new(|t: &mut Self| {
                        let _ = t.tx.send_blocking(TrayCommand::Quit);
                    }),
                    ..Default::default()
                }
                .into(),
            ]
        }
    }

    struct Counter {
        count: usize,
    }

    impl Render for Counter {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div()
                .size_full()
                .flex()
                .items_center()
                .justify_center()
                .bg(rgb(0xfff3a8))
                .child(format!("トレイから作った付箋 #{}", self.count))
        }
    }

    pub fn main() {
        common::init_tracing();
        let (tx, rx) = async_channel::unbounded::<TrayCommand>();

        // ksni は自前のスレッドで D-Bus を処理する
        let _handle = match (FusenTray { tx }).spawn() {
            Ok(h) => Some(h),
            Err(e) => {
                tracing::error!("トレイを作れません: {e}");
                None
            }
        };

        gpui_kit::application()
            .with_assets(gpui_kit::assets::Assets)
            .run(move |cx| {
                gpui_kit::init(cx);
                cx.spawn(async move |cx| {
                    let mut count = 0;
                    while let Ok(cmd) = rx.recv().await {
                        tracing::info!("トレイから: {cmd:?}");
                        match cmd {
                            TrayCommand::NewNote => {
                                count += 1;
                                let n = count;
                                cx.update(|cx| {
                                    let options = WindowOptions {
                                        window_bounds: Some(WindowBounds::Windowed(Bounds {
                                            origin: point(px(100.), px(100.)),
                                            size: size(px(300.), px(160.)),
                                        })),
                                        ..Default::default()
                                    };
                                    let _ = gpui_kit::open_window(options, cx, |_, cx| {
                                        cx.new(|_| Counter { count: n })
                                    });
                                });
                            }
                            TrayCommand::Quit => {
                                cx.update(|cx| cx.quit());
                            }
                        }
                    }
                })
                .detach();
            });
    }
}
