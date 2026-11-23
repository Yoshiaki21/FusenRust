//! FusenRust の同期サーバー。
//!
//! - axum + SQLite。添付の実体は `<data_dir>/blobs/<sha256の先頭2文字>/<sha256>` に保存する
//! - 端末ごとのトークンによる Bearer 認証（トークンはハッシュだけを保存）
//! - API は `docs/spec.md` 6.3。型は `fusen-proto`
//! - TLS はリバースプロキシ（Caddy）が担当し、このサーバーは HTTP で待ち受ける

pub mod db;
mod error;
mod routes;

use std::net::SocketAddr;
use std::path::PathBuf;

use anyhow::Context as _;
use tokio::sync::broadcast;

pub use db::Db;
pub use error::ApiError;
pub use routes::router;

/// 既定のアップロード上限（500 MB。デスクトップの既定の添付上限と同じ）
pub const DEFAULT_MAX_UPLOAD_BYTES: u64 = 500 * 1024 * 1024;

/// サーバーの設定（環境変数またはコマンドライン引数）
#[derive(Clone, Debug)]
pub struct Config {
    pub listen: SocketAddr,
    pub data_dir: PathBuf,
    pub max_upload_bytes: u64,
}

/// 全リクエストで共有する状態
#[derive(Clone)]
pub struct AppState {
    pub db: Db,
    pub blobs_dir: PathBuf,
    pub max_upload_bytes: u64,
    /// 新しい版を受け付けたら受付番号を流す（WebSocket の購読者へ）
    pub changes: broadcast::Sender<i64>,
}

impl AppState {
    pub fn new(db: Db, data_dir: &std::path::Path, max_upload_bytes: u64) -> anyhow::Result<Self> {
        let blobs_dir = data_dir.join("blobs");
        std::fs::create_dir_all(blobs_dir.join("tmp"))
            .with_context(|| format!("blob の保存先を作れません: {blobs_dir:?}"))?;
        let (changes, _) = broadcast::channel(256);
        Ok(Self {
            db,
            blobs_dir,
            max_upload_bytes,
            changes,
        })
    }

    /// blob の保存先のパス。sha256 の形式が正しくなければ None（ディレクトリをまたぐ指定を防ぐ）
    pub fn blob_path(&self, sha256: &str) -> Option<PathBuf> {
        is_valid_sha256(sha256).then(|| self.blobs_dir.join(&sha256[..2]).join(sha256))
    }
}

/// SHA-256 の 16 進表記（小文字 64 文字）か
pub fn is_valid_sha256(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// サーバーを起動し、SIGTERM / Ctrl+C で止まるまで動かす
pub async fn serve(config: Config) -> anyhow::Result<()> {
    let db = Db::open(&config.data_dir)
        .with_context(|| format!("データを開けません: {:?}", config.data_dir))?;
    let state = AppState::new(db, &config.data_dir, config.max_upload_bytes)?;
    let listener = tokio::net::TcpListener::bind(config.listen)
        .await
        .with_context(|| format!("{} で待ち受けできません", config.listen))?;
    tracing::info!(
        "fusen-server を起動しました: http://{}（データ {:?}、アップロード上限 {} バイト）",
        config.listen,
        config.data_dir,
        config.max_upload_bytes
    );
    axum::serve(listener, router(state))
        .with_graceful_shutdown(shutdown_signal())
        .await?;
    tracing::info!("fusen-server を終了しました");
    Ok(())
}

async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let term = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut s) => {
                s.recv().await;
            }
            Err(_) => std::future::pending::<()>().await,
        }
    };
    #[cfg(not(unix))]
    let term = std::future::pending::<()>();
    tokio::select! {
        _ = ctrl_c => {},
        _ = term => {},
    }
}
