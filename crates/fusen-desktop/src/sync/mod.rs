//! 同期サービス（`docs/spec.md` 6.4）。
//!
//! gpui の実行器とは別に、同期専用のスレッドで tokio を動かす（reqwest / WebSocket が tokio を必要とするため）。
//! gpui 側とはチャンネルでやり取りする:
//! - gpui → 同期: [`SyncService`] のメソッド（今すぐ同期、設定の変更、添付の取得、終了）
//! - 同期 → gpui: [`SyncEvent`]（状態、他の端末からの変更、添付の取得完了）
//!
//! 流れ:
//! 1. 接続したら差分を取り込み、outbox の版を送る（`fusen_core::sync::sync_once`）
//! 2. 本文中に表示する画像（PNG / JPEG / JXL）の実体はすぐ取得する。それ以外は開く・保存するときに取得する
//! 3. WebSocket で `changed` を受けたら差分を取りに行く。付箋の変更でも同期する（[`SyncService::kick`]）
//! 4. 切断されたら、間隔を 1 秒から 60 秒まで倍々に延ばしながら再接続する

pub mod http;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use async_channel::{Receiver, Sender};
use fusen_core::{Store, markdown};
use fusen_proto::WsMessage;
use futures_util::StreamExt as _;
use tokio_tungstenite::tungstenite::{self, client::IntoClientRequest as _};
use uuid::Uuid;

use http::HttpTransport;
pub use http::ServerConfig;

/// 再接続の間隔の最小・最大
const BACKOFF_MIN: Duration = Duration::from_secs(1);
const BACKOFF_MAX: Duration = Duration::from_secs(60);
/// 接続中に未送信の版が残っていないか確かめる間隔（変更の通知を取りこぼしたときの保険）
const SAFETY_INTERVAL: Duration = Duration::from_secs(20);

/// 接続の状態
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SyncState {
    /// サーバーの URL・トークンが設定されていない
    NotConfigured,
    Connecting,
    Connected,
    /// つながらない（理由）。間隔を空けて再接続する
    Offline(String),
}

/// 同期の状態（管理画面・トレイに表示する）
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SyncStatus {
    pub state: SyncState,
    /// 未送信の版の数
    pub pending: usize,
}

impl SyncStatus {
    /// 表示用の文
    pub fn label(&self) -> String {
        let state = match &self.state {
            SyncState::NotConfigured => "同期: 未設定".to_string(),
            SyncState::Connecting => "同期: 接続中…".to_string(),
            SyncState::Connected => "同期: 接続済み".to_string(),
            SyncState::Offline(reason) => format!("同期: オフライン（{reason}）"),
        };
        if self.pending > 0 {
            format!("{state}・未送信 {} 件", self.pending)
        } else {
            state
        }
    }
}

/// 同期から gpui への通知
#[derive(Clone, Debug)]
pub enum SyncEvent {
    Status(SyncStatus),
    /// 他の端末からの変更を取り込んだ（`new` はこの端末で初めて見る付箋）。
    /// 送信で番号が確定した付箋も `changed` に含める（表示中の番号を更新するため）
    RemoteChanges {
        changed: Vec<Uuid>,
        new: Vec<Uuid>,
    },
    /// 添付の実体を取得した（画像を描き直す）
    BlobsFetched,
}

/// 添付の取得の結果
pub type FetchResult = Result<PathBuf, String>;

enum SyncCommand {
    Kick,
    Reconfigure(Option<ServerConfig>),
    Fetch(Uuid, Sender<FetchResult>),
    Shutdown,
}

/// 同期サービスへの窓口（gpui 側が持つ）
pub struct SyncService {
    tx: Sender<SyncCommand>,
}

impl SyncService {
    /// 同期のスレッドを始める
    pub fn start(
        store: Arc<Store>,
        config: Option<ServerConfig>,
        max_blob_bytes: u64,
        events: Sender<SyncEvent>,
    ) -> Self {
        let (tx, rx) = async_channel::unbounded();
        let spawned = std::thread::Builder::new()
            .name("fusen-sync".into())
            .spawn(move || {
                let rt = match tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                {
                    Ok(rt) => rt,
                    Err(e) => {
                        tracing::error!("同期用の実行環境を作れません: {e}");
                        return;
                    }
                };
                rt.block_on(
                    Worker {
                        store,
                        max_blob_bytes,
                        commands: rx,
                        events,
                    }
                    .run(config),
                );
            });
        if let Err(e) = spawned {
            tracing::error!("同期のスレッドを作れません: {e}");
        }
        Self { tx }
    }

    /// 今すぐ同期する（付箋を変更したとき）
    pub fn kick(&self) {
        let _ = self.tx.try_send(SyncCommand::Kick);
    }

