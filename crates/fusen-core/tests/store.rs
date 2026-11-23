//! Store（端末側の保存）のテスト: 版の追記のみ・ごみ箱と復元・revert・下書き・添付・配置

use fusen_core::{CoreError, Placement, RevisionKind, Store, color, markdown};

fn open() -> (tempfile::TempDir, Store) {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    (dir, store)
}

#[test]
fn revisions_are_append_only() {
    let (_d, s) = open();
    let r1 = s.create_note("一版目", color::YELLOW).unwrap();
    assert_eq!(r1.kind, RevisionKind::Edit);
    assert_eq!(r1.rev, -1, "未同期の版は負の仮番号");
    assert_eq!(r1.base_rev, None);

    let r2 = s
        .save_edit(r1.note_id, "二版目", None, Some(r1.rev))
        .unwrap()
        .unwrap();
    let r3 = s
        .save_edit(r1.note_id, "三版目", None, None)
        .unwrap()
        .unwrap();
    assert_eq!(r2.rev, -2);
    assert_eq!(r2.base_rev, Some(-1));
    assert_eq!(r3.base_rev, Some(-2), "base_rev 省略時は最新の版");

    // 内容が同じなら版を作らない
    assert!(
        s.save_edit(r1.note_id, "三版目", None, None)
            .unwrap()
            .is_none()
    );

    let h = s.history(r1.note_id).unwrap();
    let contents: Vec<_> = h.iter().map(|r| r.content.as_str()).collect();
    assert_eq!(contents, ["一版目", "二版目", "三版目"]);
    // 古い版はそのまま残っている
    assert_eq!(s.revision(r1.note_id, r1.rev).unwrap(), r1);
    assert_eq!(s.note(r1.note_id).unwrap().content, "三版目");
    assert!(s.note(r1.note_id).unwrap().pending);
    assert_eq!(s.outbox_len().unwrap(), 3);
}

#[test]
fn color_change_creates_color_revision() {
    let (_d, s) = open();
    let r = s.create_note("本文", color::YELLOW).unwrap();
    assert!(s.set_color(r.note_id, color::YELLOW).unwrap().is_none());
    let c = s.set_color(r.note_id, color::BLUE).unwrap().unwrap();
    assert_eq!(c.kind, RevisionKind::Color);
    assert_eq!(c.content, "本文");
    assert_eq!(s.note(r.note_id).unwrap().color, color::BLUE);
}

#[test]
fn trash_and_restore() {
    let (_d, s) = open();
    let a = s.create_note("A", color::YELLOW).unwrap().note_id;
    let b = s.create_note("B", color::PINK).unwrap().note_id;

    let del = s.trash(a).unwrap().unwrap();
    assert_eq!(del.kind, RevisionKind::Delete);
    assert!(del.deleted);
    assert!(s.trash(a).unwrap().is_none(), "二重にごみ箱へは入れない");

    let live: Vec<_> = s.list_notes().unwrap().iter().map(|n| n.note_id).collect();
    let trash: Vec<_> = s.list_trash().unwrap().iter().map(|n| n.note_id).collect();
    assert_eq!(live, vec![b]);
    assert_eq!(trash, vec![a]);

    let res = s.restore(a).unwrap().unwrap();
    assert_eq!(res.kind, RevisionKind::Restore);
    assert!(!res.deleted);
    assert_eq!(res.content, "A", "復元しても内容はそのまま");
    assert!(s.restore(a).unwrap().is_none());
    assert_eq!(s.list_notes().unwrap().len(), 2);
    assert!(s.list_trash().unwrap().is_empty());
    // 削除も復元も版として残る
    assert_eq!(s.history(a).unwrap().len(), 3);
}

#[test]
fn revert_creates_new_revision_with_old_state() {
    let (_d, s) = open();
    let r1 = s.create_note("最初", color::YELLOW).unwrap();
    let id = r1.note_id;
    s.save_edit(id, "変更後", None, None).unwrap();
    s.set_color(id, color::GREEN).unwrap();

    let rv = s.revert(id, r1.rev).unwrap();
    assert_eq!(rv.kind, RevisionKind::Revert);
    assert_eq!(rv.content, "最初");
    assert_eq!(rv.color, color::YELLOW);
    let h = s.history(id).unwrap();
    assert_eq!(h.len(), 4, "戻しても過去の版は消えない");
    assert_eq!(h[1].content, "変更後");
    assert_eq!(s.note(id).unwrap().content, "最初");

    // 存在しない版
    assert!(matches!(
        s.revert(id, 99),
        Err(CoreError::RevisionNotFound { rev: 99, .. })
    ));
}

