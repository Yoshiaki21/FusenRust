//! 同期エンジンのテスト。仮のサーバー（メモリ上）を 2 つの端末で共有して確かめる:
//! 同期・オフライン中の編集の後送り・競合の印・仮番号の確定・冪等な再送・添付の送受信・完全削除（D-014）

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::Mutex;

use fusen_core::blob::sha256_hex;
use fusen_core::sync::{self, Transport};
use fusen_core::{AttachmentMeta, CoreError, Result, Revision, Store, color};
use fusen_proto::{
    ChangesResponse, NoteHistoryResponse, PurgeResponse, PurgeState, PushRevisionRequest,
    PushRevisionResponse,
};
use futures_lite::future::block_on;
use uuid::Uuid;

/// 仮のサーバー。後勝ち（受付順）と base_rev による競合の印の判定は仕様 4.3 のとおり。
#[derive(Default)]
struct FakeServer {
    state: Mutex<ServerState>,
}

#[derive(Default)]
struct ServerState {
    revisions: Vec<Revision>,
    seq: i64,
    /// op_id → 応答（冪等にするため）
    ops: HashMap<Uuid, PushRevisionResponse>,
    blobs: HashMap<String, Vec<u8>>,
    attachments: HashMap<Uuid, AttachmentMeta>,
    /// 次の版の受け付け後に応答を失う（送信成功後に端末が落ちた想定）
    lose_next_response: bool,
    /// 完全削除に必要な票の数（有効なトークンの数の代わり）
    voters: usize,
    /// 完全削除の票（付箋 → 端末）
    votes: HashMap<Uuid, HashSet<u32>>,
    /// 完全に削除した付箋
    purged: HashSet<Uuid>,
}

impl FakeServer {
    fn revision_count(&self) -> usize {
        self.state.lock().unwrap().revisions.len()
    }
    fn lose_next_response(&self) {
        self.state.lock().unwrap().lose_next_response = true;
    }
    fn corrupt_blob(&self, sha: &str) {
        self.state
            .lock()
            .unwrap()
            .blobs
            .insert(sha.to_string(), b"corrupted".to_vec());
    }
    fn has_blob_bytes(&self, sha: &str) -> bool {
        self.state.lock().unwrap().blobs.contains_key(sha)
    }
    fn with_voters(voters: usize) -> Self {
        let s = Self::default();
        s.state.lock().unwrap().voters = voters;
        s
    }
    fn note_revision_count(&self, note_id: Uuid) -> usize {
        let st = self.state.lock().unwrap();
        st.revisions.iter().filter(|r| r.note_id == note_id).count()
    }
}

/// 端末からの接続。`offline` ならすべて通信エラーにする。`id` はトークンの代わり（完全削除の票）
struct Client<'a> {
    server: &'a FakeServer,
    offline: bool,
    id: u32,
}

impl Client<'_> {
    fn check(&self) -> Result<()> {
        if self.offline {
            Err(CoreError::Transport("オフライン".into()))
        } else {
            Ok(())
        }
    }
}

