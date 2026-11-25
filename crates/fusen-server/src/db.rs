//! サーバー側の SQLite。
//!
//! - 版は追記のみ。順序は受付番号 `server_seq`（全付箋で通し番号）で決める（後勝ち）
//! - 受け取った版の `base_rev` が最新版と違えば `conflict = 1`（docs/spec.md 4.3）
//! - 同じ `op_id` の版は 1 度だけ受け付け、2 回目以降は前回と同じ応答を返す（冪等。docs/decisions.md D-004）
//! - トークンはハッシュ（SHA-256）だけを保存する
//! - 完全削除: 有効なトークン全部から票が届いた付箋は、版・添付を消して墓標だけ残す（D-014）
//!
//! rusqlite は同期 API なので、[`Db::call`] で tokio のブロッキング用スレッドに渡して使う。

use std::path::Path;
use std::sync::{Arc, Mutex};

use chrono::{DateTime, SecondsFormat, Utc};
use fusen_proto::{
    AttachmentMeta, ChangesResponse, NoteHistoryResponse, PurgeResponse, PurgeState,
    PushRevisionRequest, PushRevisionResponse, Revision,
};
use rusqlite::types::Type;
use rusqlite::{Connection, OptionalExtension, Row, params};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::error::ApiError;

const MIGRATIONS: &[&str] = &[
    // v1: 初期スキーマ
    r#"
CREATE TABLE notes (
  note_id      TEXT PRIMARY KEY,
  latest_rev   INTEGER NOT NULL,
  deleted      INTEGER NOT NULL DEFAULT 0,
  created_at   TEXT NOT NULL,
  updated_at   TEXT NOT NULL
);

CREATE TABLE revisions (
  note_id      TEXT NOT NULL,
  rev          INTEGER NOT NULL,
  server_seq   INTEGER NOT NULL UNIQUE,
  op_id        TEXT NOT NULL UNIQUE,
  device_id    TEXT NOT NULL,
  edited_at    TEXT NOT NULL,
  kind         TEXT NOT NULL,
  content      TEXT NOT NULL,
  color        TEXT NOT NULL,
  attachments  TEXT NOT NULL,
  deleted      INTEGER NOT NULL,
  base_rev     INTEGER,
  conflict     INTEGER NOT NULL DEFAULT 0,
  received_at  TEXT NOT NULL,
  token_id     TEXT,
  PRIMARY KEY (note_id, rev)
);

CREATE TABLE attachments (
  attachment_id TEXT PRIMARY KEY,
  note_id       TEXT NOT NULL,
  sha256        TEXT NOT NULL,
  file_name     TEXT NOT NULL,
  mime          TEXT NOT NULL,
  size          INTEGER NOT NULL,
  created_at    TEXT NOT NULL
);

CREATE TABLE tokens (
  token_id     TEXT PRIMARY KEY,
  device_name  TEXT NOT NULL,
  token_hash   TEXT NOT NULL UNIQUE,
  created_at   TEXT NOT NULL,
  last_used_at TEXT,
  revoked_at   TEXT
);
"#,
    // v2: 付箋のタイトル（docs/decisions.md D-013）
    r#"
ALTER TABLE revisions ADD COLUMN title TEXT NOT NULL DEFAULT '';
"#,
    // v3: 完全削除の票と墓標（docs/decisions.md D-014）
    r#"
CREATE TABLE purge_votes (
  note_id   TEXT NOT NULL,
  token_id  TEXT NOT NULL,
  voted_at  TEXT NOT NULL,
  PRIMARY KEY (note_id, token_id)
);

CREATE TABLE purged_notes (
  note_id   TEXT PRIMARY KEY,
  purged_at TEXT NOT NULL
);
"#,
];

/// SQLite の接続（全リクエストで共有。書き込みは 1 つずつ）
#[derive(Clone)]
pub struct Db {
    conn: Arc<Mutex<Connection>>,
}

