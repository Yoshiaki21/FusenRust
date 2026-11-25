//! サーバーの結合テスト。実際に HTTP で待ち受けるサーバーを立て、
//! fusen-core の同期エンジン（2 つの仮想クライアント）と、生の HTTP / WebSocket で確かめる。
//!
//! - 2 つの仮想クライアントでの同期
//! - オフライン中の編集の後送り
//! - 競合の印
//! - 不正なトークンの拒否
//! - blob のハッシュ不一致の拒否
//! - ほか: 冪等な再送、上限超過（413）、blob のない版の拒否、WebSocket の通知
//! - 完全削除（D-014）: 全トークンの票で削除・無効にしたトークンは数えない・新しい版で票を数え直す・410

use std::path::Path;
use std::time::Duration;

use fusen_core::sync::{self, Transport};
use fusen_core::{AttachmentMeta, CoreError, Result as CoreResult, Store, color};
use fusen_proto::{
    ChangesResponse, NoteHistoryResponse, PurgeResponse, PurgeState, PushRevisionRequest,
    PushRevisionResponse, RevisionKind, WsMessage,
};
use fusen_server::{AppState, Db, db, router};
use futures_util::StreamExt as _;
use uuid::Uuid;

/// テスト用のサーバー
struct TestServer {
    base: String,
    addr: std::net::SocketAddr,
    db: Db,
    _dir: tempfile::TempDir,
}

impl TestServer {
    async fn start(max_upload_bytes: u64) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(dir.path()).unwrap();
        let state = AppState::new(db.clone(), dir.path(), max_upload_bytes).unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, router(state)).await.unwrap();
        });
        Self {
            base: format!("http://{addr}/api/v1"),
            addr,
            db,
            _dir: dir,
        }
    }

    fn token(&self, device: &str) -> String {
        self.db.with(|c| db::create_token(c, device)).unwrap().1
    }

    fn token_with_id(&self, device: &str) -> (String, String) {
        self.db.with(|c| db::create_token(c, device)).unwrap()
    }

    fn blob_exists(&self, sha256: &str) -> bool {
        self._dir
            .path()
            .join("blobs")
            .join(&sha256[..2])
            .join(sha256)
            .is_file()
    }
}

/// HTTP の Transport（テスト用。Phase 4 でデスクトップ用を作る）
struct Http {
    client: reqwest::Client,
    base: String,
    token: String,
}

impl Http {
    fn new(base: &str, token: &str) -> Self {
        Self {
            client: reqwest::Client::new(),
            base: base.to_string(),
            token: token.to_string(),
        }
    }

    /// つながらない宛先（オフラインの代わり）
    fn offline() -> Self {
        Self::new("http://127.0.0.1:9/api/v1", "x")
    }

    fn req(&self, method: reqwest::Method, path: &str) -> reqwest::RequestBuilder {
        self.client
            .request(method, format!("{}{path}", self.base))
            .bearer_auth(&self.token)
    }
}

fn terr(e: impl std::fmt::Display) -> CoreError {
    CoreError::Transport(e.to_string())
}

async fn check(resp: reqwest::Response) -> CoreResult<reqwest::Response> {
    if resp.status().is_success() {
        Ok(resp)
    } else {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        Err(CoreError::Transport(format!("{status}: {body}")))
    }
}

impl Transport for Http {
    async fn get_changes(&self, since: i64, limit: u32) -> CoreResult<ChangesResponse> {
        let resp = self
            .req(
                reqwest::Method::GET,
                &format!("/changes?since={since}&limit={limit}"),
            )
            .send()
            .await
            .map_err(terr)?;
        check(resp).await?.json().await.map_err(terr)
    }

    async fn push_revision(
        &self,
        note_id: Uuid,
        req: &PushRevisionRequest,
    ) -> CoreResult<PushRevisionResponse> {
        let resp = self
            .req(
                reqwest::Method::POST,
                &format!("/notes/{note_id}/revisions"),
            )
            .json(req)
            .send()
            .await
            .map_err(terr)?;
        if resp.status() == reqwest::StatusCode::GONE {
            return Err(CoreError::NoteGone(note_id));
        }
        check(resp).await?.json().await.map_err(terr)
    }

