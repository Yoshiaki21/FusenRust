//! 端末側の保存（SQLite）。
//!
//! 版（revisions）は追記のみ。例外は同期で「仮番号 → 確定番号」に置き換えるときだけ
//! （`docs/spec.md` 4.3）。
//!
//! ## 仮番号
//! 端末で作った未同期の版は **負の仮番号**（-1, -2, ...）で保存する。サーバーの確定番号は正なので、
//! 他の端末の版を取り込んでも主キー (note_id, rev) がぶつからない。
//!
//! ## 最新の版（head）
//! - 未送信の版があれば、その中で最後に作ったもの（最も小さい仮番号）
//! - なければ、受付番号 `server_seq` が最大の版（サーバーの後勝ち）

use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};

use chrono::{DateTime, SecondsFormat, Utc};
use fusen_proto::{ChangesResponse, PushRevisionResponse};
use rusqlite::types::Type;
use rusqlite::{Connection, OptionalExtension, Row, Transaction, params};
use uuid::Uuid;

use crate::blob::BlobStore;
use crate::{AttachmentMeta, CoreError, Result, Revision, RevisionKind, db};

/// sync_state のキー: この端末の ID
const KEY_DEVICE_ID: &str = "device_id";
/// sync_state のキー: 取り込み済みの最後の受付番号
const KEY_LAST_SERVER_SEQ: &str = "last_server_seq";

/// 付箋の最新の状態
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NoteSnapshot {
    pub note_id: Uuid,
    /// 最新の版の番号（未同期なら負の仮番号。同期で確定番号に変わる）
    pub rev: i64,
    /// 最新の版の操作 ID（番号が仮番号から確定番号に変わっても同じ版を指す）
    pub op_id: Uuid,
    /// タイトル（空なら付けていない）
    pub title: String,
    pub content: String,
    pub color: String,
    pub attachments: Vec<Uuid>,
    pub deleted: bool,
    pub created_at: DateTime<Utc>,
    /// 最新の版が保存された時刻（端末の時刻）
    pub updated_at: DateTime<Utc>,
    /// 最新の版を保存した端末
    pub edited_by: Uuid,
    /// 最新の版がまだサーバーに送られていないか
    pub pending: bool,
    /// 最新の版に競合の印があるか
    pub conflict: bool,
}

/// 編集中の下書き（クラッシュ対策。版ではなく、同期もしない）
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Draft {
    pub note_id: Uuid,
    pub content: String,
    pub saved_at: DateTime<Utc>,
}

/// 端末ごとの付箋の配置（同期しない）
#[derive(Clone, Debug, PartialEq)]
pub struct Placement {
    pub note_id: Uuid,
    /// どのモニターか（名前や識別子）
    pub monitor_key: Option<String>,
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
    /// 最前面モードか（付箋ごと・端末ごと）
    pub always_on_top: bool,
    /// デスクトップに表示するか
    pub visible: bool,
}

/// サーバーの差分を取り込んだ結果
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RemoteApplyReport {
    /// 内容が変わった付箋
    pub changed_notes: Vec<Uuid>,
    /// この端末で初めて見る付箋（配置を作る必要がある）
    pub new_notes: Vec<Uuid>,
    /// 取り込んだ最後の受付番号
    pub last_server_seq: i64,
}

/// 端末側の保存
pub struct Store {
    conn: Mutex<Connection>,
    device_id: Uuid,
    blobs: BlobStore,
    data_dir: PathBuf,
}

impl Store {
    /// データディレクトリ（`fusen.db` と `blobs/`）を開く。なければ作る。
    ///
    /// 端末 ID は初回に UUIDv7 で作り、DB に保存する。
    pub fn open(data_dir: impl AsRef<Path>) -> Result<Self> {
        let data_dir = data_dir.as_ref().to_path_buf();
        std::fs::create_dir_all(&data_dir)?;
        let mut conn = Connection::open(data_dir.join("fusen.db"))?;
        db::init(&mut conn)?;

        let device_id = match get_state(&conn, KEY_DEVICE_ID)? {
            Some(s) => parse_uuid_str(&s)?,
            None => {
                let id = Uuid::now_v7();
                set_state(&conn, KEY_DEVICE_ID, &id.to_string())?;
                tracing::info!("端末 ID を作成しました: {id}");
                id
            }
        };
        let blobs = BlobStore::new(data_dir.join("blobs"))?;
        Ok(Self {
            conn: Mutex::new(conn),
            device_id,
            blobs,
            data_dir,
        })
    }

    /// この端末の ID
    pub fn device_id(&self) -> Uuid {
        self.device_id
    }

    pub fn data_dir(&self) -> &Path {
        &self.data_dir
    }

    /// 添付の実体の保存場所
    pub fn blobs(&self) -> &BlobStore {
        &self.blobs
    }