impl Db {
    /// `<data_dir>/fusen.db` を開く（なければ作る）
    pub fn open(data_dir: &Path) -> anyhow::Result<Self> {
        std::fs::create_dir_all(data_dir)?;
        let mut conn = Connection::open(data_dir.join("fusen.db"))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        migrate(&mut conn)?;
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    /// 同期の処理を tokio のブロッキング用スレッドで実行する
    pub async fn call<R, F>(&self, f: F) -> Result<R, ApiError>
    where
        F: FnOnce(&mut Connection) -> Result<R, ApiError> + Send + 'static,
        R: Send + 'static,
    {
        let conn = self.conn.clone();
        tokio::task::spawn_blocking(move || {
            let mut guard = conn.lock().unwrap_or_else(|e| e.into_inner());
            f(&mut guard)
        })
        .await
        .map_err(|e| ApiError::Internal(anyhow::anyhow!("内部の処理が中断しました: {e}")))?
    }

    /// ブロッキングのまま使う（トークン管理のコマンドなど、非同期でない場所から）
    pub fn with<R>(&self, f: impl FnOnce(&mut Connection) -> R) -> R {
        let mut guard = self.conn.lock().unwrap_or_else(|e| e.into_inner());
        f(&mut guard)
    }
}

fn migrate(conn: &mut Connection) -> anyhow::Result<()> {
    let current: i64 = conn.pragma_query_value(None, "user_version", |r| r.get(0))?;
    for (i, sql) in MIGRATIONS.iter().enumerate() {
        let version = i as i64 + 1;
        if version <= current {
            continue;
        }
        let tx = conn.transaction()?;
        tx.execute_batch(sql)?;
        tx.pragma_update(None, "user_version", version)?;
        tx.commit()?;
        tracing::info!("DB をバージョン {version} に更新しました");
    }
    Ok(())
}

pub(crate) fn fmt_time(t: DateTime<Utc>) -> String {
    t.to_rfc3339_opts(SecondsFormat::AutoSi, true)
}

// ----------------------------------------------------------------------
// 版
// ----------------------------------------------------------------------

/// 版を受け付ける。
///
/// `blob_exists` は添付の実体がサーバーにあるかを調べる関数（実体のない添付を参照する版は受け付けない）。
pub fn push_revision(
    conn: &mut Connection,
    note_id: Uuid,
    req: &PushRevisionRequest,
    token_id: &str,
    blob_exists: impl Fn(&str) -> bool,
) -> Result<(PushRevisionResponse, bool), ApiError> {
    let tx = conn.transaction()?;

    // 同じ操作の再送: 前回の応答を返す（新しい版は作らない）
    let existing: Option<(String, i64, i64, bool)> = tx
        .query_row(
            "SELECT note_id, rev, server_seq, conflict FROM revisions WHERE op_id = ?1",
            [req.op_id.to_string()],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .optional()?;
    if let Some((nid, rev, server_seq, conflict)) = existing {
        if nid != note_id.to_string() {
            return Err(ApiError::Conflict(format!(
                "op_id {} は別の付箋で使われています",
                req.op_id
            )));
        }
        return Ok((
            PushRevisionResponse {
                rev,
                server_seq,
                conflict,
            },
            false,
        ));
    }

    // 完全に削除した付箋には版を追加しない
    let purged: bool = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM purged_notes WHERE note_id = ?1)",
        [note_id.to_string()],
        |r| r.get(0),
    )?;
    if purged {
        return Err(ApiError::Gone(format!(
            "付箋 {note_id} は完全に削除されています"
        )));
    }

    // 版が参照する添付は、メタデータと実体が先に届いていなければならない
    for id in &req.attachments {
        let sha: Option<String> = tx
            .query_row(
                "SELECT sha256 FROM attachments WHERE attachment_id = ?1",
                [id.to_string()],
                |r| r.get(0),
            )
            .optional()?;
        match sha {
            None => {
                return Err(ApiError::UnprocessableEntity(format!(
                    "添付 {id} のメタデータが登録されていません"
                )));
            }
            Some(sha) if !blob_exists(&sha) => {
                return Err(ApiError::UnprocessableEntity(format!(
                    "添付 {id} の実体（{sha}）がありません"
                )));
            }
            Some(_) => {}
        }
    }