#[test]
fn revert_keeps_trash_state() {
    let (_d, s) = open();
    let r1 = s.create_note("最初", color::YELLOW).unwrap();
    s.trash(r1.note_id).unwrap();
    let rv = s.revert(r1.note_id, r1.rev).unwrap();
    assert!(rv.deleted, "ごみ箱の状態は今のまま");
}

#[test]
fn drafts_survive_reopen_and_are_cleared_on_save() {
    let dir = tempfile::tempdir().unwrap();
    let id;
    {
        let s = Store::open(dir.path()).unwrap();
        id = s.create_note("保存済み", color::YELLOW).unwrap().note_id;
        s.save_draft(id, "書きかけ1").unwrap();
        s.save_draft(id, "書きかけ2").unwrap();
        // ここでアプリが落ちたとする
    }
    let s = Store::open(dir.path()).unwrap();
    let drafts = s.drafts().unwrap();
    assert_eq!(drafts.len(), 1);
    assert_eq!(drafts[0].note_id, id);
    assert_eq!(drafts[0].content, "書きかけ2");
    // 下書きは版ではない
    assert_eq!(s.history(id).unwrap().len(), 1);

    s.save_edit(id, &drafts[0].content, None, None).unwrap();
    assert!(s.drafts().unwrap().is_empty());
    assert_eq!(s.note(id).unwrap().content, "書きかけ2");

    s.save_draft(id, "取りやめる").unwrap();
    s.clear_draft(id).unwrap();
    assert!(s.drafts().unwrap().is_empty());
}

#[test]
fn reopen_keeps_device_id_and_notes() {
    let dir = tempfile::tempdir().unwrap();
    let (dev, id) = {
        let s = Store::open(dir.path()).unwrap();
        (
            s.device_id(),
            s.create_note("残る", color::GRAY).unwrap().note_id,
        )
    };
    let s = Store::open(dir.path()).unwrap();
    assert_eq!(s.device_id(), dev);
    let n = s.note(id).unwrap();
    assert_eq!(n.content, "残る");
    assert_eq!(n.edited_by, dev);
}

#[test]
fn attachments_dedupe_blobs() {
    let (dir, s) = open();
    let id = s.create_note("添付", color::YELLOW).unwrap().note_id;
    let a = s
        .import_attachment_bytes(id, "a.png", b"same-bytes", 1024)
        .unwrap();
    let f = dir.path().join("b.txt");
    std::fs::write(&f, b"same-bytes").unwrap();
    let b = s.import_attachment_file(id, &f, 1024).unwrap();

    assert_ne!(a.attachment_id, b.attachment_id);
    assert_eq!(a.sha256, b.sha256, "同じ中身は同じ blob");
    assert_eq!(a.mime, "image/png");
    assert_eq!(b.mime, "text/plain");
    assert_eq!(b.file_name, "b.txt");

    let blob_dir = s.blobs().path_for(&a.sha256).unwrap();
    let count = std::fs::read_dir(blob_dir.parent().unwrap())
        .unwrap()
        .count();
    assert_eq!(count, 1, "実体は 1 つだけ");
    assert_eq!(
        s.blob_path(a.attachment_id).unwrap(),
        s.blob_path(b.attachment_id).unwrap()
    );
}

#[test]
fn attachment_size_limit() {
    let (dir, s) = open();
    let id = s.create_note("添付", color::YELLOW).unwrap().note_id;
    let err = s
        .import_attachment_bytes(id, "big.bin", &[0u8; 2048], 1024)
        .unwrap_err();
    assert!(matches!(
        err,
        CoreError::AttachmentTooLarge { max: 1024, .. }
    ));

    let f = dir.path().join("big.bin");
    std::fs::write(&f, [0u8; 2048]).unwrap();
    assert!(matches!(
        s.import_attachment_file(id, &f, 1024),
        Err(CoreError::AttachmentTooLarge {
            size: 2048,
            max: 1024
        })
    ));
    assert!(s.note(id).unwrap().attachments.is_empty());
}