    fn lock(&self) -> MutexGuard<'_, Connection> {
        // 他のスレッドが処理中に panic しても DB 自体はトランザクションで守られているので、続行する
        self.conn.lock().unwrap_or_else(|e| e.into_inner())
    }

    // ------------------------------------------------------------------
    // 付箋と版
    // ------------------------------------------------------------------

    /// 新しい付箋を作る（最初の版は `edit`）
    pub fn create_note(&self, content: &str, color: &str) -> Result<Revision> {
        let mut conn = self.lock();
        let tx = conn.transaction()?;
        let note_id = Uuid::now_v7();
        let now = fmt_time(Utc::now());
        tx.execute(
            "INSERT INTO notes (note_id, latest_rev, deleted, created_at, updated_at)
             VALUES (?1, 0, 0, ?2, ?2)",
            params![note_id.to_string(), now],
        )?;
        let r = append_revision(
            &tx,
            self.device_id,
            NewRevision {
                note_id,
                kind: RevisionKind::Edit,
                title: String::new(),
                content: content.to_string(),
                color: color.to_string(),
                attachments: Vec::new(),
                deleted: false,
                base_rev: None,
            },
        )?;
        tx.commit()?;
        Ok(r)
    }

    /// 付箋の最新の状態
    pub fn note(&self, note_id: Uuid) -> Result<NoteSnapshot> {
        let conn = self.lock();
        snapshot(&conn, note_id)
    }

    /// ごみ箱にない付箋の一覧（更新が新しい順）
    pub fn list_notes(&self) -> Result<Vec<NoteSnapshot>> {
        self.list_where(false)
    }

    /// ごみ箱の付箋の一覧（更新が新しい順）
    pub fn list_trash(&self) -> Result<Vec<NoteSnapshot>> {
        self.list_where(true)
    }

    fn list_where(&self, deleted: bool) -> Result<Vec<NoteSnapshot>> {
        let conn = self.lock();
        let ids: Vec<String> = conn
            .prepare("SELECT note_id FROM notes WHERE deleted = ?1 ORDER BY updated_at DESC")?
            .query_map([deleted], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        ids.iter()
            .map(|id| snapshot(&conn, parse_uuid_str(id)?))
            .collect()
    }

    /// タイトル・本文・添付のファイル名で検索する（ごみ箱にない付箋。大文字小文字を区別しない）
    pub fn search(&self, query: &str) -> Result<Vec<NoteSnapshot>> {
        let q = query.to_lowercase();
        let notes = self.list_notes()?;
        if q.trim().is_empty() {
            return Ok(notes);
        }
        let mut out = Vec::new();
        for n in notes {
            let hit = n.title.to_lowercase().contains(&q)
                || n.content.to_lowercase().contains(&q)
                || self
                    .attachments_of(&n)?
                    .iter()
                    .any(|a| a.file_name.to_lowercase().contains(&q));
            if hit {
                out.push(n);
            }
        }
        Ok(out)
    }

    /// 本文の編集を保存する（編集の終了時）。
    ///
    /// - `attachments`: 編集中に添付を増減した場合はその一覧。`None` なら今の一覧のまま
    /// - `base_rev`: 編集を始めたときの版。`None` なら今の最新の版
    ///
    /// 内容が変わっていなければ版を作らず `None` を返す。どちらの場合も下書きは消す。
    pub fn save_edit(
        &self,
        note_id: Uuid,
        content: &str,
        attachments: Option<Vec<Uuid>>,
        base_rev: Option<i64>,
    ) -> Result<Option<Revision>> {
        let mut conn = self.lock();
        let tx = conn.transaction()?;
        let head = snapshot(&tx, note_id)?;
        tx.execute(
            "DELETE FROM drafts WHERE note_id = ?1",
            [note_id.to_string()],
        )?;
        let attachments = attachments.unwrap_or_else(|| head.attachments.clone());
        if head.content == content && head.attachments == attachments {
            tx.commit()?;
            return Ok(None);
        }
        // 編集を始めた後にこの端末自身が作った版（色やタイトルの変更）しかなければ、最新の版を土台とする。
        // そうしないと、自分の変更なのにサーバーで競合の印が付いてしまう
        let base = match base_rev {
            Some(b) if !changed_by_others_since(&tx, note_id, b, self.device_id)? => head.rev,
            Some(b) => b,
            None => head.rev,
        };
        let r = append_revision(
            &tx,
            self.device_id,
            NewRevision {
                note_id,
                kind: RevisionKind::Edit,
                title: head.title,
                content: content.to_string(),
                color: head.color,
                attachments,
                deleted: head.deleted,
                base_rev: Some(base),
            },
        )?;
        tx.commit()?;
        Ok(Some(r))
    }

    /// 色を変える。同じ色なら版を作らない。
    pub fn set_color(&self, note_id: Uuid, color: &str) -> Result<Option<Revision>> {
        self.modify(note_id, RevisionKind::Color, |head| {
            (head.color != color).then(|| Changes {
                color: Some(color.to_string()),
                ..Default::default()
            })
        })
    }

    /// タイトルを変える（前後の空白と改行は除く）。同じタイトルなら版を作らない。空にするとタイトルを外す。
    pub fn set_title(&self, note_id: Uuid, title: &str) -> Result<Option<Revision>> {
        let title = normalize_title(title);
        self.modify(note_id, RevisionKind::Title, |head| {
            (head.title != title).then(|| Changes {
                title: Some(title.clone()),
                ..Default::default()
            })
        })
    }

    /// ファイルを添付として取り込む（blob の保存とメタデータの登録）。版は作らない。
    ///
    /// 付箋に付けるには続けて [`Store::add_attachment`] を呼ぶ（編集中なら編集終了時の版に含める）。
    pub fn import_attachment_file(
        &self,
        note_id: Uuid,
        path: &Path,
        max_bytes: u64,
    ) -> Result<AttachmentMeta> {
        let (sha256, size) = self.blobs.import_file(path, max_bytes)?;
        let file_name = path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| "file".to_string());
        self.register_attachment(note_id, sha256, size, file_name)
    }

    /// バイト列（クリップボードからの貼り付けなど）を添付として取り込む。版は作らない。
    pub fn import_attachment_bytes(
        &self,
        note_id: Uuid,
        file_name: &str,
        bytes: &[u8],
        max_bytes: u64,
    ) -> Result<AttachmentMeta> {
        let (sha256, size) = self.blobs.import_reader(bytes, max_bytes)?;
        self.register_attachment(note_id, sha256, size, file_name.to_string())
    }

    fn register_attachment(
        &self,
        note_id: Uuid,
        sha256: String,
        size: u64,
        file_name: String,
    ) -> Result<AttachmentMeta> {
        let mime = guess_mime(&file_name);
        let meta = AttachmentMeta {
            attachment_id: Uuid::now_v7(),
            note_id,
            sha256,
            file_name,
            mime,
            size,
            created_at: Utc::now(),
        };
        let mut conn = self.lock();
        let tx = conn.transaction()?;
        insert_attachment(&tx, &meta)?;
        tx.execute(
            "INSERT OR IGNORE INTO blob_outbox (sha256) VALUES (?1)",
            [&meta.sha256],
        )?;
        tx.execute(
            "INSERT OR IGNORE INTO attachment_outbox (attachment_id) VALUES (?1)",
            [meta.attachment_id.to_string()],
        )?;
        tx.commit()?;
        Ok(meta)
    }

    /// 添付を付箋に付ける（整形表示中の操作。`attach` 版を作る）。
    ///
    /// `new_content` に本文を渡すと本文も更新する（例: 末尾に参照を追加した本文）。
    /// 既に付いていて本文も変わらなければ版を作らない。
    pub fn add_attachment(
        &self,
        note_id: Uuid,
        attachment_id: Uuid,
        new_content: Option<&str>,
    ) -> Result<Option<Revision>> {
        self.attachment(attachment_id)?;
        self.modify(note_id, RevisionKind::Attach, |head| {
            let mut attachments = head.attachments.clone();
            if !attachments.contains(&attachment_id) {
                attachments.push(attachment_id);
            }
            let content = new_content.map(str::to_string);
            let changed = attachments != head.attachments
                || content.as_deref().is_some_and(|c| c != head.content);
            changed.then_some(Changes {
                content,
                attachments: Some(attachments),
                ..Default::default()
            })
        })
    }

    /// 添付を付箋から外す（整形表示中の操作。`detach` 版を作る）。blob は消さない。
    pub fn remove_attachment(
        &self,
        note_id: Uuid,
        attachment_id: Uuid,
        new_content: Option<&str>,
    ) -> Result<Option<Revision>> {
        self.modify(note_id, RevisionKind::Detach, |head| {
            let attachments: Vec<Uuid> = head
                .attachments
                .iter()
                .copied()
                .filter(|a| *a != attachment_id)
                .collect();
            let content = new_content.map(str::to_string);
            let changed = attachments != head.attachments
                || content.as_deref().is_some_and(|c| c != head.content);
            changed.then_some(Changes {
                content,
                attachments: Some(attachments),
                ..Default::default()
            })
        })
    }

    /// ごみ箱へ移動する（論理削除）。既にごみ箱にあれば版を作らない。
    pub fn trash(&self, note_id: Uuid) -> Result<Option<Revision>> {
        self.modify(note_id, RevisionKind::Delete, |head| {
            (!head.deleted).then_some(Changes {
                deleted: Some(true),
                ..Default::default()
            })
        })
    }

    /// ごみ箱から復元する。ごみ箱になければ版を作らない。
    pub fn restore(&self, note_id: Uuid) -> Result<Option<Revision>> {
        self.modify(note_id, RevisionKind::Restore, |head| {
            head.deleted.then_some(Changes {
                deleted: Some(false),
                ..Default::default()
            })
        })
    }

    /// 過去の版に戻す。過去の版の本文・色・添付で新しい `revert` 版を作る（ごみ箱の状態は今のまま）。
    pub fn revert(&self, note_id: Uuid, to_rev: i64) -> Result<Revision> {
        let mut conn = self.lock();
        let tx = conn.transaction()?;
        let head = snapshot(&tx, note_id)?;
        let target = revision(&tx, note_id, to_rev)?;
        let r = append_revision(
            &tx,
            self.device_id,
            NewRevision {
                note_id,
                kind: RevisionKind::Revert,
                title: target.title,
                content: target.content,
                color: target.color,
                attachments: target.attachments,
                deleted: head.deleted,
                base_rev: Some(head.rev),
            },
        )?;
        tx.commit()?;
        Ok(r)
    }

    /// 最新の版をもとに一部を変えた版を作る共通処理。`f` が `None` を返したら版を作らない。
    fn modify(
        &self,
        note_id: Uuid,
        kind: RevisionKind,
        f: impl FnOnce(&NoteSnapshot) -> Option<Changes>,
    ) -> Result<Option<Revision>> {
        let mut conn = self.lock();
        let tx = conn.transaction()?;
        let head = snapshot(&tx, note_id)?;
        let Some(ch) = f(&head) else {
            return Ok(None);
        };
        let r = append_revision(
            &tx,
            self.device_id,
            NewRevision {
                note_id,
                kind,
                title: ch.title.unwrap_or(head.title),
                content: ch.content.unwrap_or(head.content),
                color: ch.color.unwrap_or(head.color),
                attachments: ch.attachments.unwrap_or(head.attachments),
                deleted: ch.deleted.unwrap_or(head.deleted),
                base_rev: Some(head.rev),
            },
        )?;
        tx.commit()?;
        Ok(Some(r))
    }

    /// 付箋の全版（受付順。未送信の版は最後に作った順で末尾に並ぶ）
    pub fn history(&self, note_id: Uuid) -> Result<Vec<Revision>> {
        let conn = self.lock();
        ensure_note(&conn, note_id)?;
        let mut stmt = conn.prepare(&format!(
            "SELECT {REV_COLS} FROM revisions WHERE note_id = ?1
             ORDER BY server_seq IS NULL, server_seq, rev DESC"
        ))?;
        let rows = stmt
            .query_map([note_id.to_string()], row_to_revision)?
            .collect::<rusqlite::Result<_>>()?;
        Ok(rows)
    }

    /// 操作 ID の版の今の番号（編集を始めた版の番号が、同期で仮番号から確定番号に変わっても追えるように）
    pub fn rev_of_op(&self, note_id: Uuid, op_id: Uuid) -> Result<Option<i64>> {
        let conn = self.lock();
        Ok(conn
            .query_row(
                "SELECT rev FROM revisions WHERE note_id = ?1 AND op_id = ?2",
                params![note_id.to_string(), op_id.to_string()],
                |r| r.get(0),
            )
            .optional()?)
    }

    /// 指定の版
    pub fn revision(&self, note_id: Uuid, rev: i64) -> Result<Revision> {
        let conn = self.lock();
        revision(&conn, note_id, rev)
    }

    // ------------------------------------------------------------------
    // 添付
    // ------------------------------------------------------------------

    /// 添付のメタデータ
    pub fn attachment(&self, attachment_id: Uuid) -> Result<AttachmentMeta> {
        let conn = self.lock();
        attachment(&conn, attachment_id)
    }

    /// 付箋の最新の版で有効な添付（本文から参照されていないものも含む）
    pub fn attachments_of(&self, note: &NoteSnapshot) -> Result<Vec<AttachmentMeta>> {
        let conn = self.lock();
        note.attachments
            .iter()
            .filter_map(|id| match attachment(&conn, *id) {
                Ok(m) => Some(Ok(m)),
                // メタデータがまだ届いていない添付は飛ばす
                Err(CoreError::AttachmentNotFound(_)) => None,
                Err(e) => Some(Err(e)),
            })
            .collect()
    }

    /// 添付の実体のパス。この端末にまだ実体がなければ `None`（同期で取得する）。
    pub fn blob_path(&self, attachment_id: Uuid) -> Result<Option<PathBuf>> {
        let meta = self.attachment(attachment_id)?;
        let path = self.blobs.path_for(&meta.sha256)?;
        Ok(path.is_file().then_some(path))
    }

    /// ごみ箱にない付箋の最新の版が参照しているのに、実体がこの端末にない添付
    pub fn missing_blobs(&self) -> Result<Vec<AttachmentMeta>> {
        self.missing_blobs_of(self.list_notes()?)
    }

    /// この端末に貼ってある（配置があり表示中の）付箋の添付のうち、実体がまだない添付。
    /// 画像を先に取得する対象を、この端末に貼ってある付箋に絞るために使う
    pub fn missing_blobs_of_pasted(&self) -> Result<Vec<AttachmentMeta>> {
        let pasted: std::collections::HashSet<Uuid> = self
            .placements()?
            .into_iter()
            .filter(|p| p.visible)
            .map(|p| p.note_id)
            .collect();
        let notes = self
            .list_notes()?
            .into_iter()
            .filter(|n| pasted.contains(&n.note_id))
            .collect();
        self.missing_blobs_of(notes)
    }

    fn missing_blobs_of(&self, notes: Vec<NoteSnapshot>) -> Result<Vec<AttachmentMeta>> {
        let mut out: Vec<AttachmentMeta> = Vec::new();
        for n in notes {
            for m in self.attachments_of(&n)? {
                if !self.blobs.contains(&m.sha256) && !out.iter().any(|o| o.sha256 == m.sha256) {
                    out.push(m);
                }
            }
        }
        Ok(out)
    }

    // ------------------------------------------------------------------
    // 下書き
    // ------------------------------------------------------------------

    /// 編集中の下書きを保存する（数秒ごとに呼ぶ）
    pub fn save_draft(&self, note_id: Uuid, content: &str) -> Result<()> {
        let conn = self.lock();
        conn.execute(
            "INSERT INTO drafts (note_id, content, saved_at) VALUES (?1, ?2, ?3)
             ON CONFLICT(note_id) DO UPDATE SET content = excluded.content, saved_at = excluded.saved_at",
            params![note_id.to_string(), content, fmt_time(Utc::now())],
        )?;
        Ok(())
    }

    /// 残っている下書き（起動時に、編集状態で開き直して復元する）
    pub fn drafts(&self) -> Result<Vec<Draft>> {
        let conn = self.lock();
        let mut stmt =
            conn.prepare("SELECT note_id, content, saved_at FROM drafts ORDER BY saved_at")?;
        let rows = stmt
            .query_map([], |r| {
                Ok(Draft {
                    note_id: col_uuid(r, 0)?,
                    content: r.get(1)?,
                    saved_at: col_time(r, 2)?,
                })
            })?
            .collect::<rusqlite::Result<_>>()?;
        Ok(rows)
    }

    /// 下書きを捨てる（編集を取りやめたとき）。保存したときは [`Store::save_edit`] が消す。
    pub fn clear_draft(&self, note_id: Uuid) -> Result<()> {
        let conn = self.lock();
        conn.execute(
            "DELETE FROM drafts WHERE note_id = ?1",
            [note_id.to_string()],
        )?;
        Ok(())
    }

    // ------------------------------------------------------------------
    // 配置
    // ------------------------------------------------------------------

    /// 付箋の配置
    pub fn placement(&self, note_id: Uuid) -> Result<Option<Placement>> {
        let conn = self.lock();
        Ok(conn
            .query_row(
                &format!("SELECT {PLACEMENT_COLS} FROM placements WHERE note_id = ?1"),
                [note_id.to_string()],
                row_to_placement,
            )
            .optional()?)
    }

    /// 全付箋の配置
    pub fn placements(&self) -> Result<Vec<Placement>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(&format!("SELECT {PLACEMENT_COLS} FROM placements"))?;
        let rows = stmt
            .query_map([], row_to_placement)?
            .collect::<rusqlite::Result<_>>()?;
        Ok(rows)
    }

    /// 配置を保存する（位置・サイズ・最前面の変更。版は作らない）
    pub fn set_placement(&self, p: &Placement) -> Result<()> {
        let conn = self.lock();
        conn.execute(
            "INSERT INTO placements (note_id, monitor_key, x, y, width, height, always_on_top, visible)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
             ON CONFLICT(note_id) DO UPDATE SET
               monitor_key = excluded.monitor_key, x = excluded.x, y = excluded.y,
               width = excluded.width, height = excluded.height,
               always_on_top = excluded.always_on_top, visible = excluded.visible",
            params![
                p.note_id.to_string(),
                p.monitor_key,
                p.x,
                p.y,
                p.width,
                p.height,
                p.always_on_top,
                p.visible
            ],
        )?;
        Ok(())
    }

    /// 配置がなければ、画面の左上付近に既定サイズで作る（他の端末で作られた付箋など）。
    /// 重ならないよう、既にある配置の数に応じて少しずつずらす。
    pub fn ensure_placement(&self, note_id: Uuid, width: f64, height: f64) -> Result<Placement> {
        if let Some(p) = self.placement(note_id)? {
            return Ok(p);
        }
        let count = self.placements()?.len();
        // 10 枚ごとに同じ位置へ戻る（画面の外へ出ていかないように）
        let step = (count % 10) as f64 * 32.0;
        let p = Placement {
            note_id,
            monitor_key: None,
            x: 40.0 + step,
            y: 40.0 + step,
            width,
            height,
            always_on_top: false,
            visible: true,
        };
        self.set_placement(&p)?;
        Ok(p)
    }

    // ------------------------------------------------------------------
    // 同期（sync モジュールから使う）
    // ------------------------------------------------------------------

    /// 取り込み済みの最後の受付番号
    pub fn last_server_seq(&self) -> Result<i64> {
        let conn = self.lock();
        Ok(get_state(&conn, KEY_LAST_SERVER_SEQ)?
            .and_then(|s| s.parse().ok())
            .unwrap_or(0))
    }

    /// 未送信の版の数（同期状態の表示用）
    pub fn outbox_len(&self) -> Result<usize> {
        let conn = self.lock();
        let n: i64 = conn.query_row("SELECT COUNT(*) FROM outbox", [], |r| r.get(0))?;
        Ok(n as usize)
    }

    /// 未送信の版を、送る順（付箋ごとに作った順）に返す
    pub fn pending_revisions(&self) -> Result<Vec<Revision>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(&format!(
            "SELECT {REV_COLS_R} FROM outbox o
             JOIN revisions r ON r.note_id = o.note_id AND r.rev = o.rev
             ORDER BY r.note_id, r.rev DESC"
        ))?;
        let rows = stmt
            .query_map([], row_to_revision)?
            .collect::<rusqlite::Result<_>>()?;
        Ok(rows)
    }

    /// 未送信の添付メタデータなら返す
    pub fn pending_attachment(&self, attachment_id: Uuid) -> Result<Option<AttachmentMeta>> {
        let conn = self.lock();
        let pending: bool = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM attachment_outbox WHERE attachment_id = ?1)",
            [attachment_id.to_string()],
            |r| r.get(0),
        )?;
        if !pending {
            return Ok(None);
        }
        attachment(&conn, attachment_id).map(Some)
    }

    /// 未送信の blob か
    pub fn is_blob_pending(&self, sha256: &str) -> Result<bool> {
        let conn = self.lock();
        Ok(conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM blob_outbox WHERE sha256 = ?1)",
            [sha256],
            |r| r.get(0),
        )?)
    }

    /// blob を送り終えた
    pub fn mark_blob_sent(&self, sha256: &str) -> Result<()> {
        let conn = self.lock();
        conn.execute("DELETE FROM blob_outbox WHERE sha256 = ?1", [sha256])?;
        Ok(())
    }

    /// 添付メタデータを登録し終えた
    pub fn mark_attachment_sent(&self, attachment_id: Uuid) -> Result<()> {
        let conn = self.lock();
        conn.execute(
            "DELETE FROM attachment_outbox WHERE attachment_id = ?1",
            [attachment_id.to_string()],
        )?;
        Ok(())
    }

    /// サーバーの応答で、仮番号の版を確定番号に置き換える
    pub fn confirm_revision(
        &self,
        note_id: Uuid,
        temp_rev: i64,
        resp: PushRevisionResponse,
    ) -> Result<()> {
        let mut conn = self.lock();
        let tx = conn.transaction()?;
        confirm(&tx, note_id, temp_rev, resp)?;
        tx.commit()?;
        Ok(())
    }

    /// サーバーの差分（`GET /changes` の応答）を取り込む
    pub fn apply_remote(&self, changes: &ChangesResponse) -> Result<RemoteApplyReport> {
        let mut conn = self.lock();
        let tx = conn.transaction()?;
        let mut report = RemoteApplyReport {
            last_server_seq: get_state(&tx, KEY_LAST_SERVER_SEQ)?
                .and_then(|s| s.parse().ok())
                .unwrap_or(0),
            ..Default::default()
        };

        for meta in &changes.attachments {
            insert_attachment(&tx, meta)?;
        }

        for r in &changes.revisions {
            let Some(seq) = r.server_seq else {
                return Err(CoreError::InvalidData(format!(
                    "受付番号のない版を受信しました: {} rev {}",
                    r.note_id, r.rev
                )));
            };
            report.last_server_seq = report.last_server_seq.max(seq);

            // 自分が送った版（同じ op_id）が戻ってきた場合
            let existing: Option<(i64, Option<i64>)> = tx
                .query_row(
                    "SELECT rev, server_seq FROM revisions WHERE op_id = ?1",
                    [r.op_id.to_string()],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .optional()?;
            match existing {
                Some((_, Some(_))) => continue,
                Some((temp, None)) => {
                    // 送信の応答を受け取る前に落ちた場合など。差分の内容で確定させる
                    confirm(
                        &tx,
                        r.note_id,
                        temp,
                        PushRevisionResponse {
                            rev: r.rev,
                            server_seq: seq,
                            conflict: r.conflict,
                        },
                    )?;
                    continue;
                }
                None => {}
            }

            let inserted = tx.execute(
                "INSERT OR IGNORE INTO notes (note_id, latest_rev, deleted, created_at, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?4)",
                params![
                    r.note_id.to_string(),
                    r.rev,
                    r.deleted,
                    fmt_time(r.edited_at)
                ],
            )?;
            if inserted == 1 {
                report.new_notes.push(r.note_id);
            }
            insert_revision(&tx, r)?;
            recompute_note(&tx, r.note_id)?;
            if !report.changed_notes.contains(&r.note_id) {
                report.changed_notes.push(r.note_id);
            }
        }

        set_state(
            &tx,
            KEY_LAST_SERVER_SEQ,
            &report.last_server_seq.to_string(),
        )?;
        tx.commit()?;
        Ok(report)
    }
}

