use super::*;
use std::fs::File;
use std::io::{self, Write};

fn hits(e: &Env, query: &str) -> usize {
    Session::search(&e.layout, query, 10).unwrap().len()
}

/// Platform file identity (device, inode on Unix; volume serial and file index on Windows).
fn identity_of(path: &Path) -> (u64, u64) {
    let id = platform_fs::identity(&File::open(path).unwrap()).unwrap();
    (id.device, id.inode)
}

/// Replaces `from` with `to` in the journal at `path` without changing its length, identity or mtime.
/// The index must notice this rewrite even though every size check still matches.
fn rewrite_in_place(path: &Path, from: &str, to: &str) {
    assert_eq!(from.len(), to.len());
    let text = String::from_utf8(fs::read(path).unwrap()).unwrap();
    let rewritten = text.replace(from, to);
    assert_eq!(rewritten.len(), text.len());
    let before_identity = identity_of(path);
    let before = fs::metadata(path).unwrap();
    let mtime = before.modified().unwrap();
    let mut file = OpenOptions::new().write(true).open(path).unwrap();
    file.write_all(rewritten.as_bytes()).unwrap();
    file.set_times(std::fs::FileTimes::new().set_modified(mtime))
        .unwrap();
    drop(file);
    let after = fs::metadata(path).unwrap();
    assert_eq!(
        identity_of(path),
        before_identity,
        "rewrite must stay on the same file"
    );
    assert_eq!(after.len(), before.len());
    assert_eq!(after.modified().unwrap(), mtime);
}

#[test]
fn same_size_same_inode_rewrite_replaces_the_stale_search_text() {
    let e = env();
    let mut session = new_session(&e);
    session.append(user("platypus")).unwrap();
    let path = session.path().unwrap().to_owned();
    assert_eq!(hits(&e, "platypus"), 1);
    drop(session);

    rewrite_in_place(&path, "platypus", "hedgehog");

    assert_eq!(hits(&e, "hedgehog"), 1);
    assert_eq!(hits(&e, "platypus"), 0);
}

#[test]
fn rewritten_prefix_plus_append_drops_stale_text_and_indexes_new_text_once() {
    let e = env();
    let mut session = new_session(&e);
    session.append(user("platypus")).unwrap();
    assert_eq!(hits(&e, "platypus"), 1);
    // Appended after the index covers the first line, so the rewrite and the append are both new.
    session.append(user("echidna")).unwrap();
    let path = session.path().unwrap().to_owned();
    drop(session);

    rewrite_in_place(&path, "platypus", "hedgehog");

    assert_eq!(hits(&e, "platypus"), 0);
    assert_eq!(hits(&e, "hedgehog"), 1);
    assert_eq!(hits(&e, "echidna"), 1);
}

#[test]
fn normal_appends_and_completed_torn_tails_are_indexed_exactly_once() {
    let e = env();
    let mut session = new_session(&e);
    session.append(user("platypus")).unwrap();
    assert_eq!(hits(&e, "platypus"), 1);
    session.append(user("echidna")).unwrap();
    assert_eq!(hits(&e, "echidna"), 1);
    assert_eq!(
        hits(&e, "platypus"),
        1,
        "a later append must not re-index earlier lines"
    );

    let path = session.path().unwrap().to_owned();
    let line_start = fs::metadata(&path).unwrap().len() as usize;
    session.append(user("hedgehog")).unwrap();
    drop(session);
    let full = fs::read(&path).unwrap();
    // Cut the new entry in the middle of its content: the unterminated line must not be indexed.
    let content = line_start
        + full[line_start..]
            .windows(b"hedgehog".len())
            .position(|w| w == b"hedgehog")
            .unwrap();
    let split = content + 4;
    fs::write(&path, &full[..split]).unwrap();
    assert_eq!(hits(&e, "hedgehog"), 0);
    assert_eq!(hits(&e, "platypus"), 1);

    let mut tail = OpenOptions::new().append(true).open(&path).unwrap();
    tail.write_all(&full[split..]).unwrap();
    drop(tail);

    assert_eq!(hits(&e, "hedgehog"), 1);
    assert_eq!(hits(&e, "platypus"), 1);
    assert_eq!(hits(&e, "echidna"), 1);
}

