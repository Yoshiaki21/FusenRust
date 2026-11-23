//! FusenRust のデータモデル・端末側の保存（SQLite）・履歴・添付・同期エンジンの土台。
//!
//! gpui や OS 固有の依存は入れない（将来 Android 版から UniFFI 経由で使うため）。
//!
//! - [`Store`] … 端末側の保存。付箋の作成・版の追加・ごみ箱・復元・過去の版に戻す・下書き・配置
//! - [`blob`] … 添付の実体（SHA-256 で管理し、同じ中身は 1 つだけ持つ）
//! - [`markdown`] … 本文中の `attachment://` 参照の抽出と作成
//! - [`sync`] … 同期エンジン（通信部分は [`sync::Transport`] トレイトで差し替える）

pub mod blob;
pub mod color;
mod db;
mod error;
pub mod markdown;
mod store;
pub mod sync;

pub use error::{CoreError, Result};
pub use fusen_proto::{AttachmentMeta, Revision, RevisionKind};
pub use store::{Draft, NoteSnapshot, Placement, RemoteApplyReport, Store};

/// 1 ファイルの添付の上限サイズの既定値（500 MB）
pub const DEFAULT_MAX_ATTACHMENT_BYTES: u64 = 500 * 1024 * 1024;
