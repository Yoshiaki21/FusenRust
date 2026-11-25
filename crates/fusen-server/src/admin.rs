//! 管理画面（docs/decisions.md D-016）。
//!
//! - `FUSEN_ADMIN_LISTEN` を設定したときだけ、同期の API とは別のポートで待ち受ける（例 9010。Caddy では中継しない）。
//!   使うときだけ有効にして、終わったら設定を戻して作り直す運用を想定している
//! - `FUSEN_ADMIN_PASSWORD` のパスワード画面でログインし、セッションのクッキー（HttpOnly・SameSite=Strict）で保つ。
//!   セッションはメモリだけに持つので、サーバーを再起動するとログアウトする
//! - 変更の操作（POST）は、セッションごとの確認用の値（CSRF トークン）を確かめる
//! - 新しく発行したトークンは、発行直後の画面に 1 回だけ表示する（URL には載せない）
//! - 画面はサーバー側で HTML を組み立てる（JavaScript は確認とコピーのボタンだけ）

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::Router;
use axum::extract::{Form, Path, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::middleware;
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use chrono::{DateTime, Local, Utc};
use serde::Deserialize;
use sha2::{Digest, Sha256};

use crate::{AppState, Db, db};

/// セッションのクッキーの名前
const COOKIE: &str = "fusen_admin";
/// 操作がなければこの時間でログアウトする
const SESSION_TTL: Duration = Duration::from_secs(8 * 60 * 60);
/// パスワードの最短の長さ
pub const MIN_PASSWORD_LEN: usize = 8;
/// 端末名の最大の長さ（文字数）
const MAX_DEVICE_NAME: usize = 64;

/// 管理画面の設定
#[derive(Clone)]
pub struct AdminConfig {
    pub listen: SocketAddr,
    pub password: String,
}

impl std::fmt::Debug for AdminConfig {
    // パスワードをログに出さない
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AdminConfig")
            .field("listen", &self.listen)
            .finish_non_exhaustive()
    }
}

impl AdminConfig {
    /// 環境変数の値から作る。待ち受けるアドレスが空なら管理画面は無効（`None`）。
    /// 有効なのにパスワードがない・短いときはエラー（パスワードなしでは起動しない）
    pub fn from_values(
        listen: Option<&str>,
        password: Option<&str>,
    ) -> anyhow::Result<Option<Self>> {
        let Some(listen) = listen.map(str::trim).filter(|s| !s.is_empty()) else {
            return Ok(None);
        };
        let listen: SocketAddr = listen.parse().map_err(|e| {
            anyhow::anyhow!("FUSEN_ADMIN_LISTEN が正しくありません（{listen}）: {e}")
        })?;
        let password = password.unwrap_or_default();
        if password.trim().is_empty() {
            anyhow::bail!("管理画面を有効にするには FUSEN_ADMIN_PASSWORD を設定してください");
        }
        if password.chars().count() < MIN_PASSWORD_LEN {
            anyhow::bail!("FUSEN_ADMIN_PASSWORD は {MIN_PASSWORD_LEN} 文字以上にしてください");
        }
        Ok(Some(Self {
            listen,
            password: password.to_string(),
        }))
    }
}

/// 管理画面の状態
#[derive(Clone)]
pub struct AdminState {
    db: Db,
    data_dir: PathBuf,
    password_hash: [u8; 32],
    sessions: Arc<Mutex<HashMap<String, Session>>>,
}

struct Session {
    csrf: String,
    expires: Instant,
    /// 次に画面を出したときに 1 度だけ表示するお知らせ
    flash: Option<Flash>,
}

enum Flash {
    Info(String),
    Error(String),
    /// 発行したトークン（この 1 回だけ表示する）
    NewToken {
        device: String,
        token: String,
    },
}