    async fn register_attachment(&self, meta: &AttachmentMeta) -> CoreResult<()> {
        let resp = self
            .req(reqwest::Method::POST, "/attachments")
            .json(meta)
            .send()
            .await
            .map_err(terr)?;
        check(resp).await.map(|_| ())
    }

    async fn has_blob(&self, sha256: &str) -> CoreResult<bool> {
        let resp = self
            .req(reqwest::Method::HEAD, &format!("/blobs/{sha256}"))
            .send()
            .await
            .map_err(terr)?;
        match resp.status().as_u16() {
            200 => Ok(true),
            404 => Ok(false),
            s => Err(CoreError::Transport(format!("HEAD blob: {s}"))),
        }
    }

    async fn put_blob(&self, sha256: &str, path: &Path) -> CoreResult<()> {
        let bytes = tokio::fs::read(path).await?;
        let resp = self
            .req(reqwest::Method::PUT, &format!("/blobs/{sha256}"))
            .body(bytes)
            .send()
            .await
            .map_err(terr)?;
        check(resp).await.map(|_| ())
    }

    async fn download_blob(&self, sha256: &str, dest: &Path) -> CoreResult<()> {
        let resp = self
            .req(reqwest::Method::GET, &format!("/blobs/{sha256}"))
            .send()
            .await
            .map_err(terr)?;
        let bytes = check(resp).await?.bytes().await.map_err(terr)?;
        tokio::fs::write(dest, &bytes).await?;
        Ok(())
    }

    async fn get_note_history(&self, note_id: Uuid) -> CoreResult<Option<NoteHistoryResponse>> {
        let resp = self
            .req(reqwest::Method::GET, &format!("/notes/{note_id}/revisions"))
            .send()
            .await
            .map_err(terr)?;
        if resp.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        check(resp).await?.json().await.map(Some).map_err(terr)
    }

    async fn purge_note(&self, note_id: Uuid) -> CoreResult<PurgeResponse> {
        let resp = self
            .req(reqwest::Method::POST, &format!("/notes/{note_id}/purge"))
            .send()
            .await
            .map_err(terr)?;
        check(resp).await?.json().await.map_err(terr)
    }
}

fn device() -> (tempfile::TempDir, Store) {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    (dir, store)
}

// ----------------------------------------------------------------------
// 同期（fusen-core のエンジンで）
// ----------------------------------------------------------------------

#[tokio::test]
async fn two_clients_sync() {
    let srv = TestServer::start(1024 * 1024).await;
    let ta = Http::new(&srv.base, &srv.token("a"));
    let tb = Http::new(&srv.base, &srv.token("b"));
    let (_da, a) = device();
    let (_db, b) = device();

    let r = a.create_note("# A で作った付箋", color::BLUE).unwrap();
    a.save_edit(r.note_id, "# A で作った付箋\n- [ ] やること", None, None)
        .unwrap();
    let rep = sync::sync_once(&a, &ta).await.unwrap();
    assert_eq!(rep.pushed.pushed, 2);
    assert_eq!(a.note(r.note_id).unwrap().rev, 2);

    let rep = sync::sync_once(&b, &tb).await.unwrap();
    assert_eq!(rep.pulled.new_notes, vec![r.note_id]);
    let nb = b.note(r.note_id).unwrap();
    assert_eq!(nb.content, "# A で作った付箋\n- [ ] やること");
    assert_eq!(nb.color, color::BLUE);
    assert_eq!(nb.edited_by, a.device_id());

    // B が編集 → A に届く
    b.save_edit(r.note_id, "B が更新", None, None).unwrap();
    sync::sync_once(&b, &tb).await.unwrap();
    sync::sync_once(&a, &ta).await.unwrap();
    assert_eq!(a.note(r.note_id).unwrap().content, "B が更新");
    assert_eq!(a.history(r.note_id).unwrap(), b.history(r.note_id).unwrap());

    // タイトルもサーバーを通して届く
    a.set_title(r.note_id, "買い物リスト").unwrap();
    sync::sync_once(&a, &ta).await.unwrap();
    sync::sync_once(&b, &tb).await.unwrap();
    let nb = b.note(r.note_id).unwrap();
    assert_eq!(nb.title, "買い物リスト");
    assert_eq!(nb.content, "B が更新", "タイトルの変更で本文は変わらない");
    let h = b.history(r.note_id).unwrap();
    assert_eq!(h.last().unwrap().kind, RevisionKind::Title);
}

