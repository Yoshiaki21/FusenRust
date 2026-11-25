//! 管理画面の結合テスト（D-016）: ログイン・CSRF・発行（1 回だけ表示）・無効化・端末名の変更・一覧から消す

use fusen_server::admin::{AdminState, router};
use fusen_server::{AppState, Db, db};

const PASSWORD: &str = "test-password";

struct Admin {
    base: String,
    db: Db,
    client: reqwest::Client,
    _dir: tempfile::TempDir,
}

impl Admin {
    async fn start() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(dir.path()).unwrap();
        let app = AppState::new(db.clone(), dir.path(), 1024).unwrap();
        let state = AdminState::new(&app, dir.path().to_path_buf(), PASSWORD);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, router(state)).await.unwrap();
        });
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap();
        Self {
            base: format!("http://{addr}"),
            db,
            client,
            _dir: dir,
        }
    }

    async fn post(&self, path: &str, cookie: Option<&str>, body: &str) -> reqwest::Response {
        let mut req = self
            .client
            .post(format!("{}{path}", self.base))
            .header("content-type", "application/x-www-form-urlencoded")
            .body(body.to_string());
        if let Some(c) = cookie {
            req = req.header("cookie", c);
        }
        req.send().await.unwrap()
    }

    async fn get(&self, path: &str, cookie: Option<&str>) -> reqwest::Response {
        let mut req = self.client.get(format!("{}{path}", self.base));
        if let Some(c) = cookie {
            req = req.header("cookie", c);
        }
        req.send().await.unwrap()
    }

    /// ログインして (クッキー, CSRF トークン) を返す
    async fn login(&self) -> (String, String) {
        let resp = self
            .post("/login", None, &format!("password={PASSWORD}"))
            .await;
        assert_eq!(resp.status(), 303);
        let set_cookie = resp.headers()["set-cookie"].to_str().unwrap().to_string();
        assert!(set_cookie.contains("HttpOnly") && set_cookie.contains("SameSite=Strict"));
        let cookie = set_cookie.split(';').next().unwrap().to_string();
        let page = self.get("/", Some(&cookie)).await.text().await.unwrap();
        (cookie, csrf_of(&page))
    }
}

fn csrf_of(page: &str) -> String {
    let key = r#"name="csrf" value=""#;
    let start = page.find(key).unwrap() + key.len();
    page[start..start + 64].to_string()
}

#[tokio::test]
async fn login_is_required() {
    let a = Admin::start().await;
    let resp = a.get("/", None).await;
    assert_eq!(resp.status(), 303);
    assert_eq!(resp.headers()["location"], "/login");

    let resp = a.post("/login", None, "password=wrong-password").await;
    assert_eq!(resp.status(), 401);
    assert!(resp.headers().get("set-cookie").is_none());

    // 偽のクッキーでは入れない
    let resp = a.get("/", Some("fusen_admin=deadbeef")).await;
    assert_eq!(resp.status(), 303);

    let (cookie, _) = a.login().await;
    let resp = a.get("/", Some(&cookie)).await;
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.headers()["cache-control"], "no-store");
    assert_eq!(resp.headers()["x-frame-options"], "DENY");
}

#[tokio::test]
async fn create_token_is_shown_once_and_needs_csrf() {
    let a = Admin::start().await;
    let (cookie, csrf) = a.login().await;

    // CSRF トークンがなければ拒否
    let resp = a.post("/tokens", Some(&cookie), "csrf=x&device=pc").await;
    assert_eq!(resp.status(), 403);
    assert!(a.db.with(|c| db::list_tokens(c)).unwrap().is_empty());

    let resp = a
        .post(
            "/tokens",
            Some(&cookie),
            &format!("csrf={csrf}&device=new-pc"),
        )
        .await;
    assert_eq!(resp.status(), 303);
    let tokens = a.db.with(|c| db::list_tokens(c)).unwrap();
    assert_eq!(tokens.len(), 1);
    assert_eq!(tokens[0].device_name, "new-pc");

    // 発行したトークンは次の画面に 1 回だけ出る
    let page = a.get("/", Some(&cookie)).await.text().await.unwrap();
    let start = page.find("fsn_").expect("トークンが表示される");
    let token = &page[start..start + 68];
    assert!(
        a.db.with(|c| db::verify_token(c, token)).unwrap().is_some(),
        "表示したトークンで認証できる"
    );
    let again = a.get("/", Some(&cookie)).await.text().await.unwrap();
    assert!(!again.contains("fsn_"), "2 回目は表示しない");

    // 端末名は空にできない
    let resp = a
        .post("/tokens", Some(&cookie), &format!("csrf={csrf}&device=+"))
        .await;
    assert_eq!(resp.status(), 303);
    let page = a.get("/", Some(&cookie)).await.text().await.unwrap();
    assert!(page.contains("端末名を入力してください"));
    assert_eq!(a.db.with(|c| db::list_tokens(c)).unwrap().len(), 1);
}

#[tokio::test]
async fn revoke_rename_and_delete() {
    let a = Admin::start().await;
    let (id, _) = a.db.with(|c| db::create_token(c, "old-pc")).unwrap();
    let (cookie, csrf) = a.login().await;

    // 有効なトークンは一覧から消せない
    let resp = a
        .post(
            &format!("/tokens/{id}/delete"),
            Some(&cookie),
            &format!("csrf={csrf}"),
        )
        .await;
    assert_eq!(resp.status(), 303);
    assert_eq!(a.db.with(|c| db::list_tokens(c)).unwrap().len(), 1);

    a.post(
        &format!("/tokens/{id}/rename"),
        Some(&cookie),
        &format!("csrf={csrf}&device=renamed-pc"),
    )
    .await;
    assert_eq!(
        a.db.with(|c| db::list_tokens(c)).unwrap()[0].device_name,
        "renamed-pc"
    );

    a.post(
        &format!("/tokens/{id}/revoke"),
        Some(&cookie),
        &format!("csrf={csrf}"),
    )
    .await;
    assert!(
        a.db.with(|c| db::list_tokens(c)).unwrap()[0]
            .revoked_at
            .is_some()
    );
    let page = a.get("/", Some(&cookie)).await.text().await.unwrap();
    assert!(page.contains("トークンを無効にしました"));

    a.post(
        &format!("/tokens/{id}/delete"),
        Some(&cookie),
        &format!("csrf={csrf}"),
    )
    .await;
    assert!(a.db.with(|c| db::list_tokens(c)).unwrap().is_empty());

    // ログアウトしたら入れない
    a.post("/logout", Some(&cookie), &format!("csrf={csrf}"))
        .await;
    assert_eq!(a.get("/", Some(&cookie)).await.status(), 303);
}
