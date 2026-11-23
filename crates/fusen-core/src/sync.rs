//! 同期エンジン（クライアント側）。
//!
//! 通信部分は [`Transport`] トレイトで差し替える（デスクトップは HTTP、テストは仮のサーバー）。
//! 流れ（`docs/spec.md` 6.4）:
//! 1. [`pull`]: `GET /changes?since=<last_server_seq>` で差分を取り込む
//! 2. [`push`]: outbox の版を順に送る。版が参照する添付は、先に blob → メタデータの順で送る
//! 3. 応答で仮番号を確定番号に置き換える
//!
//! WebSocket の購読や再接続の間隔は呼び出し側（デスクトップアプリ）が担当する。

use std::path::{Path, PathBuf};

use fusen_proto::{ChangesResponse, PushRevisionRequest, PushRevisionResponse};
use uuid::Uuid;

use crate::{AttachmentMeta, CoreError, RemoteApplyReport, Result, Store};

/// 1 回の `GET /changes` で受け取る最大件数
pub const CHANGES_PAGE_SIZE: u32 = 500;

/// サーバーとの通信。実装は HTTP クライアントなど。
///
/// エラーは [`CoreError::Transport`] で返す（オフラインなど）。
#[allow(async_fn_in_trait)]
pub trait Transport {
    /// `GET /changes?since=&limit=`
    async fn get_changes(&self, since: i64, limit: u32) -> Result<ChangesResponse>;
    /// `POST /notes/{note_id}/revisions`
    async fn push_revision(
        &self,
        note_id: Uuid,
        req: &PushRevisionRequest,
    ) -> Result<PushRevisionResponse>;
    /// `POST /attachments`（同じものを 2 回送ってもよい）
    async fn register_attachment(&self, meta: &AttachmentMeta) -> Result<()>;
    /// `HEAD /blobs/{sha256}`
    async fn has_blob(&self, sha256: &str) -> Result<bool>;
    /// `PUT /blobs/{sha256}`（`path` のファイルを送る）
    async fn put_blob(&self, sha256: &str, path: &Path) -> Result<()>;
    /// `GET /blobs/{sha256}`（`dest` のファイルに書く）
    async fn download_blob(&self, sha256: &str, dest: &Path) -> Result<()>;
}

/// 送信の結果
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PushReport {
    /// 送った版の数
    pub pushed: usize,
    /// 競合の印が付いた版 (note_id, 確定した rev)
    pub conflicts: Vec<(Uuid, i64)>,
}

/// 1 回の同期の結果
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SyncReport {
    pub pulled: RemoteApplyReport,
    pub pushed: PushReport,
}

/// 差分を取り込み、未送信の版を送る
pub async fn sync_once<T: Transport>(store: &Store, transport: &T) -> Result<SyncReport> {
    let pulled = pull(store, transport).await?;
    let pushed = push(store, transport).await?;
    Ok(SyncReport { pulled, pushed })
}

/// サーバーの差分をすべて取り込む
pub async fn pull<T: Transport>(store: &Store, transport: &T) -> Result<RemoteApplyReport> {
    let mut total = RemoteApplyReport {
        last_server_seq: store.last_server_seq()?,
        ..Default::default()
    };
    loop {
        let since = total.last_server_seq;
        let changes = transport.get_changes(since, CHANGES_PAGE_SIZE).await?;
        let report = store.apply_remote(&changes)?;
        for id in report.changed_notes {
            if !total.changed_notes.contains(&id) {
                total.changed_notes.push(id);
            }
        }
        total.new_notes.extend(report.new_notes);
        total.last_server_seq = report.last_server_seq;
        // 進まない応答が続くと無限に回るので、受付番号が進んだときだけ続ける
        if !changes.has_more || report.last_server_seq <= since {
            break;
        }
    }
    Ok(total)
}

/// outbox の版を順に送る。途中で通信に失敗したら、残りは outbox に残したままエラーを返す。
pub async fn push<T: Transport>(store: &Store, transport: &T) -> Result<PushReport> {
    let mut report = PushReport::default();
    for pending in store.pending_revisions()? {
        // 先に送った版の確定で base_rev が書き換わっていることがあるので、読み直す
        let r = store.revision(pending.note_id, pending.rev)?;
        if r.server_seq.is_some() {
            continue;
        }

        for attachment_id in &r.attachments {
            send_attachment(store, transport, *attachment_id).await?;
        }

        let resp = transport
            .push_revision(r.note_id, &PushRevisionRequest::from_revision(&r))
            .await?;
        store.confirm_revision(r.note_id, r.rev, resp)?;
        report.pushed += 1;
        if resp.conflict {
            tracing::info!("競合の印が付きました: {} rev {}", r.note_id, resp.rev);
            report.conflicts.push((r.note_id, resp.rev));
        }
    }
    Ok(report)
}

/// 未送信の添付なら、blob（サーバーになければ）→ メタデータの順に送る
async fn send_attachment<T: Transport>(
    store: &Store,
    transport: &T,
    attachment_id: Uuid,
) -> Result<()> {
    let Some(meta) = store.pending_attachment(attachment_id)? else {
        return Ok(());
    };
    if store.is_blob_pending(&meta.sha256)? {
        if !transport.has_blob(&meta.sha256).await? {
            let path = store.blobs().path_for(&meta.sha256)?;
            if !path.is_file() {
                return Err(CoreError::BlobMissing(meta.sha256.clone()));
            }
            transport.put_blob(&meta.sha256, &path).await?;
        }
        store.mark_blob_sent(&meta.sha256)?;
    }
    transport.register_attachment(&meta).await?;
    store.mark_attachment_sent(attachment_id)?;
    Ok(())
}

/// 添付の実体をサーバーから取得して保存し、パスを返す（既にあれば取得しない）。
///
/// 受け取った内容のハッシュが合わなければ保存しない。
pub async fn fetch_blob<T: Transport>(
    store: &Store,
    transport: &T,
    meta: &AttachmentMeta,
    max_bytes: u64,
) -> Result<PathBuf> {
    let dest = store.blobs().path_for(&meta.sha256)?;
    if dest.is_file() {
        return Ok(dest);
    }
    let tmp = store
        .blobs()
        .root()
        .join("tmp")
        .join(format!("{}.download", Uuid::now_v7()));
    let result = async {
        transport.download_blob(&meta.sha256, &tmp).await?;
        let file = std::fs::File::open(&tmp)?;
        store
            .blobs()
            .import_verified(file, &meta.sha256, max_bytes)?;
        Ok(())
    }
    .await;
    let _ = std::fs::remove_file(&tmp);
    result.map(|()| dest)
}