#[tokio::test]
async fn offline_edits_are_sent_later() {
    let srv = TestServer::start(1024 * 1024).await;
    let ta = Http::new(&srv.base, &srv.token("a"));
    let tb = Http::new(&srv.base, &srv.token("b"));
    let (_da, a) = device();
    let (_db, b) = device();

    let r = a.create_note("オフラインで作成", color::YELLOW).unwrap();
    a.save_edit(r.note_id, "オフラインで 2 回目", None, None)
        .unwrap();
    assert!(sync::sync_once(&a, &Http::offline()).await.is_err());
    assert_eq!(a.outbox_len().unwrap(), 2);

    let rep = sync::sync_once(&a, &ta).await.unwrap();
    assert_eq!(rep.pushed.pushed, 2);
    assert!(rep.pushed.conflicts.is_empty());
    assert_eq!(a.outbox_len().unwrap(), 0);

    sync::sync_once(&b, &tb).await.unwrap();
    assert_eq!(b.note(r.note_id).unwrap().content, "オフラインで 2 回目");
}

#[tokio::test]
async fn concurrent_edits_get_conflict_flag() {
    let srv = TestServer::start(1024 * 1024).await;
    let ta = Http::new(&srv.base, &srv.token("a"));
    let tb = Http::new(&srv.base, &srv.token("b"));
    let (_da, a) = device();
    let (_db, b) = device();

    let r = a.create_note("元", color::YELLOW).unwrap();
    sync::sync_once(&a, &ta).await.unwrap();
    sync::sync_once(&b, &tb).await.unwrap();

    a.save_edit(r.note_id, "A の編集", None, Some(1)).unwrap();
    b.save_edit(r.note_id, "B の編集", None, Some(1)).unwrap();
    let ra = sync::sync_once(&a, &ta).await.unwrap();
    assert!(ra.pushed.conflicts.is_empty());
    let rb = sync::sync_once(&b, &tb).await.unwrap();
    assert_eq!(rb.pushed.conflicts, vec![(r.note_id, 3)]);

    sync::sync_once(&a, &ta).await.unwrap();
    let na = a.note(r.note_id).unwrap();
    assert_eq!(na.content, "B の編集", "後勝ち");
    assert!(na.conflict);
    // 負けた側の版も履歴に残る
    let contents: Vec<_> = a
        .history(r.note_id)
        .unwrap()
        .into_iter()
        .map(|r| r.content)
        .collect();
    assert_eq!(contents, ["元", "A の編集", "B の編集"]);

    // サーバーの全版の API でも確かめる
    let all = ta.get_note_history(r.note_id).await.unwrap().unwrap();
    assert_eq!(
        all.revisions.iter().map(|r| r.conflict).collect::<Vec<_>>(),
        [false, false, true]
    );
}

#[tokio::test]
async fn attachments_roundtrip() {
    let srv = TestServer::start(1024 * 1024).await;
    let ta = Http::new(&srv.base, &srv.token("a"));
    let tb = Http::new(&srv.base, &srv.token("b"));
    let (_da, a) = device();
    let (_db, b) = device();

    let id = a.create_note("画像つき", color::YELLOW).unwrap().note_id;
    let png = vec![0x89u8, b'P', b'N', b'G', 1, 2, 3, 4];
    let meta = a
        .import_attachment_bytes(id, "写真.png", &png, 1024)
        .unwrap();
    a.add_attachment(id, meta.attachment_id, None).unwrap();
    sync::sync_once(&a, &ta).await.unwrap();
    assert!(ta.has_blob(&meta.sha256).await.unwrap());

    sync::sync_once(&b, &tb).await.unwrap();
    assert_eq!(
        b.attachment(meta.attachment_id).unwrap().file_name,
        "写真.png"
    );
    let missing = b.missing_blobs().unwrap();
    assert_eq!(missing.len(), 1);
    let path = sync::fetch_blob(&b, &tb, &missing[0], 1024).await.unwrap();
    assert_eq!(std::fs::read(path).unwrap(), png);
}

// ----------------------------------------------------------------------
// 認証
// ----------------------------------------------------------------------

