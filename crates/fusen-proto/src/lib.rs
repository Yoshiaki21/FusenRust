//! サーバーとクライアントで共有する API の型。
//!
//! すべて serde で JSON にする。gpui や OS 固有の依存は入れない（将来 Android 版からも使うため）。
//! API の一覧は `docs/spec.md` の 6.3 を参照。

use std::fmt;
use std::str::FromStr;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// API のパスの接頭辞
pub const API_PREFIX: &str = "/api/v1";

/// 版の種類（どの操作で作られたか）
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RevisionKind {
    /// 本文の編集（付箋の作成もこれ）
    Edit,
    /// 色の変更
    Color,
    /// 添付の追加
    Attach,
    /// 添付の削除
    Detach,
    /// ごみ箱へ移動
    Delete,
    /// ごみ箱から復元
    Restore,
    /// 過去の版に戻す
    Revert,
    /// タイトルの変更
    Title,
    /// このアプリの版が知らない種類（新しい版のアプリが作った版）。
    /// 知らない種類を受け取っても同期が止まらないようにするため
    #[serde(other)]
    Other,
}

impl RevisionKind {
    /// DB や表示に使う文字列
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Edit => "edit",
            Self::Color => "color",
            Self::Attach => "attach",
            Self::Detach => "detach",
            Self::Delete => "delete",
            Self::Restore => "restore",
            Self::Revert => "revert",
            Self::Title => "title",
            Self::Other => "other",
        }
    }
}

impl fmt::Display for RevisionKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// 不明な版の種類
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnknownRevisionKind(pub String);

impl fmt::Display for UnknownRevisionKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "不明な版の種類: {}", self.0)
    }
}

impl std::error::Error for UnknownRevisionKind {}

impl FromStr for RevisionKind {
    type Err = UnknownRevisionKind;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(match s {
            "edit" => Self::Edit,
            "color" => Self::Color,
            "attach" => Self::Attach,
            "detach" => Self::Detach,
            "delete" => Self::Delete,
            "restore" => Self::Restore,
            "revert" => Self::Revert,
            "title" => Self::Title,
            "other" => Self::Other,
            other => return Err(UnknownRevisionKind(other.to_string())),
        })
    }
}

/// 付箋のある時点の全状態（版）
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Revision {
    pub note_id: Uuid,
    /// 付箋ごとの連番（サーバーが採番。端末で未確定の間は負の仮番号）
    pub rev: i64,
    /// サーバーの受付番号。未同期の間は `None`
    pub server_seq: Option<i64>,
    /// 版を作った操作の ID（UUIDv7）。同じ版を 2 回送っても重複させないために使う
    pub op_id: Uuid,
    /// 保存した端末
    pub device_id: Uuid,
    /// 端末の時刻（表示用。順序の判定には使わない）
    pub edited_at: DateTime<Utc>,
    pub kind: RevisionKind,
    /// タイトル（空なら付けていない。表示では本文の 1 行目で代用する）
    #[serde(default)]
    pub title: String,
    /// Markdown 本文（その版の全文）
    pub content: String,
    /// 付箋の色（パレットのキー）
    pub color: String,
    /// その版で有効な添付
    pub attachments: Vec<Uuid>,
    /// その版の時点でごみ箱にあるか
    pub deleted: bool,
    /// 編集を始めたときの版（競合の検出用）
    pub base_rev: Option<i64>,
    /// 競合ありの印（サーバーが設定）
    pub conflict: bool,
}

/// `POST /notes/{note_id}/revisions` の本文
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PushRevisionRequest {
    pub op_id: Uuid,
    pub device_id: Uuid,
    pub edited_at: DateTime<Utc>,
    pub kind: RevisionKind,
    #[serde(default)]
    pub title: String,
    pub content: String,
    pub color: String,
    pub attachments: Vec<Uuid>,
    pub deleted: bool,
    pub base_rev: Option<i64>,
}

impl PushRevisionRequest {
    /// 端末の版から送信用の本文を作る
    pub fn from_revision(r: &Revision) -> Self {
        Self {
            op_id: r.op_id,
            device_id: r.device_id,
            edited_at: r.edited_at,
            kind: r.kind,
            title: r.title.clone(),
            content: r.content.clone(),
            color: r.color.clone(),
            attachments: r.attachments.clone(),
            deleted: r.deleted,
            base_rev: r.base_rev,
        }
    }
}

/// `POST /notes/{note_id}/revisions` の応答（確定した番号）
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PushRevisionResponse {
    pub rev: i64,
    pub server_seq: i64,
    pub conflict: bool,
}