    let latest: Option<i64> = tx
        .query_row(
            "SELECT latest_rev FROM notes WHERE note_id = ?1",
            [note_id.to_string()],
            |r| r.get(0),
        )
        .optional()?;
    let rev = latest.unwrap_or(0) + 1;
    // 編集を始めたときの版が最新でなければ競合（後勝ちで受け付けるが印を付ける）
    let conflict = latest.is_some() && req.base_rev != latest;
    let server_seq: i64 = tx.query_row(
        "SELECT COALESCE(MAX(server_seq), 0) + 1 FROM revisions",
        [],
        |r| r.get(0),
    )?;
    let edited_at = fmt_time(req.edited_at);
    tx.execute(
        "INSERT INTO revisions (note_id, rev, server_seq, op_id, device_id, edited_at, kind, content,
           color, attachments, deleted, base_rev, conflict, received_at, token_id, title)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16)",
        params![
            note_id.to_string(),
            rev,
            server_seq,
            req.op_id.to_string(),
            req.device_id.to_string(),
            edited_at,
            req.kind.as_str(),
            req.content,
            req.color,
            serde_json::to_string(&req.attachments)
                .map_err(|e| ApiError::Internal(e.into()))?,
            req.deleted,
            req.base_rev,
            conflict,
            fmt_time(Utc::now()),
            token_id,
            req.title,
        ],
    )?;
    tx.execute(
        "INSERT INTO notes (note_id, latest_rev, deleted, created_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?4)
         ON CONFLICT(note_id) DO UPDATE SET
           latest_rev = excluded.latest_rev, deleted = excluded.deleted, updated_at = excluded.updated_at",
        params![note_id.to_string(), rev, req.deleted, edited_at],
    )?;
    // 新しい版が来たら完全削除の票は数え直す（復元された・別の端末がまた変えた）
    tx.execute(
        "DELETE FROM purge_votes WHERE note_id = ?1",
        [note_id.to_string()],
    )?;
    tx.commit()?;
    Ok((
        PushRevisionResponse {
            rev,
            server_seq,
            conflict,
        },
        true,
    ))
}

const REV_COLS: &str = "note_id, rev, server_seq, op_id, device_id, edited_at, kind, content, \
                        color, attachments, deleted, base_rev, conflict, title";

/// 指定の受付番号より後の版（最大 `limit` 件）と、それらが参照する添付のメタデータ
pub fn changes(conn: &Connection, since: i64, limit: u32) -> Result<ChangesResponse, ApiError> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {REV_COLS} FROM revisions WHERE server_seq > ?1 ORDER BY server_seq LIMIT ?2"
    ))?;
    let mut revisions: Vec<Revision> = stmt
        .query_map(params![since, limit as i64 + 1], row_to_revision)?
        .collect::<rusqlite::Result<_>>()?;
    let has_more = revisions.len() > limit as usize;
    revisions.truncate(limit as usize);

    let mut ids: Vec<Uuid> = Vec::new();
    for r in &revisions {
        for a in &r.attachments {
            if !ids.contains(a) {
                ids.push(*a);
            }
        }
    }
    let mut attachments = Vec::new();
    for id in ids {
        if let Some(m) = attachment(conn, id)? {
            attachments.push(m);
        }
    }
    let last_server_seq = revisions.last().and_then(|r| r.server_seq).unwrap_or(since);
    Ok(ChangesResponse {
        revisions,
        attachments,
        last_server_seq,
        has_more,
    })
}

/// 付箋の全版（版番号の順）と、それらが参照する添付のメタデータ
pub fn note_history(conn: &Connection, note_id: Uuid) -> Result<NoteHistoryResponse, ApiError> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {REV_COLS} FROM revisions WHERE note_id = ?1 ORDER BY rev"
    ))?;
    let revisions: Vec<Revision> = stmt
        .query_map([note_id.to_string()], row_to_revision)?
        .collect::<rusqlite::Result<_>>()?;
    if revisions.is_empty() {
        return Err(ApiError::NotFound(format!("付箋 {note_id} がありません")));
    }
    let mut ids: Vec<Uuid> = Vec::new();
    for r in &revisions {
        for a in &r.attachments {
            if !ids.contains(a) {
                ids.push(*a);
            }
        }
    }
    let mut attachments = Vec::new();
    for id in ids {
        if let Some(m) = attachment(conn, id)? {
            attachments.push(m);
        }
    }
    Ok(NoteHistoryResponse {
        revisions,
        attachments,
    })
}