impl Transport for Client<'_> {
    async fn get_changes(&self, since: i64, limit: u32) -> Result<ChangesResponse> {
        self.check()?;
        let st = self.server.state.lock().unwrap();
        let mut revs: Vec<Revision> = st
            .revisions
            .iter()
            .filter(|r| r.server_seq.unwrap_or(0) > since)
            .cloned()
            .collect();
        revs.sort_by_key(|r| r.server_seq);
        let has_more = revs.len() > limit as usize;
        revs.truncate(limit as usize);
        let attachments = revs
            .iter()
            .flat_map(|r| r.attachments.iter())
            .filter_map(|id| st.attachments.get(id).cloned())
            .collect();
        let last = revs.last().and_then(|r| r.server_seq).unwrap_or(since);
        Ok(ChangesResponse {
            revisions: revs,
            attachments,
            last_server_seq: last,
            has_more,
        })
    }

    async fn push_revision(
        &self,
        note_id: Uuid,
        req: &PushRevisionRequest,
    ) -> Result<PushRevisionResponse> {
        self.check()?;
        let mut st = self.server.state.lock().unwrap();
        if let Some(resp) = st.ops.get(&req.op_id) {
            return Ok(*resp);
        }
        if st.purged.contains(&note_id) {
            return Err(CoreError::NoteGone(note_id));
        }
        st.votes.remove(&note_id);
        // blob・メタデータがない版は受け付けない
        for id in &req.attachments {
            let Some(meta) = st.attachments.get(id) else {
                return Err(CoreError::Transport(format!("未登録の添付 {id}")));
            };
            if !st.blobs.contains_key(&meta.sha256) {
                return Err(CoreError::Transport(format!("blob がない {}", meta.sha256)));
            }
        }
        let latest = st
            .revisions
            .iter()
            .filter(|r| r.note_id == note_id)
            .map(|r| r.rev)
            .max();
        let rev = latest.unwrap_or(0) + 1;
        let conflict = latest.is_some() && req.base_rev != latest;
        st.seq += 1;
        let seq = st.seq;
        let resp = PushRevisionResponse {
            rev,
            server_seq: seq,
            conflict,
        };
        st.revisions.push(Revision {
            note_id,
            rev,
            server_seq: Some(seq),
            op_id: req.op_id,
            device_id: req.device_id,
            edited_at: req.edited_at,
            kind: req.kind,
            title: req.title.clone(),
            content: req.content.clone(),
            color: req.color.clone(),
            attachments: req.attachments.clone(),
            deleted: req.deleted,
            base_rev: req.base_rev,
            conflict,
        });
        st.ops.insert(req.op_id, resp);
        if std::mem::take(&mut st.lose_next_response) {
            return Err(CoreError::Transport("応答が失われた".into()));
        }
        Ok(resp)
    }

    async fn register_attachment(&self, meta: &AttachmentMeta) -> Result<()> {
        self.check()?;
        let mut st = self.server.state.lock().unwrap();
        if !st.blobs.contains_key(&meta.sha256) {
            return Err(CoreError::Transport("blob が先".into()));
        }
        st.attachments.insert(meta.attachment_id, meta.clone());
        Ok(())
    }

    async fn has_blob(&self, sha256: &str) -> Result<bool> {
        self.check()?;
        Ok(self.server.has_blob_bytes(sha256))
    }

    async fn put_blob(&self, sha256: &str, path: &Path) -> Result<()> {
        self.check()?;
        let bytes = std::fs::read(path)?;
        // サーバーはハッシュを検証してから保存する
        if sha256_hex(&bytes) != sha256 {
            return Err(CoreError::Transport("ハッシュ不一致".into()));
        }
        self.server
            .state
            .lock()
            .unwrap()
            .blobs
            .insert(sha256.to_string(), bytes);
        Ok(())
    }

    async fn download_blob(&self, sha256: &str, dest: &Path) -> Result<()> {
        self.check()?;
        let bytes = self
            .server
            .state
            .lock()
            .unwrap()
            .blobs
            .get(sha256)
            .cloned()
            .ok_or_else(|| CoreError::Transport("blob がない".into()))?;
        std::fs::write(dest, bytes)?;
        Ok(())
    }

    async fn get_note_history(&self, note_id: Uuid) -> Result<Option<NoteHistoryResponse>> {
        self.check()?;
        let st = self.server.state.lock().unwrap();
        let mut revisions: Vec<Revision> = st
            .revisions
            .iter()
            .filter(|r| r.note_id == note_id)
            .cloned()
            .collect();
        if revisions.is_empty() {
            return Ok(None);
        }
        revisions.sort_by_key(|r| r.rev);
        let attachments = revisions
            .iter()
            .flat_map(|r| r.attachments.iter())
            .filter_map(|id| st.attachments.get(id).cloned())
            .collect();
        Ok(Some(NoteHistoryResponse {
            revisions,
            attachments,
        }))
    }

    async fn purge_note(&self, note_id: Uuid) -> Result<PurgeResponse> {
        self.check()?;
        let mut st = self.server.state.lock().unwrap();
        let done = |state| PurgeResponse {
            state,
            remaining: 0,
        };
        let head = st
            .revisions
            .iter()
            .filter(|r| r.note_id == note_id)
            .max_by_key(|r| r.server_seq)
            .map(|r| r.deleted);
        match head {
            None => Ok(done(PurgeState::Purged)),
            Some(false) => Ok(done(PurgeState::NotDeleted)),
            Some(true) => {
                let voters = st.voters;
                let votes = st.votes.entry(note_id).or_default();
                votes.insert(self.id);
                let remaining = voters.saturating_sub(votes.len()) as u32;
                if remaining > 0 {
                    return Ok(PurgeResponse {
                        state: PurgeState::Waiting,
                        remaining,
                    });
                }
                st.revisions.retain(|r| r.note_id != note_id);
                st.votes.remove(&note_id);
                st.purged.insert(note_id);
                Ok(done(PurgeState::Purged))
            }
        }
    }
}

