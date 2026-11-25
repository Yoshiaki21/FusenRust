//! HTTP の API（`docs/spec.md` 6.3）。`/api/v1` の下に置く。`/healthz` だけ認証不要。

use std::time::Duration;

use axum::body::Body;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{DefaultBodyLimit, Path, Query, Request, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Extension, Json, Router};
use fusen_proto::{
    API_PREFIX, AttachmentMeta, ChangesQuery, ChangesResponse, NoteHistoryResponse, PurgeResponse,
    PushRevisionRequest, PushRevisionResponse, WsMessage,
};
use futures_util::StreamExt as _;
use sha2::{Digest, Sha256};
use tokio::io::AsyncWriteExt as _;
use uuid::Uuid;

use crate::db::{self, AuthDevice};
use crate::{ApiError, AppState};

/// `GET /changes` の既定・最大の件数
const DEFAULT_CHANGES_LIMIT: u32 = 500;
const MAX_CHANGES_LIMIT: u32 = 1000;
/// JSON の本文の上限（付箋の本文はテキストなので十分）
const JSON_BODY_LIMIT: usize = 16 * 1024 * 1024;
/// WebSocket の生存確認の間隔（リバースプロキシの無通信タイムアウトで切られないように）
const WS_PING_INTERVAL: Duration = Duration::from_secs(30);

pub fn router(state: AppState) -> Router {
    let authed = Router::new()
        .route("/changes", get(get_changes))
        .route(
            "/notes/{note_id}/revisions",
            get(get_revisions).post(push_revision),
        )
        .route("/notes/{note_id}/purge", post(purge_note))
        .route("/attachments", post(register_attachment))
        .route("/ws", get(websocket))
        .layer(DefaultBodyLimit::max(JSON_BODY_LIMIT))
        // blob は大きいので本文の上限を外し、受け取りながら上限を自分で確かめる
        .route(
            "/blobs/{sha256}",
            get(get_blob)
                .head(head_blob)
                .put(put_blob)
                .layer(DefaultBodyLimit::disable()),
        )
        .route_layer(middleware::from_fn_with_state(state.clone(), auth));

    Router::new()
        .route("/healthz", get(healthz))
        .nest(
            API_PREFIX,
            Router::new().route("/healthz", get(healthz)).merge(authed),
        )
        .with_state(state)
}

async fn healthz() -> &'static str {
    "ok"
}

/// `Authorization: Bearer <token>` を確かめる。WebSocket も同じ
async fn auth(
    State(st): State<AppState>,
    mut req: Request,
    next: Next,
) -> Result<Response, ApiError> {
    let token = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .map(str::to_string)
        .ok_or(ApiError::Unauthorized)?;
    let device = st
        .db
        .call(move |c| db::verify_token(c, &token))
        .await?
        .ok_or(ApiError::Unauthorized)?;
    req.extensions_mut().insert(device);
    Ok(next.run(req).await)
}

fn parse_uuid(s: &str) -> Result<Uuid, ApiError> {
    Uuid::parse_str(s).map_err(|_| ApiError::BadRequest(format!("UUID ではありません: {s}")))
}

// ----------------------------------------------------------------------
// 版
// ----------------------------------------------------------------------

async fn get_changes(
    State(st): State<AppState>,
    Query(q): Query<ChangesQuery>,
) -> Result<Json<ChangesResponse>, ApiError> {
    let limit = q
        .limit
        .unwrap_or(DEFAULT_CHANGES_LIMIT)
        .clamp(1, MAX_CHANGES_LIMIT);
    let since = q.since.max(0);
    let resp = st.db.call(move |c| db::changes(c, since, limit)).await?;
    Ok(Json(resp))
}

async fn get_revisions(
    State(st): State<AppState>,
    Path(note_id): Path<String>,
) -> Result<Json<NoteHistoryResponse>, ApiError> {
    let note_id = parse_uuid(&note_id)?;
    Ok(Json(
        st.db.call(move |c| db::note_history(c, note_id)).await?,
    ))
}