#[test]
fn version_one_index_without_prefix_digest_discards_stale_rows_on_first_search() {
    let e = env();
    let mut session = new_session(&e);
    session.append(user("platypus")).unwrap();
    let path = session.path().unwrap().to_owned();
    let sid = session.id().to_owned();
    drop(session);

    // A version-1 database: indexed rows carry an identity but no prefix digest, and its message
    // rows were built from text that is no longer in the journal ("ghost").
    crate::platform::atomic::private_dir(&e.layout.state_dir).unwrap();
    let db = e.layout.state_dir.join("sessions.sqlite3");
    let conn = Connection::open(&db).unwrap();
    conn.execute_batch(
        "CREATE TABLE indexed(session_id TEXT PRIMARY KEY, bytes INTEGER NOT NULL, identity TEXT NOT NULL DEFAULT '');
         CREATE VIRTUAL TABLE msgs USING fts5(text, session_id UNINDEXED, role UNINDEXED, ts UNINDEXED, tokenize='porter unicode61');
         PRAGMA user_version = 1;",
    )
    .unwrap();
    let len = fs::metadata(&path).unwrap().len() as i64;
    let (device, inode) = identity_of(&path);
    conn.execute(
        "INSERT INTO indexed(session_id, bytes, identity) VALUES(?1, ?2, ?3)",
        rusqlite::params![sid, len, format!("{device}:{inode}")],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO msgs(text, session_id, role, ts) VALUES('ghost', ?1, 'user', '')",
        [&sid],
    )
    .unwrap();
    drop(conn);

    assert_eq!(hits(&e, "ghost"), 0, "stale rows must be discarded");
    assert_eq!(
        hits(&e, "platypus"),
        1,
        "the journal is rebuilt from source"
    );
    let conn = Connection::open(&db).unwrap();
    assert_eq!(crate::storage::sqlite::check_version(&conn, 3).unwrap(), 3);
}

#[test]
fn header_sync_failure_keeps_applied_roots_and_the_new_writer() {
    let e = env();
    let mut session = new_session(&e);
    session.append(user("before roots")).unwrap();
    let path = session.path().unwrap().to_owned();
    let old_identity = identity_of(&path);
    let dirs = vec![e._home.path().join("extra-root")];

    let err = session
        .set_additional_dirs_with(&dirs, |_| Err(io::Error::other("injected sync failure")))
        .unwrap_err();

    let message = format!("{err:#}");
    assert!(message.contains("durability is uncertain"), "{message}");
    assert!(message.contains("injected sync failure"), "{message}");
    assert_eq!(session.additional_dirs(), dirs.as_slice());
    assert_ne!(
        identity_of(&path),
        old_identity,
        "the rename publishes the header on a new file"
    );
    assert_eq!(
        Session::read(&path).unwrap().additional_dirs(),
        dirs.as_slice()
    );

    session.append(user("after roots")).unwrap();
    drop(session);
    let reread = Session::read(&path).unwrap();
    assert_eq!(reread.additional_dirs(), dirs.as_slice());
    assert_eq!(
        contents(&reread.messages()),
        ["before roots", "after roots"]
    );
}

/// A destination layout whose user dir exists, so transfer-ledger files can be placed in it.
fn target_layout(e: &Env) -> (tempfile::TempDir, Layout) {
    let project = tempfile::tempdir().unwrap();
    let target = Layout::with_user_dir(project.path(), e._home.path().join("dial")).unwrap();
    fs::create_dir_all(&target.user_dir).unwrap();
    (project, target)
}

#[test]
fn ledger_lock_failure_removes_the_copy_and_keeps_the_source() {
    let e = env();
    let mut session = new_session(&e);
    session.append(user("move me")).unwrap();
    let src = session.path().unwrap().to_owned();
    let src_bytes = fs::read(&src).unwrap();
    let (_project, target) = target_layout(&e);
    // The lock file is opened as `<lock>.lock`; a directory there refuses the lock before any write.
    fs::create_dir(target.user_dir.join("session-transfers.lock.lock")).unwrap();
    let dst = target.sessions_dir().join(src.file_name().unwrap());

    let err = session
        .relocate_to(&target)
        .err()
        .expect("the move must be refused");

    let message = format!("{err:#}");
    assert!(
        message.contains("source retained and destination removed"),
        "{message}"
    );
    assert!(!dst.exists(), "the unrecorded copy must be removed");
    assert!(!crate::runtime::stream_journal::path(&dst).exists());
    assert_eq!(fs::read(&src).unwrap(), src_bytes);
    assert_eq!(contents(&session.messages()), ["move me"]);
}

