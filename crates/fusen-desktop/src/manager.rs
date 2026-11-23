//! 管理画面（`docs/spec.md` 5.3）。通常のウィンドウ 1 つにタブで並べる。
//!
//! - 一覧: 全付箋（本文の先頭・色・更新日時・更新した端末）。検索。クリックでデスクトップに表示して前面に出す
//! - ごみ箱: 削除した付箋と復元
//! - 履歴: 選んだ付箋の版を時系列で表示し、「この版に戻す」で revert 版を作る
//! - 設定: サーバー URL・トークン・端末名（同期は Phase 4 で有効化）、フォント、添付の上限、自動起動

use fusen_core::{NoteSnapshot, Revision, RevisionKind};
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, Sizable as _, WindowExt as _,
    button::{Button, ButtonVariants as _},
    checkbox::Checkbox,
    h_flex,
    input::{Input, InputEvent, InputState},
    notification::Notification,
    tab::{Tab, TabBar},
    text::TextView,
    v_flex,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use uuid::Uuid;

use crate::platform::imp;
use crate::settings::Settings;
use crate::{app, palette};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ManagerTab {
    List,
    Trash,
    History,
    Settings,
}

impl ManagerTab {
    const ALL: [ManagerTab; 4] = [Self::List, Self::Trash, Self::History, Self::Settings];

    fn label(self) -> &'static str {
        match self {
            Self::List => "一覧",
            Self::Trash => "ごみ箱",
            Self::History => "履歴",
            Self::Settings => "設定",
        }
    }
}

/// 設定画面の入力欄
struct SettingsForm {
    server_url: Entity<InputState>,
    token: Entity<InputState>,
    device_name: Entity<InputState>,
    font_family: Entity<InputState>,
    max_attachment_mb: Entity<InputState>,
    autostart: bool,
}

pub struct ManagerWindow {
    tab: ManagerTab,
    search: Entity<InputState>,
    notes: Vec<NoteSnapshot>,
    trash: Vec<NoteSnapshot>,
    history_note: Option<NoteSnapshot>,
    history: Vec<Revision>,
    selected_rev: Option<i64>,
    form: SettingsForm,
    _subscriptions: Vec<Subscription>,
}

impl ManagerWindow {
    pub fn new(
        tab: ManagerTab,
        note_id: Option<Uuid>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let search =
            cx.new(|cx| InputState::new(window, cx).placeholder("本文・添付ファイル名で検索"));
        let s = app::settings(cx).clone();
        let input = |value: String, window: &mut Window, cx: &mut Context<Self>| {
            cx.new(|cx| InputState::new(window, cx).default_value(value))
        };
        let form = SettingsForm {
            server_url: input(s.server_url.clone(), window, cx),
            token: cx.new(|cx| {
                InputState::new(window, cx)
                    .masked(true)
                    .default_value(s.token.clone())
            }),
            device_name: input(s.device_name.clone(), window, cx),
            font_family: input(s.font_family.clone(), window, cx),
            max_attachment_mb: input(s.max_attachment_mb.to_string(), window, cx),
            autostart: s.autostart,
        };

        let subscriptions = vec![
            cx.subscribe(&search, |this, _, ev: &InputEvent, cx| {
                if matches!(ev, InputEvent::Change) {
                    this.refresh(cx);
                }
            }),
            cx.on_release(|_, cx| app::forget_manager(cx)),
        ];

        let mut this = Self {
            tab,
            search,
            notes: Vec::new(),
            trash: Vec::new(),
            history_note: None,
            history: Vec::new(),
            selected_rev: None,
            form,
            _subscriptions: subscriptions,
        };
        this.select(tab, note_id, cx);
        this
    }

    /// タブ（と履歴の付箋）を選ぶ
    pub fn select(&mut self, tab: ManagerTab, note_id: Option<Uuid>, cx: &mut Context<Self>) {
        self.tab = tab;
        if let Some(id) = note_id {
            self.history_note = app::store(cx).note(id).ok();
            self.selected_rev = None;
        }
        self.refresh(cx);
    }