impl AdminState {
    pub fn new(app: &AppState, data_dir: PathBuf, password: &str) -> Self {
        Self {
            db: app.db.clone(),
            data_dir,
            password_hash: Sha256::digest(password.as_bytes()).into(),
            sessions: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    fn sessions(&self) -> std::sync::MutexGuard<'_, HashMap<String, Session>> {
        self.sessions.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// クッキーのセッションが有効なら (セッション ID, CSRF トークン) を返し、期限を延ばす
    fn session(&self, headers: &HeaderMap) -> Option<(String, String)> {
        let sid = cookie_value(headers, COOKIE)?;
        let mut sessions = self.sessions();
        let now = Instant::now();
        sessions.retain(|_, s| s.expires > now);
        let s = sessions.get_mut(&sid)?;
        s.expires = now + SESSION_TTL;
        Some((sid, s.csrf.clone()))
    }

    /// 変更の操作: セッションと CSRF トークンを確かめる
    fn authorize(&self, headers: &HeaderMap, csrf: &str) -> Result<String, Box<Response>> {
        let Some((sid, expected)) = self.session(headers) else {
            return Err(Box::new(Redirect::to("/login").into_response()));
        };
        if !constant_time_eq(expected.as_bytes(), csrf.as_bytes()) {
            return Err(Box::new(
                (
                    StatusCode::FORBIDDEN,
                    "確認用の値が正しくありません。画面を読み込み直してください",
                )
                    .into_response(),
            ));
        }
        Ok(sid)
    }

    fn set_flash(&self, sid: &str, flash: Flash) {
        if let Some(s) = self.sessions().get_mut(sid) {
            s.flash = Some(flash);
        }
    }

    fn take_flash(&self, sid: &str) -> Option<Flash> {
        self.sessions().get_mut(sid).and_then(|s| s.flash.take())
    }

    /// DB の処理をブロッキング用のスレッドで行う
    async fn blocking<R: Send + 'static>(
        &self,
        f: impl FnOnce(&Db, &std::path::Path) -> anyhow::Result<R> + Send + 'static,
    ) -> anyhow::Result<R> {
        let db = self.db.clone();
        let dir = self.data_dir.clone();
        tokio::task::spawn_blocking(move || f(&db, &dir))
            .await
            .map_err(|e| anyhow::anyhow!("内部の処理が中断しました: {e}"))?
    }
}

/// 管理画面のルーティング
pub fn router(state: AdminState) -> Router {
    Router::new()
        .route("/", get(dashboard))
        .route("/login", get(login_page).post(login))
        .route("/logout", post(logout))
        .route("/tokens", post(create_token))
        .route("/tokens/{id}/revoke", post(revoke_token))
        .route("/tokens/{id}/rename", post(rename_token))
        .route("/tokens/{id}/delete", post(delete_token))
        .layer(middleware::map_response(security_headers))
        .with_state(state)
}

/// すべての応答に付けるヘッダー（キャッシュさせない・埋め込ませない・外部の読み込みを禁止）
async fn security_headers(mut resp: Response) -> Response {
    let h = resp.headers_mut();
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    h.insert(header::X_FRAME_OPTIONS, HeaderValue::from_static("DENY"));
    h.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("no-referrer"),
    );
    h.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    h.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(
            "default-src 'none'; style-src 'unsafe-inline'; script-src 'unsafe-inline'; \
             form-action 'self'; frame-ancestors 'none'; base-uri 'none'",
        ),
    );
    resp
}

// ----------------------------------------------------------------------
// ログイン
// ----------------------------------------------------------------------

#[derive(Deserialize)]
struct LoginForm {
    password: String,
}

#[derive(Deserialize)]
struct CsrfForm {
    csrf: String,
}

#[derive(Deserialize)]
struct DeviceForm {
    csrf: String,
    device: String,
}

async fn login_page(State(st): State<AdminState>, headers: HeaderMap) -> Response {
    if st.session(&headers).is_some() {
        return Redirect::to("/").into_response();
    }
    Html(render_login(None)).into_response()
}