/// 端末で完全に削除した票（docs/decisions.md D-014）
async fn purge_note(
    State(st): State<AppState>,
    Extension(device): Extension<AuthDevice>,
    Path(note_id): Path<String>,
) -> Result<Json<PurgeResponse>, ApiError> {
    let note_id = parse_uuid(&note_id)?;
    let token_id = device.token_id.clone();
    let (resp, unused) = st
        .db
        .call(move |c| db::vote_purge(c, note_id, &token_id))
        .await?;
    tracing::info!(
        "完全削除の票: 付箋 {note_id} 端末 {} → {:?}（残り {}）",
        device.device_name,
        resp.state,
        resp.remaining
    );
    crate::remove_blobs(&st.blobs_dir, &unused);
    Ok(Json(resp))
}

async fn push_revision(
    State(st): State<AppState>,
    Extension(device): Extension<AuthDevice>,
    Path(note_id): Path<String>,
    Json(req): Json<PushRevisionRequest>,
) -> Result<Json<PushRevisionResponse>, ApiError> {
    let note_id = parse_uuid(&note_id)?;
    let blobs = st.clone();
    let (resp, created) = st
        .db
        .call(move |c| {
            db::push_revision(c, note_id, &req, &device.token_id, |sha| {
                blobs.blob_path(sha).is_some_and(|p| p.is_file())
            })
        })
        .await?;
    if created {
        tracing::info!(
            "版を受け付けました: 付箋 {note_id} 版 {} 受付 {}{}",
            resp.rev,
            resp.server_seq,
            if resp.conflict { "（競合）" } else { "" }
        );
        // 購読者がいなくてもエラーにしない
        let _ = st.changes.send(resp.server_seq);
    }
    Ok(Json(resp))
}

// ----------------------------------------------------------------------
// 添付
// ----------------------------------------------------------------------

async fn register_attachment(
    State(st): State<AppState>,
    Json(meta): Json<AttachmentMeta>,
) -> Result<StatusCode, ApiError> {
    let Some(path) = st.blob_path(&meta.sha256) else {
        return Err(ApiError::BadRequest(format!(
            "不正な sha256: {}",
            meta.sha256
        )));
    };
    // 実体（blob）を先に送ってもらう（docs/spec.md 6.3）
    if !path.is_file() {
        return Err(ApiError::UnprocessableEntity(format!(
            "添付の実体（{}）を先に送ってください",
            meta.sha256
        )));
    }
    let created = st
        .db
        .call(move |c| db::register_attachment(c, &meta))
        .await?;
    Ok(if created {
        StatusCode::CREATED
    } else {
        StatusCode::OK
    })
}

fn blob_path_or_400(st: &AppState, sha256: &str) -> Result<std::path::PathBuf, ApiError> {
    st.blob_path(sha256)
        .ok_or_else(|| ApiError::BadRequest(format!("不正な sha256: {sha256}")))
}

async fn head_blob(
    State(st): State<AppState>,
    Path(sha256): Path<String>,
) -> Result<StatusCode, ApiError> {
    let path = blob_path_or_400(&st, &sha256)?;
    Ok(if tokio::fs::try_exists(&path).await? {
        StatusCode::OK
    } else {
        StatusCode::NOT_FOUND
    })
}

async fn get_blob(
    State(st): State<AppState>,
    Path(sha256): Path<String>,
) -> Result<Response, ApiError> {
    let path = blob_path_or_400(&st, &sha256)?;
    let file = match tokio::fs::File::open(&path).await {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(ApiError::NotFound(format!("blob {sha256} がありません")));
        }
        Err(e) => return Err(e.into()),
    };
    let len = file.metadata().await?.len();
    let body = Body::from_stream(tokio_util::io::ReaderStream::new(file));
    Ok((
        [
            (header::CONTENT_TYPE, "application/octet-stream".to_string()),
            (header::CONTENT_LENGTH, len.to_string()),
        ],
        body,
    )
        .into_response())
}

