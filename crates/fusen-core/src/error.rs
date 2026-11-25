//! fusen-core のエラー型

use uuid::Uuid;

/// fusen-core の結果型
pub type Result<T, E = CoreError> = std::result::Result<T, E>;

/// fusen-core のエラー
#[derive(Debug, thiserror::Error)]
pub enum CoreError {
    #[error("データベースのエラー: {0}")]
    Db(#[from] rusqlite::Error),

    #[error("ファイルの入出力エラー: {0}")]
    Io(#[from] std::io::Error),

    #[error("JSON のエラー: {0}")]
    Json(#[from] serde_json::Error),

    #[error("付箋が見つかりません: {0}")]
    NoteNotFound(Uuid),

    #[error("版が見つかりません: 付箋 {note_id} の版 {rev}")]
    RevisionNotFound { note_id: Uuid, rev: i64 },

    #[error("添付が見つかりません: {0}")]
    AttachmentNotFound(Uuid),

    #[error("添付が大きすぎます: {size} バイト（上限 {max} バイト）")]
    AttachmentTooLarge { size: u64, max: u64 },

    #[error("添付の実体（blob）がありません: {0}")]
    BlobMissing(String),

    #[error("blob のハッシュが一致しません: 期待 {expected}、実際 {actual}")]
    BlobHashMismatch { expected: String, actual: String },

    #[error("DB の値が不正です: {0}")]
    InvalidData(String),

    #[error("通信エラー: {0}")]
    Transport(String),

    #[error("ごみ箱にない付箋は完全に削除できません: {0}")]
    NotInTrash(Uuid),

    #[error(
        "サーバーに送っていない変更があるため、まだ完全に削除できません（同期が終わってから削除してください）: {0}"
    )]
    UnsentRevisions(Uuid),

    /// サーバーで完全に削除された付箋に版を送った（Transport が 410 Gone をこれで返す）
    #[error("サーバーで完全に削除された付箋です: {0}")]
    NoteGone(Uuid),
}