#[test]
fn unreadable_ledger_removes_the_copy_before_any_write_and_keeps_the_source() {
    let e = env();
    let mut session = new_session(&e);
    session.append(user("move me")).unwrap();
    let src = session.path().unwrap().to_owned();
    let src_bytes = fs::read(&src).unwrap();
    let (_project, target) = target_layout(&e);
    // Reading the ledger fails before anything is written, so the copy is provably unpublished.
    fs::create_dir(target.user_dir.join("session-transfers.json")).unwrap();
    let dst = target.sessions_dir().join(src.file_name().unwrap());

    let err = session
        .relocate_to(&target)
        .err()
        .expect("the move must be refused");

    let message = format!("{err:#}");
    assert!(
        message.contains("source retained and destination removed"),
        "{message}"
    );
    assert!(!dst.exists(), "an unpublished copy must be removed");
    assert_eq!(fs::read(&src).unwrap(), src_bytes);
}

#[test]
fn uncertain_publication_retains_the_copy_and_the_recorded_move() {
    let e = env();
    let mut session = new_session(&e);
    session.append(user("move me")).unwrap();
    let src = session.path().unwrap().to_owned();
    let (_project, target) = target_layout(&e);
    let dst = target.sessions_dir().join(src.file_name().unwrap());

    // The ledger write lands, then publication reports failure: the move is recorded, so the
    // copy must be kept and no rollback may be claimed.
    let err = session
        .relocate_with_record(&target, Session::open, |layout, id, from, to| {
            super::super::transfers::record(layout, id, from, to)?;
            anyhow::bail!("injected publication failure")
        })
        .err()
        .expect("the injected failure must surface");

    let message = format!("{err:#}");
    assert!(message.contains("no rollback claimed"), "{message}");
    assert!(dst.exists(), "an uncertain move keeps the destination");
    let moved = super::super::transfers::moved_away_from(&target, &src).unwrap();
    assert_eq!(moved.len(), 1);
    assert_eq!(moved[0].session_id, session.id());
    assert_eq!(contents(&session.messages()), ["move me"]);
}

#[test]
fn rolled_back_relocation_leaves_the_source_as_an_owned_journal() {
    let e = env();
    let mut session = new_session(&e);
    session.append(user("move me")).unwrap();
    let src = session.path().unwrap().to_owned();
    let (_project, target) = target_layout(&e);
    let moved = session.relocate_to(&target).unwrap();
    let dst = moved.path().unwrap().to_owned();

    moved.rollback_relocation(&target, &session).unwrap();

    assert!(!dst.exists(), "the rolled-back copy must be removed");
    drop(session);
    // The ledger entry is gone, so the source resumes in place instead of forking.
    match Session::open_or_fork(&target, &src).unwrap() {
        Resumed::Owned(resumed) => assert_eq!(contents(&resumed.messages()), ["move me"]),
        Resumed::Forked { .. } => panic!("a rolled-back move must not leave a fork trigger"),
    }
}

#[test]
fn failed_ledger_rollback_retains_the_destination() {
    let e = env();
    let mut session = new_session(&e);
    session.append(user("move me")).unwrap();
    let src = session.path().unwrap().to_owned();
    let src_bytes = fs::read(&src).unwrap();
    let (_project, target) = target_layout(&e);
    let moved = session.relocate_to(&target).unwrap();
    let dst = moved.path().unwrap().to_owned();
    // Rollback must read the ledger; a directory in its place makes that read fail.
    let ledger = target.user_dir.join("session-transfers.json");
    fs::remove_file(&ledger).unwrap();
    fs::create_dir(&ledger).unwrap();

    let err = moved
        .rollback_relocation(&target, &session)
        .expect_err("an unreadable ledger must refuse the rollback");

    let message = format!("{err:#}");
    assert!(
        message.contains("rollback uncertain; copied journals retained"),
        "{message}"
    );
    assert!(
        dst.exists(),
        "the destination must survive an unsettled rollback"
    );
    assert_eq!(fs::read(&src).unwrap(), src_bytes);
}