    /// 接続先を変える（設定の保存時）。`None` なら同期を止める
    pub fn reconfigure(&self, config: Option<ServerConfig>) {
        let _ = self.tx.try_send(SyncCommand::Reconfigure(config));
    }

    /// 添付の実体を取得する（開く・保存するときに、この端末にまだない場合）
    pub fn fetch(&self, attachment_id: Uuid) -> Receiver<FetchResult> {
        let (tx, rx) = async_channel::bounded(1);
        if self
            .tx
            .try_send(SyncCommand::Fetch(attachment_id, tx.clone()))
            .is_err()
        {
            let _ = tx.try_send(Err("同期が動いていません".into()));
        }
        rx
    }

    pub fn shutdown(&self) {
        let _ = self.tx.try_send(SyncCommand::Shutdown);
    }
}

/// 接続が終わった理由
enum SessionEnd {
    Reconfigure(Option<ServerConfig>),
    Shutdown,
    Disconnected(String),
}

struct Worker {
    store: Arc<Store>,
    max_blob_bytes: u64,
    commands: Receiver<SyncCommand>,
    events: Sender<SyncEvent>,
}

impl Worker {
    fn status(&self, state: SyncState) {
        let pending = self.store.outbox_len().unwrap_or(0);
        let _ = self
            .events
            .try_send(SyncEvent::Status(SyncStatus { state, pending }));
    }

    async fn run(self, mut config: Option<ServerConfig>) {
        let mut backoff = BACKOFF_MIN;
        loop {
            let Some(cfg) = config.clone() else {
                self.status(SyncState::NotConfigured);
                match self.commands.recv().await {
                    Ok(SyncCommand::Reconfigure(c)) => config = c,
                    Ok(SyncCommand::Fetch(_, reply)) => {
                        let _ = reply.try_send(Err("同期サーバーが設定されていません".into()));
                    }
                    Ok(SyncCommand::Kick) => {}
                    Ok(SyncCommand::Shutdown) | Err(_) => return,
                }
                continue;
            };

            match self.session(&cfg, &mut backoff).await {
                SessionEnd::Reconfigure(c) => {
                    config = c;
                    backoff = BACKOFF_MIN;
                }
                SessionEnd::Shutdown => return,
                SessionEnd::Disconnected(reason) => {
                    tracing::warn!(
                        "同期できません（{}秒後に再接続）: {reason}",
                        backoff.as_secs()
                    );
                    self.status(SyncState::Offline(reason));
                    // 待つ間も命令は受け付ける。設定の変更・付箋の変更があればすぐ再接続する
                    tokio::select! {
                        _ = tokio::time::sleep(backoff) => {}
                        cmd = self.commands.recv() => match cmd {
                            Ok(SyncCommand::Reconfigure(c)) => {
                                config = c;
                                backoff = BACKOFF_MIN;
                                continue;
                            }
                            Ok(SyncCommand::Fetch(_, reply)) => {
                                let _ = reply.try_send(Err("サーバーにつながりません".into()));
                            }
                            Ok(SyncCommand::Kick) => {}
                            Ok(SyncCommand::Shutdown) | Err(_) => return,
                        }
                    }
                    backoff = (backoff * 2).min(BACKOFF_MAX);
                }
            }
        }
    }

    /// 1 回の接続（つながっている間ずっと）
    async fn session(&self, cfg: &ServerConfig, backoff: &mut Duration) -> SessionEnd {
        self.status(SyncState::Connecting);
        let transport = match HttpTransport::new(cfg.clone()) {
            Ok(t) => t,
            Err(e) => return SessionEnd::Disconnected(e.to_string()),
        };
        if let Err(e) = self.sync_round(&transport).await {
            return SessionEnd::Disconnected(e);
        }

        let mut ws = match connect_ws(cfg).await {
            Ok(ws) => ws,
            Err(e) => return SessionEnd::Disconnected(format!("WebSocket: {e}")),
        };
        // 接続中に変更があったかもしれないので、もう一度差分を取る
        if let Err(e) = self.sync_round(&transport).await {
            return SessionEnd::Disconnected(e);
        }
        *backoff = BACKOFF_MIN;
        self.status(SyncState::Connected);
        tracing::info!("同期サーバーに接続しました: {}", cfg.base());

        let mut safety = tokio::time::interval(SAFETY_INTERVAL);
        safety.tick().await;
        loop {
            tokio::select! {
                msg = ws.next() => match msg {
                    Some(Ok(tungstenite::Message::Text(text))) => {
                        if let Ok(WsMessage::Changed { server_seq }) = serde_json::from_str(&text)
                            && server_seq > self.store.last_server_seq().unwrap_or(0)
                            && let Err(e) = self.sync_round(&transport).await
                        {
                            return SessionEnd::Disconnected(e);
                        }
                    }
                    Some(Ok(tungstenite::Message::Close(_))) | None => {
                        return SessionEnd::Disconnected("サーバーとの接続が切れました".into());
                    }
                    Some(Err(e)) => return SessionEnd::Disconnected(format!("WebSocket: {e}")),
                    // ping への pong は tungstenite が返す
                    Some(Ok(_)) => {}
                },
                cmd = self.commands.recv() => match cmd {
                    Ok(SyncCommand::Kick) => {
                        if let Err(e) = self.sync_round(&transport).await {
                            return SessionEnd::Disconnected(e);
                        }
                        self.status(SyncState::Connected);
                    }
                    Ok(SyncCommand::Fetch(id, reply)) => {
                        let r = self.fetch(&transport, id).await;
                        let _ = reply.try_send(r);
                    }
                    Ok(SyncCommand::Reconfigure(c)) => {
                        let _ = ws.close(None).await;
                        return SessionEnd::Reconfigure(c);
                    }
                    Ok(SyncCommand::Shutdown) | Err(_) => {
                        let _ = ws.close(None).await;
                        return SessionEnd::Shutdown;
                    }
                },
                _ = safety.tick() => {
                    if self.store.outbox_len().unwrap_or(0) > 0 {
                        if let Err(e) = self.sync_round(&transport).await {
                            return SessionEnd::Disconnected(e);
                        }
                        self.status(SyncState::Connected);
                    }
                }
            }
        }
    }