// ----------------------------------------------------------------------
// 完全削除（docs/decisions.md D-014）
// ----------------------------------------------------------------------

/// 端末（トークン）から完全削除の票を受け付ける。
///
/// 有効なトークン（無効にしておらず、一度でも使われたもの）全部の票がそろったら、付箋の版・添付の
/// メタデータを消して墓標を残す。戻り値の 2 つ目は、どこからも参照されなくなった blob の sha256
/// （ファイルは呼び出し側が消す）。
pub fn vote_purge(
    conn: &mut Connection,
    note_id: Uuid,
    token_id: &str,
) -> Result<(PurgeResponse, Vec<String>), ApiError> {
    let tx = conn.transaction()?;
    let id = note_id.to_string();
    let purged: bool = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM purged_notes WHERE note_id = ?1)",
        [&id],
        |r| r.get(0),
    )?;
    let deleted: Option<bool> = tx
        .query_row("SELECT deleted FROM notes WHERE note_id = ?1", [&id], |r| {
            r.get(0)
        })
        .optional()?;
    let done = |state| PurgeResponse {
        state,
        remaining: 0,
    };
    let resp = match (purged, deleted) {
        // 削除済み、またはサーバーが知らない付箋（この端末でしか作られなかった）
        (true, _) | (false, None) => (done(PurgeState::Purged), Vec::new()),
        (false, Some(false)) => (done(PurgeState::NotDeleted), Vec::new()),
        (false, Some(true)) => {
            tx.execute(
                "INSERT OR IGNORE INTO purge_votes (note_id, token_id, voted_at) VALUES (?1, ?2, ?3)",
                params![id, token_id, fmt_time(Utc::now())],
            )?;
            let remaining = remaining_voters(&tx, note_id)?;
            if remaining == 0 {
                (done(PurgeState::Purged), purge_note_rows(&tx, note_id)?)
            } else {
                (
                    PurgeResponse {
                        state: PurgeState::Waiting,
                        remaining,
                    },
                    Vec::new(),
                )
            }
        }
    };
    tx.commit()?;
    Ok(resp)
}

/// 票がそろった付箋をすべて削除する（トークンを無効にした後に使う）。
/// 戻り値は（削除した付箋の数、参照されなくなった blob の sha256）
pub fn purge_ready_notes(conn: &mut Connection) -> Result<(usize, Vec<String>), ApiError> {
    let tx = conn.transaction()?;
    let ids: Vec<String> = tx
        .prepare(
            "SELECT DISTINCT v.note_id FROM purge_votes v
             JOIN notes n ON n.note_id = v.note_id WHERE n.deleted = 1",
        )?
        .query_map([], |r| r.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    let mut count = 0;
    let mut unused = Vec::new();
    for id in ids {
        let note_id: Uuid = id
            .parse()
            .map_err(|e| ApiError::Internal(anyhow::anyhow!("不正な付箋 ID {id}: {e}")))?;
        if remaining_voters(&tx, note_id)? == 0 {
            unused.extend(purge_note_rows(&tx, note_id)?);
            count += 1;
        }
    }
    tx.commit()?;
    Ok((count, unused))
}

/// まだ票を入れていない有効なトークンの数
fn remaining_voters(conn: &Connection, note_id: Uuid) -> Result<u32, ApiError> {
    Ok(conn.query_row(
        "SELECT COUNT(*) FROM tokens t
         WHERE t.revoked_at IS NULL AND t.last_used_at IS NOT NULL
           AND NOT EXISTS (SELECT 1 FROM purge_votes v
                           WHERE v.note_id = ?1 AND v.token_id = t.token_id)",
        [note_id.to_string()],
        |r| r.get(0),
    )?)
}

/// 付箋の版・添付のメタデータ・票を消して墓標を入れ、どこからも参照されなくなった blob を返す。
/// 他の付箋の版が参照している添付は残す（メタデータの note_id は最初に添付した付箋なので）。
fn purge_note_rows(tx: &Connection, note_id: Uuid) -> Result<Vec<String>, ApiError> {
    let id = note_id.to_string();
    let targets: Vec<(String, String)> = tx
        .prepare(
            "SELECT attachment_id, sha256 FROM attachments
             WHERE (note_id = ?1 OR attachment_id IN
                     (SELECT j.value FROM revisions r, json_each(r.attachments) j WHERE r.note_id = ?1))
               AND attachment_id NOT IN
                     (SELECT j.value FROM revisions r, json_each(r.attachments) j WHERE r.note_id <> ?1)",
        )?
        .query_map([&id], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<rusqlite::Result<_>>()?;
    let mut shas: Vec<String> = Vec::new();
    for (attachment_id, sha) in targets {
        tx.execute(
            "DELETE FROM attachments WHERE attachment_id = ?1",
            [&attachment_id],
        )?;
        if !shas.contains(&sha) {
            shas.push(sha);
        }
    }
    for table in ["revisions", "notes", "purge_votes"] {
        tx.execute(&format!("DELETE FROM {table} WHERE note_id = ?1"), [&id])?;
    }
    tx.execute(
        "INSERT OR IGNORE INTO purged_notes (note_id, purged_at) VALUES (?1, ?2)",
        params![id, fmt_time(Utc::now())],
    )?;
    let mut unused = Vec::new();
    for sha in shas {
        let used: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM attachments WHERE sha256 = ?1)",
            [&sha],
            |r| r.get(0),
        )?;
        if !used {
            unused.push(sha);
        }
    }
    tracing::info!("付箋を完全に削除しました: {note_id}");
    Ok(unused)
}