fn device() -> (tempfile::TempDir, Store) {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    (dir, store)
}

fn online(server: &FakeServer) -> Client<'_> {
    online_as(server, 0)
}

fn online_as(server: &FakeServer, id: u32) -> Client<'_> {
    Client {
        server,
        offline: false,
        id,
    }
}

fn offline(server: &FakeServer) -> Client<'_> {
    Client {
        server,
        offline: true,
        id: 0,
    }
}

#[test]
fn two_devices_sync() {
    let server = FakeServer::default();
    let (_da, a) = device();
    let (_db, b) = device();

    let r = a.create_note("# A で作成", color::PINK).unwrap();
    let rep = block_on(sync::sync_once(&a, &online(&server))).unwrap();
    assert_eq!(rep.pushed.pushed, 1);
    assert_eq!(a.outbox_len().unwrap(), 0);

    // 仮番号が確定番号に置き換わる
    let na = a.note(r.note_id).unwrap();
    assert_eq!(na.rev, 1);
    assert!(!na.pending);
    assert_eq!(a.history(r.note_id).unwrap()[0].server_seq, Some(1));

    let rep = block_on(sync::sync_once(&b, &online(&server))).unwrap();
    assert_eq!(
        rep.pulled.new_notes,
        vec![r.note_id],
        "配置を作るべき新しい付箋"
    );
    let nb = b.note(r.note_id).unwrap();
    assert_eq!(nb.content, "# A で作成");
    assert_eq!(nb.color, color::PINK);
    assert_eq!(nb.rev, 1);
    assert_eq!(nb.edited_by, a.device_id());
    assert_eq!(b.last_server_seq().unwrap(), 1);

    // 自分の版が差分で戻ってきても重複しない
    block_on(sync::sync_once(&a, &online(&server))).unwrap();
    assert_eq!(a.history(r.note_id).unwrap().len(), 1);
    assert_eq!(a.last_server_seq().unwrap(), 1);
}

#[test]
fn offline_edits_are_sent_later() {
    let server = FakeServer::default();
    let (_da, a) = device();
    let (_db, b) = device();

    // オフライン中に作成と 2 回の編集（仮番号 -1, -2, -3。base_rev も仮番号）
    let r = a.create_note("一", color::YELLOW).unwrap();
    a.save_edit(r.note_id, "二", None, None).unwrap();
    a.save_edit(r.note_id, "三", None, None).unwrap();
    assert!(block_on(sync::sync_once(&a, &offline(&server))).is_err());
    assert_eq!(a.outbox_len().unwrap(), 3, "送れなかった版は outbox に残る");
    assert_eq!(server.revision_count(), 0);

    // 再接続したら順に送る。base_rev は確定番号に直して送るので競合にならない
    let rep = block_on(sync::sync_once(&a, &online(&server))).unwrap();
    assert_eq!(rep.pushed.pushed, 3);
    assert!(rep.pushed.conflicts.is_empty());
    assert_eq!(a.outbox_len().unwrap(), 0);
    let revs: Vec<_> = a
        .history(r.note_id)
        .unwrap()
        .iter()
        .map(|r| r.rev)
        .collect();
    assert_eq!(revs, vec![1, 2, 3]);
    let bases: Vec<_> = a
        .history(r.note_id)
        .unwrap()
        .iter()
        .map(|r| r.base_rev)
        .collect();
    assert_eq!(bases, vec![None, Some(1), Some(2)]);

    block_on(sync::sync_once(&b, &online(&server))).unwrap();
    assert_eq!(b.note(r.note_id).unwrap().content, "三");
    assert_eq!(b.history(r.note_id).unwrap().len(), 3);
}