    /// 差分の取り込みと送信、画像の取得
    async fn sync_round(&self, transport: &HttpTransport) -> Result<(), String> {
        // 送信で番号が確定する付箋も、表示を更新する
        let mut changed: Vec<Uuid> = self
            .store
            .pending_revisions()
            .map_err(|e| e.to_string())?
            .into_iter()
            .map(|r| r.note_id)
            .collect();
        let report = fusen_core::sync::sync_once(&self.store, transport)
            .await
            .map_err(|e| e.to_string())?;
        for id in report.pulled.changed_notes {
            changed.push(id);
        }
        // サーバーで完全に削除済みだったので消した付箋（ウィンドウを閉じる）
        changed.extend(report.pushed.gone);
        changed.sort();
        changed.dedup();
        if !changed.is_empty() || !report.pulled.new_notes.is_empty() {
            let _ = self.events.try_send(SyncEvent::RemoteChanges {
                changed,
                new: report.pulled.new_notes,
            });
        }
        if !report.pushed.conflicts.is_empty() {
            tracing::info!("競合の印が付いた版: {:?}", report.pushed.conflicts);
        }

        // この端末に貼ってある付箋の、本文中に表示する画像はすぐ取得する
        // （貼っていない付箋の画像は、貼ったとき・管理画面で見たときに取得する。D-012）
        let mut fetched = false;
        for meta in self
            .store
            .missing_blobs_of_pasted()
            .map_err(|e| e.to_string())?
        {
            if !markdown::is_inline_image(&meta.mime) || meta.size > self.max_blob_bytes {
                continue;
            }
            match fusen_core::sync::fetch_blob(&self.store, transport, &meta, self.max_blob_bytes)
                .await
            {
                Ok(_) => fetched = true,
                Err(e) => tracing::warn!("画像を取得できません（{}）: {e}", meta.file_name),
            }
        }
        if fetched {
            let _ = self.events.try_send(SyncEvent::BlobsFetched);
        }
        Ok(())
    }

    async fn fetch(&self, transport: &HttpTransport, attachment_id: Uuid) -> FetchResult {
        let meta = self
            .store
            .attachment(attachment_id)
            .map_err(|e| e.to_string())?;
        // 開く・保存するために取得する添付は、この端末の添付の上限に関係なく取得する
        let path = fusen_core::sync::fetch_blob(&self.store, transport, &meta, u64::MAX)
            .await
            .map_err(|e| e.to_string())?;
        let _ = self.events.try_send(SyncEvent::BlobsFetched);
        Ok(path)
    }
}

type WsStream =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

/// トークン付きで WebSocket に接続する
async fn connect_ws(cfg: &ServerConfig) -> Result<WsStream, String> {
    let mut req = cfg
        .ws()
        .into_client_request()
        .map_err(|e| format!("URL が正しくありません: {e}"))?;
    let auth = format!("Bearer {}", cfg.token)
        .parse()
        .map_err(|_| "トークンに使えない文字が含まれています".to_string())?;
    req.headers_mut().insert("Authorization", auth);
    let (ws, _) = tokio::time::timeout(
        Duration::from_secs(15),
        tokio_tungstenite::connect_async(req),
    )
    .await
    .map_err(|_| "接続がタイムアウトしました".to_string())?
    .map_err(|e| match e {
        tungstenite::Error::Http(resp) if resp.status() == 401 => {
            "トークンが正しくありません（設定を確認してください）".to_string()
        }
        other => other.to_string(),
    })?;
    Ok(ws)
}