#[test]
fn add_and_remove_attachment_revisions() {
    let (_d, s) = open();
    let id = s.create_note("本文", color::YELLOW).unwrap().note_id;
    let meta = s
        .import_attachment_bytes(id, "写真.png", b"png", 1024)
        .unwrap();

    // 整形表示中に添付 → 本文の末尾に参照を追加（付箋の一番下に表示）
    let new_content = markdown::append_reference("本文", &markdown::reference_for(&meta));
    let r = s
        .add_attachment(id, meta.attachment_id, Some(&new_content))
        .unwrap()
        .unwrap();
    assert_eq!(r.kind, RevisionKind::Attach);
    assert_eq!(r.attachments, vec![meta.attachment_id]);
    assert_eq!(
        markdown::extract_attachment_refs(&r.content),
        vec![meta.attachment_id]
    );
    assert!(
        s.add_attachment(id, meta.attachment_id, None)
            .unwrap()
            .is_none()
    );

    let n = s.note(id).unwrap();
    assert_eq!(s.attachments_of(&n).unwrap()[0].file_name, "写真.png");

    let without = markdown::remove_reference(&n.content, meta.attachment_id);
    let d = s
        .remove_attachment(id, meta.attachment_id, Some(&without))
        .unwrap()
        .unwrap();
    assert_eq!(d.kind, RevisionKind::Detach);
    assert!(d.attachments.is_empty());
    // blob は消さない（過去の版が参照するため）
    assert!(s.blob_path(meta.attachment_id).unwrap().is_some());

    // 過去の版に戻すと添付も戻る
    let rv = s.revert(id, r.rev).unwrap();
    assert_eq!(rv.attachments, vec![meta.attachment_id]);

    // 登録されていない添付は付けられない
    assert!(matches!(
        s.add_attachment(id, uuid::Uuid::now_v7(), None),
        Err(CoreError::AttachmentNotFound(_))
    ));
}

#[test]
fn edit_can_include_attachments_added_while_editing() {
    let (_d, s) = open();
    let r1 = s.create_note("本文", color::YELLOW).unwrap();
    let meta = s
        .import_attachment_bytes(r1.note_id, "x.pdf", b"pdf", 1024)
        .unwrap();
    let content = format!("本文\n{}", markdown::reference_for(&meta));
    let r = s
        .save_edit(
            r1.note_id,
            &content,
            Some(vec![meta.attachment_id]),
            Some(r1.rev),
        )
        .unwrap()
        .unwrap();
    assert_eq!(
        r.kind,
        RevisionKind::Edit,
        "編集中の添付は edit 版にまとめる"
    );
    assert_eq!(r.attachments, vec![meta.attachment_id]);
}

#[test]
fn search_by_content_and_attachment_name() {
    let (_d, s) = open();
    let a = s
        .create_note("買い物リスト: 牛乳", color::YELLOW)
        .unwrap()
        .note_id;
    let b = s.create_note("会議メモ", color::YELLOW).unwrap().note_id;
    let meta = s
        .import_attachment_bytes(b, "Report.PDF", b"x", 1024)
        .unwrap();
    s.add_attachment(b, meta.attachment_id, None).unwrap();
    let c = s
        .create_note("牛乳は捨てた", color::YELLOW)
        .unwrap()
        .note_id;
    s.trash(c).unwrap();

    let ids = |q: &str| -> Vec<_> { s.search(q).unwrap().iter().map(|n| n.note_id).collect() };
    assert_eq!(ids("牛乳"), vec![a], "ごみ箱の付箋は検索しない");
    assert_eq!(
        ids("report"),
        vec![b],
        "添付のファイル名も検索する（大文字小文字は無視）"
    );
    assert_eq!(ids("").len(), 2);
}

#[test]
fn placements_are_saved_and_cascaded() {
    let (_d, s) = open();
    let a = s.create_note("A", color::YELLOW).unwrap().note_id;
    let b = s.create_note("B", color::YELLOW).unwrap().note_id;
    assert!(s.placement(a).unwrap().is_none());

    let pa = s.ensure_placement(a, 300.0, 400.0).unwrap();
    let pb = s.ensure_placement(b, 300.0, 400.0).unwrap();
    assert_ne!((pa.x, pa.y), (pb.x, pb.y), "重ならないようずらす");
    assert_eq!(
        s.ensure_placement(a, 1.0, 1.0).unwrap(),
        pa,
        "既にあれば変えない"
    );

    let moved = Placement {
        x: 1500.5,
        y: 20.0,
        always_on_top: true,
        monitor_key: Some("DP-1".into()),
        ..pa
    };
    s.set_placement(&moved).unwrap();
    assert_eq!(s.placement(a).unwrap().unwrap(), moved);
    assert_eq!(s.placements().unwrap().len(), 2);
    // 配置の変更は版を作らない
    assert_eq!(s.history(a).unwrap().len(), 1);
}

#[test]
fn unknown_note_is_an_error() {
    let (_d, s) = open();
    let id = uuid::Uuid::now_v7();
    assert!(matches!(s.note(id), Err(CoreError::NoteNotFound(_))));
    assert!(matches!(s.history(id), Err(CoreError::NoteNotFound(_))));
    assert!(matches!(s.trash(id), Err(CoreError::NoteNotFound(_))));
}