// ----------------------------------------------------------------------
// 添付
// ----------------------------------------------------------------------

pub fn attachment(conn: &Connection, id: Uuid) -> Result<Option<AttachmentMeta>, ApiError> {
    Ok(conn
        .query_row(
            "SELECT attachment_id, note_id, sha256, file_name, mime, size, created_at
             FROM attachments WHERE attachment_id = ?1",
            [id.to_string()],
            |r| {
                Ok(AttachmentMeta {
                    attachment_id: col_uuid(r, 0)?,
                    note_id: col_uuid(r, 1)?,
                    sha256: r.get(2)?,
                    file_name: r.get(3)?,
                    mime: r.get(4)?,
                    size: r.get::<_, i64>(5)? as u64,
                    created_at: col_time(r, 6)?,
                })
            },
        )
        .optional()?)
}

/// 添付のメタデータを登録する。同じ内容なら何もしない（冪等）。違う内容で同じ ID なら競合。
pub fn register_attachment(conn: &Connection, meta: &AttachmentMeta) -> Result<bool, ApiError> {
    if let Some(existing) = attachment(conn, meta.attachment_id)? {
        // 時刻の精度の違いは無視して比べる
        let same = existing.note_id == meta.note_id
            && existing.sha256 == meta.sha256
            && existing.file_name == meta.file_name
            && existing.mime == meta.mime
            && existing.size == meta.size;
        return if same {
            Ok(false)
        } else {
            Err(ApiError::Conflict(format!(
                "添付 {} は別の内容で登録済みです",
                meta.attachment_id
            )))
        };
    }
    conn.execute(
        "INSERT INTO attachments (attachment_id, note_id, sha256, file_name, mime, size, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![
            meta.attachment_id.to_string(),
            meta.note_id.to_string(),
            meta.sha256,
            meta.file_name,
            meta.mime,
            meta.size as i64,
            fmt_time(meta.created_at),
        ],
    )?;
    Ok(true)
}

// ----------------------------------------------------------------------
// トークン
// ----------------------------------------------------------------------

/// トークンの一覧の 1 行（ハッシュは含めない）
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TokenInfo {
    pub token_id: String,
    pub device_name: String,
    pub created_at: String,
    pub last_used_at: Option<String>,
    pub revoked_at: Option<String>,
}

/// 認証済みの端末
#[derive(Debug, Clone)]
pub struct AuthDevice {
    pub token_id: String,
    pub device_name: String,
}

pub fn token_hash(token: &str) -> String {
    hex::encode(Sha256::digest(token.as_bytes()))
}

