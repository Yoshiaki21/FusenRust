//! 本文中の `attachment://<attachment_id>` 参照の抽出と作成。
//!
//! 参照の形式（`docs/spec.md` 5.2）:
//! - 画像: `![説明](attachment://<attachment_id>)`
//! - その他: `[ファイル名](attachment://<attachment_id>)`

use uuid::Uuid;

use crate::AttachmentMeta;

/// 参照の URL の接頭辞
pub const SCHEME: &str = "attachment://";

/// 本文中に現れる `attachment://<uuid>` を、現れた順に重複なしで返す。
///
/// UUID として読めないものは無視する。
pub fn extract_attachment_refs(content: &str) -> Vec<Uuid> {
    let mut out = Vec::new();
    let mut rest = content;
    while let Some(pos) = rest.find(SCHEME) {
        let after = &rest[pos + SCHEME.len()..];
        // UUID のハイフン付き表記は 36 文字
        let candidate: String = after
            .chars()
            .take_while(|c| c.is_ascii_hexdigit() || *c == '-')
            .collect();
        if let Ok(id) = Uuid::parse_str(&candidate)
            && !out.contains(&id)
        {
            out.push(id);
        }
        rest = &after[candidate.len()..];
    }
    out
}

/// 本文中に画像として表示する形式か（PNG / JPEG / JXL）
pub fn is_inline_image(mime: &str) -> bool {
    matches!(mime, "image/png" | "image/jpeg" | "image/jxl")
}

/// 添付を本文に書くときの参照を作る。画像は `![..](..)`、それ以外は `[..](..)`。
pub fn reference_for(meta: &AttachmentMeta) -> String {
    // ファイル名の中の [ ] は Markdown のリンクの区切りと紛れるのでエスケープする
    let label = meta.file_name.replace('[', "\\[").replace(']', "\\]");
    if is_inline_image(&meta.mime) {
        format!("![{label}]({SCHEME}{})", meta.attachment_id)
    } else {
        format!("[{label}]({SCHEME}{})", meta.attachment_id)
    }
}

/// 本文の末尾に参照を追加した新しい本文を返す（整形表示中に添付したとき＝付箋の一番下に表示）。
pub fn append_reference(content: &str, reference: &str) -> String {
    let mut s = content.trim_end_matches('\n').to_string();
    if !s.is_empty() {
        s.push_str("\n\n");
    }
    s.push_str(reference);
    s.push('\n');
    s
}

/// 本文から指定の添付への参照（`![..](attachment://id)` / `[..](attachment://id)`）を取り除く。
pub fn remove_reference(content: &str, id: Uuid) -> String {
    let target = format!("({SCHEME}{id})");
    let mut out = String::with_capacity(content.len());
    let mut rest = content;
    while let Some(pos) = rest.find(&target) {
        // 直前の `[` または `![` までさかのぼって取り除く
        let before = &rest[..pos];
        let start = before
            .rfind('[')
            .map(|i| {
                if i > 0 && before.as_bytes()[i - 1] == b'!' {
                    i - 1
                } else {
                    i
                }
            })
            .unwrap_or(pos);
        out.push_str(&rest[..start]);
        rest = &rest[pos + target.len()..];
    }
    out.push_str(rest);
    // 参照を消して空になった行の並びを詰める
    while out.contains("\n\n\n") {
        out = out.replace("\n\n\n", "\n\n");
    }
    out
}

/// 一覧などに出す付箋の名前の最大の長さ（文字数）
const DISPLAY_TITLE_CHARS: usize = 80;
/// タイトルも本文もないときの名前
pub const EMPTY_NOTE_TITLE: &str = "（空の付箋）";

/// 一覧などに出す付箋の名前: タイトルがあればタイトル、なければ本文の最初の空でない行
/// （見出し・リスト・引用・チェックボックスの記号と、リンク・画像の書式を落とす）。どちらも空なら「（空の付箋）」
pub fn display_title(title: &str, content: &str) -> String {
    let t = title.trim();
    let t = if t.is_empty() {
        content
            .lines()
            .map(plain_line)
            .find(|l| !l.is_empty())
            .unwrap_or_default()
    } else {
        t.to_string()
    };
    if t.is_empty() {
        EMPTY_NOTE_TITLE.to_string()
    } else {
        t.chars().take(DISPLAY_TITLE_CHARS).collect()
    }
}

