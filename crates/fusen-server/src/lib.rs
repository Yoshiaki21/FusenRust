//! FusenRust の同期サーバー。
//!
//! - axum + SQLite。添付の実体は `<data_dir>/blobs/<sha256の先頭2文字>/<sha256>` に保存する
//! - 端末ごとのトークンによる Bearer 認証（トークンはハッシュだけを保存）
//! - API は `docs/spec.md` 6.3。型は `fusen-proto`
//! - TLS はリバースプロキシ（Caddy）が担当し、このサーバーは HTTP で待ち受ける
//! - 管理画面（[`admin`]）は設定したときだけ、別のポートで待ち受ける（D-016）

pub mod admin;
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
    /// 管理画面（`None` なら起動しない）
    pub admin: Option<admin::AdminConfig>,
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

/// トークンを無効にし、その端末の票を待っていた完全削除を処理する（コマンドと管理画面から使う。D-014）。
/// 無効にしたら `Some(完全に削除した付箋の数)`、当てはまる有効なトークンがなければ `None`
pub fn revoke_and_purge(
    db: &Db,
    data_dir: &std::path::Path,
    id_or_prefix: &str,
) -> anyhow::Result<Option<usize>> {
    if !db.with(|c| db::revoke_token(c, id_or_prefix))? {
        return Ok(None);
    }
    let (count, unused) = db
        .with(db::purge_ready_notes)
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    remove_blobs(&data_dir.join("blobs"), &unused);
    Ok(Some(count))
}

/// どこからも参照されなくなった blob のファイルを消す（完全削除の後。D-014）
pub fn remove_blobs(blobs_dir: &std::path::Path, shas: &[String]) {
    for sha in shas.iter().filter(|s| is_valid_sha256(s)) {
        let path = blobs_dir.join(&sha[..2]).join(sha);
        match std::fs::remove_file(&path) {
            Ok(()) => tracing::info!("添付の実体を消しました: {sha}"),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => tracing::warn!("添付の実体を消せません（{sha}）: {e}"),
        }
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

    // 管理画面（設定したときだけ。同期の API とは別のポート）
    let admin_task = match &config.admin {
        None => {
            tracing::info!("管理画面は無効です（FUSEN_ADMIN_LISTEN が未設定）");
            None
        }
        Some(a) => {
            let admin_listener = tokio::net::TcpListener::bind(a.listen)
                .await
                .with_context(|| format!("管理画面を {} で待ち受けできません", a.listen))?;
            let admin_state = admin::AdminState::new(&state, config.data_dir.clone(), &a.password);
            let listen = a.listen;
            // 使うときだけ有効にする運用なので、無効にし忘れに気づけるよう目立つ WARN で出す
            tracing::warn!(
                "管理画面が有効です: http://{listen}（使い終わったら FUSEN_ADMIN_LISTEN を外して起動し直してください）"
            );
            let reminder = tokio::spawn(admin_reminder(listen));
            Some(tokio::spawn(async move {
                let result = axum::serve(admin_listener, admin::router(admin_state))
                    .with_graceful_shutdown(shutdown_signal())
                    .await;
                reminder.abort();
                tracing::info!("管理画面を停止しました: http://{listen}");
                result
            }))
        }
    };

    tracing::info!(
        "fusen-server を起動しました: http://{}（データ {:?}、アップロード上限 {} バイト）",
        config.listen,
        config.data_dir,
        config.max_upload_bytes
    );
    axum::serve(listener, router(state))
        .with_graceful_shutdown(shutdown_signal())
        .await?;
    if let Some(task) = admin_task {
        task.await??;
    }
    tracing::info!("fusen-server を終了しました");
    Ok(())
}

/// 管理画面が有効な間、1 時間ごとに「まだ有効です」とログに出す（無効にし忘れ対策。D-016）
async fn admin_reminder(listen: SocketAddr) {
    const INTERVAL: std::time::Duration = std::time::Duration::from_secs(60 * 60);
    let mut hours = 0u64;
    loop {
        tokio::time::sleep(INTERVAL).await;
        hours += 1;
        tracing::warn!(
            "管理画面が有効のままです: http://{listen}（有効にしてから {hours} 時間。使い終わったら無効にしてください）"
        );
    }
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