#[tokio::test]
async fn invalid_tokens_are_rejected() {
    let srv = TestServer::start(1024).await;
    let client = reqwest::Client::new();
    let url = format!("{}/changes?since=0", srv.base);

    // 死活確認は認証不要
    for path in [
        format!("http://{}/healthz", srv.addr),
        format!("{}/healthz", srv.base),
    ] {
        let r = client.get(path).send().await.unwrap();
        assert_eq!(r.status(), 200);
        assert_eq!(r.text().await.unwrap(), "ok");
    }

    // トークンなし・不正なトークン
    assert_eq!(client.get(&url).send().await.unwrap().status(), 401);
    let r = client
        .get(&url)
        .bearer_auth("fsn_wrong")
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 401);
    let body: fusen_proto::ErrorResponse = r.json().await.unwrap();
    assert!(!body.error.is_empty());
    let r = client
        .get(&url)
        .header("Authorization", "Basic abc")
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 401);

    // 有効なトークン → 無効化すると拒否
    let token = srv.token("a");
    assert_eq!(
        client
            .get(&url)
            .bearer_auth(&token)
            .send()
            .await
            .unwrap()
            .status(),
        200
    );
    let id = srv.db.with(|c| db::list_tokens(c)).unwrap()[0]
        .token_id
        .clone();
    assert!(srv.db.with(|c| db::revoke_token(c, &id)).unwrap());
    assert_eq!(
        client
            .get(&url)
            .bearer_auth(&token)
            .send()
            .await
            .unwrap()
            .status(),
        401
    );

    // blob も WebSocket も同じ
    let r = client
        .put(format!("{}/blobs/{}", srv.base, "0".repeat(64)))
        .body("x")
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 401);
    let ws = tokio_tungstenite::connect_async(format!("ws://{}/api/v1/ws", srv.addr)).await;
    assert!(ws.is_err(), "トークンなしの WebSocket は拒否");
}

// ----------------------------------------------------------------------
// blob
// ----------------------------------------------------------------------

#[tokio::test]
async fn blob_hash_mismatch_is_rejected() {
    let srv = TestServer::start(1024).await;
    let t = Http::new(&srv.base, &srv.token("a"));
    let claimed = fusen_core::blob::sha256_hex(b"expected content");

    let r = t
        .req(reqwest::Method::PUT, &format!("/blobs/{claimed}"))
        .body("other content")
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 400);
    assert!(
        !t.has_blob(&claimed).await.unwrap(),
        "不一致の blob は保存しない"
    );

    // 正しい内容なら保存される。2 回目は「既にある」
    let r = t
        .req(reqwest::Method::PUT, &format!("/blobs/{claimed}"))
        .body("expected content")
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 201);
    let r = t
        .req(reqwest::Method::PUT, &format!("/blobs/{claimed}"))
        .body("expected content")
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let got = t
        .req(reqwest::Method::GET, &format!("/blobs/{claimed}"))
        .send()
        .await
        .unwrap()
        .bytes()
        .await
        .unwrap();
    assert_eq!(&got[..], b"expected content");

    // 不正な sha256（ディレクトリをまたぐ指定など）
    let r = t
        .req(reqwest::Method::GET, "/blobs/..%2F..%2Fetc%2Fpasswd")
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 400);
    let r = t
        .req(reqwest::Method::GET, &format!("/blobs/{}", "a".repeat(64)))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 404);
}

#[tokio::test]
async fn too_large_blob_gets_413() {
    let srv = TestServer::start(100).await;
    let t = Http::new(&srv.base, &srv.token("a"));
    let data = vec![1u8; 101];
    let sha = fusen_core::blob::sha256_hex(&data);
    let r = t
        .req(reqwest::Method::PUT, &format!("/blobs/{sha}"))
        .body(data)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 413);
    assert!(!t.has_blob(&sha).await.unwrap());

    // Content-Length のない（ストリーミングの）送信でも、受け取りながら打ち切る
    let chunks: Vec<Result<Vec<u8>, std::io::Error>> = vec![Ok(vec![2u8; 60]), Ok(vec![2u8; 60])];
    let stream_sha = fusen_core::blob::sha256_hex(&[2u8; 120]);
    let r = t
        .req(reqwest::Method::PUT, &format!("/blobs/{stream_sha}"))
        .body(reqwest::Body::wrap_stream(futures_util::stream::iter(
            chunks,
        )))
        .send()
        .await;
    // 途中で接続を切られることもあるが、成功（2xx）にはならない
    if let Ok(r) = r {
        assert_eq!(r.status(), 413);
    }
    assert!(!t.has_blob(&stream_sha).await.unwrap());

    // 上限ちょうどは受け付ける
    let ok = vec![3u8; 100];
    let r = t
        .req(
            reqwest::Method::PUT,
            &format!("/blobs/{}", fusen_core::blob::sha256_hex(&ok)),
        )
        .body(ok)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 201);
}