    /// 保存内容を読み直す
    pub fn refresh(&mut self, cx: &mut Context<Self>) {
        let store = app::store(cx);
        let query = self.search.read(cx).value().to_string();
        self.notes = store.search(&query).unwrap_or_else(|e| {
            tracing::error!("一覧を読めません: {e}");
            Vec::new()
        });
        self.trash = store.list_trash().unwrap_or_default();
        if let Some(n) = &self.history_note {
            let id = n.note_id;
            self.history_note = store.note(id).ok();
            self.history = store.history(id).unwrap_or_default();
            // 新しい版を上に
            self.history.reverse();
        } else {
            self.history.clear();
        }
        cx.notify();
    }

    fn device_label(&self, device_id: Uuid, cx: &App) -> String {
        if device_id == app::store(cx).device_id() {
            "この端末".to_string()
        } else {
            // 端末名の同期は Phase 4。それまでは ID の先頭で見分ける
            format!("端末 {}", &device_id.to_string()[..8])
        }
    }

    // ------------------------------------------------------------------
    // 操作
    // ------------------------------------------------------------------

    fn restore(&mut self, id: Uuid, cx: &mut Context<Self>) {
        if let Err(e) = app::store(cx).restore(id) {
            tracing::error!("復元できません: {e}");
        }
        self.refresh(cx);
        cx.defer(move |cx| {
            app::open_note(cx, id, None);
            app::notes_changed(cx, id);
        });
    }

    fn revert(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let (Some(n), Some(rev)) = (&self.history_note, self.selected_rev) else {
            return;
        };
        let id = n.note_id;
        match app::store(cx).revert(id, rev) {
            Ok(_) => window.push_notification(Notification::success("この版に戻しました"), cx),
            Err(e) => window.push_notification(Notification::error(format!("戻せません: {e}")), cx),
        }
        self.selected_rev = None;
        self.refresh(cx);
        app::notes_changed(cx, id);
    }

    fn save_settings(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let f = &self.form;
        let read = |e: &Entity<InputState>, cx: &App| e.read(cx).value().trim().to_string();
        let max_mb = match read(&f.max_attachment_mb, cx).parse::<u64>() {
            Ok(v) if v > 0 => v,
            _ => {
                window.push_notification(
                    Notification::error("添付の上限は 1 以上の整数（MB）で入力してください"),
                    cx,
                );
                return;
            }
        };
        let font = read(&f.font_family, cx);
        let new = Settings {
            server_url: read(&f.server_url, cx),
            token: f.token.read(cx).value().to_string(),
            device_name: read(&f.device_name, cx),
            font_family: if font.is_empty() {
                crate::settings::DEFAULT_FONT.to_string()
            } else {
                font
            },
            max_attachment_mb: max_mb,
            autostart: f.autostart,
        };

        let fusen = cx.global::<app::FusenApp>();
        let path = fusen.paths.settings_file.clone();
        let profile = fusen.paths.profile.clone();
        if let Err(e) = new.save(&path) {
            window.push_notification(
                Notification::error(format!("設定を保存できません: {e:#}")),
                cx,
            );
            return;
        }
        if let Err(e) = imp::set_autostart(profile.as_deref(), new.autostart) {
            window.push_notification(
                Notification::error(format!("自動起動を設定できません: {e:#}")),
                cx,
            );
        }
        cx.global_mut::<app::FusenApp>().settings = new;
        // フォントなどを付箋に反映する
        cx.refresh_windows();
        window.push_notification(Notification::success("設定を保存しました"), cx);
    }

    // ------------------------------------------------------------------
    // 描画
    // ------------------------------------------------------------------