// ----------------------------------------------------------------------
// 内部の処理（トランザクションの中で使う）
// ----------------------------------------------------------------------

/// 最新の版から変える部分
#[derive(Default)]
struct Changes {
    title: Option<String>,
    content: Option<String>,
    color: Option<String>,
    attachments: Option<Vec<Uuid>>,
    deleted: Option<bool>,
}

/// 追加する版の内容
struct NewRevision {
    note_id: Uuid,
    kind: RevisionKind,
    title: String,
    content: String,
    color: String,
    attachments: Vec<Uuid>,
    deleted: bool,
    base_rev: Option<i64>,
}

const REV_COLS: &str = "note_id, rev, server_seq, op_id, device_id, edited_at, kind, content, \
                        color, attachments, deleted, base_rev, conflict, title";
const REV_COLS_R: &str = "r.note_id, r.rev, r.server_seq, r.op_id, r.device_id, r.edited_at, \
                          r.kind, r.content, r.color, r.attachments, r.deleted, r.base_rev, \
                          r.conflict, r.title";
const PLACEMENT_COLS: &str = "note_id, monitor_key, x, y, width, height, always_on_top, visible";

/// 版 `base_rev` より後に、他の端末が作った版があるか（版が見つからなければ true とみなす）。
/// 順序は履歴と同じ（受付順、未送信の版は作った順で末尾）
fn changed_by_others_since(
    conn: &Connection,
    note_id: Uuid,
    base_rev: i64,
    me: Uuid,
) -> Result<bool> {
    let rows: Vec<(i64, String)> = conn
        .prepare(
            "SELECT rev, device_id FROM revisions WHERE note_id = ?1
             ORDER BY server_seq IS NULL, server_seq, rev DESC",
        )?
        .query_map([note_id.to_string()], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<rusqlite::Result<_>>()?;
    let Some(pos) = rows.iter().position(|(rev, _)| *rev == base_rev) else {
        return Ok(true);
    };
    let me = me.to_string();
    Ok(rows[pos + 1..].iter().any(|(_, device)| *device != me))
}

/// 仮番号で版を追加し、outbox に入れて、付箋の最新の状態を更新する
fn append_revision(tx: &Transaction<'_>, device_id: Uuid, n: NewRevision) -> Result<Revision> {
    let min_rev: Option<i64> = tx.query_row(
        "SELECT MIN(rev) FROM revisions WHERE note_id = ?1",
        [n.note_id.to_string()],
        |r| r.get(0),
    )?;
    let temp_rev = min_rev.unwrap_or(0).min(0) - 1;
    let r = Revision {
        note_id: n.note_id,
        rev: temp_rev,
        server_seq: None,
        op_id: Uuid::now_v7(),
        device_id,
        edited_at: Utc::now(),
        kind: n.kind,
        title: n.title,
        content: n.content,
        color: n.color,
        attachments: n.attachments,
        deleted: n.deleted,
        base_rev: n.base_rev,
        conflict: false,
    };
    insert_revision(tx, &r)?;
    tx.execute(
        "INSERT INTO outbox (note_id, rev) VALUES (?1, ?2)",
        params![r.note_id.to_string(), r.rev],
    )?;
    recompute_note(tx, r.note_id)?;
    Ok(r)
}

fn insert_revision(conn: &Connection, r: &Revision) -> Result<()> {
    conn.execute(
        &format!(
            "INSERT INTO revisions ({REV_COLS}) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)"
        ),
        params![
            r.note_id.to_string(),
            r.rev,
            r.server_seq,
            r.op_id.to_string(),
            r.device_id.to_string(),
            fmt_time(r.edited_at),
            r.kind.as_str(),
            r.content,
            r.color,
            serde_json::to_string(&r.attachments)?,
            r.deleted,
            r.base_rev,
            r.conflict,
            r.title,
        ],
    )?;
    Ok(())
}

fn insert_attachment(conn: &Connection, m: &AttachmentMeta) -> Result<()> {
    conn.execute(
        "INSERT OR IGNORE INTO attachments
           (attachment_id, note_id, sha256, file_name, mime, size, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![
            m.attachment_id.to_string(),
            m.note_id.to_string(),
            m.sha256,
            m.file_name,
            m.mime,
            m.size as i64,
            fmt_time(m.created_at),
        ],
    )?;
    Ok(())
}

/// 仮番号を確定番号に置き換える（版の UPDATE はここだけ）
fn confirm(
    tx: &Transaction<'_>,
    note_id: Uuid,
    temp_rev: i64,
    resp: PushRevisionResponse,
) -> Result<()> {
    let nid = note_id.to_string();
    let updated = tx.execute(
        "UPDATE revisions SET rev = ?1, server_seq = ?2, conflict = ?3
         WHERE note_id = ?4 AND rev = ?5 AND server_seq IS NULL",
        params![resp.rev, resp.server_seq, resp.conflict, nid, temp_rev],
    )?;
    if updated == 0 {
        // 既に確定済み（差分の取り込みで先に確定した場合など）
        return Ok(());
    }
    tx.execute(
        "DELETE FROM outbox WHERE note_id = ?1 AND rev = ?2",
        params![nid, temp_rev],
    )?;
    // この版の上に重ねた未送信の版の base_rev も確定番号に直す
    tx.execute(
        "UPDATE revisions SET base_rev = ?1
         WHERE note_id = ?2 AND base_rev = ?3 AND server_seq IS NULL",
        params![resp.rev, nid, temp_rev],
    )?;
    recompute_note(tx, note_id)
}

/// 最新の版の番号（未送信があれば最後に作った未送信の版、なければ受付番号が最大の版）
fn head_rev(conn: &Connection, note_id: Uuid) -> Result<Option<i64>> {
    let nid = note_id.to_string();
    let pending: Option<i64> = conn
        .query_row(
            "SELECT rev FROM revisions WHERE note_id = ?1 AND server_seq IS NULL
             ORDER BY rev ASC LIMIT 1",
            [&nid],
            |r| r.get(0),
        )
        .optional()?;
    if pending.is_some() {
        return Ok(pending);
    }
    Ok(conn
        .query_row(
            "SELECT rev FROM revisions WHERE note_id = ?1 AND server_seq IS NOT NULL
             ORDER BY server_seq DESC LIMIT 1",
            [&nid],
            |r| r.get(0),
        )
        .optional()?)
}

/// notes の索引（最新の版・ごみ箱・更新日時）を版から計算し直す
fn recompute_note(conn: &Connection, note_id: Uuid) -> Result<()> {
    let Some(rev) = head_rev(conn, note_id)? else {
        return Ok(());
    };
    let head = revision(conn, note_id, rev)?;
    conn.execute(
        "UPDATE notes SET latest_rev = ?1, deleted = ?2, updated_at = ?3 WHERE note_id = ?4",
        params![
            head.rev,
            head.deleted,
            fmt_time(head.edited_at),
            note_id.to_string()
        ],
    )?;
    Ok(())
}

fn ensure_note(conn: &Connection, note_id: Uuid) -> Result<()> {
    let exists: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM notes WHERE note_id = ?1)",
        [note_id.to_string()],
        |r| r.get(0),
    )?;
    if exists {
        Ok(())
    } else {
        Err(CoreError::NoteNotFound(note_id))
    }
}

