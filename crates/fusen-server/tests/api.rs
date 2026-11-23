//! サーバーの結合テスト。実際に HTTP で待ち受けるサーバーを立て、
//! fusen-core の同期エンジン（2 つの仮想クライアント）と、生の HTTP / WebSocket で確かめる。
//!
//! - 2 つの仮想クライアントでの同期
//! - オフライン中の編集の後送り
//! - 競合の印
//! - 不正なトークンの拒否
//! - blob のハッシュ不一致の拒否
//! - ほか: 冪等な再送、上限超過（413）、blob のない版の拒否、WebSocket の通知

use std::path::Path;
use std::time::Duration;

use fusen_core::sync::{self, Transport};
use fusen_core::{AttachmentMeta, CoreError, Result as CoreResult, Store, color};
use fusen_proto::{
    ChangesResponse, PushRevisionRequest, PushRevisionResponse, RevisionKind, WsMessage,
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
    let all: Vec<fusen_proto::Revision> = ta
        .req(
            reqwest::Method::GET,
            &format!("/notes/{}/revisions", r.note_id),
        )
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        all.iter().map(|r| r.conflict).collect::<Vec<_>>(),
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