    fn render_note_row(
        &self,
        i: usize,
        n: &NoteSnapshot,
        in_trash: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let id = n.note_id;
        let title = note_title(&n.content);
        let mut meta = vec![
            n.updated_at
                .with_timezone(&chrono::Local)
                .format("%Y/%m/%d %H:%M")
                .to_string(),
            self.device_label(n.edited_by, cx),
        ];
        if !n.attachments.is_empty() {
            meta.push(format!("添付 {}", n.attachments.len()));
        }
        if n.pending {
            meta.push("未送信".into());
        }
        if n.conflict {
            meta.push("競合あり".into());
        }

        h_flex()
            .id(("row", i))
            .gap_3()
            .px_3()
            .py_2()
            .border_b_1()
            .border_color(cx.theme().border)
            .when(!in_trash, |d| {
                d.cursor_pointer()
                    .hover(|s| s.bg(cx.theme().accent))
                    .on_click(move |_, _, cx| cx.defer(move |cx| app::show_note(cx, id)))
            })
            .child(
                div()
                    .size(px(14.))
                    .flex_shrink_0()
                    .rounded_sm()
                    .border_1()
                    .border_color(rgba(0x00000033))
                    .bg(palette::background(&n.color)),
            )
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .child(div().truncate().child(title))
                    .child(
                        div()
                            .text_xs()
                            .text_color(cx.theme().muted_foreground)
                            .child(meta.join(" ・ ")),
                    ),
            )
            .child(
                Button::new(("history", i))
                    .small()
                    .ghost()
                    .label("履歴")
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.select(ManagerTab::History, Some(id), cx)
                    })),
            )
            .when(in_trash, |d| {
                d.child(
                    Button::new(("restore", i))
                        .small()
                        .outline()
                        .label("復元")
                        .on_click(cx.listener(move |this, _, _, cx| this.restore(id, cx))),
                )
            })
            .into_any_element()
    }

    fn render_list(&self, cx: &mut Context<Self>) -> AnyElement {
        let rows: Vec<AnyElement> = self
            .notes
            .iter()
            .enumerate()
            .map(|(i, n)| self.render_note_row(i, n, false, cx))
            .collect();
        v_flex()
            .size_full()
            .child(
                h_flex()
                    .gap_2()
                    .p_2()
                    .child(div().flex_1().child(Input::new(&self.search)))
                    .child(
                        Button::new("new-note")
                            .primary()
                            .label("新しい付箋")
                            .on_click(|_, _, cx| cx.defer(app::new_note)),
                    ),
            )
            .child(
                div()
                    .id("list")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .when(rows.is_empty(), |d| d.child(empty("付箋はありません")))
                    .children(rows),
            )
            .into_any_element()
    }

    fn render_trash(&self, cx: &mut Context<Self>) -> AnyElement {
        let rows: Vec<AnyElement> = self
            .trash
            .iter()
            .enumerate()
            .map(|(i, n)| self.render_note_row(i, n, true, cx))
            .collect();
        div()
            .id("trash")
            .size_full()
            .overflow_y_scroll()
            .when(rows.is_empty(), |d| d.child(empty("ごみ箱は空です")))
            .children(rows)
            .into_any_element()
    }

    fn render_history(&self, cx: &mut Context<Self>) -> AnyElement {
        let Some(note) = &self.history_note else {
            return empty("一覧の「履歴」から付箋を選んでください").into_any_element();
        };
        let rows: Vec<AnyElement> = self
            .history
            .iter()
            .enumerate()
            .map(|(i, r)| {
                let rev = r.rev;
                let selected = self.selected_rev == Some(rev);
                let mut tags = vec![kind_label(r.kind).to_string()];
                if r.server_seq.is_none() {
                    tags.push("未送信".into());
                }
                v_flex()
                    .id(("rev", i))
                    .px_3()
                    .py_2()
                    .border_b_1()
                    .border_color(cx.theme().border)
                    .cursor_pointer()
                    .when(selected, |d| d.bg(cx.theme().accent))
                    .hover(|s| s.bg(cx.theme().accent))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.selected_rev = Some(rev);
                        cx.notify();
                    }))
                    .child(
                        h_flex()
                            .gap_2()
                            .child(
                                r.edited_at
                                    .with_timezone(&chrono::Local)
                                    .format("%Y/%m/%d %H:%M:%S")
                                    .to_string(),
                            )
                            .when(r.conflict, |d| {
                                d.child(
                                    div()
                                        .px_1()
                                        .rounded_sm()
                                        .text_xs()
                                        .bg(cx.theme().danger)
                                        .text_color(cx.theme().danger_foreground)
                                        .child("競合"),
                                )
                            }),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(cx.theme().muted_foreground)
                            .child(format!(
                                "{} ・ {}",
                                self.device_label(r.device_id, cx),
                                tags.join(" ・ ")
                            )),
                    )
                    .into_any_element()
            })
            .collect();

        let selected = self
            .selected_rev
            .and_then(|rev| self.history.iter().find(|r| r.rev == rev));
        let preview: AnyElement = match selected {
            Some(r) => v_flex()
                .size_full()
                .child(
                    h_flex()
                        .p_2()
                        .gap_2()
                        .border_b_1()
                        .border_color(cx.theme().border)
                        .child(
                            div()
                                .flex_1()
                                .text_sm()
                                .child(format!("色: {}", color_label(&r.color))),
                        )
                        .child(
                            Button::new("revert")
                                .primary()
                                .small()
                                .label("この版に戻す")
                                .disabled(Some(r.rev) == Some(note.rev))
                                .on_click(
                                    cx.listener(|this, _, window, cx| this.revert(window, cx)),
                                ),
                        ),
                )
                .child(
                    div()
                        .id("preview")
                        .flex_1()
                        .min_h_0()
                        .overflow_y_scroll()
                        .p_3()
                        .bg(palette::background(&r.color))
                        .text_color(rgb(palette::TEXT))
                        .child(TextView::markdown(
                            ElementId::Name(format!("rev-{}-{}", r.note_id, r.rev).into()),
                            r.content.clone(),
                        )),
                )
                .into_any_element(),
            None => empty("版を選ぶと、その時点の内容を表示します").into_any_element(),
        };

        v_flex()
            .size_full()
            .child(
                div()
                    .p_2()
                    .border_b_1()
                    .border_color(cx.theme().border)
                    .truncate()
                    .child(format!("履歴: {}", note_title(&note.content))),
            )
            .child(
                h_flex()
                    .flex_1()
                    .min_h_0()
                    .child(
                        div()
                            .id("revs")
                            .w(px(300.))
                            .h_full()
                            .flex_shrink_0()
                            .overflow_y_scroll()
                            .border_r_1()
                            .border_color(cx.theme().border)
                            .children(rows),
                    )
                    .child(div().flex_1().h_full().min_w_0().child(preview)),
            )
            .into_any_element()
    }

    fn render_settings(&self, cx: &mut Context<Self>) -> AnyElement {
        let f = &self.form;
        let field = |label: &str, input: &Entity<InputState>, note: Option<&str>| {
            v_flex()
                .gap_1()
                .child(div().text_sm().child(label.to_string()))
                .child(Input::new(input))
                .when_some(note, |d, n| {
                    d.child(
                        div()
                            .text_xs()
                            .text_color(cx.theme().muted_foreground)
                            .child(n.to_string()),
                    )
                })
        };
        div()
            .id("settings")
            .size_full()
            .overflow_y_scroll()
            .child(
                v_flex()
                    .gap_4()
                    .p_4()
                    .max_w(px(560.))
                    .child(
                        div()
                            .font_weight(FontWeight::BOLD)
                            .child("同期（Phase 4 で有効になります）"),
                    )
                    .child(field(
                        "サーバー URL",
                        &f.server_url,
                        Some("例: https://fusen.example.com"),
                    ))
                    .child(field("トークン", &f.token, None))
                    .child(field("端末名", &f.device_name, None))
                    .child(div().font_weight(FontWeight::BOLD).child("表示"))
                    .child(field(
                        "フォント",
                        &f.font_family,
                        Some("既定: Noto Sans CJK JP"),
                    ))
                    .child(div().font_weight(FontWeight::BOLD).child("添付"))
                    .child(field("1 ファイルの上限（MB）", &f.max_attachment_mb, None))
                    .child(div().font_weight(FontWeight::BOLD).child("起動"))
                    .child(
                        Checkbox::new("autostart")
                            .label("ログイン時に自動起動する")
                            .checked(f.autostart)
                            .on_click(cx.listener(|this, checked: &bool, _, cx| {
                                this.form.autostart = *checked;
                                cx.notify();
                            })),
                    )
                    .child(
                        h_flex().child(
                            Button::new("save-settings")
                                .primary()
                                .label("保存")
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.save_settings(window, cx)
                                })),
                        ),
                    ),
            )
            .into_any_element()
    }
}