// ----------------------------------------------------------------------
// 版の受け付け
// ----------------------------------------------------------------------

fn push_req(attachments: Vec<Uuid>) -> PushRevisionRequest {
    PushRevisionRequest {
        op_id: Uuid::now_v7(),
        device_id: Uuid::now_v7(),
        edited_at: chrono::Utc::now(),
        kind: RevisionKind::Edit,
        title: "題".into(),
        content: "本文".into(),
        color: "yellow".into(),
        attachments,
        deleted: false,
        base_rev: None,
    }
}

#[tokio::test]
async fn same_revision_twice_is_not_duplicated() {
    let srv = TestServer::start(1024).await;
    let t = Http::new(&srv.base, &srv.token("a"));
    let note = Uuid::now_v7();
    let req = push_req(vec![]);
    let first = t.push_revision(note, &req).await.unwrap();
    let second = t.push_revision(note, &req).await.unwrap();
    assert_eq!(first, second);
    let changes = t.get_changes(0, 100).await.unwrap();
    assert_eq!(changes.revisions.len(), 1);
    assert_eq!(changes.last_server_seq, 1);
    assert!(!changes.has_more);
}

#[tokio::test]
async fn revision_without_blob_is_rejected() {
    let srv = TestServer::start(1024).await;
    let t = Http::new(&srv.base, &srv.token("a"));
    let note = Uuid::now_v7();

    // メタデータのない添付を参照する版
    let err = t
        .push_revision(note, &push_req(vec![Uuid::now_v7()]))
        .await
        .unwrap_err();
    assert!(err.to_string().contains("422"), "{err}");

    // 実体のない添付はメタデータも登録できない
    let meta = AttachmentMeta {
        attachment_id: Uuid::now_v7(),
        note_id: note,
        sha256: fusen_core::blob::sha256_hex(b"not uploaded"),
        file_name: "a.txt".into(),
        mime: "text/plain".into(),
        size: 12,
        created_at: chrono::Utc::now(),
    };
    let err = t.register_attachment(&meta).await.unwrap_err();
    assert!(err.to_string().contains("422"), "{err}");
    assert_eq!(t.get_changes(0, 10).await.unwrap().revisions.len(), 0);
}

#[tokio::test]
async fn changes_are_paged() {
    let srv = TestServer::start(1024).await;
    let t = Http::new(&srv.base, &srv.token("a"));
    for _ in 0..5 {
        t.push_revision(Uuid::now_v7(), &push_req(vec![]))
            .await
            .unwrap();
    }
    let p1 = t.get_changes(0, 2).await.unwrap();
    assert_eq!(
        (p1.revisions.len(), p1.last_server_seq, p1.has_more),
        (2, 2, true)
    );
    let p3 = t.get_changes(4, 2).await.unwrap();
    assert_eq!(
        (p3.revisions.len(), p3.last_server_seq, p3.has_more),
        (1, 5, false)
    );
    let none = t.get_changes(5, 2).await.unwrap();
    assert_eq!((none.revisions.len(), none.last_server_seq), (0, 5));
}

// ----------------------------------------------------------------------
// WebSocket
// ----------------------------------------------------------------------

