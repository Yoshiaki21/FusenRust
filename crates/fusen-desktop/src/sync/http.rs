//! サーバーとの通信（HTTP）。fusen-core の `Transport` を実装する。
//!
//! - 添付の送受信はファイルをストリーミングで扱う（大きなファイルでもメモリに載せない）
//! - エラーは `CoreError::Transport` にまとめる（オフラインなど）

use std::path::Path;
use std::time::Duration;

use fusen_core::sync::Transport;
use fusen_core::{AttachmentMeta, CoreError, Result};
use fusen_proto::{
    API_PREFIX, ChangesResponse, NoteHistoryResponse, PurgeResponse, PushRevisionRequest,
    PushRevisionResponse,
};
use futures_util::StreamExt as _;
use tokio::io::AsyncWriteExt as _;
use uuid::Uuid;

/// 接続先とトークン
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ServerConfig {
    /// 例: `https://fusen.example.com`（末尾の / や /api/v1 はあってもなくてもよい）
    pub url: String,
    pub token: String,
}

impl ServerConfig {
    /// URL を正規化する（末尾の / と /api/v1 を取り除く）
    pub fn base(&self) -> String {
        let mut u = self.url.trim().trim_end_matches('/').to_string();
        if let Some(stripped) = u.strip_suffix(API_PREFIX) {
            u = stripped.to_string();
        }
        u
    }

    /// API の URL（`<base>/api/v1`）
    pub fn api(&self) -> String {
        format!("{}{API_PREFIX}", self.base())
    }

    /// WebSocket の URL（http → ws、https → wss）
    pub fn ws(&self) -> String {
        let base = self.base();
        let ws = if let Some(rest) = base.strip_prefix("https://") {
            format!("wss://{rest}")
        } else if let Some(rest) = base.strip_prefix("http://") {
            format!("ws://{rest}")
        } else {
            base
        };
        format!("{ws}{API_PREFIX}/ws")
    }
}

pub struct HttpTransport {
    client: reqwest::Client,
    config: ServerConfig,
}

impl HttpTransport {
    pub fn new(config: ServerConfig) -> Result<Self> {
        let client = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(10))
            // 大きな添付の送受信があるので全体の時間制限は付けない。読み取りが止まったら打ち切る
            .read_timeout(Duration::from_secs(120))
            .user_agent(concat!("fusen-desktop/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(terr)?;
        Ok(Self { client, config })
    }

    fn req(&self, method: reqwest::Method, path: &str) -> reqwest::RequestBuilder {
        self.client
            .request(method, format!("{}{path}", self.config.api()))
            .bearer_auth(&self.config.token)
    }
}

fn terr(e: impl std::fmt::Display) -> CoreError {
    CoreError::Transport(e.to_string())
}

/// 2xx 以外はエラーにする（本文の `{"error": ...}` をメッセージに含める）
async fn check(resp: reqwest::Response) -> Result<reqwest::Response> {
    let status = resp.status();
    if status.is_success() {
        return Ok(resp);
    }
    let body = resp.text().await.unwrap_or_default();
    let msg = serde_json::from_str::<fusen_proto::ErrorResponse>(&body)
        .map(|e| e.error)
        .unwrap_or(body);
    Err(CoreError::Transport(match status.as_u16() {
        401 => "トークンが正しくありません（設定を確認してください）".to_string(),
        413 => format!("ファイルがサーバーの上限を超えています: {msg}"),
        _ => format!("サーバーのエラー {status}: {msg}"),
    }))
}

impl Transport for HttpTransport {
    async fn get_changes(&self, since: i64, limit: u32) -> Result<ChangesResponse> {
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
    ) -> Result<PushRevisionResponse> {
        let resp = self
            .req(
                reqwest::Method::POST,
                &format!("/notes/{note_id}/revisions"),
            )
            .json(req)
            .send()
            .await
            .map_err(terr)?;
        // 完全に削除された付箋（D-014）
        if resp.status() == reqwest::StatusCode::GONE {
            return Err(CoreError::NoteGone(note_id));
        }
        check(resp).await?.json().await.map_err(terr)
    }

    async fn register_attachment(&self, meta: &AttachmentMeta) -> Result<()> {
        let resp = self
            .req(reqwest::Method::POST, "/attachments")
            .json(meta)
            .send()
            .await
            .map_err(terr)?;
        check(resp).await.map(|_| ())
    }

    async fn has_blob(&self, sha256: &str) -> Result<bool> {
        let resp = self
            .req(reqwest::Method::HEAD, &format!("/blobs/{sha256}"))
            .send()
            .await
            .map_err(terr)?;
        match resp.status().as_u16() {
            200 => Ok(true),
            404 => Ok(false),
            _ => check(resp).await.map(|_| false),
        }
    }

    async fn put_blob(&self, sha256: &str, path: &Path) -> Result<()> {
        let file = tokio::fs::File::open(path).await?;
        let len = file.metadata().await?.len();
        let body = reqwest::Body::wrap_stream(tokio_util::io::ReaderStream::new(file));
        let resp = self
            .req(reqwest::Method::PUT, &format!("/blobs/{sha256}"))
            .header(reqwest::header::CONTENT_LENGTH, len)
            .body(body)
            .send()
            .await
            .map_err(terr)?;
        check(resp).await.map(|_| ())
    }

    async fn download_blob(&self, sha256: &str, dest: &Path) -> Result<()> {
        let resp = self
            .req(reqwest::Method::GET, &format!("/blobs/{sha256}"))
            .send()
            .await
            .map_err(terr)?;
        let resp = check(resp).await?;
        let mut file = tokio::fs::File::create(dest).await?;
        let mut stream = resp.bytes_stream();
        while let Some(chunk) = stream.next().await {
            file.write_all(&chunk.map_err(terr)?).await?;
        }
        file.sync_all().await?;
        Ok(())
    }

    async fn get_note_history(&self, note_id: Uuid) -> Result<Option<NoteHistoryResponse>> {
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

    async fn purge_note(&self, note_id: Uuid) -> Result<PurgeResponse> {
        let resp = self
            .req(reqwest::Method::POST, &format!("/notes/{note_id}/purge"))
            .send()
            .await
            .map_err(terr)?;
        check(resp).await?.json().await.map_err(terr)
    }
}

#[cfg(test)]
mod tests {
    use super::ServerConfig;

    fn cfg(url: &str) -> ServerConfig {
        ServerConfig {
            url: url.into(),
            token: "t".into(),
        }
    }

    #[test]
    fn normalizes_urls() {
        for u in [
            "https://fusen.example.com",
            "https://fusen.example.com/",
            "https://fusen.example.com/api/v1",
            " https://fusen.example.com/api/v1/ ",
        ] {
            assert_eq!(cfg(u).api(), "https://fusen.example.com/api/v1");
            assert_eq!(cfg(u).ws(), "wss://fusen.example.com/api/v1/ws");
        }
        assert_eq!(
            cfg("http://127.0.0.1:9090").ws(),
            "ws://127.0.0.1:9090/api/v1/ws"
        );
    }
}