/// 添付のメタデータ（不変）
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttachmentMeta {
    pub attachment_id: Uuid,
    /// 最初に添付された付箋
    pub note_id: Uuid,
    /// 実体（blob）の SHA-256（小文字の 16 進）
    pub sha256: String,
    /// 元のファイル名
    pub file_name: String,
    pub mime: String,
    pub size: u64,
    pub created_at: DateTime<Utc>,
}

/// `GET /changes?since=&limit=` の応答
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChangesResponse {
    /// 受付番号の順に並んだ版
    pub revisions: Vec<Revision>,
    /// 上の版が参照する添付のメタデータ
    pub attachments: Vec<AttachmentMeta>,
    /// この応答に含まれる最後の受付番号（次の `since` に使う）
    pub last_server_seq: i64,
    /// まだ続きがあるか
    pub has_more: bool,
}

/// `GET /changes` のクエリ
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChangesQuery {
    pub since: i64,
    pub limit: Option<u32>,
}

/// `GET /notes/{note_id}/revisions` の応答（付箋の全版と、それらが参照する添付のメタデータ）
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct NoteHistoryResponse {
    /// 版番号の順
    pub revisions: Vec<Revision>,
    pub attachments: Vec<AttachmentMeta>,
}

/// 完全削除の票の状態（docs/decisions.md D-014）
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PurgeState {
    /// 票を記録した。まだ削除していない端末がある
    Waiting,
    /// サーバーからも削除した（または、もともとサーバーにない）
    Purged,
    /// サーバーではごみ箱に入っていない（他の端末で復元された）。票は記録しない
    NotDeleted,
}

/// `POST /notes/{note_id}/purge` の応答
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PurgeResponse {
    pub state: PurgeState,
    /// まだ削除していない端末（有効なトークン）の数
    pub remaining: u32,
}

/// WebSocket でサーバーから送るメッセージ
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum WsMessage {
    /// 新しい版を受け付けた
    Changed { server_seq: i64 },
}

/// エラー応答の本文
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ErrorResponse {
    pub error: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ws_message_json_matches_spec() {
        let json = serde_json::to_string(&WsMessage::Changed { server_seq: 42 }).unwrap();
        assert_eq!(json, r#"{"type":"changed","server_seq":42}"#);
        let back: WsMessage = serde_json::from_str(&json).unwrap();
        assert_eq!(back, WsMessage::Changed { server_seq: 42 });
    }

    #[test]
    fn revision_kind_roundtrip() {
        for k in [
            RevisionKind::Edit,
            RevisionKind::Color,
            RevisionKind::Attach,
            RevisionKind::Detach,
            RevisionKind::Delete,
            RevisionKind::Restore,
            RevisionKind::Revert,
            RevisionKind::Title,
            RevisionKind::Other,
        ] {
            assert_eq!(k.as_str().parse::<RevisionKind>().unwrap(), k);
            assert_eq!(
                serde_json::to_string(&k).unwrap(),
                format!("\"{}\"", k.as_str())
            );
        }
        assert!("create".parse::<RevisionKind>().is_err());
        // 知らない種類（新しい版のアプリが作った版）を受け取っても読める
        assert_eq!(
            serde_json::from_str::<RevisionKind>("\"pin\"").unwrap(),
            RevisionKind::Other
        );
    }

    #[test]
    fn revision_json_roundtrip() {
        let r = Revision {
            note_id: Uuid::now_v7(),
            rev: 3,
            server_seq: Some(10),
            op_id: Uuid::now_v7(),
            device_id: Uuid::now_v7(),
            edited_at: Utc::now(),
            kind: RevisionKind::Edit,
            title: "買い物".into(),
            content: "# 見出し\n本文".into(),
            color: "yellow".into(),
            attachments: vec![Uuid::now_v7()],
            deleted: false,
            base_rev: Some(2),
            conflict: false,
        };
        let json = serde_json::to_string(&r).unwrap();
        let back: Revision = serde_json::from_str(&json).unwrap();
        assert_eq!(back, r);

        let req = PushRevisionRequest::from_revision(&r);
        assert_eq!(req.op_id, r.op_id);
        assert_eq!(req.base_rev, Some(2));
        assert_eq!(req.title, "買い物");

        // タイトルのない（古い形式の）JSON は空のタイトルとして読む
        let mut v: serde_json::Value = serde_json::from_str(&json).unwrap();
        v.as_object_mut().unwrap().remove("title");
        let old: Revision = serde_json::from_value(v).unwrap();
        assert_eq!(old.title, "");
    }
}