fn snapshot(conn: &Connection, note_id: Uuid) -> Result<NoteSnapshot> {
    let created_at: String = conn
        .query_row(
            "SELECT created_at FROM notes WHERE note_id = ?1",
            [note_id.to_string()],
            |r| r.get(0),
        )
        .optional()?
        .ok_or(CoreError::NoteNotFound(note_id))?;
    let rev = head_rev(conn, note_id)?.ok_or(CoreError::NoteNotFound(note_id))?;
    let head = revision(conn, note_id, rev)?;
    Ok(NoteSnapshot {
        note_id,
        rev: head.rev,
        op_id: head.op_id,
        title: head.title,
        content: head.content,
        color: head.color,
        attachments: head.attachments,
        deleted: head.deleted,
        created_at: parse_time_str(&created_at)?,
        updated_at: head.edited_at,
        edited_by: head.device_id,
        pending: head.server_seq.is_none(),
        conflict: head.conflict,
    })
}

fn revision(conn: &Connection, note_id: Uuid, rev: i64) -> Result<Revision> {
    conn.query_row(
        &format!("SELECT {REV_COLS} FROM revisions WHERE note_id = ?1 AND rev = ?2"),
        params![note_id.to_string(), rev],
        row_to_revision,
    )
    .optional()?
    .ok_or(CoreError::RevisionNotFound { note_id, rev })
}