#[test]
fn concurrent_edits_are_marked_as_conflict() {
    let server = FakeServer::default();
    let (_da, a) = device();
    let (_db, b) = device();

    let r = a.create_note("元", color::YELLOW).unwrap();
    block_on(sync::sync_once(&a, &online(&server))).unwrap();
    block_on(sync::sync_once(&b, &online(&server))).unwrap();

    // 両方の端末でオフライン中に編集
    a.save_edit(r.note_id, "A の編集", None, Some(1)).unwrap();
    b.save_edit(r.note_id, "B の編集", None, Some(1)).unwrap();

    // A が先に送る → 競合なし
    let ra = block_on(sync::sync_once(&a, &online(&server))).unwrap();
    assert!(ra.pushed.conflicts.is_empty());

    // B は A の版を取り込んだ後に自分の版を送る → base_rev(1) が最新(2)と違うので競合の印
    let rb = block_on(sync::sync_once(&b, &online(&server))).unwrap();
    assert_eq!(rb.pushed.conflicts, vec![(r.note_id, 3)]);
    // 後勝ち: B の版が最新
    let nb = b.note(r.note_id).unwrap();
    assert_eq!(nb.content, "B の編集");
    assert!(nb.conflict);

    // A も取り込むと B の版が最新になり、A の版は履歴に残る
    block_on(sync::sync_once(&a, &online(&server))).unwrap();
    let na = a.note(r.note_id).unwrap();
    assert_eq!(na.content, "B の編集");
    assert!(na.conflict);
    let ha: Vec<_> = a
        .history(r.note_id)
        .unwrap()
        .iter()
        .map(|r| (r.rev, r.content.clone(), r.conflict))
        .collect();
    assert_eq!(
        ha,
        vec![
            (1, "元".to_string(), false),
            (2, "A の編集".to_string(), false),
            (3, "B の編集".to_string(), true),
        ]
    );
    // 両端末の履歴が一致する
    assert_eq!(a.history(r.note_id).unwrap(), b.history(r.note_id).unwrap());
}

#[test]
fn own_title_change_during_edit_is_not_a_conflict() {
    let server = FakeServer::default();
    let (_da, a) = device();
    let (_db, b) = device();
    let r = a.create_note("元", color::YELLOW).unwrap();
    block_on(sync::sync_once(&a, &online(&server))).unwrap();
    block_on(sync::sync_once(&b, &online(&server))).unwrap();

    // A: 本文の編集を始めた（土台は 1）→ 編集中にタイトルを変えて送る → 本文を保存して送る
    a.set_title(r.note_id, "題").unwrap();
    block_on(sync::sync_once(&a, &online(&server))).unwrap();
    a.save_edit(r.note_id, "A の編集", None, Some(1)).unwrap();
    let ra = block_on(sync::sync_once(&a, &online(&server))).unwrap();
    assert!(
        ra.pushed.conflicts.is_empty(),
        "自分の変更だけなら競合ではない"
    );
    let na = a.note(r.note_id).unwrap();
    assert_eq!((na.title.as_str(), na.content.as_str()), ("題", "A の編集"));

    // B: 編集を始めた（土台は 3）後に A の変更を取り込んでから保存 → 競合の印が付く
    a.set_title(r.note_id, "A の題").unwrap();
    block_on(sync::sync_once(&a, &online(&server))).unwrap();
    block_on(sync::sync_once(&b, &online(&server))).unwrap();
    let base = 3;
    b.save_edit(r.note_id, "B の編集", None, Some(base))
        .unwrap();
    let rb = block_on(sync::sync_once(&b, &online(&server))).unwrap();
    assert_eq!(rb.pushed.conflicts.len(), 1, "他の端末の変更があれば競合");
}