/// Markdown の 1 行から、名前として出すための文字だけを取り出す
fn plain_line(line: &str) -> String {
    // 見出し・リスト・引用・チェックボックスの記号を外す（各段階で前の空白も落とす）
    let t = line.trim();
    let t = t.trim_start_matches('#').trim_start();
    let t = t.trim_start_matches(['-', '*', '>', '+']).trim_start();
    let t = t
        .strip_prefix("[ ]")
        .or_else(|| t.strip_prefix("[x]"))
        .or_else(|| t.strip_prefix("[X]"))
        .unwrap_or(t)
        .trim();
    strip_links(t).trim().to_string()
}

/// `![説明](url)` / `[文字](url)` を「説明」「文字」にする
fn strip_links(s: &str) -> String {
    let mut out = String::new();
    let mut rest = s;
    while let Some(open) = rest.find('[') {
        let Some(close_rel) = rest[open..].find("](") else {
            break;
        };
        let close = open + close_rel;
        let Some(end_rel) = rest[close + 2..].find(')') else {
            break;
        };
        let before = &rest[..open];
        out.push_str(before.strip_suffix('!').unwrap_or(before));
        out.push_str(&rest[open + 1..close]);
        rest = &rest[close + 2 + end_rel + 1..];
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;

    fn meta(name: &str, mime: &str) -> AttachmentMeta {
        AttachmentMeta {
            attachment_id: Uuid::now_v7(),
            note_id: Uuid::now_v7(),
            sha256: "00".repeat(32),
            file_name: name.into(),
            mime: mime.into(),
            size: 1,
            created_at: Utc::now(),
        }
    }

    #[test]
    fn extracts_refs_in_order_without_duplicates() {
        let a = Uuid::now_v7();
        let b = Uuid::now_v7();
        let text = format!(
            "前 ![画像](attachment://{a}) 中 [資料.pdf](attachment://{b}) 後 ![再](attachment://{a}) attachment://xyz"
        );
        assert_eq!(extract_attachment_refs(&text), vec![a, b]);
        assert!(extract_attachment_refs("参照なし").is_empty());
    }

    #[test]
    fn builds_image_and_link_references() {
        let img = meta("写真.png", "image/png");
        assert_eq!(
            reference_for(&img),
            format!("![写真.png](attachment://{})", img.attachment_id)
        );
        let jxl = meta("a.jxl", "image/jxl");
        assert!(reference_for(&jxl).starts_with("!["));
        // WebP は画像として表示しない（チップ表示）
        let webp = meta("b.webp", "image/webp");
        assert!(reference_for(&webp).starts_with("[b.webp]"));
        let odd = meta("a[1].txt", "text/plain");
        assert!(reference_for(&odd).starts_with("[a\\[1\\].txt]"));
    }

    #[test]
    fn append_and_remove_reference() {
        let m = meta("x.png", "image/png");
        let r = reference_for(&m);
        let s = append_reference("本文\n", &r);
        assert_eq!(s, format!("本文\n\n{r}\n"));
        assert_eq!(append_reference("", &r), format!("{r}\n"));
        let removed = remove_reference(&s, m.attachment_id);
        assert_eq!(removed.trim_end(), "本文");
        assert!(extract_attachment_refs(&removed).is_empty());
    }

    #[test]
    fn display_title_prefers_title_then_first_line() {
        assert_eq!(display_title(" 買い物 ", "# 見出し"), "買い物");
        assert_eq!(display_title("", "\n\n# 見出し\n本文"), "見出し");
        assert_eq!(display_title("", "- [ ] 牛乳を買う"), "牛乳を買う");
        assert_eq!(display_title("", "> 引用"), "引用");
        assert_eq!(
            display_title("", "![地図](attachment://x) と [資料](https://e.x/a)"),
            "地図 と 資料"
        );
        assert_eq!(display_title("  ", "  \n "), EMPTY_NOTE_TITLE);
        assert_eq!(display_title(&"あ".repeat(100), "").chars().count(), 80);
    }
}