async fn login(State(st): State<AdminState>, Form(form): Form<LoginForm>) -> Response {
    let hash: [u8; 32] = Sha256::digest(form.password.as_bytes()).into();
    if !constant_time_eq(&hash, &st.password_hash) {
        tracing::warn!("管理画面: ログインに失敗しました");
        // 総当たりを遅くする
        tokio::time::sleep(Duration::from_secs(1)).await;
        return (
            StatusCode::UNAUTHORIZED,
            Html(render_login(Some("パスワードが違います"))),
        )
            .into_response();
    }
    let (Ok(sid), Ok(csrf)) = (random_hex(), random_hex()) else {
        return (StatusCode::INTERNAL_SERVER_ERROR, "乱数を作れません").into_response();
    };
    st.sessions().insert(
        sid.clone(),
        Session {
            csrf,
            expires: Instant::now() + SESSION_TTL,
            flash: None,
        },
    );
    tracing::info!("管理画面: ログインしました");
    let cookie = format!(
        "{COOKIE}={sid}; Path=/; HttpOnly; SameSite=Strict; Max-Age={}",
        SESSION_TTL.as_secs()
    );
    let mut resp = Redirect::to("/").into_response();
    if let Ok(v) = HeaderValue::from_str(&cookie) {
        resp.headers_mut().insert(header::SET_COOKIE, v);
    }
    resp
}

async fn logout(
    State(st): State<AdminState>,
    headers: HeaderMap,
    Form(form): Form<CsrfForm>,
) -> Response {
    if let Ok(sid) = st.authorize(&headers, &form.csrf) {
        st.sessions().remove(&sid);
    }
    let mut resp = Redirect::to("/login").into_response();
    resp.headers_mut().insert(
        header::SET_COOKIE,
        HeaderValue::from_static("fusen_admin=; Path=/; HttpOnly; SameSite=Strict; Max-Age=0"),
    );
    resp
}

// ----------------------------------------------------------------------
// 画面と操作
// ----------------------------------------------------------------------

async fn dashboard(State(st): State<AdminState>, headers: HeaderMap) -> Response {
    let Some((sid, csrf)) = st.session(&headers) else {
        return Redirect::to("/login").into_response();
    };
    let flash = st.take_flash(&sid);
    let data = st
        .blocking(|db, _| {
            db.with(|c| -> Result<_, crate::ApiError> {
                Ok((db::token_details(c)?, db::purge_waiting(c)?, db::usage(c)?))
            })
            .map_err(|e| anyhow::anyhow!("{e}"))
        })
        .await;
    match data {
        Ok((tokens, waiting, usage)) => {
            Html(render_dashboard(&csrf, flash, &tokens, &waiting, &usage)).into_response()
        }
        Err(e) => {
            tracing::error!("管理画面: {e:#}");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("読み込めません: {e}"),
            )
                .into_response()
        }
    }
}

/// 端末名を整える（前後の空白を除き、空・長すぎ・制御文字はエラー）
fn clean_device_name(name: &str) -> Result<String, String> {
    let name = name.trim();
    if name.is_empty() {
        return Err("端末名を入力してください".into());
    }
    if name.chars().count() > MAX_DEVICE_NAME {
        return Err(format!("端末名は {MAX_DEVICE_NAME} 文字までにしてください"));
    }
    if name.chars().any(char::is_control) {
        return Err("端末名に制御文字は使えません".into());
    }
    Ok(name.to_string())
}

async fn create_token(
    State(st): State<AdminState>,
    headers: HeaderMap,
    Form(form): Form<DeviceForm>,
) -> Response {
    let sid = match st.authorize(&headers, &form.csrf) {
        Ok(s) => s,
        Err(r) => return *r,
    };
    let flash = match clean_device_name(&form.device) {
        Err(msg) => Flash::Error(msg),
        Ok(device) => {
            let d = device.clone();
            match st
                .blocking(move |db, _| db.with(|c| db::create_token(c, &d)))
                .await
            {
                Ok((id, token)) => {
                    tracing::info!("管理画面: トークンを発行しました（{device}、{id}）");
                    Flash::NewToken { device, token }
                }
                Err(e) => Flash::Error(format!("発行できません: {e}")),
            }
        }
    };
    st.set_flash(&sid, flash);
    Redirect::to("/").into_response()
}