#[test]
fn local_pending_edit_stays_head_while_remote_arrives() {
    let server = FakeServer::default();
    let (_da, a) = device();
    let (_db, b) = device();
    let r = a.create_note("元", color::YELLOW).unwrap();
    block_on(sync::sync_once(&a, &online(&server))).unwrap();
    block_on(sync::sync_once(&b, &online(&server))).unwrap();

    a.save_edit(r.note_id, "A の新しい編集", None, None)
        .unwrap();
    block_on(sync::sync_once(&a, &online(&server))).unwrap();

    // B は未送信の編集を持ったまま差分だけ取り込む
    b.save_edit(r.note_id, "B の未送信", None, None).unwrap();
    block_on(sync::pull(&b, &online(&server))).unwrap();
    let nb = b.note(r.note_id).unwrap();
    assert_eq!(nb.content, "B の未送信", "未送信の自分の版が最新のまま");
    assert!(nb.pending);
    assert_eq!(b.history(r.note_id).unwrap().len(), 3);
}

#[test]
fn resend_after_lost_response_is_idempotent() {
    let server = FakeServer::default();
    let (_da, a) = device();
    let r = a.create_note("一度だけ", color::YELLOW).unwrap();

    // サーバーは受け付けたが応答が失われた
    server.lose_next_response();
    assert!(block_on(sync::push(&a, &online(&server))).is_err());
    assert_eq!(server.revision_count(), 1);
    assert_eq!(a.outbox_len().unwrap(), 1);

    // もう一度送っても重複しない（op_id で冪等）
    let rep = block_on(sync::push(&a, &online(&server))).unwrap();
    assert_eq!(rep.pushed, 1);
    assert_eq!(server.revision_count(), 1);
    assert_eq!(a.outbox_len().unwrap(), 0);
    assert_eq!(a.note(r.note_id).unwrap().rev, 1);
}

#[test]
fn lost_response_is_recovered_by_pull() {
    let server = FakeServer::default();
    let (_da, a) = device();
    let r = a.create_note("取り込みで確定", color::YELLOW).unwrap();
    server.lose_next_response();
    assert!(block_on(sync::push(&a, &online(&server))).is_err());

    // 次の同期では先に差分を取り込み、同じ op_id の版で仮番号を確定させる
    let rep = block_on(sync::sync_once(&a, &online(&server))).unwrap();
    assert_eq!(rep.pushed.pushed, 0);
    assert_eq!(server.revision_count(), 1);
    assert_eq!(a.outbox_len().unwrap(), 0);
    let h = a.history(r.note_id).unwrap();
    assert_eq!(h.len(), 1);
    assert_eq!((h[0].rev, h[0].server_seq), (1, Some(1)));
}

#[test]
fn attachments_are_sent_before_revision_and_fetched() {
    let server = FakeServer::default();
    let (_da, a) = device();
    let (_db, b) = device();

    let id = a.create_note("画像つき", color::YELLOW).unwrap().note_id;
    let m1 = a
        .import_attachment_bytes(id, "p.png", b"PNGDATA", 1024)
        .unwrap();
    // 同じ中身の別の添付（blob は 1 つ、メタデータは 2 つ）
    let m2 = a
        .import_attachment_bytes(id, "copy.png", b"PNGDATA", 1024)
        .unwrap();
    a.add_attachment(id, m1.attachment_id, None).unwrap();
    a.add_attachment(id, m2.attachment_id, None).unwrap();

    block_on(sync::sync_once(&a, &online(&server))).unwrap();
    assert!(server.has_blob_bytes(&m1.sha256));
    assert_eq!(server.state.lock().unwrap().attachments.len(), 2);
    assert!(!a.is_blob_pending(&m1.sha256).unwrap());

    block_on(sync::sync_once(&b, &online(&server))).unwrap();
    let nb = b.note(id).unwrap();
    assert_eq!(nb.attachments, vec![m1.attachment_id, m2.attachment_id]);
    assert_eq!(b.attachment(m1.attachment_id).unwrap(), m1);
    // 実体はまだない → 取得する
    let missing = b.missing_blobs().unwrap();
    assert_eq!(missing.len(), 1, "同じ中身は 1 回だけ取得");
    assert!(b.blob_path(m1.attachment_id).unwrap().is_none());
    let path = block_on(sync::fetch_blob(&b, &online(&server), &missing[0], 1024)).unwrap();
    assert_eq!(std::fs::read(path).unwrap(), b"PNGDATA");
    assert!(b.blob_path(m2.attachment_id).unwrap().is_some());
    assert!(b.missing_blobs().unwrap().is_empty());
}

