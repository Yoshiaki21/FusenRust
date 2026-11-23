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
}