/// blob を受け取る。受け取りながらハッシュとサイズを計算し、
/// 上限を超えたら 413、ハッシュが URL と違えば 400 を返して保存しない。
async fn put_blob(
    State(st): State<AppState>,
    Path(sha256): Path<String>,
    headers: HeaderMap,
    body: Body,
) -> Result<StatusCode, ApiError> {
    let dest = blob_path_or_400(&st, &sha256)?;
    let max = st.max_upload_bytes;

    // Content-Length が分かれば、受け取る前に上限を確かめる
    if let Some(len) = headers
        .get(header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok())
        && len > max
    {
        return Err(ApiError::PayloadTooLarge(max));
    }
    if tokio::fs::try_exists(&dest).await? {
        // 同じ中身は 1 つだけ持つ
        return Ok(StatusCode::OK);
    }

    let tmp = st
        .blobs_dir
        .join("tmp")
        .join(format!("{}.part", Uuid::now_v7()));
    let result = receive_to_file(body, &tmp, max).await;
    let actual = match result {
        Ok(h) => h,
        Err(e) => {
            let _ = tokio::fs::remove_file(&tmp).await;
            return Err(e);
        }
    };
    if actual != sha256 {
        let _ = tokio::fs::remove_file(&tmp).await;
        return Err(ApiError::BadRequest(format!(
            "blob のハッシュが一致しません（URL {sha256}、実際 {actual}）"
        )));
    }
    if let Some(dir) = dest.parent() {
        tokio::fs::create_dir_all(dir).await?;
    }
    tokio::fs::rename(&tmp, &dest).await?;
    tracing::info!("blob を保存しました: {sha256}");
    Ok(StatusCode::CREATED)
}

async fn receive_to_file(body: Body, tmp: &std::path::Path, max: u64) -> Result<String, ApiError> {
    let mut file = tokio::fs::File::create(tmp).await?;
    let mut hasher = Sha256::new();
    let mut size: u64 = 0;
    let mut stream = body.into_data_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| ApiError::BadRequest(format!("受信に失敗しました: {e}")))?;
        size += chunk.len() as u64;
        if size > max {
            return Err(ApiError::PayloadTooLarge(max));
        }
        hasher.update(&chunk);
        file.write_all(&chunk).await?;
    }
    file.sync_all().await?;
    Ok(hex::encode(hasher.finalize()))
}

// ----------------------------------------------------------------------
// WebSocket
// ----------------------------------------------------------------------

/// 新しい版を受け付けたら `{"type":"changed","server_seq":N}` を全端末に送る
async fn websocket(
    State(st): State<AppState>,
    Extension(device): Extension<AuthDevice>,
    ws: WebSocketUpgrade,
) -> Response {
    let rx = st.changes.subscribe();
    ws.on_upgrade(move |socket| ws_loop(socket, rx, device))
}

async fn ws_loop(
    mut socket: WebSocket,
    mut rx: tokio::sync::broadcast::Receiver<i64>,
    device: AuthDevice,
) {
    tracing::info!("WebSocket 接続: {}", device.device_name);
    let mut ping = tokio::time::interval(WS_PING_INTERVAL);
    ping.tick().await;
    loop {
        tokio::select! {
            msg = rx.recv() => {
                let seq = match msg {
                    Ok(seq) => seq,
                    // 取りこぼした: 最新の番号だけ知らせれば、端末が差分を取りに来る
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                };
                let text = match serde_json::to_string(&WsMessage::Changed { server_seq: seq }) {
                    Ok(t) => t,
                    Err(_) => continue,
                };
                if socket.send(Message::Text(text.into())).await.is_err() {
                    break;
                }
            }
            incoming = socket.recv() => {
                match incoming {
                    // 端末からのメッセージは使わない（ping への pong は axum が返す）
                    Some(Ok(Message::Close(_))) | None | Some(Err(_)) => break,
                    Some(Ok(_)) => {}
                }
            }
            _ = ping.tick() => {
                if socket.send(Message::Ping(Vec::new().into())).await.is_err() {
                    break;
                }
            }
        }
    }
    tracing::info!("WebSocket 切断: {}", device.device_name);
}