#[test]
fn only_pasted_notes_need_blobs_first() {
    let server = FakeServer::default();
    let (_da, a) = device();
    let (_db, b) = device();

    let id = a.create_note("画像つき", color::YELLOW).unwrap().note_id;
    let m = a
        .import_attachment_bytes(id, "p.png", b"PNGDATA", 1024)
        .unwrap();
    a.add_attachment(id, m.attachment_id, None).unwrap();
    block_on(sync::sync_once(&a, &online(&server))).unwrap();
    block_on(sync::sync_once(&b, &online(&server))).unwrap();

    // 他の端末で作られた付箋は、この端末には貼られていない（配置がない）
    assert!(b.placement(id).unwrap().is_none());
    assert_eq!(b.missing_blobs().unwrap().len(), 1);
    assert!(b.missing_blobs_of_pasted().unwrap().is_empty());

    // 貼ると対象になる。デスクトップから外すと対象から外れる
    let p = b.ensure_placement(id, 280.0, 320.0).unwrap();
    assert_eq!(b.missing_blobs_of_pasted().unwrap().len(), 1);
    b.set_placement(&fusen_core::Placement {
        visible: false,
        ..p
    })
    .unwrap();
    assert!(b.missing_blobs_of_pasted().unwrap().is_empty());
}

#[test]
fn fetched_blob_with_wrong_hash_is_rejected() {
    let server = FakeServer::default();
    let (_da, a) = device();
    let (_db, b) = device();
    let id = a.create_note("x", color::YELLOW).unwrap().note_id;
    let m = a
        .import_attachment_bytes(id, "f.bin", b"real", 1024)
        .unwrap();
    a.add_attachment(id, m.attachment_id, None).unwrap();
    block_on(sync::sync_once(&a, &online(&server))).unwrap();
    block_on(sync::sync_once(&b, &online(&server))).unwrap();

    server.corrupt_blob(&m.sha256);
    let err = block_on(sync::fetch_blob(&b, &online(&server), &m, 1024)).unwrap_err();
    assert!(matches!(err, CoreError::BlobHashMismatch { .. }));
    assert!(b.blob_path(m.attachment_id).unwrap().is_none());
}

#[test]
fn trash_and_revert_sync() {
    let server = FakeServer::default();
    let (_da, a) = device();
    let (_db, b) = device();
    let r = a.create_note("v1", color::YELLOW).unwrap();
    block_on(sync::sync_once(&a, &online(&server))).unwrap();
    a.save_edit(r.note_id, "v2", None, None).unwrap();
    a.trash(r.note_id).unwrap();
    block_on(sync::sync_once(&a, &online(&server))).unwrap();

    block_on(sync::sync_once(&b, &online(&server))).unwrap();
    assert!(b.list_notes().unwrap().is_empty());
    assert_eq!(b.list_trash().unwrap().len(), 1);

    b.restore(r.note_id).unwrap();
    b.revert(r.note_id, 1).unwrap();
    block_on(sync::sync_once(&b, &online(&server))).unwrap();
    block_on(sync::sync_once(&a, &online(&server))).unwrap();
    let na = a.note(r.note_id).unwrap();
    assert_eq!(na.content, "v1");
    assert!(!na.deleted);
    assert!(!na.conflict);
}