#[tokio::test]
async fn websocket_notifies_changes() {
    use tokio_tungstenite::tungstenite::client::IntoClientRequest as _;

    let srv = TestServer::start(1024).await;
    let token = srv.token("a");
    let mut req = format!("ws://{}/api/v1/ws", srv.addr)
        .into_client_request()
        .unwrap();
    req.headers_mut()
        .insert("Authorization", format!("Bearer {token}").parse().unwrap());
    let (mut ws, _) = tokio_tungstenite::connect_async(req).await.unwrap();

    let t = Http::new(&srv.base, &token);
    let resp = t
        .push_revision(Uuid::now_v7(), &push_req(vec![]))
        .await
        .unwrap();

    let msg = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match ws.next().await {
                Some(Ok(tokio_tungstenite::tungstenite::Message::Text(t))) => return t.to_string(),
                Some(Ok(_)) => continue,
                other => panic!("WebSocket が切れました: {other:?}"),
            }
        }
    })
    .await
    .expect("通知が届かない");
    assert_eq!(
        msg,
        format!(r#"{{"type":"changed","server_seq":{}}}"#, resp.server_seq)
    );
    let parsed: WsMessage = serde_json::from_str(&msg).unwrap();
    assert_eq!(
        parsed,
        WsMessage::Changed {
            server_seq: resp.server_seq
        }
    );
}

// ----------------------------------------------------------------------
// 完全削除（D-014）
// ----------------------------------------------------------------------

/// 添付付きの付箋を A で作ってごみ箱に入れ、A・B の両方が取り込んだ状態にする。(付箋, 添付の sha256)
async fn trashed_note(a: &Store, ta: &Http, b: &Store, tb: &Http) -> (Uuid, String) {
    let id = a.create_note("消す付箋", color::YELLOW).unwrap().note_id;
    let m = a
        .import_attachment_bytes(id, "x.png", b"purge-me", 1024)
        .unwrap();
    a.add_attachment(id, m.attachment_id, None).unwrap();
    a.trash(id).unwrap();
    sync::sync_once(a, ta).await.unwrap();
    sync::sync_once(b, tb).await.unwrap();
    assert!(b.note(id).unwrap().deleted);
    (id, m.sha256)
}

#[tokio::test]
async fn purge_after_all_tokens_voted() {
    let srv = TestServer::start(1024 * 1024).await;
    let ta = Http::new(&srv.base, &srv.token("a"));
    let tb = Http::new(&srv.base, &srv.token("b"));
    // 一度も使っていないトークンは数えない
    let _unused = srv.token("never-used");
    let (_da, a) = device();
    let (_db, b) = device();
    let (id, sha) = trashed_note(&a, &ta, &b, &tb).await;

    a.purge(id, false).unwrap();
    let rep = sync::sync_once(&a, &ta).await.unwrap();
    assert_eq!(
        rep.purges[0].1,
        PurgeResponse {
            state: PurgeState::Waiting,
            remaining: 1
        }
    );
    assert!(ta.get_note_history(id).await.unwrap().is_some());
    assert!(srv.blob_exists(&sha));

    b.purge(id, false).unwrap();
    let rep = sync::sync_once(&b, &tb).await.unwrap();
    assert_eq!(rep.purges[0].1.state, PurgeState::Purged);
    assert!(ta.get_note_history(id).await.unwrap().is_none());
    assert!(!srv.blob_exists(&sha), "参照されなくなった実体も消す");

    // 新しく同期を始めた端末には届かない
    let (_dc, c) = device();
    let tc = Http::new(&srv.base, &srv.token("c"));
    sync::sync_once(&c, &tc).await.unwrap();
    assert!(c.list_trash().unwrap().is_empty());

    // 削除済みの付箋への版は 410
    let req = PushRevisionRequest {
        op_id: Uuid::now_v7(),
        device_id: Uuid::now_v7(),
        edited_at: chrono::Utc::now(),
        kind: RevisionKind::Restore,
        title: String::new(),
        content: "戻す".into(),
        color: color::YELLOW.into(),
        attachments: vec![],
        deleted: false,
        base_rev: None,
    };
    assert!(matches!(
        tc.push_revision(id, &req).await,
        Err(CoreError::NoteGone(_))
    ));
    // 票をもう一度送っても成功扱い
    assert_eq!(tc.purge_note(id).await.unwrap().state, PurgeState::Purged);
}