async fn revoke_token(
    State(st): State<AdminState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Form(form): Form<CsrfForm>,
) -> Response {
    let sid = match st.authorize(&headers, &form.csrf) {
        Ok(s) => s,
        Err(r) => return *r,
    };
    if uuid::Uuid::parse_str(&id).is_err() {
        return (StatusCode::BAD_REQUEST, "トークンの ID が正しくありません").into_response();
    }
    let i = id.clone();
    let flash = match st
        .blocking(move |db, dir| crate::revoke_and_purge(db, dir, &i))
        .await
    {
        Ok(Some(count)) => {
            tracing::info!("管理画面: トークンを無効にしました（{id}）");
            let mut msg = "トークンを無効にしました".to_string();
            if count > 0 {
                msg.push_str(&format!(
                    "。全端末で削除済みになった付箋 {count} 件を完全に削除しました"
                ));
            }
            Flash::Info(msg)
        }
        Ok(None) => Flash::Error("有効なトークンが見つかりません".into()),
        Err(e) => Flash::Error(format!("無効にできません: {e}")),
    };
    st.set_flash(&sid, flash);
    Redirect::to("/").into_response()
}

async fn rename_token(
    State(st): State<AdminState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Form(form): Form<DeviceForm>,
) -> Response {
    let sid = match st.authorize(&headers, &form.csrf) {
        Ok(s) => s,
        Err(r) => return *r,
    };
    let flash = match clean_device_name(&form.device) {
        Err(msg) => Flash::Error(msg),
        Ok(device) => {
            let (i, d) = (id.clone(), device.clone());
            match st
                .blocking(move |db, _| {
                    db.with(|c| db::rename_token(c, &i, &d))
                        .map_err(|e| anyhow::anyhow!("{e}"))
                })
                .await
            {
                Ok(true) => {
                    tracing::info!("管理画面: 端末名を変えました（{id} → {device}）");
                    Flash::Info(format!("端末名を「{device}」に変えました"))
                }
                Ok(false) => Flash::Error("トークンが見つかりません".into()),
                Err(e) => Flash::Error(format!("変えられません: {e}")),
            }
        }
    };
    st.set_flash(&sid, flash);
    Redirect::to("/").into_response()
}

async fn delete_token(
    State(st): State<AdminState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Form(form): Form<CsrfForm>,
) -> Response {
    let sid = match st.authorize(&headers, &form.csrf) {
        Ok(s) => s,
        Err(r) => return *r,
    };
    let i = id.clone();
    let flash = match st
        .blocking(move |db, _| {
            db.with(|c| db::delete_revoked_token(c, &i))
                .map_err(|e| anyhow::anyhow!("{e}"))
        })
        .await
    {
        Ok(true) => {
            tracing::info!("管理画面: 無効のトークンを一覧から消しました（{id}）");
            Flash::Info("無効のトークンを一覧から消しました".into())
        }
        Ok(false) => Flash::Error("消せるのは無効にしたトークンだけです".into()),
        Err(e) => Flash::Error(format!("消せません: {e}")),
    };
    st.set_flash(&sid, flash);
    Redirect::to("/").into_response()
}

// ----------------------------------------------------------------------
// HTML
// ----------------------------------------------------------------------