#[test]
fn pull_pages_through_changes() {
    let server = FakeServer::default();
    let (_da, a) = device();
    let (_db, b) = device();
    // 1 ページ（500 件）を超える版
    let r = a.create_note("0", color::YELLOW).unwrap();
    for i in 1..=fusen_core::sync::CHANGES_PAGE_SIZE {
        a.save_edit(r.note_id, &i.to_string(), None, None).unwrap();
    }
    block_on(sync::sync_once(&a, &online(&server))).unwrap();
    block_on(sync::pull(&b, &online(&server))).unwrap();
    assert_eq!(
        b.history(r.note_id).unwrap().len(),
        fusen_core::sync::CHANGES_PAGE_SIZE as usize + 1
    );
    assert_eq!(
        b.note(r.note_id).unwrap().content,
        fusen_core::sync::CHANGES_PAGE_SIZE.to_string()
    );
}

#[test]
fn rev_of_op_follows_renumbering() {
    let server = FakeServer::default();
    let (_da, a) = device();
    let r = a.create_note("元", color::YELLOW).unwrap();
    let head = a.note(r.note_id).unwrap();
    assert_eq!((head.rev, head.op_id), (-1, r.op_id));
    // 編集を始めた後に同期で仮番号が確定番号に変わっても、操作 ID で今の番号が分かる
    block_on(sync::sync_once(&a, &online(&server))).unwrap();
    assert_eq!(a.rev_of_op(r.note_id, r.op_id).unwrap(), Some(1));
    let saved = a
        .save_edit(
            r.note_id,
            "編集",
            None,
            a.rev_of_op(r.note_id, r.op_id).unwrap(),
        )
        .unwrap()
        .unwrap();
    assert_eq!(saved.base_rev, Some(1));
    let rep = block_on(sync::sync_once(&a, &online(&server))).unwrap();
    assert!(
        rep.pushed.conflicts.is_empty(),
        "古い仮番号を送らないので競合にならない"
    );
    assert_eq!(a.rev_of_op(r.note_id, uuid::Uuid::now_v7()).unwrap(), None);
}

// ----------------------------------------------------------------------
// 完全削除（D-014）
// ----------------------------------------------------------------------

/// A・B の 2 台で付箋を作ってごみ箱に入れ、両方が取り込んだ状態にする
fn trashed_on_both(server: &FakeServer, a: &Store, b: &Store) -> Uuid {
    let id = a.create_note("一", color::YELLOW).unwrap().note_id;
    a.save_edit(id, "二", None, None).unwrap();
    block_on(sync::sync_once(a, &online_as(server, 1))).unwrap();
    block_on(sync::sync_once(b, &online_as(server, 2))).unwrap();
    a.trash(id).unwrap();
    block_on(sync::sync_once(a, &online_as(server, 1))).unwrap();
    block_on(sync::sync_once(b, &online_as(server, 2))).unwrap();
    assert!(b.note(id).unwrap().deleted);
    id
}

#[test]
fn purge_on_all_devices_removes_from_server() {
    let server = FakeServer::with_voters(2);
    let (_da, a) = device();
    let (_db, b) = device();
    let id = trashed_on_both(&server, &a, &b);

    a.purge(id, false).unwrap();
    let rep = block_on(sync::sync_once(&a, &online_as(&server, 1))).unwrap();
    assert_eq!(rep.purges.len(), 1);
    assert_eq!(rep.purges[0].1.state, PurgeState::Waiting);
    assert_eq!(rep.purges[0].1.remaining, 1);
    assert!(a.unreported_purges().unwrap().is_empty());
    assert!(
        matches!(a.note(id), Err(CoreError::NoteNotFound(_))),
        "自分が送った版（ごみ箱へ移した版）が差分で戻ってきても取り込み直さない"
    );
    assert_eq!(
        server.note_revision_count(id),
        3,
        "B がまだ持っているので残る"
    );

    // 票は 1 度だけ送る
    let rep = block_on(sync::sync_once(&a, &online_as(&server, 1))).unwrap();
    assert!(rep.purges.is_empty());

    b.purge(id, false).unwrap();
    let rep = block_on(sync::sync_once(&b, &online_as(&server, 2))).unwrap();
    assert_eq!(rep.purges[0].1.state, PurgeState::Purged);
    assert_eq!(server.note_revision_count(id), 0);
}

