//! 端末側 SQLite のスキーマとマイグレーション。
//!
//! スキーマは `docs/spec.md` 3 章に従う。仕様からの追加点（`docs/decisions.md` D-004）:
//! - `revisions.op_id`: 同じ版を 2 回送っても重複させないための操作 ID（UUIDv7）
//! - `attachment_outbox`: 未送信の添付メタデータ（同じ中身の blob を別の添付が共有するため、
//!   `blob_outbox` だけでは登録漏れが起きる）
//!
//! マイグレーションは `PRAGMA user_version` で管理し、番号順に 1 度だけ実行する。

use rusqlite::Connection;

use crate::Result;

/// マイグレーション（添字 + 1 がバージョン番号）。追加するときは末尾に足すだけにする。
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
  server_seq   INTEGER,
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
  PRIMARY KEY (note_id, rev)
);
CREATE INDEX revisions_server_seq ON revisions(server_seq);

CREATE TABLE attachments (
  attachment_id TEXT PRIMARY KEY,
  note_id       TEXT NOT NULL,
  sha256        TEXT NOT NULL,
  file_name     TEXT NOT NULL,
  mime          TEXT NOT NULL,
  size          INTEGER NOT NULL,
  created_at    TEXT NOT NULL
);
CREATE INDEX attachments_sha256 ON attachments(sha256);

CREATE TABLE placements (
  note_id       TEXT PRIMARY KEY,
  monitor_key   TEXT,
  x             REAL NOT NULL,
  y             REAL NOT NULL,
  width         REAL NOT NULL,
  height        REAL NOT NULL,
  always_on_top INTEGER NOT NULL DEFAULT 0,
  visible       INTEGER NOT NULL DEFAULT 1
);

CREATE TABLE outbox (
  note_id TEXT NOT NULL,
  rev     INTEGER NOT NULL,
  PRIMARY KEY (note_id, rev)
);

CREATE TABLE blob_outbox (sha256 TEXT PRIMARY KEY);

CREATE TABLE attachment_outbox (attachment_id TEXT PRIMARY KEY);

CREATE TABLE drafts (
  note_id    TEXT PRIMARY KEY,
  content    TEXT NOT NULL,
  saved_at   TEXT NOT NULL
);

CREATE TABLE sync_state (
  key   TEXT PRIMARY KEY,
  value TEXT NOT NULL
);
"#,
];

/// 接続の初期設定とマイグレーションを行う。
pub(crate) fn init(conn: &mut Connection) -> Result<()> {
    // WAL: 書き込み中でも読み出せ、クラッシュに強い。
    // foreign_keys は使っていないが、将来のために有効にしておく。
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "synchronous", "NORMAL")?;
    conn.pragma_update(None, "foreign_keys", "ON")?;
    migrate(conn)
}

fn migrate(conn: &mut Connection) -> Result<()> {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn migrate_is_idempotent() {
        let mut conn = Connection::open_in_memory().unwrap();
        init(&mut conn).unwrap();
        init(&mut conn).unwrap();
        let v: i64 = conn
            .pragma_query_value(None, "user_version", |r| r.get(0))
            .unwrap();
        assert_eq!(v, MIGRATIONS.len() as i64);
    }
}