#[tokio::test]
async fn revoking_last_token_completes_purge() {
    let srv = TestServer::start(1024 * 1024).await;
    let ta = Http::new(&srv.base, &srv.token("a"));
    let (b_id, b_token) = srv.token_with_id("b");
    let tb = Http::new(&srv.base, &b_token);
    let (_da, a) = device();
    let (_db, b) = device();
    let (id, _) = trashed_note(&a, &ta, &b, &tb).await;

    a.purge(id, false).unwrap();
    sync::sync_once(&a, &ta).await.unwrap();
    assert_eq!(srv.db.with(db::purge_ready_notes).unwrap().0, 0);

    // 管理画面の待ち状況: A は削除済み、B 待ち
    let waiting = srv.db.with(|c| db::purge_waiting(c)).unwrap();
    assert_eq!(waiting.len(), 1);
    assert_eq!(waiting[0].title, "消す付箋");
    assert_eq!(waiting[0].voted, ["a"]);
    assert_eq!(waiting[0].waiting, ["b"]);
    let usage = srv.db.with(|c| db::usage(c)).unwrap();
    assert_eq!((usage.trashed, usage.blobs, usage.blob_bytes), (1, 1, 8));

    // B の PC を使わなくなったのでトークンを無効にする → A の票だけで削除される
    assert!(srv.db.with(|c| db::revoke_token(c, &b_id)).unwrap());
    let (count, unused) = srv.db.with(db::purge_ready_notes).unwrap();
    assert_eq!(count, 1);
    assert_eq!(unused.len(), 1);
    assert!(ta.get_note_history(id).await.unwrap().is_none());
    assert!(srv.db.with(|c| db::purge_waiting(c)).unwrap().is_empty());
    assert_eq!(srv.db.with(|c| db::usage(c)).unwrap().purged, 1);
}

#[tokio::test]
async fn new_revision_resets_votes() {
    let srv = TestServer::start(1024 * 1024).await;
    let ta = Http::new(&srv.base, &srv.token("a"));
    let tb = Http::new(&srv.base, &srv.token("b"));
    let (_da, a) = device();
    let (_db, b) = device();
    let (id, _) = trashed_note(&a, &ta, &b, &tb).await;

    a.purge(id, false).unwrap();
    sync::sync_once(&a, &ta).await.unwrap();

    // B が戻した → サーバーではごみ箱にないので、B の票は記録しない
    b.restore(id).unwrap();
    sync::sync_once(&b, &tb).await.unwrap();
    assert_eq!(
        tb.purge_note(id).await.unwrap().state,
        PurgeState::NotDeleted
    );

    // A は取り込み直す（全履歴と添付も）
    sync::sync_once(&a, &ta).await.unwrap();
    let n = a.note(id).unwrap();
    assert!(!n.deleted);
    assert_eq!(a.history(id).unwrap().len(), 4);
    assert_eq!(a.attachments_of(&n).unwrap().len(), 1);

    // 再びごみ箱へ。B だけが消しても、A の古い票は数えない
    b.trash(id).unwrap();
    sync::sync_once(&b, &tb).await.unwrap();
    b.purge(id, false).unwrap();
    let rep = sync::sync_once(&b, &tb).await.unwrap();
    assert_eq!(rep.purges[0].1.state, PurgeState::Waiting);
}

/// 受付番号の最大の版を完全削除しても、番号を使い回さない（D-015。使い回すと端末が取りこぼす）
#[tokio::test]
async fn purge_does_not_reuse_server_seq() {
    let srv = TestServer::start(1024 * 1024).await;
    let ta = Http::new(&srv.base, &srv.token("a"));
    let tb = Http::new(&srv.base, &srv.token("b"));
    let (_da, a) = device();
    let (_db, b) = device();

    let keep = a.create_note("残す", color::YELLOW).unwrap().note_id;
    sync::sync_once(&a, &ta).await.unwrap();
    // 後から作った付箋（受付番号が最大）を両方で完全に削除する
    let (gone, _) = trashed_note(&a, &ta, &b, &tb).await;
    let before = b.last_server_seq().unwrap();
    a.purge(gone, false).unwrap();
    sync::sync_once(&a, &ta).await.unwrap();
    b.purge(gone, false).unwrap();
    sync::sync_once(&b, &tb).await.unwrap();
    assert!(ta.get_note_history(gone).await.unwrap().is_none());

    // その後の編集は、前より大きい番号で受け付けられ、相手に届く
    a.save_edit(keep, "削除の後の編集", None, None).unwrap();
    sync::sync_once(&a, &ta).await.unwrap();
    let h = a.history(keep).unwrap();
    assert!(h.last().unwrap().server_seq.unwrap() > before);
    sync::sync_once(&b, &tb).await.unwrap();
    assert_eq!(b.note(keep).unwrap().content, "削除の後の編集");
}