#[test]
fn restored_on_other_device_after_purge_is_revived_with_full_history() {
    let server = FakeServer::with_voters(2);
    let (_da, a) = device();
    let (_db, b) = device();
    let id = trashed_on_both(&server, &a, &b);

    a.purge(id, false).unwrap();
    block_on(sync::sync_once(&a, &online_as(&server, 1))).unwrap();

    // B はごみ箱から戻した
    b.restore(id).unwrap();
    block_on(sync::sync_once(&b, &online_as(&server, 2))).unwrap();

    let rep = block_on(sync::sync_once(&a, &online_as(&server, 1))).unwrap();
    assert!(rep.pulled.changed_notes.contains(&id));
    let n = a.note(id).unwrap();
    assert!(!n.deleted);
    assert_eq!(n.content, "二");
    let revs: Vec<_> = a.history(id).unwrap().iter().map(|r| r.rev).collect();
    assert_eq!(revs, vec![1, 2, 3, 4], "全履歴を取り直す");
    assert!(!a.is_purged(id).unwrap());
    assert!(a.revived_notes().unwrap().is_empty());
    assert!(a.placement(id).unwrap().is_none(), "この端末には貼らない");

    // 戻された後は、A の票は数えない（B が消しても消えない）
    b.trash(id).unwrap();
    block_on(sync::sync_once(&b, &online_as(&server, 2))).unwrap();
    b.purge(id, false).unwrap();
    let rep = block_on(sync::sync_once(&b, &online_as(&server, 2))).unwrap();
    assert_eq!(rep.purges[0].1.state, PurgeState::Waiting);
}

#[test]
fn change_in_trash_after_purge_is_not_imported_but_voted_again() {
    let server = FakeServer::with_voters(2);
    let (_da, a) = device();
    let (_db, b) = device();
    let id = trashed_on_both(&server, &a, &b);

    a.purge(id, false).unwrap();
    block_on(sync::sync_once(&a, &online_as(&server, 1))).unwrap();

    // B がごみ箱の中の付箋の色を変えた（ごみ箱にあるままの版）。サーバーは A の票を消す
    b.set_color(id, color::PINK).unwrap();
    block_on(sync::sync_once(&b, &online_as(&server, 2))).unwrap();

    // A は取り込まず、票を送り直す
    let rep = block_on(sync::sync_once(&a, &online_as(&server, 1))).unwrap();
    assert!(matches!(a.note(id), Err(CoreError::NoteNotFound(_))));
    assert_eq!(rep.purges.len(), 1);
    assert_eq!(rep.purges[0].1.state, PurgeState::Waiting);

    b.purge(id, false).unwrap();
    let rep = block_on(sync::sync_once(&b, &online_as(&server, 2))).unwrap();
    assert_eq!(rep.purges[0].1.state, PurgeState::Purged);
}

#[test]
fn push_to_note_purged_on_server_removes_it_locally() {
    let server = FakeServer::with_voters(2);
    let (_da, a) = device();
    let (_db, b) = device();
    let (_dc, c) = device();
    let id = trashed_on_both(&server, &a, &b);
    block_on(sync::sync_once(&c, &online_as(&server, 1))).unwrap();

    // C はオフライン中にごみ箱から戻した（同じトークンを 2 台で使っていた想定）
    c.restore(id).unwrap();

    a.purge(id, false).unwrap();
    block_on(sync::sync_once(&a, &online_as(&server, 1))).unwrap();
    b.purge(id, false).unwrap();
    block_on(sync::sync_once(&b, &online_as(&server, 2))).unwrap();

    let rep = block_on(sync::sync_once(&c, &online_as(&server, 1))).unwrap();
    assert_eq!(rep.pushed.gone, vec![id]);
    assert!(matches!(c.note(id), Err(CoreError::NoteNotFound(_))));
    assert_eq!(c.outbox_len().unwrap(), 0);
    assert!(c.unreported_purges().unwrap().is_empty(), "票は送らない");
}