fn attachment(conn: &Connection, attachment_id: Uuid) -> Result<AttachmentMeta> {
    conn.query_row(
        "SELECT attachment_id, note_id, sha256, file_name, mime, size, created_at
         FROM attachments WHERE attachment_id = ?1",
        [attachment_id.to_string()],
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
    .optional()?
    .ok_or(CoreError::AttachmentNotFound(attachment_id))
}

fn get_state(conn: &Connection, key: &str) -> Result<Option<String>> {
    Ok(conn
        .query_row("SELECT value FROM sync_state WHERE key = ?1", [key], |r| {
            r.get(0)
        })
        .optional()?)
}

fn set_state(conn: &Connection, key: &str, value: &str) -> Result<()> {
    conn.execute(
        "INSERT INTO sync_state (key, value) VALUES (?1, ?2)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        params![key, value],
    )?;
    Ok(())
}

// ----------------------------------------------------------------------
// 行の変換
// ----------------------------------------------------------------------

fn row_to_revision(r: &Row<'_>) -> rusqlite::Result<Revision> {
    let kind: String = r.get(6)?;
    let attachments: String = r.get(9)?;
    Ok(Revision {
        note_id: col_uuid(r, 0)?,
        rev: r.get(1)?,
        server_seq: r.get(2)?,
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

fn row_to_placement(r: &Row<'_>) -> rusqlite::Result<Placement> {
    Ok(Placement {
        note_id: col_uuid(r, 0)?,
        monitor_key: r.get(1)?,
        x: r.get(2)?,
        y: r.get(3)?,
        width: r.get(4)?,
        height: r.get(5)?,
        always_on_top: r.get(6)?,
        visible: r.get(7)?,
    })
}

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

fn parse_uuid_str(s: &str) -> Result<Uuid> {
    Uuid::parse_str(s)
        .map_err(|e| CoreError::InvalidData(format!("UUID ではありません: {s} ({e})")))
}

fn parse_time_str(s: &str) -> Result<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(s)
        .map(|t| t.with_timezone(&Utc))
        .map_err(|e| CoreError::InvalidData(format!("RFC3339 ではありません: {s} ({e})")))
}

/// RFC3339（UTC、精度は落とさない）
/// タイトルの最大の長さ（文字数）
const MAX_TITLE_CHARS: usize = 200;

/// 保存するタイトルに整える: 改行・タブは空白にし、前後の空白を除き、長すぎれば切る
fn normalize_title(title: &str) -> String {
    let one_line: String = title
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    one_line.trim().chars().take(MAX_TITLE_CHARS).collect()
}

impl NoteSnapshot {
    /// 一覧などに出す名前（タイトル、なければ本文の 1 行目）
    pub fn display_title(&self) -> String {
        crate::markdown::display_title(&self.title, &self.content)
    }
}

fn fmt_time(t: DateTime<Utc>) -> String {
    t.to_rfc3339_opts(SecondsFormat::AutoSi, true)
}

/// ファイル名から MIME タイプを推測する。JXL は mime_guess が知らないことがあるので補う。
fn guess_mime(file_name: &str) -> String {
    let lower = file_name.to_ascii_lowercase();
    if lower.ends_with(".jxl") {
        return "image/jxl".to_string();
    }
    mime_guess::from_path(file_name)
        .first_raw()
        .unwrap_or("application/octet-stream")
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn guesses_mime() {
        assert_eq!(guess_mime("a.PNG"), "image/png");
        assert_eq!(guess_mime("b.jpg"), "image/jpeg");
        assert_eq!(guess_mime("c.jxl"), "image/jxl");
        assert_eq!(guess_mime("d.unknownext"), "application/octet-stream");
    }

    #[test]
    fn time_roundtrip_keeps_precision() {
        let t = Utc::now();
        assert_eq!(parse_time_str(&fmt_time(t)).unwrap(), t);
    }
}