/// 新しいトークンを作り、`(トークンID, トークン)` を返す。トークン自体は保存しない（表示は 1 回だけ）
pub fn create_token(conn: &Connection, device_name: &str) -> anyhow::Result<(String, String)> {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).map_err(|e| anyhow::anyhow!("乱数を作れません: {e}"))?;
    let token = format!("fsn_{}", hex::encode(bytes));
    let token_id = Uuid::now_v7().to_string();
    conn.execute(
        "INSERT INTO tokens (token_id, device_name, token_hash, created_at) VALUES (?1, ?2, ?3, ?4)",
        params![token_id, device_name, token_hash(&token), fmt_time(Utc::now())],
    )?;
    Ok((token_id, token))
}

pub fn list_tokens(conn: &Connection) -> anyhow::Result<Vec<TokenInfo>> {
    let mut stmt = conn.prepare(
        "SELECT token_id, device_name, created_at, last_used_at, revoked_at FROM tokens ORDER BY created_at",
    )?;
    let rows = stmt
        .query_map([], |r| {
            Ok(TokenInfo {
                token_id: r.get(0)?,
                device_name: r.get(1)?,
                created_at: r.get(2)?,
                last_used_at: r.get(3)?,
                revoked_at: r.get(4)?,
            })
        })?
        .collect::<rusqlite::Result<_>>()?;
    Ok(rows)
}

