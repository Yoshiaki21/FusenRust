//! 同期エンジン（クライアント側）。
//!
//! 通信部分は [`Transport`] トレイトで差し替える（デスクトップは HTTP、テストは仮のサーバー）。
//! 流れ（`docs/spec.md` 6.4）:
//! 1. [`pull`]: `GET /changes?since=<last_server_seq>` で差分を取り込む
//! 2. [`push`]: outbox の版を順に送る。版が参照する添付は、先に blob → メタデータの順で送る
//! 3. 応答で仮番号を確定番号に置き換える
//! 4. [`report_purges`]: この端末で完全に削除した付箋の票を送る（docs/decisions.md D-014）
//!
//! WebSocket の購読や再接続の間隔は呼び出し側（デスクトップアプリ）が担当する。

use std::path::{Path, PathBuf};

use fusen_proto::{
    ChangesResponse, NoteHistoryResponse, PurgeResponse, PurgeState, PushRevisionRequest,
    PushRevisionResponse,
};
use uuid::Uuid;

use crate::{AttachmentMeta, CoreError, RemoteApplyReport, Result, Store};

/// 1 回の `GET /changes` で受け取る最大件数
pub const CHANGES_PAGE_SIZE: u32 = 500;

/// サーバーとの通信。実装は HTTP クライアントなど。
///
/// エラーは [`CoreError::Transport`] で返す（オフラインなど）。
/// 完全に削除された付箋への版の送信（410 Gone）は [`CoreError::NoteGone`] で返す。
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
    /// `GET /notes/{note_id}/revisions`（サーバーにない付箋なら `None`）
    async fn get_note_history(&self, note_id: Uuid) -> Result<Option<NoteHistoryResponse>>;
    /// `POST /notes/{note_id}/purge`（この端末で完全に削除した票）
    async fn purge_note(&self, note_id: Uuid) -> Result<PurgeResponse>;
}

/// 送信の結果
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PushReport {
    /// 送った版の数
    pub pushed: usize,
    /// 競合の印が付いた版 (note_id, 確定した rev)
    pub conflicts: Vec<(Uuid, i64)>,
    /// サーバーで完全に削除済みだったので、この端末からも消した付箋
    pub gone: Vec<Uuid>,
}

/// 1 回の同期の結果
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SyncReport {
    pub pulled: RemoteApplyReport,
    pub pushed: PushReport,
    /// サーバーに送った完全削除の票（付箋, 応答）
    pub purges: Vec<(Uuid, PurgeResponse)>,
}

/// 差分を取り込み、未送信の版と完全削除の票を送る
pub async fn sync_once<T: Transport>(store: &Store, transport: &T) -> Result<SyncReport> {
    let pulled = pull(store, transport).await?;
    let pushed = push(store, transport).await?;
    let purges = report_purges(store, transport).await?;
    Ok(SyncReport {
        pulled,
        pushed,
        purges,
    })
}

/// サーバーの差分をすべて取り込む
pub async fn pull<T: Transport>(store: &Store, transport: &T) -> Result<RemoteApplyReport> {
    let mut total = RemoteApplyReport {
        last_server_seq: store.last_server_seq()?,
        ..Default::default()
    };
    let mut restarted = false;
    loop {
        let since = total.last_server_seq;
        let changes = transport.get_changes(since, CHANGES_PAGE_SIZE).await?;
        // サーバーが配った番号が、取り込み済みの番号より小さい: サーバーが巻き戻っている
        // （バックアップから戻した、など）。取りこぼさないよう最初から取り直す。
        // 既に持っている版は op_id で重複しない（D-015）
        if let Some(max) = changes.max_server_seq
            && max < since
            && !restarted
        {
            tracing::warn!(
                "サーバーの受付番号（{max}）が取り込み済みの番号（{since}）より小さいので、最初から取り直します"
            );
            store.reset_last_server_seq()?;
            total.last_server_seq = 0;
            restarted = true;
            continue;
        }
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

    // 完全に削除した後で他の端末で復元された付箋は、全履歴を取り直す（D-014）
    for note_id in store.revived_notes()? {
        let history = transport.get_note_history(note_id).await?;
        store.import_history(note_id, history.as_ref())?;
        if !total.changed_notes.contains(&note_id) {
            total.changed_notes.push(note_id);
        }
    }
    Ok(total)
}

/// outbox の版を順に送る。途中で通信に失敗したら、残りは outbox に残したままエラーを返す。
pub async fn push<T: Transport>(store: &Store, transport: &T) -> Result<PushReport> {
    let mut report = PushReport::default();
    for pending in store.pending_revisions()? {
        if report.gone.contains(&pending.note_id) {
            continue;
        }
        // 先に送った版の確定で base_rev が書き換わっていることがあるので、読み直す
        let r = store.revision(pending.note_id, pending.rev)?;
        if r.server_seq.is_some() {
            continue;
        }

        for attachment_id in &r.attachments {
            send_attachment(store, transport, *attachment_id).await?;
        }

        let resp = match transport
            .push_revision(r.note_id, &PushRevisionRequest::from_revision(&r))
            .await
        {
            Ok(resp) => resp,
            Err(CoreError::NoteGone(_)) => {
                // 有効な全端末で削除され、サーバーから消えた付箋（同じトークンを 2 台で使っていた場合など）
                store.purge_gone(r.note_id)?;
                report.gone.push(r.note_id);
                continue;
            }
            Err(e) => return Err(e),
        };
        store.confirm_revision(r.note_id, r.rev, resp)?;
        report.pushed += 1;
        if resp.conflict {
            tracing::info!("競合の印が付きました: {} rev {}", r.note_id, resp.rev);
            report.conflicts.push((r.note_id, resp.rev));
        }
    }
    Ok(report)
}

/// この端末で完全に削除した付箋の票を送る
pub async fn report_purges<T: Transport>(
    store: &Store,
    transport: &T,
) -> Result<Vec<(Uuid, PurgeResponse)>> {
    let mut out = Vec::new();
    for note_id in store.unreported_purges()? {
        let resp = transport.purge_note(note_id).await?;
        match resp.state {
            PurgeState::Waiting => tracing::info!(
                "完全削除の票を送りました（まだ削除していない端末: {}）: {note_id}",
                resp.remaining
            ),
            PurgeState::Purged => tracing::info!("サーバーからも完全に削除されました: {note_id}"),
            // 他の端末で復元されていた。次の差分の取り込みで復元の版が届けば取り込み直す
            PurgeState::NotDeleted => {
                tracing::info!("サーバーではごみ箱に入っていません: {note_id}")
            }
        }
        store.mark_purge_reported(note_id)?;
        out.push((note_id, resp));
    }
    Ok(out)
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
    if let Err(e) = transport.register_attachment(&meta).await {
        // 同じ中身の blob が、完全削除でちょうどサーバーから消えた場合に備え、次の同期で送り直す
        if !store.is_blob_pending(&meta.sha256)?
            && matches!(transport.has_blob(&meta.sha256).await, Ok(false))
        {
            store.mark_blob_pending(&meta.sha256)?;
        }
        return Err(e);
    }
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