const STYLE: &str = r#"
body { font-family: system-ui, "Noto Sans CJK JP", "Yu Gothic UI", sans-serif; margin: 0; background: #f5f5f4; color: #1c1917; }
header { background: #292524; color: #fafaf9; padding: 12px 24px; display: flex; align-items: center; gap: 16px; }
header h1 { font-size: 18px; margin: 0; flex: 1; }
main { max-width: 1100px; margin: 0 auto; padding: 16px 24px 48px; }
section { background: #fff; border: 1px solid #e7e5e4; border-radius: 8px; padding: 16px 20px; margin-top: 16px; }
h2 { font-size: 16px; margin: 0 0 12px; }
table { border-collapse: collapse; width: 100%; font-size: 14px; }
th, td { text-align: left; padding: 8px 6px; border-bottom: 1px solid #e7e5e4; vertical-align: top; }
th { color: #57534e; font-weight: 600; }
.muted { color: #78716c; font-size: 12px; }
.mono { font-family: ui-monospace, monospace; }
.ok { color: #15803d; font-weight: 600; }
.ng { color: #a8a29e; }
.flash { padding: 10px 14px; border-radius: 6px; margin-top: 16px; }
.info { background: #ecfdf5; border: 1px solid #a7f3d0; }
.error { background: #fef2f2; border: 1px solid #fecaca; }
.token { background: #fffbeb; border: 1px solid #fcd34d; }
.token code { display: block; font-size: 15px; padding: 8px; background: #fff; border: 1px solid #e7e5e4; border-radius: 4px; margin: 8px 0; word-break: break-all; }
input[type=text], input[type=password] { padding: 6px 8px; border: 1px solid #d6d3d1; border-radius: 4px; font-size: 14px; }
button { padding: 6px 12px; border: 1px solid #d6d3d1; background: #fafaf9; border-radius: 4px; cursor: pointer; font-size: 13px; }
button.primary { background: #2563eb; border-color: #2563eb; color: #fff; }
button.danger { color: #b91c1c; border-color: #fca5a5; }
header button { background: transparent; color: #fafaf9; border-color: #57534e; }
form.inline { display: inline; }
details summary { cursor: pointer; color: #2563eb; font-size: 12px; }
.grid { display: grid; grid-template-columns: repeat(auto-fill, minmax(160px, 1fr)); gap: 12px; }
.stat { border: 1px solid #e7e5e4; border-radius: 6px; padding: 10px 12px; }
.stat b { display: block; font-size: 20px; }
.login { max-width: 360px; margin: 80px auto; }
"#;

fn page(title: &str, header_extra: &str, body: &str) -> String {
    format!(
        r#"<!doctype html>
<html lang="ja"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width, initial-scale=1">
<title>{title}</title><style>{STYLE}</style></head>
<body><header><h1>FusenRust 管理画面</h1>{header_extra}</header><main>{body}</main></body></html>"#,
        title = esc(title)
    )
}

fn render_login(error: Option<&str>) -> String {
    let err = error
        .map(|e| format!(r#"<div class="flash error">{}</div>"#, esc(e)))
        .unwrap_or_default();
    page(
        "ログイン - FusenRust 管理画面",
        "",
        &format!(
            r#"<section class="login"><h2>ログイン</h2>{err}
<form method="post" action="/login">
<p><input type="password" name="password" placeholder="管理用のパスワード" autofocus required style="width:100%;box-sizing:border-box"></p>
<p><button class="primary" type="submit">ログイン</button></p>
</form>
<p class="muted">パスワードはサーバーの設定 FUSEN_ADMIN_PASSWORD です。</p></section>"#
        ),
    )
}

fn render_dashboard(
    csrf: &str,
    flash: Option<Flash>,
    tokens: &[db::TokenDetail],
    waiting: &[db::PurgeWaiting],
    usage: &db::Usage,
) -> String {
    let csrf_input = format!(r#"<input type="hidden" name="csrf" value="{}">"#, esc(csrf));
    let header = format!(
        r#"<form class="inline" method="post" action="/logout">{csrf_input}<button type="submit">ログアウト</button></form>"#
    );

    let flash = match flash {
        None => String::new(),
        Some(Flash::Info(m)) => format!(r#"<div class="flash info">{}</div>"#, esc(&m)),
        Some(Flash::Error(m)) => format!(r#"<div class="flash error">{}</div>"#, esc(&m)),
        Some(Flash::NewToken { device, token }) => format!(
            r#"<div class="flash token"><b>「{device}」のトークンを発行しました。</b>
このトークンはこの画面でしか表示されません。今すぐコピーして、端末の管理画面の「設定」に貼ってください。
<code id="new-token">{token}</code>
<button type="button" onclick="navigator.clipboard.writeText(document.getElementById('new-token').textContent).then(()=>{{this.textContent='コピーしました'}})">コピー</button></div>"#,
            device = esc(&device),
            token = esc(&token)
        ),
    };

    let mut rows = String::new();
    for t in tokens {
        let i = &t.info;
        let id = esc(&i.token_id);
        let active = i.revoked_at.is_none();
        let state = if active {
            r#"<span class="ok">有効</span>"#.to_string()
        } else {
            format!(
                r#"<span class="ng">無効</span><div class="muted">{}</div>"#,
                esc(&fmt_time(&i.revoked_at.clone().unwrap_or_default()))
            )
        };
        let last_used = match &i.last_used_at {
            Some(t) => format!(
                r#"{}<div class="muted">{}</div>"#,
                esc(&relative(t)),
                esc(&fmt_time(t))
            ),
            None => r#"<span class="muted">未使用</span>"#.to_string(),
        };
        let action = if active {
            format!(
                r#"<form class="inline" method="post" action="/tokens/{id}/revoke" onsubmit="return confirm('「{name}」のトークンを無効にしますか？\nこの端末は同期できなくなります（元に戻せません）。')">{csrf_input}<button class="danger" type="submit">無効にする</button></form>"#,
                name = esc_js(&i.device_name)
            )
        } else {
            format!(
                r#"<form class="inline" method="post" action="/tokens/{id}/delete" onsubmit="return confirm('無効のトークン「{name}」を一覧から消しますか？')">{csrf_input}<button type="submit">一覧から消す</button></form>"#,
                name = esc_js(&i.device_name)
            )
        };
        rows.push_str(&format!(
            r#"<tr><td><b>{name}</b>
<details><summary>端末名を変える</summary><form method="post" action="/tokens/{id}/rename">{csrf_input}<input type="text" name="device" value="{name}" maxlength="{MAX_DEVICE_NAME}" required> <button type="submit">変更</button></form></details></td>
<td class="mono muted" title="{id}">{short}…</td><td>{state}</td><td>{created}</td><td>{last_used}</td><td>{revs}</td><td>{action}</td></tr>"#,
            name = esc(&i.device_name),
            short = esc(&i.token_id.chars().take(13).collect::<String>()),
            created = esc(&fmt_time(&i.created_at)),
            revs = t.revisions,
        ));
    }
    if tokens.is_empty() {
        rows.push_str(r#"<tr><td colspan="7" class="muted">トークンはありません</td></tr>"#);
    }

    let mut purge_rows = String::new();
    for w in waiting {
        purge_rows.push_str(&format!(
            r#"<tr><td>{title}<div class="mono muted">{id}</div></td><td>{voted}</td><td>{waiting}</td></tr>"#,
            title = esc(&w.title),
            id = esc(&w.note_id),
            voted = esc(&w.voted.join("、")),
            waiting = if w.waiting.is_empty() {
                "（なし）".to_string()
            } else {
                esc(&w.waiting.join("、"))
            },
        ));
    }
    let purge_section = if waiting.is_empty() {
        r#"<p class="muted">完全削除の票を待っている付箋はありません。</p>"#.to_string()
    } else {
        format!(
            r#"<p class="muted">まだ削除していない端末でも完全に削除するか、使わなくなった端末のトークンを無効にすると、サーバーから削除されます。</p>
<table><tr><th>付箋</th><th>削除済みの端末</th><th>まだ削除していない端末</th></tr>{purge_rows}</table>"#
        )
    };

    let stat = |label: &str, value: String| {
        format!(r#"<div class="stat"><span class="muted">{label}</span><b>{value}</b></div>"#)
    };
    let usage_html = [
        stat("付箋", usage.notes.to_string()),
        stat("ごみ箱", usage.trashed.to_string()),
        stat("版", usage.revisions.to_string()),
        stat("添付", usage.attachments.to_string()),
        stat(
            "添付の実体",
            format!("{}（{}）", usage.blobs, human_bytes(usage.blob_bytes)),
        ),
        stat("完全に削除した付箋", usage.purged.to_string()),
        stat("最後の受付番号", usage.last_server_seq.to_string()),
    ]
    .concat();

    page(
        "FusenRust 管理画面",
        &header,
        &format!(
            r#"{flash}
<section><h2>トークンを発行する</h2>
<form method="post" action="/tokens">{csrf_input}<input type="text" name="device" placeholder="端末名（例: cachyos-main）" maxlength="{MAX_DEVICE_NAME}" required> <button class="primary" type="submit">発行</button></form>
<p class="muted">トークンは発行した直後に 1 回だけ表示されます（サーバーにはハッシュだけを保存します）。</p></section>
<section><h2>トークン</h2>
<table><tr><th>端末名</th><th>ID</th><th>状態</th><th>発行</th><th>最終使用</th><th>受け付けた版</th><th></th></tr>{rows}</table>
<p class="muted">完全削除は「有効」で一度でも使われたトークン全部の票がそろうと行われます。使わなくなった端末は無効にしてください。</p></section>
<section><h2>完全削除の待ち状況</h2>{purge_section}</section>
<section><h2>使用量</h2><div class="grid">{usage_html}</div></section>"#
        ),
    )
}

// ----------------------------------------------------------------------
// 小さな道具
// ----------------------------------------------------------------------

/// HTML に埋め込む文字列をエスケープする
fn esc(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(c),
        }
    }
    out
}

/// HTML の属性の中の JavaScript の文字列に埋め込む（`\` と `'` を JavaScript 側でもエスケープしてから HTML のエスケープ）
fn esc_js(s: &str) -> String {
    esc(&s.replace('\\', "\\\\").replace('\'', "\\'"))
}

/// RFC 3339 の時刻を、この環境の時刻で「2026/11/25 23:22:40」の形にする
fn fmt_time(s: &str) -> String {
    match DateTime::parse_from_rfc3339(s) {
        Ok(t) => t
            .with_timezone(&Local)
            .format("%Y/%m/%d %H:%M:%S")
            .to_string(),
        Err(_) => s.to_string(),
    }
}

/// 「3 分前」のような表示
fn relative(s: &str) -> String {
    let Ok(t) = DateTime::parse_from_rfc3339(s) else {
        return s.to_string();
    };
    let secs = (Utc::now() - t.with_timezone(&Utc)).num_seconds().max(0);
    match secs {
        0..60 => "1 分以内".into(),
        60..3600 => format!("{} 分前", secs / 60),
        3600..86400 => format!("{} 時間前", secs / 3600),
        _ => format!("{} 日前", secs / 86400),
    }
}

fn human_bytes(n: i64) -> String {
    const UNITS: [&str; 4] = ["B", "KB", "MB", "GB"];
    let mut v = n as f64;
    let mut u = 0;
    while v >= 1024.0 && u < UNITS.len() - 1 {
        v /= 1024.0;
        u += 1;
    }
    if u == 0 {
        format!("{n} B")
    } else {
        format!("{v:.1} {}", UNITS[u])
    }
}

fn cookie_value(headers: &HeaderMap, name: &str) -> Option<String> {
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .filter_map(|kv| kv.trim().split_once('='))
        .find(|(k, _)| *k == name)
        .map(|(_, v)| v.to_string())
        .filter(|v| !v.is_empty())
}

fn random_hex() -> anyhow::Result<String> {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).map_err(|e| anyhow::anyhow!("乱数を作れません: {e}"))?;
    Ok(hex::encode(bytes))
}

/// 長さが同じなら、中身によらず同じ時間で比べる
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

#[cfg(test)]
mod tests {
    use super::{AdminConfig, constant_time_eq, esc, esc_js, relative};

    #[test]
    fn disabled_unless_listen_is_set() {
        assert!(
            AdminConfig::from_values(None, Some("password123"))
                .unwrap()
                .is_none()
        );
        assert!(
            AdminConfig::from_values(Some("  "), None)
                .unwrap()
                .is_none()
        );
        let c = AdminConfig::from_values(Some("0.0.0.0:9010"), Some("password123"))
            .unwrap()
            .unwrap();
        assert_eq!(c.listen.port(), 9010);
    }

    #[test]
    fn password_is_required_when_enabled() {
        assert!(AdminConfig::from_values(Some("127.0.0.1:9010"), None).is_err());
        assert!(AdminConfig::from_values(Some("127.0.0.1:9010"), Some("")).is_err());
        assert!(AdminConfig::from_values(Some("127.0.0.1:9010"), Some("short")).is_err());
        assert!(AdminConfig::from_values(Some("not an address"), Some("password123")).is_err());
    }

    #[test]
    fn escapes_html_and_js() {
        assert_eq!(
            esc(r#"<a href="x">'&'</a>"#),
            "&lt;a href=&quot;x&quot;&gt;&#39;&amp;&#39;&lt;/a&gt;"
        );
        assert_eq!(esc_js(r"it's\"), r"it\&#39;s\\");
    }

    #[test]
    fn compares_in_constant_time() {
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"abcd"));
    }

    #[test]
    fn relative_time() {
        let now = chrono::Utc::now();
        let s = (now - chrono::Duration::minutes(5)).to_rfc3339();
        assert_eq!(relative(&s), "5 分前");
    }
}