impl Render for ManagerWindow {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let index = ManagerTab::ALL
            .iter()
            .position(|t| *t == self.tab)
            .unwrap_or(0);
        let body = match self.tab {
            ManagerTab::List => self.render_list(cx),
            ManagerTab::Trash => self.render_trash(cx),
            ManagerTab::History => self.render_history(cx),
            ManagerTab::Settings => self.render_settings(cx),
        };
        v_flex()
            .size_full()
            .bg(cx.theme().background)
            .text_color(cx.theme().foreground)
            .font_family(app::settings(cx).font_family.clone())
            .child(
                TabBar::new("tabs")
                    .w_full()
                    .underline()
                    .selected_index(index)
                    .on_click(cx.listener(|this, ix: &usize, _, cx| {
                        this.tab = ManagerTab::ALL[*ix];
                        this.refresh(cx);
                    }))
                    .children(ManagerTab::ALL.iter().map(|t| Tab::new().label(t.label()))),
            )
            .child(div().flex_1().min_h_0().child(body))
    }
}

fn empty(msg: &str) -> Div {
    div()
        .size_full()
        .flex()
        .items_center()
        .justify_center()
        .p_8()
        .text_color(rgba(0x80808099))
        .child(msg.to_string())
}

/// 一覧に出す題名: 本文の最初の空でない行（Markdown の記号を少し落とす）
pub fn note_title(content: &str) -> String {
    let line = content
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("");
    // 見出し・リスト・引用・チェックボックスの記号を外す（各段階で前の空白も落とす）
    let t = line.trim_start_matches('#').trim_start();
    let t = t.trim_start_matches(['-', '*', '>', '+']).trim_start();
    let t = t
        .strip_prefix("[ ]")
        .or_else(|| t.strip_prefix("[x]"))
        .or_else(|| t.strip_prefix("[X]"))
        .unwrap_or(t)
        .trim();
    if t.is_empty() {
        "（空の付箋）".to_string()
    } else {
        t.chars().take(80).collect()
    }
}