/// トークンを無効にする。ID の先頭だけでも、1 つに決まれば受け付ける。無効にしたら true
pub fn revoke_token(conn: &Connection, id_or_prefix: &str) -> anyhow::Result<bool> {
    let ids: Vec<String> = conn
        .prepare(
            "SELECT token_id FROM tokens WHERE token_id LIKE ?1 || '%' AND revoked_at IS NULL",
        )?
        .query_map([id_or_prefix], |r| r.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    match ids.as_slice() {
        [] => Ok(false),
        [id] => {
            conn.execute(
                "UPDATE tokens SET revoked_at = ?1 WHERE token_id = ?2",
                params![fmt_time(Utc::now()), id],
            )?;
            Ok(true)
        }
        _ => anyhow::bail!(
            "ID「{id_or_prefix}」に当てはまるトークンが複数あります。もっと長く指定してください"
        ),
    }
}

/// トークンを確かめる（有効なら最終使用日時を更新する）
pub fn verify_token(conn: &Connection, token: &str) -> Result<Option<AuthDevice>, ApiError> {
    let found: Option<AuthDevice> = conn
        .query_row(
            "SELECT token_id, device_name FROM tokens WHERE token_hash = ?1 AND revoked_at IS NULL",
            [token_hash(token)],
            |r| {
                Ok(AuthDevice {
                    token_id: r.get(0)?,
                    device_name: r.get(1)?,
                })
            },
        )
        .optional()?;
    if let Some(d) = &found {
        conn.execute(
            "UPDATE tokens SET last_used_at = ?1 WHERE token_id = ?2",
            params![fmt_time(Utc::now()), d.token_id],
        )?;
    }
    Ok(found)
}

// ----------------------------------------------------------------------
// 行の変換
// ----------------------------------------------------------------------

fn conv_err(idx: usize, e: impl std::error::Error + Send + Sync + 'static) -> rusqlite::Error {
    rusqlite::Error::FromSqlConversionFailure(idx, Type::Text, Box::new(e))
}

fn col_uuid(r: &Row<'_>, idx: usize) -> rusqlite::Result<Uuid> {
    let s: String = r.get(idx)?;
    Uuid::parse_str(&s).map_err(|e| conv_err(idx, e))
}

fn col_time(r: &Row<'_>, idx: usize) -> rusqlite::Result<DateTime<Utc>> {
    let s: String = r.get(idx)?;
    DateTime::parse_from_rfc3339(&s)
        .map(|t| t.with_timezone(&Utc))
        .map_err(|e| conv_err(idx, e))
}

fn row_to_revision(r: &Row<'_>) -> rusqlite::Result<Revision> {
    let kind: String = r.get(6)?;
    let attachments: String = r.get(9)?;
    Ok(Revision {
        note_id: col_uuid(r, 0)?,
        rev: r.get(1)?,
        server_seq: Some(r.get(2)?),
        op_id: col_uuid(r, 3)?,
        device_id: col_uuid(r, 4)?,
        edited_at: col_time(r, 5)?,
        kind: kind.parse().map_err(|e| conv_err(6, e))?,
        content: r.get(7)?,
        color: r.get(8)?,
        attachments: serde_json::from_str(&attachments).map_err(|e| conv_err(9, e))?,
        deleted: r.get(10)?,
        base_rev: r.get(11)?,
        conflict: r.get(12)?,
        title: r.get(13)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use fusen_proto::RevisionKind;

    fn db() -> (tempfile::TempDir, Db) {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(dir.path()).unwrap();
        (dir, db)
    }

    fn req(base_rev: Option<i64>, content: &str) -> PushRevisionRequest {
        PushRevisionRequest {
            op_id: Uuid::now_v7(),
            device_id: Uuid::now_v7(),
            edited_at: Utc::now(),
            kind: RevisionKind::Edit,
            title: String::new(),
            content: content.into(),
            color: "yellow".into(),
            attachments: vec![],
            deleted: false,
            base_rev,
        }
    }

    #[test]
    fn last_writer_wins_and_conflict_flag() {
        let (_d, db) = db();
        let note = Uuid::now_v7();
        db.with(|c| {
            let (r1, _) = push_revision(c, note, &req(None, "1"), "t", |_| true).unwrap();
            assert_eq!((r1.rev, r1.server_seq, r1.conflict), (1, 1, false));
            let (r2, _) = push_revision(c, note, &req(Some(1), "2"), "t", |_| true).unwrap();
            assert_eq!((r2.rev, r2.conflict), (2, false));
            // 古い版（1）をもとにした編集 → 競合の印、ただし後勝ちで最新になる
            let (r3, _) = push_revision(c, note, &req(Some(1), "3"), "t", |_| true).unwrap();
            assert_eq!((r3.rev, r3.server_seq, r3.conflict), (3, 3, true));
            let all = note_history(c, note).unwrap().revisions;
            assert_eq!(all.len(), 3);
            assert_eq!(all[2].content, "3");
            // 新しい付箋を base_rev なしで作るのは競合ではない。受付番号は全付箋で通し
            let (other, _) =
                push_revision(c, Uuid::now_v7(), &req(None, "x"), "t", |_| true).unwrap();
            assert_eq!((other.rev, other.server_seq, other.conflict), (1, 4, false));
        });
    }

    #[test]
    fn same_op_id_is_idempotent() {
        let (_d, db) = db();
        let note = Uuid::now_v7();
        let r = req(None, "once");
        db.with(|c| {
            let (a, created_a) = push_revision(c, note, &r, "t", |_| true).unwrap();
            let (b, created_b) = push_revision(c, note, &r, "t", |_| true).unwrap();
            assert_eq!(a, b);
            assert!(created_a && !created_b);
            assert_eq!(note_history(c, note).unwrap().revisions.len(), 1);
            // 同じ op_id を別の付箋で使うのは不正
            assert!(matches!(
                push_revision(c, Uuid::now_v7(), &r, "t", |_| true),
                Err(ApiError::Conflict(_))
            ));
        });
    }

    #[test]
    fn tokens_are_hashed_and_revocable() {
        let (_d, db) = db();
        db.with(|c| {
            let (id, token) = create_token(c, "cachyos-main").unwrap();
            assert!(token.starts_with("fsn_"));
            // トークンそのものは保存しない
            let stored: String = c
                .query_row("SELECT token_hash FROM tokens", [], |r| r.get(0))
                .unwrap();
            assert_ne!(stored, token);
            assert_eq!(stored, token_hash(&token));

            assert_eq!(
                verify_token(c, &token).unwrap().unwrap().device_name,
                "cachyos-main"
            );
            assert!(verify_token(c, "fsn_wrong").unwrap().is_none());
            assert!(list_tokens(c).unwrap()[0].last_used_at.is_some());

            assert!(revoke_token(c, &id[..13]).unwrap());
            assert!(verify_token(c, &token).unwrap().is_none());
            assert!(!revoke_token(c, &id).unwrap(), "無効化済み");
        });
    }
}