fn kind_label(k: RevisionKind) -> &'static str {
    match k {
        RevisionKind::Edit => "編集",
        RevisionKind::Color => "色の変更",
        RevisionKind::Attach => "添付の追加",
        RevisionKind::Detach => "添付の削除",
        RevisionKind::Delete => "ごみ箱へ移動",
        RevisionKind::Restore => "復元",
        RevisionKind::Revert => "版を戻す",
    }
}

fn color_label(key: &str) -> &'static str {
    palette::COLORS
        .iter()
        .find(|(k, _, _)| *k == key)
        .map(|(_, l, _)| *l)
        .unwrap_or("不明")
}

#[cfg(test)]
mod tests {
    // gpui_kit::* には gpui の test 属性マクロが含まれ、#[test] と衝突するので必要なものだけ使う
    use super::note_title;

    #[test]
    fn titles() {
        assert_eq!(note_title("\n\n# 見出し\n本文"), "見出し");
        assert_eq!(note_title("- [ ] 買い物"), "買い物");
        assert_eq!(note_title("* [x] 済み"), "済み");
        assert_eq!(note_title("> 引用"), "引用");
        assert_eq!(note_title(""), "（空の付箋）");
        assert_eq!(note_title("![x](attachment://a)"), "![x](attachment://a)");
    }
}
