use super::*;
use serde_json::json;
mod relocation_artifacts;
mod schema;
mod timing;
mod tree;
#[test]
fn streamed_observations_survive_reopen_before_any_final_reply() {
    let e = env();
    let mut session = new_session(&e);
    session.append(user("stream until interrupted")).unwrap();
    let tree = session.runtime();
    let answer = tree.start("assistant", "answer", None);
    answer.append("visible prefix α");
    let path = session.path().unwrap().to_owned();
    // No final assistant message and no flush_runtime: only streaming persistence can recover it.
    drop(answer);
    drop(tree);
    drop(session);
    let reopened = Session::open(&path).unwrap();
    let body = reopened.runtime().bodies("assistant")[0].snapshot();
    assert_eq!(body.result.text, "visible prefix α");
    assert_eq!(body.result.phase, crate::runtime::Phase::Interrupted);
    assert!(!reopened.messages().iter().any(|m| m["role"] == "assistant"));
}

#[test]
fn failed_reopen_after_stream_copy_removes_destination_and_keeps_source() {
    let e = env();
    let mut session = new_session(&e);
    session.append(user("stream until interrupted")).unwrap();
    let tree = session.runtime();
    let answer = tree.start("assistant", "answer", None);
    answer.append("visible prefix");
    let src = session.path().unwrap().to_owned();
    let src_streams = crate::runtime::stream_journal::path(&src);
    let src_bytes = fs::read(&src).unwrap();
    let streams_bytes = fs::read(&src_streams).unwrap();
    assert!(
        !streams_bytes.is_empty(),
        "the live runtime must be journaled"
    );
    let target_project = tempfile::tempdir().unwrap();
    let target = Layout::with_user_dir(target_project.path(), e._home.path().join("dial")).unwrap();
    let dst = target.sessions_dir().join(src.file_name().unwrap());
    let dst_streams = crate::runtime::stream_journal::path(&dst);
    // A second hard link makes the copied stream journal invalid, so the reopen fails only
    // after `stream_journal::copy` has already written it.
    let extra_link = e._home.path().join("extra-link.streams");
    let reopen = |moved: &Path| {
        fs::hard_link(crate::runtime::stream_journal::path(moved), &extra_link).unwrap();
        Session::open(moved)
    };
    assert!(session.relocate_with(&target, reopen).is_err());
    fs::remove_file(&extra_link).unwrap();
    assert!(!dst.exists(), "moved main file must be removed");
    assert!(
        !dst_streams.exists(),
        "moved stream journal must be removed"
    );
    assert_eq!(fs::read(&src).unwrap(), src_bytes);
    assert_eq!(fs::read(&src_streams).unwrap(), streams_bytes);
    let body = session.runtime().bodies("assistant")[0].snapshot();
    assert_eq!(body.result.text, "visible prefix");
    drop(answer);
    drop(tree);
}

#[test]
fn relocation_refuses_a_preexisting_stream_journal_and_leaves_it_untouched() {
    let e = env();
    let mut session = new_session(&e);
    session.append(user("stream until interrupted")).unwrap();
    let tree = session.runtime();
    let answer = tree.start("assistant", "answer", None);
    answer.append("visible prefix");
    let src = session.path().unwrap().to_owned();
    let src_bytes = fs::read(&src).unwrap();
    let src_streams = crate::runtime::stream_journal::path(&src);
    let src_streams_bytes = fs::read(&src_streams).unwrap();
    // Another session's journal already sits where the moved copy would go; it is not ours.
    let other = new_session(&e);
    let other_tree = other.runtime();
    let other_answer = other_tree.start("assistant", "other", None);
    other_answer.append("unrelated observation");
    let orphan = fs::read(crate::runtime::stream_journal::path(other.path().unwrap())).unwrap();
    assert!(!orphan.is_empty());
    let target_project = tempfile::tempdir().unwrap();
    let target = Layout::with_user_dir(target_project.path(), e._home.path().join("dial")).unwrap();
    crate::platform::atomic::private_dir(&target.sessions_dir()).unwrap();
    let dst = target.sessions_dir().join(src.file_name().unwrap());
    let dst_streams = crate::runtime::stream_journal::path(&dst);
    fs::write(&dst_streams, &orphan).unwrap();
    assert!(
        session.relocate_to(&target).is_err(),
        "a foreign stream journal must not be adopted"
    );
    assert!(!dst.exists(), "the moved main file must be removed");
    assert_eq!(fs::read(&dst_streams).unwrap(), orphan);
    assert_eq!(fs::read(&src).unwrap(), src_bytes);
    assert_eq!(fs::read(&src_streams).unwrap(), src_streams_bytes);
    drop(answer);
    drop(tree);
    drop(other_answer);
    drop(other_tree);
}

#[test]
fn empty_session_relocation_cannot_adopt_an_orphan_stream_journal() {
    let e = env();
    let session = new_session(&e);
    let src = session.path().unwrap();
    let before = fs::read(src).unwrap();
    assert!(
        crate::runtime::stream_journal::recover(src)
            .unwrap()
            .is_empty()
    );
    let target_project = tempfile::tempdir().unwrap();
    let target = Layout::with_user_dir(target_project.path(), e._home.path().join("dial")).unwrap();
    crate::platform::atomic::private_dir(&target.sessions_dir()).unwrap();
    let dst = target.sessions_dir().join(src.file_name().unwrap());
    let streams = crate::runtime::stream_journal::path(&dst);
    fs::write(&streams, b"unrelated journal\n").unwrap();
    assert!(session.relocate_to(&target).is_err());
    assert!(!dst.exists());
    assert_eq!(fs::read(&streams).unwrap(), b"unrelated journal\n");
    assert_eq!(fs::read(src).unwrap(), before);
}

#[test]
fn discarding_a_moved_copy_removes_only_companions_the_move_created() {
    let e = env();
    let dir = e.layout.sessions_dir();
    crate::platform::atomic::private_dir(&dir).unwrap();
    let dst = dir.join("moved.jsonl");
    let streams = crate::runtime::stream_journal::path(&dst);
    let side = side_thread::path_for(&dst);
    fs::write(&dst, b"moved\n").unwrap();
    fs::write(&streams, b"foreign\n").unwrap();
    fs::write(&side, b"foreign side\n").unwrap();
    discard_moved_copy(&dst, CreatedCompanions::default());
    assert!(!dst.exists());
    assert_eq!(fs::read(&streams).unwrap(), b"foreign\n");
    assert_eq!(fs::read(&side).unwrap(), b"foreign side\n");
    fs::write(&dst, b"moved\n").unwrap();
    discard_moved_copy(
        &dst,
        CreatedCompanions {
            streams: true,
            side: false,
        },
    );
    assert!(!dst.exists());
    assert!(!streams.exists());
    assert_eq!(fs::read(&side).unwrap(), b"foreign side\n");
    fs::write(&dst, b"moved\n").unwrap();
    fs::write(&streams, b"foreign\n").unwrap();
    discard_moved_copy(
        &dst,
        CreatedCompanions {
            streams: false,
            side: true,
        },
    );
    assert!(!dst.exists());
    assert!(!side.exists());
    assert_eq!(fs::read(&streams).unwrap(), b"foreign\n");
}

/// A pending side-thread exchange, stored verbatim by the side-thread owner.
const PENDING_SIDE: &[u8] =
    br#"{"thread":[],"pending":{"thread":"t","question":"q","answer":"partial"}}"#;

#[test]
fn relocation_moves_the_side_thread_verbatim_and_deletion_leaves_no_orphan() {
    let e = env();
    let session = new_session(&e);
    let src = session.path().unwrap().to_owned();
    let src_side = side_thread::path_for(&src);
    fs::write(&src_side, PENDING_SIDE).unwrap();
    let target_project = tempfile::tempdir().unwrap();
    let target = Layout::with_user_dir(target_project.path(), e._home.path().join("dial")).unwrap();
    let moved = session.relocate_to(&target).unwrap();
    let dst = moved.path().unwrap().to_owned();
    drop(moved);
    assert_eq!(fs::read(side_thread::path_for(&dst)).unwrap(), PENDING_SIDE);
    assert_eq!(fs::read(&src_side).unwrap(), PENDING_SIDE);
    session.remove_file().unwrap();
    assert!(!src.exists());
    assert!(
        !src_side.exists(),
        "deleting a session must delete its side thread"
    );
    assert_eq!(fs::read(side_thread::path_for(&dst)).unwrap(), PENDING_SIDE);
}

#[test]
fn deleting_a_session_removes_its_side_thread_and_root_listing_ignores_it() {
    let e = env();
    let session = new_session(&e);
    let path = session.path().unwrap().to_owned();
    let side = side_thread::path_for(&path);
    fs::write(&side, PENDING_SIDE).unwrap();
    assert_eq!(Session::list(&e.layout, 10).unwrap().len(), 1);
    session.remove_file().unwrap();
    assert!(!path.exists());
    assert!(!side.exists());
    assert!(Session::list(&e.layout, 10).unwrap().is_empty());
}

#[test]
fn relocation_refuses_a_preexisting_destination_side_thread_and_writes_nothing() {
    let e = env();
    let session = new_session(&e);
    let src = session.path().unwrap().to_owned();
    let src_bytes = fs::read(&src).unwrap();
    let src_side = side_thread::path_for(&src);
    fs::write(&src_side, PENDING_SIDE).unwrap();
    let target_project = tempfile::tempdir().unwrap();
    let target = Layout::with_user_dir(target_project.path(), e._home.path().join("dial")).unwrap();
    crate::platform::atomic::private_dir(&target.sessions_dir()).unwrap();
    let dst = target.sessions_dir().join(src.file_name().unwrap());
    let dst_side = side_thread::path_for(&dst);
    let foreign = b"another session's side thread\n";
    fs::write(&dst_side, foreign).unwrap();
    assert!(session.relocate_to(&target).is_err());
    assert!(!dst.exists(), "no main file may be created");
    assert!(!crate::runtime::stream_journal::path(&dst).exists());
    assert_eq!(fs::read(&dst_side).unwrap(), foreign);
    assert_eq!(fs::read(&src).unwrap(), src_bytes);
    assert_eq!(fs::read(&src_side).unwrap(), PENDING_SIDE);
    #[cfg(unix)]
    {
        // A dangling symlink at the destination is a taken name too; it must not be followed.
        fs::remove_file(&dst_side).unwrap();
        std::os::unix::fs::symlink(target.sessions_dir().join("nowhere"), &dst_side).unwrap();
        assert!(session.relocate_to(&target).is_err());
        assert!(!dst.exists());
        assert!(
            fs::symlink_metadata(&dst_side)
                .unwrap()
                .file_type()
                .is_symlink()
        );
    }
}

#[test]
fn failed_reopen_after_side_copy_removes_only_the_side_copy_it_created() {
    let e = env();
    let session = new_session(&e);
    let src = session.path().unwrap().to_owned();
    let src_side = side_thread::path_for(&src);
    fs::write(&src_side, PENDING_SIDE).unwrap();
    let target_project = tempfile::tempdir().unwrap();
    let target = Layout::with_user_dir(target_project.path(), e._home.path().join("dial")).unwrap();
    let dst = target.sessions_dir().join(src.file_name().unwrap());
    let refuse = |_: &Path| -> Result<Session> { bail!("reopen refused") };
    assert!(session.relocate_with(&target, refuse).is_err());
    assert!(!dst.exists());
    assert!(
        !side_thread::path_for(&dst).exists(),
        "the created side copy must be removed"
    );
    assert!(!crate::runtime::stream_journal::path(&dst).exists());
    assert_eq!(fs::read(&src_side).unwrap(), PENDING_SIDE);
    assert!(src.exists());
}

#[cfg(unix)]
#[test]
fn a_non_regular_source_side_thread_rolls_back_the_whole_move() {
    let e = env();
    let session = new_session(&e);
    let src = session.path().unwrap().to_owned();
    let src_side = side_thread::path_for(&src);
    fs::create_dir(&src_side).unwrap();
    let target_project = tempfile::tempdir().unwrap();
    let target = Layout::with_user_dir(target_project.path(), e._home.path().join("dial")).unwrap();
    let dst = target.sessions_dir().join(src.file_name().unwrap());
    assert!(session.relocate_to(&target).is_err());
    assert!(!dst.exists());
    assert!(
        !crate::runtime::stream_journal::path(&dst).exists(),
        "the stream journal copied before the refusal must be removed"
    );
    assert!(src_side.is_dir());
    assert!(src.exists());
}

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

struct Env {
    _home: tempfile::TempDir,
    _project: tempfile::TempDir,
    layout: Layout,
}

fn env() -> Env {
    let home = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    let layout = Layout::with_user_dir(project.path(), home.path().join("dial")).unwrap();
    Env {
        _home: home,
        _project: project,
        layout,
    }
}

fn user(t: &str) -> Value {
    json!({"role": "user", "content": t})
}
fn assistant(t: &str) -> Value {
    json!({"role": "assistant", "content": t})
}
fn new_session(e: &Env) -> Session {
    Session::create(&e.layout, SessionMeta { model: "m".into() }).unwrap()
}
#[test]
fn ordinary_images_survive_source_changes_and_session_reopen() {
    let e = env();
    let root = e._project.path();
    let pixels = b"\x89PNG\r\n\x1a\nfixture";
    std::fs::write(root.join("image.png"), pixels).unwrap();
    let reference = crate::llm::images::attachment(root, Path::new("image.png")).unwrap();
    let mut session = new_session(&e);
    session
        .append(json!({"role":"user","content":"inspect","dial_images":[reference]}))
        .unwrap();
    let path = session.path().unwrap().to_owned();
    std::fs::write(root.join("image.png"), b"changed").unwrap();
    std::fs::remove_file(root.join("image.png")).unwrap();
    drop(session);
    let reopened = Session::open(&path).unwrap();
    let messages = reopened.messages();
    let reference = &messages[0]["dial_images"][0];
    assert_eq!(
        crate::llm::images::verified_bytes(reference).unwrap().0,
        pixels
    );
    assert!(
        reference["snapshot"]["uri"]
            .as_str()
            .unwrap()
            .starts_with("artifact://")
    );
}
fn contents(msgs: &[Value]) -> Vec<String> {
    msgs.iter().map(content_text).collect()
}

/// Independent persisted-parent walk: catches stale derived indices after every mutation.
fn assert_active_reads(session: &Session) {
    let mut branch = Vec::new();
    let mut seen = HashSet::new();
    let mut cur = session
        .leaf
        .as_ref()
        .and_then(|id| session.by_id.get(id))
        .copied();
    while let Some(i) = cur {
        if !seen.insert(i) {
            break;
        }
        branch.push(i);
        cur = session.entries[i]
            .parent
            .as_ref()
            .and_then(|id| session.by_id.get(id))
            .copied();
    }
    branch.reverse();
    assert_eq!(session.branch(), branch);
    let mut state = Value::Null;
    for &i in &branch {
        if let Kind::Message(m) = &session.entries[i].kind
            && role_of(m) == "dial_state"
        {
            apply_control_entry(&mut state, m);
        }
    }
    if state.is_null() {
        state = json!({});
    }
    // The provider generation never regresses (see `with_history_generation_floor`), so a
    // branch that predates a compaction still reports the highest generation on the file.
    let mut expected = state;
    let floor = session.context_history_generation();
    if expected["provider_identity"]["history_generation"]
        .as_u64()
        .unwrap_or(0)
        < floor
    {
        if !expected.is_object() {
            expected = json!({});
        }
        expected["provider_identity"]["history_generation"] = json!(floor);
    }
    assert_eq!(session.control_state(), expected);

    let mut summary = None;
    let mut replacement = None;
    let visible = if let Some(p) = branch.iter().rposition(|&i| {
        matches!(
            session.entries[i].kind,
            Kind::Compaction { .. } | Kind::ContextReset { .. }
        )
    }) {
        let start = match &session.entries[branch[p]].kind {
            Kind::Compaction {
                summary: text,
                first_kept,
                prefix,
            } => {
                summary = Some(text);
                replacement = prefix.as_ref();
                first_kept.as_ref().and_then(|id| {
                    branch[..p]
                        .iter()
                        .position(|&i| &session.entries[i].id == id)
                })
            }
            Kind::ContextReset { bootstrap, .. } => {
                replacement = Some(bootstrap);
                None
            }
            _ => unreachable!(),
        };
        let mut indices = start.map(|s| branch[s..p].to_vec()).unwrap_or_default();
        indices.extend_from_slice(&branch[p + 1..]);
        indices
    } else {
        branch
    };
    let mut expected = Vec::new();
    let mut weighted = Vec::new();
    if let Some(prefix) = replacement {
        expected.extend_from_slice(prefix);
        weighted.extend_from_slice(prefix);
    } else if let Some(summary) = summary {
        let message = Session::compaction_message(summary);
        expected.push(message.clone());
        weighted.push(message);
    }
    for i in visible {
        if let Kind::Message(m) = &session.entries[i].kind
            && role_of(m) != "dial_state"
        {
            expected.push(m.clone());
            weighted.push(m.clone());
        }
    }
    assert_eq!(session.messages(), expected);
    assert_eq!(session.message_count(), expected.len());
    let mut request = vec![json!({"role":"system","content":"prefix"})];
    session.extend_messages(&mut request);
    assert_eq!(&request[1..], expected);
    let weigh = |m: &Value| content_text(m).len() + role_of(m).len();
    for from in 0..=expected.len() + 1 {
        assert_eq!(
            session.weigh_from(from, weigh),
            weighted.iter().skip(from).map(weigh).sum::<usize>(),
        );
    }
    let calls = std::cell::Cell::new(0);
    assert_eq!(
        session.weigh_from(expected.len(), |_| {
            calls.set(calls.get() + 1);
            1
        }),
        0
    );
    assert_eq!(calls.get(), 0, "out-of-view messages must not be weighed");
}

#[test]
fn active_reads_follow_append_compaction_rewind_fork_and_reopen() {
    use crate::runtime::Phase;
    let e = env();
    let mut s = new_session(&e);
    assert_active_reads(&s);
    s.append(user("first")).unwrap();
    s.set_control_state(json!({"turn":1})).unwrap();
    let first = s.runtime().start("tool", "first branch result", None);
    first.settle("done".into(), None, Phase::Completed);
    s.flush_runtime().unwrap();
    s.append(assistant("first answer")).unwrap();
    assert_active_reads(&s);
    s.append(user("second")).unwrap();
    s.set_control_state(json!({"turn":2})).unwrap();
    let second = s.runtime().start("tool", "abandoned result", None);
    second.settle("done".into(), None, Phase::Completed);
    s.flush_runtime().unwrap();
    s.append(assistant("second answer")).unwrap();
    assert_active_reads(&s);
    s.compact("earlier conversation".into(), 3).unwrap();
    s.rename("branch title").unwrap();
    assert_active_reads(&s);
    // Compaction advances the provider history generation; the floor keeps it across rewinds.
    assert_eq!(
        s.control_state(),
        json!({"turn":2,"provider_identity":{"history_generation":1}})
    );
    assert_eq!(s.rewind(1).unwrap(), 2);
    assert_active_reads(&s);
    assert_eq!(s.messages(), vec![user("first"), assistant("first answer")]);
    assert_eq!(
        s.control_state(),
        json!({"turn":1,"provider_identity":{"history_generation":2}})
    );
    assert_eq!(s.runtime().snapshots().len(), 1);
    assert_eq!(s.title(), "branch title");
    s.append(user("replacement")).unwrap();
    s.set_control_state(json!({"turn":3})).unwrap();
    s.compact("replacement summary".into(), 1).unwrap();
    assert_active_reads(&s);
    let mut fork = s.fork(&e.layout).unwrap();
    assert_active_reads(&fork);
    assert_eq!(fork.messages(), s.messages());
    assert_eq!(fork.control_state(), s.control_state());
    assert_eq!(fork.runtime().snapshots().len(), 1);
    fork.append(assistant("fork only")).unwrap();
    assert_active_reads(&fork);
    assert_active_reads(&s);
    for session in [&s, &fork] {
        let reopened = Session::read(session.path().unwrap()).unwrap();
        assert_active_reads(&reopened);
        assert_eq!(reopened.messages(), session.messages());
        assert_eq!(reopened.control_state(), session.control_state());
        assert_eq!(reopened.title(), session.title());
        assert_eq!(
            reopened.runtime().snapshots().len(),
            session.runtime().snapshots().len()
        );
    }
}

#[test]
fn active_reads_preserve_missing_forward_and_cyclic_parent_behavior() {
    let fixtures = [
        vec![
            json!({"type":"message","id":"a","parent":"missing","message":user("orphan")}),
            json!({"type":"message","id":"b","parent":"a","message":{"role":"dial_state","state":{"case":"missing"}}}),
        ],
        vec![
            json!({"type":"message","id":"a","parent":"b","message":user("cycle")}),
            json!({"type":"message","id":"b","parent":"a","message":{"role":"dial_state","state":{"case":"cycle"}}}),
        ],
        vec![
            json!({"type":"message","id":"a","parent":"b","message":user("forward")}),
            json!({"type":"message","id":"b","parent":null,"message":assistant("root")}),
            json!({"type":"compaction","id":"c","parent":"a","summary":"summary","first_kept":"not-on-branch"}),
        ],
        vec![json!({"type":"message","id":"a","parent":"a","message":{"role":"dial_state"}})],
    ];
    let e = env();
    for entries in fixtures {
        let original = new_session(&e);
        let path = original.path().unwrap().to_owned();
        let mut file = OpenOptions::new().append(true).open(&path).unwrap();
        for entry in entries {
            writeln!(file, "{entry}").unwrap();
        }
        drop(original);
        let mut s = Session::open(&path).unwrap();
        assert_active_reads(&s);
        s.append(user("new turn")).unwrap();
        assert_active_reads(&s);
        s.set_control_state(json!({"case":"new"})).unwrap();
        assert_active_reads(&s);
        s.compact("new summary".into(), 1).unwrap();
        assert_active_reads(&s);
        s.rewind(1).unwrap();
        assert_active_reads(&s);
        assert_active_reads(&s.fork(&e.layout).unwrap());
        assert_active_reads(&Session::read(&path).unwrap());
    }
}

#[test]
fn rename_persists_without_changing_messages_and_export_escapes_tools() {
    let e = env();
    let mut s = new_session(&e);
    s.append(user("first prompt")).unwrap();
    s.append(json!({"role":"assistant","content":"answer","tool_calls":[{"id":"one","function":{"name":"bash","arguments":"{}"}}]})).unwrap();
    s.append(json!({"role":"tool","tool_call_id":"one","content":"<script>alert(1)</script>"}))
        .unwrap();
    let messages = s.messages();
    s.rename("A named session").unwrap();
    let reopened = Session::read(s.path().unwrap()).unwrap();
    assert_eq!(reopened.title(), "A named session");
    assert_eq!(reopened.messages(), messages);
    let info = Session::list(&e.layout, usize::MAX).unwrap().remove(0);
    assert_eq!(info.title, "A named session");
    assert_eq!(info.first_message, "first prompt");
    assert_eq!(info.model, "m");
    let fork = reopened.fork(&e.layout).unwrap();
    let infos = Session::list_all(&e.layout, usize::MAX).unwrap();
    assert_eq!(
        infos
            .iter()
            .find(|i| i.id == fork.id())
            .unwrap()
            .parent_id
            .as_deref(),
        Some(s.id())
    );
    for html in [false, true] {
        let path = e
            .layout
            .project_root
            .join(if html { "out.html" } else { "out.md" });
        reopened.export(&e.layout, &path, html).unwrap();
        let output = fs::read_to_string(&path).unwrap();
        assert!(output.contains("A named session"));
        assert!(output.contains("first prompt"));
        // Only HTML output wraps tool results in <details> and escapes them; Markdown keeps
        // them in code fences (see export.rs `markdown_uses_fences_not_html`).
        assert_eq!(output.contains("<details>"), html);
        if html {
            assert!(output.contains("&lt;script&gt;"));
            assert!(!output.contains("<script>"));
        } else {
            assert!(output.contains("**Tool result**"));
        }
        assert!(
            reopened.export(&e.layout, &path, html).is_err(),
            "exports must not overwrite existing files"
        );
    }
}

#[test]
fn old_message_only_sessions_still_get_titles() {
    let e = env();
    let s = new_session(&e);
    let path = s.path().unwrap().to_owned();
    let mut file = OpenOptions::new().append(true).open(&path).unwrap();
    writeln!(file, "{}", json!({"type":"message","id":"old","parent":null,"ts":"","message":user("old first message")})).unwrap();
    drop(s);
    let mut loaded = Session::open(&path).unwrap();
    assert_eq!(loaded.title(), "old first message");
    loaded.rename("new title").unwrap();
    assert_eq!(Session::read(&path).unwrap().title(), "new title");
    assert_eq!(loaded.messages(), vec![user("old first message")]);
}

#[test]
fn root_workers_round_trip_and_forks_reparent_to_the_new_session() {
    let e = env();
    let mut session = new_session(&e);
    let worker = session
        .runtime()
        .start("worker", "before first message", None);
    session.flush_runtime().unwrap();
    assert_eq!(worker.read(|n| n.parent.clone()), Some(session.id().into()));
    let loaded = Session::read(session.path().unwrap()).unwrap();
    let fresh = loaded.runtime().start("worker", "after reopen", None);
    assert_eq!(fresh.read(|n| n.parent.clone()), Some(loaded.id().into()));
    let fork = loaded.fork(&e.layout).unwrap();
    assert_eq!(
        fork.runtime().snapshots()[0].parent.as_deref(),
        Some(fork.id())
    );
}

#[test]
fn runtime_journal_recovers_typed_results_and_rewind_prunes_only_the_active_view() {
    use crate::runtime::Phase;
    let e = env();
    let mut session = new_session(&e);
    let first = session.append(user("first")).unwrap();
    let tool = session.runtime().start("tool", "browser", None);
    tool.settle(
        "shown text".into(),
        Some(json!({"tabs":[{"id":7}]})),
        Phase::Completed,
    );
    session.flush_runtime().unwrap();
    session.append(assistant("first answer")).unwrap();
    session.append(user("second")).unwrap();
    let worker = session.runtime().start("worker", "inspect", None);
    worker.settle(
        "working".into(),
        Some(json!({"status":"running"})),
        Phase::Running,
    );
    session.flush_runtime().unwrap();
    let path = session.path().unwrap().to_owned();
    drop(session);
    let mut reopened = Session::open(&path).unwrap();
    let nodes = reopened.runtime().snapshots();
    assert_eq!(nodes[0].parent.as_deref(), Some(first.as_str()));
    assert_eq!(nodes[0].result.data.as_ref().unwrap()["tabs"][0]["id"], 7);
    assert_eq!(nodes[1].result.phase, Phase::Interrupted);
    assert_eq!(
        nodes[1].result.data.as_ref().unwrap()["status"],
        "interrupted"
    );
    let tree = reopened.execution_tree();
    for node in &tree {
        if let Some(parent) = &node.parent {
            assert!(tree.iter().any(|n| &n.id == parent), "orphan {}", node.id);
        }
    }
    let fork = reopened.fork(&e.layout).unwrap();
    assert_eq!(fork.runtime().snapshots().len(), 2);
    reopened.rewind(1).unwrap();
    assert_eq!(reopened.runtime().snapshots().len(), 1);
    let fresh = reopened.runtime().start("assistant", "new branch", None);
    assert_ne!(fresh.read(|n| n.id.clone()), worker.read(|n| n.id.clone()));
    // Newly started observations are now durable before they reach the UI.
    assert_eq!(Session::read(&path).unwrap().runtime().snapshots().len(), 2);
    assert_eq!(
        reopened.messages().len(),
        2,
        "runtime snapshots are not provider messages"
    );
}

#[test]
fn create_append_reopen_round_trip_with_private_permissions() {
    let e = env();
    let mut s = new_session(&e);
    s.append(user("hello")).unwrap();
    let call = json!({"role": "assistant", "content": null, "tool_calls": [{"id": "c1", "type": "function", "function": {"name": "read", "arguments": "{}"}}]});
    s.append(call.clone()).unwrap();
    s.append(json!({"role": "tool", "tool_call_id": "c1", "content": "ok"}))
        .unwrap();
    let path = s.path().unwrap().to_path_buf();

    let header: Value =
        serde_json::from_str(fs::read_to_string(&path).unwrap().lines().next().unwrap()).unwrap();
    assert_eq!(header["type"], "session");
    assert_eq!(header["id"], s.id());
    assert_eq!(header["model"], "m");
    assert_eq!(header["cwd"], json!(e.layout.project_root));
    assert!(header["started"].as_str().unwrap().ends_with('Z'));

    let reopened = Session::read(&path).unwrap();
    assert_eq!(reopened.id(), s.id());
    assert_eq!(reopened.messages(), s.messages());
    assert_eq!(reopened.messages()[1], call);
    #[cfg(unix)]
    {
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(
            fs::metadata(path.parent().unwrap())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
    }
    #[cfg(windows)]
    {
        use crate::platform::fs as platform_fs;
        assert!(platform_fs::is_owner_only(&fs::File::open(&path).unwrap()).unwrap());
        assert!(
            platform_fs::is_owner_only(
                &platform_fs::open_dir_nofollow(path.parent().unwrap()).unwrap()
            )
            .unwrap()
        );
    }

    // Appending after a reopen continues the same branch.
    drop(s);
    let mut reopened = Session::open(&path).unwrap();
    reopened.append(assistant("done")).unwrap();
    assert_eq!(Session::read(&path).unwrap().messages().len(), 4);
}

#[test]
fn additional_dirs_round_trip_through_header() {
    let e = env();
    let mut s = new_session(&e);
    s.append(user("hi")).unwrap();
    let path = s.path().unwrap().to_path_buf();
    let dirs = vec![PathBuf::from("/tmp/extra-a"), PathBuf::from("/tmp/extra-b")];

    s.set_additional_dirs(&dirs).unwrap();
    assert_eq!(s.additional_dirs(), dirs.as_slice());
    let header: Value =
        serde_json::from_str(fs::read_to_string(&path).unwrap().lines().next().unwrap()).unwrap();
    assert_eq!(
        header["additional_dirs"],
        json!(["/tmp/extra-a", "/tmp/extra-b"])
    );

    // The writer was reopened on the rewritten file, so appends land after the new header.
    s.append(user("after")).unwrap();
    let reopened = Session::read(&path).unwrap();
    assert_eq!(reopened.additional_dirs(), dirs.as_slice());
    assert_eq!(reopened.messages(), s.messages());

    // Clearing drops the key, which is exactly the shape of a header written before this field.
    s.set_additional_dirs(&[]).unwrap();
    let header: Value =
        serde_json::from_str(fs::read_to_string(&path).unwrap().lines().next().unwrap()).unwrap();
    assert!(header.get("additional_dirs").is_none());
    assert!(Session::read(&path).unwrap().additional_dirs().is_empty());

    // Forks inherit the parent's roots in their own header, and stay independent afterwards.
    s.set_additional_dirs(&dirs).unwrap();
    let mut fork = s.fork(&e.layout).unwrap();
    assert_eq!(fork.additional_dirs(), dirs.as_slice());
    let fork_path = fork.path().unwrap().to_path_buf();
    let fork_header: Value = serde_json::from_str(
        fs::read_to_string(&fork_path)
            .unwrap()
            .lines()
            .next()
            .unwrap(),
    )
    .unwrap();
    assert_eq!(
        fork_header["additional_dirs"],
        json!(["/tmp/extra-a", "/tmp/extra-b"])
    );
    assert_eq!(
        Session::read(&fork_path).unwrap().additional_dirs(),
        dirs.as_slice()
    );
    fork.set_additional_dirs(&[]).unwrap();
    assert_eq!(
        Session::read(&path).unwrap().additional_dirs(),
        dirs.as_slice()
    );
}

#[test]
fn control_state_journals_only_changed_keys_and_reloads_merged() {
    let e = env();
    let mut session = new_session(&e);
    session
        .set_control_state(json!({"big":"x".repeat(4000),"step":1,"gone":true}))
        .unwrap();
    session
        .set_control_state(json!({"big":"x".repeat(4000),"step":2}))
        .unwrap();
    let last = match &session.entries.last().unwrap().kind {
        Kind::Message(m) => m.clone(),
        _ => panic!("state entry"),
    };
    assert_eq!(last["patch"], json!({"step":2,"gone":null}));
    assert!(last.get("state").is_none());
    let expected = json!({"big":"x".repeat(4000),"step":2});
    assert_eq!(session.control_state(), expected);
    let path = session.path().unwrap().to_path_buf();
    drop(session);
    assert_eq!(Session::open(&path).unwrap().control_state(), expected);
}

#[test]
fn append_rejects_non_messages() {
    let e = env();
    let mut s = new_session(&e);
    assert!(s.append(json!("just a string")).is_err());
    assert!(s.append(json!({"content": "no role"})).is_err());
    assert!(s.messages().is_empty());
}

#[test]
fn ephemeral_sessions_touch_no_disk() {
    let mut s = Session::ephemeral();
    s.append(user("x")).unwrap();
    s.compact("sum".into(), 0).unwrap();
    s.append(user("y")).unwrap();
    assert!(s.path().is_none());
    assert_eq!(contents(&s.messages()).len(), 2);
    assert!(s.messages()[0]["content"].as_str().unwrap().contains("sum"));
}

#[test]
fn compaction_keeps_tail_summary_first_and_survives_reopen() {
    let e = env();
    let mut s = new_session(&e);
    for i in 0..4 {
        s.append(user(&format!("q{i}"))).unwrap();
        s.append(assistant(&format!("a{i}"))).unwrap();
    }
    s.compact("SUMMARY-ONE".into(), 3).unwrap();
    let m = s.messages();
    assert_eq!(m[0]["role"], "user");
    let head = m[0]["content"].as_str().unwrap();
    assert!(
        head.starts_with("<summary>") && head.contains("SUMMARY-ONE"),
        "{head}"
    );
    assert_eq!(contents(&m[1..]), ["a2", "q3", "a3"].map(String::from));

    s.append(user("after")).unwrap();
    let reopened = Session::read(s.path().unwrap()).unwrap();
    assert_eq!(reopened.messages(), s.messages());
    assert_eq!(
        contents(&reopened.messages()[1..]),
        ["a2", "q3", "a3", "after"].map(String::from)
    );

    // A second compaction replaces the first summary and counts only what is still in view.
    drop(s);
    let mut s = Session::open(reopened.path().unwrap()).unwrap();
    s.compact("SUMMARY-TWO".into(), 1).unwrap();
    let m = s.messages();
    assert_eq!(m.len(), 2);
    assert!(m[0]["content"].as_str().unwrap().contains("SUMMARY-TWO"));
    assert!(!m[0]["content"].as_str().unwrap().contains("SUMMARY-ONE"));
    assert_eq!(content_text(&m[1]), "after");
}

#[test]
fn compaction_never_starts_on_an_orphaned_tool_result() {
    let e = env();
    let mut s = new_session(&e);
    s.append(user("go")).unwrap();
    s.append(json!({"role": "assistant", "content": null, "tool_calls": [{"id": "c", "type": "function", "function": {"name": "bash", "arguments": "{}"}}]})).unwrap();
    s.append(json!({"role": "tool", "tool_call_id": "c", "content": "out"}))
        .unwrap();
    s.append(assistant("finished")).unwrap();
    s.compact("S".into(), 2).unwrap(); // would cut exactly at the tool result
    let m = s.messages();
    assert_eq!(m[1]["role"], "assistant");
    assert!(m[1]["tool_calls"].is_array());
    assert_eq!(m[2]["role"], "tool");
    s.compact("S2".into(), 0).unwrap();
    assert_eq!(s.messages().len(), 1);
}

#[test]
fn rewind_branches_without_deleting_history() {
    let e = env();
    let mut s = new_session(&e);
    for i in 1..=3 {
        s.append(user(&format!("u{i}"))).unwrap();
        s.append(assistant(&format!("a{i}"))).unwrap();
    }
    assert_eq!(s.rewind(0).unwrap(), 0);
    assert_eq!(s.rewind(1).unwrap(), 2);
    assert_eq!(
        contents(&s.messages()),
        ["u1", "a1", "u2", "a2"].map(String::from)
    );
    s.append(user("u3b")).unwrap();
    let path = s.path().unwrap().to_path_buf();

    let reopened = Session::read(&path).unwrap();
    assert_eq!(
        contents(&reopened.messages()),
        ["u1", "a1", "u2", "a2", "u3b"].map(String::from)
    );
    // The abandoned branch is still in the file.
    assert!(fs::read_to_string(&path).unwrap().contains("\"u3\""));

    drop(s);
    let mut s = Session::open(&path).unwrap();
    assert_eq!(s.rewind(2).unwrap(), 3); // u2 a2 u3b
    assert_eq!(contents(&s.messages()), ["u1", "a1"].map(String::from));
    // More turns than exist clamps to the start.
    assert_eq!(s.rewind(99).unwrap(), 2);
    assert!(s.messages().is_empty());
    s.append(user("fresh")).unwrap();
    assert_eq!(
        contents(&Session::read(&path).unwrap().messages()),
        ["fresh".to_string()]
    );
    drop(s);
    assert_eq!(Session::open(&path).unwrap().rewind(1).unwrap(), 1);
}

#[test]
fn fork_copies_only_the_active_branch_and_diverges() {
    let e = env();
    let mut s = new_session(&e);
    s.append(user("u1")).unwrap();
    s.append(assistant("a1")).unwrap();
    s.append(user("u2")).unwrap();
    s.rewind(1).unwrap();
    s.append(user("u2b")).unwrap();
    s.compact("SUM".into(), 2).unwrap();

    let mut f = s.fork(&e.layout).unwrap();
    assert_ne!(f.id(), s.id());
    assert_ne!(f.path(), s.path());
    assert_eq!(f.messages(), s.messages());
    let text = fs::read_to_string(f.path().unwrap()).unwrap();
    assert!(!text.contains("\"u2\"") && text.contains("forked_from"));
    f.append(user("only in fork")).unwrap();
    let reopened = Session::read(f.path().unwrap()).unwrap();
    assert_eq!(reopened.messages(), f.messages());
    assert_eq!(s.messages().len(), 3);
}

#[test]
fn share_records_are_the_active_branch_without_abandoned_or_internal_records() {
    let e = env();
    let mut s = new_session(&e);
    s.append(user("u1")).unwrap();
    s.append(assistant("a1")).unwrap();
    s.append(user("abandoned")).unwrap();
    s.rewind(1).unwrap();
    s.append(user("kept")).unwrap();
    let records = s.share_records();
    assert_eq!(records[0]["type"], "session");
    let text = serde_json::to_string(&records).unwrap();
    assert!(
        text.contains("\"kept\"") && text.contains("\"u1\""),
        "{text}"
    );
    assert!(!text.contains("abandoned"), "{text}");
    let journal = fs::read_to_string(s.path().unwrap()).unwrap();
    assert!(journal.contains("abandoned"));
}

#[test]
fn list_is_newest_first_with_titles_and_counts() {
    let e = env();
    assert!(Session::list(&e.layout, 10).unwrap().is_empty());
    assert!(Session::list_newest(&e.layout, 1).unwrap().is_empty());
    let mut ids = Vec::new();
    for i in 0..3 {
        let mut s = new_session(&e);
        s.append(user(&format!("topic   number {i}\nwith newline")))
            .unwrap();
        s.append(assistant("ok")).unwrap();
        ids.push(s.id().to_string());
    }
    let long = {
        let mut s = new_session(&e);
        s.append(user(&"x".repeat(500))).unwrap();
        s.id().to_string()
    };
    let list = Session::list(&e.layout, 10).unwrap();
    let order: Vec<_> = list.iter().map(|i| i.id.clone()).collect();
    assert_eq!(
        order,
        [long.clone(), ids[2].clone(), ids[1].clone(), ids[0].clone()]
    );
    assert_eq!(list[1].title, "topic number 2 with newline");
    assert_eq!(list[1].messages, 2);
    assert!(list[0].title.chars().count() <= TITLE_CHARS + 1 && list[0].title.ends_with('…'));
    assert!(list[0].started.ends_with('Z'));
    assert_eq!(Session::list(&e.layout, 2).unwrap().len(), 2);
    assert_eq!(
        Session::list_newest(&e.layout, 1).unwrap()[0].path,
        list[0].path
    );
    // Pins reorder the picker list only; "latest" selections still get the newest session.
    session_pins::toggle(&e.layout.user_dir, &ids[0]).unwrap();
    assert_eq!(Session::list(&e.layout, 10).unwrap()[0].id, ids[0]);
    assert_eq!(Session::list_newest(&e.layout, 10).unwrap()[0].id, long);
    assert_eq!(Session::list_newest(&e.layout, 1).unwrap()[0].id, long);
}

#[test]
fn listing_index_reuses_unchanged_summaries_and_refreshes_changed_ones() {
    let e = env();
    let mut s = new_session(&e);
    s.append(user("first question")).unwrap();
    let path = s.path().unwrap().to_path_buf();
    let listed = Session::list_for_picker(&e.layout, 10).unwrap();
    assert_eq!(listed[0].title, "first question");
    assert_eq!(listed[0].path, path);
    let index = e.layout.state_dir.join("session-index.json");
    assert!(index.is_file(), "the listing saves its summaries");
    // An append changes the journal's stamp, so the next listing parses it again.
    s.append(assistant("an answer")).unwrap();
    s.rename("Renamed").unwrap();
    let listed = Session::list_for_picker(&e.layout, 10).unwrap();
    assert_eq!(listed[0].title, "Renamed");
    assert_eq!(listed[0].last_answer, "an answer");
    assert_eq!(listed[0].messages, 2);
    // An unchanged journal is served from the index: a doctored summary shows through.
    let doctored = fs::read_to_string(&index)
        .unwrap()
        .replace("\"Renamed\"", "\"From the index\"");
    crate::platform::atomic::write_private(&index, doctored.as_bytes()).unwrap();
    assert_eq!(
        Session::list_newest(&e.layout, 10).unwrap()[0].title,
        "From the index"
    );
    // Deleted journals drop out of the index on the next save, and a corrupt index is ignored.
    drop(s);
    let other = {
        let mut other = new_session(&e);
        other.append(user("second")).unwrap();
        other.id().to_owned()
    };
    fs::remove_file(&path).unwrap();
    let listed = Session::list_for_picker(&e.layout, 10).unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].id, other);
    assert!(
        !fs::read_to_string(&index)
            .unwrap()
            .contains("From the index")
    );
    crate::platform::atomic::write_private(&index, b"not json").unwrap();
    assert_eq!(
        Session::list_for_picker(&e.layout, 10).unwrap()[0].id,
        other
    );
}

#[test]
fn picker_activity_pins_and_separate_empty_budget() {
    let e = env();
    let mut old = new_session(&e);
    old.append(user("older conversation")).unwrap();
    let mut newer = new_session(&e);
    newer.append(user("newer conversation")).unwrap();
    let mut empty_id = String::new();
    for _ in 0..201 {
        let mut empty = new_session(&e);
        empty
            .append(json!({"role":"user","content":"bootstrap","dial_transient":true}))
            .unwrap();
        empty_id = empty.id().to_owned();
    }
    old.rename("recent journal activity").unwrap();
    let last_ts = old.entries.last().unwrap().ts.clone();
    let rows = Session::list_for_picker(&e.layout, 1).unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].id, old.id());
    assert_eq!(rows[0].updated, last_ts);
    assert_eq!(rows[1].id, empty_id);
    assert_eq!(rows[1].user_messages, 0);
    assert!(rows[1].last_message.is_empty());
    assert_eq!(Session::list_newest(&e.layout, 1).unwrap()[0].id, empty_id);
    session_pins::toggle(&e.layout.user_dir, newer.id()).unwrap();
    let rows = Session::list_for_picker(&e.layout, 1).unwrap();
    assert_eq!(rows[0].id, newer.id());
    session_pins::toggle(&e.layout.user_dir, &empty_id).unwrap();
    let rows = Session::list_for_picker(&e.layout, 2).unwrap();
    assert_eq!(rows[0].id, empty_id);
    assert_eq!(rows[1].id, newer.id());
    assert_eq!(rows.len(), 4);
    assert!(Session::list_for_picker(&e.layout, 0).unwrap().is_empty());
}

#[test]
fn listing_helpers_pick_latest_conversation_report_project_and_lock_state() {
    let e = env();
    let mut old = new_session(&e);
    old.append(user("older conversation")).unwrap();
    old.append(assistant("the answer")).unwrap();
    let mut empty = new_session(&e);
    empty
        .append(json!({"role":"user","content":"bootstrap","dial_transient":true}))
        .unwrap();
    let rows = Session::list_by_activity(&e.layout, usize::MAX).unwrap();
    assert_eq!(rows.len(), 2);
    let empty_row = rows.iter().find(|s| s.id == empty.id()).unwrap();
    assert!(!empty_row.is_conversation());
    let old_row = rows.iter().find(|s| s.id == old.id()).unwrap();
    assert!(old_row.is_conversation());
    assert_eq!(latest_conversation(&rows).unwrap().id, old.id());
    assert_eq!(old_row.project, e.layout.project_root.display().to_string());
    assert_eq!(Session::count_for_picker(&e.layout, false).unwrap(), 2);
    let path = old.path().unwrap().to_owned();
    assert_eq!(
        Session::preview_turns(&path).unwrap(),
        [
            ("user".to_owned(), "older conversation".to_owned()),
            ("assistant".to_owned(), "the answer".to_owned())
        ]
    );
    // A live writer in this process holds the journal's lock, so another opener sees it busy.
    assert!(Session::open_elsewhere(&path));
    drop(old);
    assert!(!Session::open_elsewhere(&path));
    assert_eq!(
        Session::listing_notice(30, 45).as_deref(),
        Some("Showing 30 of 45 sessions; use --all for the full list")
    );
    assert!(Session::listing_notice(3, 3).is_none());
}

#[test]
fn picker_previews_filter_context_and_tools_and_bound_unicode() {
    let e = env();
    let mut s = new_session(&e);
    s.append(user("first request")).unwrap();
    let prompt = "雪".repeat(1300);
    let answer = "🦀".repeat(1300);
    s.append(json!({"role":"user","content":[{"type":"text","text":prompt}]}))
        .unwrap();
    s.append(assistant(&answer)).unwrap();
    for flag in [
        "dial_transient",
        "dial_portable_rules",
        "dial_context_notes",
        "dial_context_rollover",
    ] {
        for role in ["user", "assistant"] {
            let mut message = json!({"role":role,"content":"synthetic context"});
            message[flag] = json!(true);
            s.append(message).unwrap();
        }
    }
    s.append(json!({"role":"tool","content":"tool output"}))
        .unwrap();
    s.append(json!({"role":"assistant","content":null,"tool_calls":[]}))
        .unwrap();
    s.push(Kind::Compaction {
        summary: "synthetic summary".into(),
        first_kept: None,
        prefix: Some(vec![user("synthetic prefix")]),
    })
    .unwrap();
    s.push(Kind::ContextReset {
        bootstrap: vec![user("bootstrap request"), assistant("bootstrap answer")],
        notes: None,
        digest: String::new(),
    })
    .unwrap();
    let path = s.path().unwrap().to_owned();
    let before = fs::read(&path).unwrap();
    let rows = Session::list_for_picker(&e.layout, 10).unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].user_messages, 2);
    assert_eq!(
        rows[0].last_message,
        truncate_chars(&prompt, PICKER_PREVIEW_CHARS)
    );
    assert_eq!(
        rows[0].last_answer,
        truncate_chars(&answer, PICKER_PREVIEW_CHARS)
    );
    assert_eq!(
        rows[0].last_message.chars().count(),
        PICKER_PREVIEW_CHARS + 1
    );
    assert_eq!(
        rows[0].last_answer.chars().count(),
        PICKER_PREVIEW_CHARS + 1
    );
    assert_eq!(fs::read(&path).unwrap(), before);
    // An unparseable tail must neither change previews nor be healed by listing.
    OpenOptions::new()
        .append(true)
        .open(&path)
        .unwrap()
        .write_all(b"{torn")
        .unwrap();
    let torn = fs::read(&path).unwrap();
    let rows = Session::list_for_picker(&e.layout, 10).unwrap();
    assert_eq!(rows[0].user_messages, 2);
    assert_eq!(
        rows[0].last_answer,
        truncate_chars(&answer, PICKER_PREVIEW_CHARS)
    );
    assert_eq!(fs::read(&path).unwrap(), torn);
}

#[test]
fn picker_header_only_session_uses_creation_time() {
    let e = env();
    let s = new_session(&e);
    let rows = Session::list_for_picker(&e.layout, 1).unwrap();
    assert_eq!(rows[0].id, s.id());
    assert_eq!(rows[0].updated, rows[0].started);
    assert_eq!(rows[0].user_messages, 0);
    assert!(rows[0].last_message.is_empty() && rows[0].last_answer.is_empty());
}

#[test]
fn truncated_or_corrupt_tail_is_skipped_and_healed_on_next_append() {
    let e = env();
    let mut s = new_session(&e);
    s.append(user("one")).unwrap();
    s.append(assistant("two")).unwrap();
    let path = s.path().unwrap().to_path_buf();
    drop(s);
    let mut f = OpenOptions::new().append(true).open(&path).unwrap();
    f.write_all(b"{\"type\":\"message\",\"id\":\"zz\",\"parent\":null,\"message\":{\"role\":\"us")
        .unwrap();
    drop(f);

    let mut s = Session::open(&path).unwrap();
    assert_eq!(contents(&s.messages()), ["one", "two"].map(String::from));
    s.append(user("three")).unwrap();
    drop(s);
    let s = Session::read(&path).unwrap();
    assert_eq!(
        contents(&s.messages()),
        ["one", "two", "three"].map(String::from)
    );

    // Garbage and blank lines in the middle are skipped too.
    let mut f = OpenOptions::new().append(true).open(&path).unwrap();
    f.write_all(b"\nnot json at all\n[1,2]\n").unwrap();
    drop(f);
    let mut s = Session::open(&path).unwrap();
    s.append(user("four")).unwrap();
    assert_eq!(Session::read(&path).unwrap().messages().len(), 4);
}

#[test]
fn ids_from_disk_never_reach_paths_and_oversized_lines_are_skipped() {
    let e = env();
    let dir = e.layout.sessions_dir();
    crate::platform::atomic::private_dir(&dir).unwrap();
    let path = dir.join("20260101T000000000Z_abc123.jsonl");
    let huge = "y".repeat(MAX_LINE + 10);
    let body = format!(
        "{}\n{}\n{}\n",
        json!({"type": "session", "id": "../../etc/passwd", "started": "2026-01-01T00:00:00.000Z", "model": "m"}),
        json!({"type": "message", "id": "m1", "parent": null, "ts": "t", "message": {"role": "user", "content": huge}}),
        json!({"type": "message", "id": "m2", "parent": "m1", "ts": "t", "message": {"role": "user", "content": "small"}}),
    );
    fs::write(&path, body).unwrap();
    let s = Session::open(&path).unwrap();
    assert_eq!(s.id(), "abc123");
    // m1 was dropped as oversized, so m2's parent is missing and it becomes a root.
    assert_eq!(contents(&s.messages()), ["small".to_string()]);

    let mut s = s;
    let huge_id = s.append(user(&"z".repeat(MAX_LINE))).unwrap();
    assert_eq!(s.messages().len(), 2);
    assert!(
        s.messages().last().unwrap()["content"]
            .as_str()
            .unwrap()
            .contains("SESSION MESSAGE TRUNCATED")
    );
    assert!(!huge_id.is_empty());
    assert!(Session::open(&dir.join("nope.jsonl")).is_err());
}

#[test]
fn search_ranks_stems_and_follows_new_appends() {
    let e = env();
    let mut a = new_session(&e);
    a.append(user("how do I configure the database migrations?"))
        .unwrap();
    a.append(assistant("Run the migration tool against postgres."))
        .unwrap();
    let mut b = new_session(&e);
    b.append(user("unrelated chat about sourdough bread"))
        .unwrap();
    b.append(json!({"role": "assistant", "content": null, "tool_calls": [{"id": "c", "type": "function", "function": {"name": "frobnicate", "arguments": "{\"secret\":\"zebra\"}"}}]})).unwrap();
    b.append(json!({"role": "tool", "tool_call_id": "c", "content": format!("kiwi {}", "filler ".repeat(400))})).unwrap();
    b.append(json!({"role": "tool", "tool_call_id": "c", "content": "small tool output mango"}))
        .unwrap();

    let hits = Session::search(&e.layout, "migrations postgres", 5).unwrap();
    assert!(!hits.is_empty());
    assert_eq!(hits[0].session_id, a.id());
    assert_eq!(hits[0].role, "assistant"); // both words in one message ranks above one word
    assert!(hits[0].snippet.to_lowercase().contains("postgres"));
    assert!(hits[0].when.ends_with('Z'));
    // Stemming: "migrate" finds "migrations"/"migration".
    assert!(!Session::search(&e.layout, "migrate", 5).unwrap().is_empty());

    assert_eq!(
        Session::search(&e.layout, "frobnicate", 5).unwrap()[0].session_id,
        b.id()
    );
    assert!(
        Session::search(&e.layout, "zebra", 5).unwrap().is_empty(),
        "tool arguments are not indexed"
    );
    assert!(
        Session::search(&e.layout, "kiwi", 5).unwrap().is_empty(),
        "large tool bodies are not indexed"
    );
    assert_eq!(Session::search(&e.layout, "mango", 5).unwrap().len(), 1);
    // AND fails across messages -> falls back to OR.
    assert_eq!(
        Session::search(&e.layout, "sourdough postgres", 5)
            .unwrap()
            .len(),
        2
    );

    assert!(Session::search(&e.layout, "quokka", 5).unwrap().is_empty());
    b.append(user("now we discuss quokka habitats")).unwrap();
    assert_eq!(
        Session::search(&e.layout, "quokka", 5).unwrap()[0].session_id,
        b.id()
    );
    assert_eq!(
        Session::search(&e.layout, "migrations OR sourdough", 1)
            .unwrap()
            .len(),
        1
    );

    let db = e.layout.state_dir.join("sessions.sqlite3");
    #[cfg(unix)]
    assert_eq!(
        fs::metadata(db).unwrap().permissions().mode() & 0o777,
        0o600
    );
    #[cfg(windows)]
    assert!(crate::platform::fs::is_owner_only(&fs::File::open(db).unwrap()).unwrap());
}

#[test]
fn search_queries_cannot_inject_fts_syntax() {
    let e = env();
    let mut s = new_session(&e);
    s.append(user("foo and bar")).unwrap();
    s.append(user("OR is a word 1 time")).unwrap();
    for q in [
        "foo\" OR 1",
        "*",
        "NEAR(",
        "",
        "   ",
        "\"",
        "text:foo",
        "foo AND NOT bar",
        "(((",
        "foo*",
        "^foo",
        "a:b",
        "\\",
        "-foo",
        "{foo}",
        "foo\0bar",
    ] {
        let r = Session::search(&e.layout, q, 5);
        assert!(r.is_ok(), "query {q:?} errored: {:?}", r.err());
    }
    assert!(Session::search(&e.layout, "", 5).unwrap().is_empty());
    assert!(Session::search(&e.layout, "*", 5).unwrap().is_empty());
    // Operators are plain words: "foo\" OR 1" is the terms foo, OR, 1.
    let hits = Session::search(&e.layout, "foo\" OR 1", 5).unwrap();
    assert!(!hits.is_empty());
    assert!(
        !Session::search(&e.layout, "nonexistentword AND foo", 5)
            .unwrap()
            .is_empty()
    ); // OR fallback, not an FTS AND
}

#[test]
fn search_without_sessions_is_empty_and_forks_do_not_duplicate_hits() {
    let e = env();
    assert!(
        Session::search(&e.layout, "anything", 5)
            .unwrap()
            .is_empty()
    );
    assert!(!e.layout.state_dir.join("sessions.sqlite3").exists());
    let mut s = new_session(&e);
    s.append(user("the platypus problem")).unwrap();
    let mut f = s.fork(&e.layout).unwrap();
    assert_eq!(Session::search(&e.layout, "platypus", 10).unwrap().len(), 1);
    f.append(user("fork-only echidna")).unwrap();
    assert_eq!(
        Session::search(&e.layout, "echidna", 10).unwrap()[0].session_id,
        f.id()
    );
    assert_eq!(Session::search(&e.layout, "platypus", 10).unwrap().len(), 1);
    // Deleting a session file removes its hits.
    fs::remove_file(s.path().unwrap()).unwrap();
    assert!(
        Session::search(&e.layout, "platypus", 10)
            .unwrap()
            .is_empty()
    );
}

#[test]
fn search_tool_formats_hits_and_reports_bad_args() {
    let e = env();
    let mut s = new_session(&e);
    s.append(user("remember the wombat decision")).unwrap();
    let out = session_search_tool::call(&e.layout, &json!({"query": "wombat"}));
    assert!(
        !out.is_error && out.text.contains("wombat") && out.text.contains(s.id()),
        "{}",
        out.text
    );
    assert!(!session_search_tool::call(&e.layout, &json!({"query": "zzzz"})).is_error);
    assert!(session_search_tool::call(&e.layout, &json!({"limit": 3})).is_error);
    assert!(session_search_tool::call(&e.layout, &json!({"query": 7})).is_error);
    let schema = session_search_tool::schema();
    assert_eq!(schema["function"]["name"], "session_search");
    assert_eq!(
        schema["function"]["parameters"]["required"],
        json!(["query"])
    );
}

#[test]
fn iso_formats_known_instants() {
    assert_eq!(iso(0), "1970-01-01T00:00:00.000Z");
    assert_eq!(iso(951_782_400_000 + 1_234), "2000-02-29T00:00:01.234Z");
    assert_eq!(iso(1_790_000_000_000), "2026-09-21T14:13:20.000Z");
}

fn journal_header(id: &str) -> String {
    json!({"type": "session", "id": id, "started": "2026-01-01T00:00:00.000Z", "model": "m"})
        .to_string()
}

fn journal_message(id: &str, parent: Option<&str>, text: &str) -> String {
    json!({"type": "message", "id": id, "parent": parent, "ts": "t",
        "message": {"role": "user", "content": text}})
    .to_string()
}

fn write_journal(dir: &Path, name: &str, body: &[u8]) -> PathBuf {
    crate::platform::atomic::private_dir(dir).unwrap();
    let path = dir.join(name);
    fs::write(&path, body).unwrap();
    path
}

#[test]
fn torn_tail_is_reported_tolerated_and_left_byte_for_byte() {
    let e = env();
    let mut s = new_session(&e);
    s.append(user("one")).unwrap();
    s.append(assistant("two")).unwrap();
    let path = s.path().unwrap().to_path_buf();
    drop(s);
    let mut f = OpenOptions::new().append(true).open(&path).unwrap();
    f.write_all(b"{\"type\":\"message\",\"id\":\"zz\"").unwrap();
    drop(f);
    let before = fs::read(&path).unwrap();

    let mut read = Session::read(&path).unwrap();
    assert_eq!(contents(&read.messages()), ["one", "two"].map(String::from));
    let found = read.take_recovery_diagnostics();
    assert_eq!(found.len(), 1, "{found:?}");
    assert_eq!(found[0].kind, JournalIssueKind::TornTail);
    assert_eq!(
        found[0].line,
        before.iter().filter(|&&byte| byte == b'\n').count() + 1
    );
    assert_eq!(
        serde_json::to_value(&found[0]).unwrap()["kind"],
        json!("torn_tail")
    );
    // Draining is one-shot, and inspection never rewrites the source journal.
    assert!(read.take_recovery_diagnostics().is_empty());
    assert_eq!(fs::read(&path).unwrap(), before);
}

#[test]
fn malformed_interior_and_torn_tail_are_distinguished_and_recovered_around() {
    let dir = tempfile::tempdir().unwrap();
    let mut body = String::new();
    body += &format!("{}\n", journal_header("abc123"));
    body += &format!("{}\n", journal_message("m1", None, "one"));
    body += "not json at all\n";
    body += &format!("{}\n", journal_message("m2", Some("m1"), "two"));
    body += "{\"type\":\"message\",\"id\":\"m3\"";
    let path = write_journal(
        dir.path(),
        "20260101T000000000Z_abc123.jsonl",
        body.as_bytes(),
    );
    let before = fs::read(&path).unwrap();

    let mut read = Session::read(&path).unwrap();
    // The interior garbage is skipped; the chain through it is intact and still loads.
    assert_eq!(contents(&read.messages()), ["one", "two"].map(String::from));
    let found = read.take_recovery_diagnostics();
    let kinds: Vec<_> = found.iter().map(|d| (d.kind, d.line)).collect();
    assert_eq!(
        kinds,
        [
            (JournalIssueKind::MalformedInterior, 3),
            (JournalIssueKind::TornTail, 5)
        ]
    );
    assert_eq!(fs::read(&path).unwrap(), before);
}

#[test]
fn oversized_interior_line_is_reported_and_its_children_show_missing_parent() {
    let dir = tempfile::tempdir().unwrap();
    let huge = "y".repeat(MAX_LINE + 10);
    let huge_line = json!({"type": "message", "id": "m1", "parent": null, "ts": "t",
        "message": {"role": "user", "content": huge}})
    .to_string();
    let mut body = format!("{}\n", journal_header("abc124"));
    body += &format!("{huge_line}\n");
    body += &format!("{}\n", journal_message("m2", Some("m1"), "small"));
    let path = write_journal(
        dir.path(),
        "20260101T000000000Z_abc124.jsonl",
        body.as_bytes(),
    );
    let mut read = Session::read(&path).unwrap();
    assert_eq!(contents(&read.messages()), ["small".to_string()]);
    let found = read.take_recovery_diagnostics();
    assert_eq!(found.len(), 2, "{found:?}");
    assert_eq!(found[0].kind, JournalIssueKind::OversizedInterior);
    assert_eq!(found[0].line, 2);
    assert_eq!(found[1].kind, JournalIssueKind::MissingParent);
    assert_eq!(found[1].line, 3);
    assert_eq!(found[1].entry_id.as_deref(), Some("m2"));
}

#[test]
fn missing_parent_truncates_only_the_unreachable_prefix_and_is_reported() {
    let dir = tempfile::tempdir().unwrap();
    let mut body = format!("{}\n", journal_header("abc125"));
    body += &format!("{}\n", journal_message("m2", Some("gone"), "two"));
    body += &format!("{}\n", journal_message("m3", Some("m2"), "three"));
    let path = write_journal(
        dir.path(),
        "20260101T000000000Z_abc125.jsonl",
        body.as_bytes(),
    );
    let before = fs::read(&path).unwrap();
    let mut read = Session::read(&path).unwrap();
    assert_eq!(
        contents(&read.messages()),
        ["two", "three"].map(String::from)
    );
    let found = read.take_recovery_diagnostics();
    assert_eq!(found.len(), 1, "{found:?}");
    assert_eq!(found[0].kind, JournalIssueKind::MissingParent);
    assert_eq!(found[0].line, 2);
    assert_eq!(found[0].entry_id.as_deref(), Some("m2"));
    assert_eq!(fs::read(&path).unwrap(), before);
}

#[test]
fn compaction_advances_history_generation_and_rewind_keeps_it_monotonic() {
    let e = env();
    let mut s = new_session(&e);
    for text in ["one", "two", "three", "four"] {
        s.append(user(text)).unwrap();
    }
    assert_eq!(s.context_history_generation(), 0);
    s.compact("summary".into(), 2).unwrap();
    assert_eq!(s.context_history_generation(), 1);
    // A real rewind removes facts from context, so it mints a new generation; the session-monotonic
    // provider generation is never reused across the rewind or after reopen.
    s.rewind(1).unwrap();
    assert_eq!(s.context_history_generation(), 2);
    // A no-op rewind (no user message in view) mints nothing.
    assert_eq!(s.rewind(0).unwrap(), 0);
    assert_eq!(s.context_history_generation(), 2);
    let path = s.path().unwrap().to_path_buf();
    drop(s);
    assert_eq!(
        Session::read(&path).unwrap().context_history_generation(),
        2
    );
}

#[test]
fn rewind_generation_increments_from_the_current_value_across_non_head_parents_and_reopen() {
    let e = env();
    let mut s = new_session(&e);
    for text in ["one", "two", "three"] {
        s.append(user(text)).unwrap();
        s.append(assistant(&format!("a-{text}"))).unwrap();
    }
    s.compact("summary".into(), 2).unwrap();
    assert_eq!(s.context_history_generation(), 1);
    assert!(s.rewind(1).unwrap() > 0);
    assert_eq!(s.context_history_generation(), 2);
    // Appending below the rewind point makes the next marker's parent a non-head entry.
    s.append(user("four")).unwrap();
    s.append(assistant("a-four")).unwrap();
    assert!(s.rewind(1).unwrap() > 0);
    assert_eq!(s.context_history_generation(), 3);
    assert_eq!(
        s.control_state()["provider_identity"]["history_generation"],
        3
    );
    let path = s.path().unwrap().to_path_buf();
    drop(s);
    {
        let read = Session::read(&path).unwrap();
        assert_eq!(read.context_history_generation(), 3);
        assert_eq!(
            read.control_state()["provider_identity"]["history_generation"],
            3
        );
    }
    // A reopened writer continues from the durable floor, not from the branch it lands on.
    let mut writer = Session::open(&path).unwrap();
    writer.append(user("five")).unwrap();
    writer.append(assistant("a-five")).unwrap();
    assert!(writer.rewind(1).unwrap() > 0);
    assert_eq!(writer.context_history_generation(), 4);
    drop(writer);
    assert_eq!(
        Session::read(&path).unwrap().context_history_generation(),
        4
    );
}

#[test]
fn context_reset_rewind_mints_a_generation_and_stays_monotonic_after_reopen() {
    let e = env();
    let mut s = new_session(&e);
    s.append(user("one")).unwrap();
    s.append(assistant("a1")).unwrap();
    s.append(user("two")).unwrap();
    s.append(assistant("a2")).unwrap();
    let owner = s.id().to_owned();
    let leaf = s.branch_leaf().map(str::to_owned);
    // The bootstrap user equals its source entry, so the reset resolves to ordinary rewind targets.
    s.reset_context(
        &owner,
        leaf.as_deref(),
        vec![user("two"), assistant("a2")],
        false,
        "a".repeat(64),
    )
    .unwrap();
    assert_eq!(s.context_history_generation(), 1);
    assert!(s.context_rewind_targets().is_some());
    // A no-op context rewind mints nothing.
    assert_eq!(s.rewind(0).unwrap(), 0);
    assert_eq!(s.context_history_generation(), 1);
    assert!(s.rewind(1).unwrap() > 0);
    assert_eq!(s.context_history_generation(), 2);
    let path = s.path().unwrap().to_path_buf();
    drop(s);
    assert_eq!(
        Session::read(&path).unwrap().context_history_generation(),
        2
    );
}

/// Reads a complete artifact by following `retrieve_bytes` pages; the store caps one page at 16 KiB.
fn artifact_text(path: &Path, uri: &str) -> String {
    use base64::Engine;
    let store =
        crate::storage::artifact::ArtifactStore::new(&artifact_root(path).unwrap()).unwrap();
    let mut bytes = Vec::new();
    let mut offset = 0u64;
    loop {
        let page: Value =
            serde_json::from_str(&store.retrieve_bytes(uri, offset, 16384).unwrap()).unwrap();
        let data = base64::engine::general_purpose::STANDARD
            .decode(page["data"].as_str().unwrap())
            .unwrap();
        let next = page["next_offset"].as_u64().unwrap();
        bytes.extend_from_slice(&data);
        if next >= page["total_bytes"].as_u64().unwrap() {
            break;
        }
        assert!(next > offset, "artifact pages must make progress");
        offset = next;
    }
    String::from_utf8(bytes).unwrap()
}

/// Multi-byte text puts the 256 KiB inline bound on a char boundary; the rest of the record is kept.
fn oversized_text(head: &str, unit: &str, bytes: usize) -> String {
    let mut text = head.to_owned();
    while text.len() < bytes {
        text.push_str(unit);
    }
    text.push_str("TAIL-END");
    text
}

#[test]
fn oversized_compaction_summary_is_bounded_and_recovers_the_complete_artifact_after_reopen() {
    let e = env();
    let mut s = new_session(&e);
    for text in ["one", "two"] {
        s.append(user(text)).unwrap();
        s.append(assistant(&format!("a-{text}"))).unwrap();
    }
    let summary = oversized_text("HEAD-", "é", 9 * 1024 * 1024);
    s.compact(summary.clone(), 2).unwrap();
    let path = s.path().unwrap().to_path_buf();
    let line_bytes = std::fs::read_to_string(&path)
        .unwrap()
        .lines()
        .map(str::len)
        .max()
        .unwrap();
    assert!(
        line_bytes < MAX_LINE,
        "record {line_bytes} bytes must be bounded"
    );
    drop(s);

    let reopened = Session::read(&path).unwrap();
    let stored = reopened.latest_compaction_summary().unwrap().to_owned();
    assert!(
        stored.len() < summary.len(),
        "the journal keeps a bounded view"
    );
    assert!(stored.starts_with("HEAD-") && stored.ends_with("TAIL-END"));
    assert!(stored.contains("SESSION MESSAGE TRUNCATED"));
    let uri = crate::storage::artifact::uris(&stored)[0];
    assert_eq!(artifact_text(&path, uri), summary);
    // The recovered artifact is the complete original, not the marker-bearing view.
    let mut writer = Session::open(&path).unwrap();
    writer.append(user("after")).unwrap();
    assert_eq!(
        Session::read(&path)
            .unwrap()
            .latest_compaction_summary()
            .unwrap(),
        stored
    );
}

#[test]
fn oversized_compaction_prefix_recovers_the_complete_message_after_reopen() {
    let e = env();
    let mut s = new_session(&e);
    s.append(user("tail")).unwrap();
    let big = oversized_text("PREFIX-", "ß", 9 * 1024 * 1024);
    s.compact_with_prefix(
        "summary".into(),
        Some(vec![user(&big), assistant("prefix-answer")]),
        1,
    )
    .unwrap();
    let path = s.path().unwrap().to_path_buf();
    drop(s);

    let reopened = Session::read(&path).unwrap();
    let messages = reopened.messages();
    let stored = messages[0]["content"].as_str().unwrap();
    assert!(stored.len() < big.len() && stored.contains("SESSION MESSAGE TRUNCATED"));
    assert_eq!(messages[1]["content"], "prefix-answer");
    let uri = crate::storage::artifact::uris(stored)[0];
    assert_eq!(artifact_text(&path, uri), big);
}

#[test]
fn message_deltas_visit_only_new_bodies_as_history_grows() {
    let mut session = Session::ephemeral();
    let mut archive_cursor = MessageCursor::default();
    let mut view_cursor = MessageCursor::default();
    let mut archive = Vec::new();
    let mut view = Vec::new();
    let mut visited = 0;
    for n in 0..512 {
        session.append(user(&format!("turn {n}"))).unwrap();
        for (cursor, out, archived) in [
            (&mut archive_cursor, &mut archive, true),
            (&mut view_cursor, &mut view, false),
        ] {
            let delta = session.message_delta(cursor, archived);
            assert_eq!(delta.reset, n == 0);
            assert_eq!(delta.messages.len(), 1);
            assert!(matches!(delta.messages[0], std::borrow::Cow::Borrowed(_)));
            visited += delta.messages.len();
            if delta.reset {
                out.clear();
            }
            out.extend(delta.messages.into_iter().map(|m| m.into_owned()));
            assert!(session.message_delta(cursor, archived).messages.is_empty());
        }
    }
    assert_eq!(
        visited, 1024,
        "two projections must visit 2N bodies, not N(N+1)"
    );
    assert_eq!(archive, session.archived_messages());
    assert_eq!(view, session.messages());
    session.set_control_state(json!({"turn":512})).unwrap();
    assert!(
        session
            .message_delta(&mut view_cursor, false)
            .messages
            .is_empty()
    );
    session.compact("summary".into(), 2).unwrap();
    assert!(!session.message_delta(&mut archive_cursor, true).reset);
    let delta = session.message_delta(&mut view_cursor, false);
    assert!(delta.reset);
    assert_eq!(
        delta
            .messages
            .into_iter()
            .map(|m| m.into_owned())
            .collect::<Vec<_>>(),
        session.messages()
    );
    session.rewind(1).unwrap();
    for (cursor, archived) in [(&mut archive_cursor, true), (&mut view_cursor, false)] {
        let delta = session.message_delta(cursor, archived);
        assert!(delta.reset);
        assert_eq!(
            delta
                .messages
                .into_iter()
                .map(|m| m.into_owned())
                .collect::<Vec<_>>(),
            if archived {
                session.archived_messages()
            } else {
                session.messages()
            }
        );
    }
}

fn session_files(e: &Env) -> Vec<PathBuf> {
    let mut files: Vec<PathBuf> = fs::read_dir(e.layout.sessions_dir())
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|p| p.extension().is_some_and(|ext| ext == "jsonl"))
        .collect();
    files.sort();
    files
}

#[test]
fn resume_opens_an_unowned_journal_in_place_without_forking() {
    let e = env();
    let mut s = new_session(&e);
    s.append(user("resume me")).unwrap();
    let path = s.path().unwrap().to_owned();
    drop(s);
    let before = session_files(&e);
    match Session::open_or_fork(&e.layout, &path).unwrap() {
        Resumed::Owned(resumed) => assert_eq!(contents(&resumed.messages()), ["resume me"]),
        Resumed::Forked { .. } => panic!("an unowned journal must open in place"),
    }
    assert_eq!(session_files(&e), before);
}

#[test]
fn busy_resume_forks_the_active_branch_and_leaves_the_owner_byte_for_byte() {
    let e = env();
    let mut owner = new_session(&e);
    owner.append(user("older conversation")).unwrap();
    owner.append(assistant("the answer")).unwrap();
    let path = owner.path().unwrap().to_owned();
    let owner_id = owner.id().to_owned();
    let owner_bytes = fs::read(&path).unwrap();
    let before = session_files(&e);
    match Session::open_or_fork(&e.layout, &path).unwrap() {
        Resumed::Forked { source_id, session } => {
            assert_eq!(source_id, owner_id);
            assert_ne!(session.id(), owner_id);
            let fork_path = session.path().unwrap().to_owned();
            assert_ne!(fork_path, path);
            let header = fs::read_to_string(&fork_path).unwrap();
            assert!(header.contains(&format!("\"forked_from\":\"{owner_id}\"")));
            assert_eq!(
                contents(&session.messages()),
                ["older conversation", "the answer"]
            );
        }
        Resumed::Owned(_) => panic!("a busy journal must not be claimed twice"),
    }
    assert_eq!(fs::read(&path).unwrap(), owner_bytes);
    assert_eq!(session_files(&e).len(), before.len() + 1);
    drop(owner);
}

#[test]
fn busy_resume_fork_carries_the_owners_live_stream_supplement() {
    let e = env();
    let mut owner = new_session(&e);
    owner.append(user("stream until interrupted")).unwrap();
    let tree = owner.runtime();
    let answer = tree.start("assistant", "answer", None);
    answer.append("visible prefix");
    let path = owner.path().unwrap().to_owned();
    drop(answer);
    drop(tree);
    match Session::open_or_fork(&e.layout, &path).unwrap() {
        Resumed::Forked { session, .. } => {
            let bodies = session.runtime().bodies("assistant");
            assert_eq!(bodies.len(), 1);
            let body = bodies[0].snapshot();
            assert_eq!(body.result.text, "visible prefix");
            assert_eq!(body.result.phase, crate::runtime::Phase::Interrupted);
        }
        Resumed::Owned(_) => panic!("a busy journal must not be claimed twice"),
    }
    drop(owner);
}

#[test]
fn busy_resume_tolerates_a_torn_tail_and_leaves_its_bytes_alone() {
    let e = env();
    let mut owner = new_session(&e);
    owner.append(user("before the crash")).unwrap();
    let path = owner.path().unwrap().to_owned();
    let mut tail = OpenOptions::new().append(true).open(&path).unwrap();
    tail.write_all(b"{\"type\":\"message\",\"id\":\"torn")
        .unwrap();
    drop(tail);
    let owner_bytes = fs::read(&path).unwrap();
    match Session::open_or_fork(&e.layout, &path).unwrap() {
        Resumed::Forked { session, .. } => {
            assert_eq!(contents(&session.messages()), ["before the crash"]);
        }
        Resumed::Owned(_) => panic!("a busy journal must not be claimed twice"),
    }
    assert_eq!(fs::read(&path).unwrap(), owner_bytes);
    drop(owner);
}

#[test]
fn busy_resume_refuses_to_fork_over_an_interior_integrity_issue() {
    let e = env();
    let mut owner = new_session(&e);
    owner.append(user("first")).unwrap();
    let path = owner.path().unwrap().to_owned();
    let mut garbage = OpenOptions::new().append(true).open(&path).unwrap();
    garbage.write_all(b"not json at all\n").unwrap();
    drop(garbage);
    owner.append(user("after the garbage")).unwrap();
    let owner_bytes = fs::read(&path).unwrap();
    let before = session_files(&e);
    let error = Session::open_or_fork(&e.layout, &path)
        .err()
        .expect("corruption must be an error, not a fork");
    assert!(format!("{error:#}").contains("integrity issue"));
    assert_eq!(session_files(&e), before);
    assert_eq!(fs::read(&path).unwrap(), owner_bytes);
    drop(owner);
}

fn current_leaf(s: &Session) -> Option<String> {
    s.tree().into_iter().find(|n| n.current).map(|n| n.id)
}

#[test]
fn fork_at_an_old_anchor_excludes_the_later_main_branch_and_leaves_the_source_untouched() {
    let e = env();
    let mut owner = new_session(&e);
    owner.append(user("q1")).unwrap();
    let a1 = owner.append(assistant("a1")).unwrap();
    let c = owner.append(user("main later")).unwrap();
    owner.append(assistant("main answer")).unwrap();
    // Rewind to before `main later` and ask a side question: the main branch is now inactive.
    owner.navigate_to(&c).unwrap();
    owner.append(user("side question")).unwrap();
    let path = owner.path().unwrap().to_owned();
    let owner_bytes = fs::read(&path).unwrap();
    let leaf_before = current_leaf(&owner);
    let active_before = contents(&owner.messages());
    let before = session_files(&e);

    let fork = owner.fork_at(&e.layout, Some(&a1)).unwrap();

    assert_eq!(contents(&fork.messages()), ["q1", "a1"]);
    assert_eq!(fs::read(&path).unwrap(), owner_bytes);
    assert_eq!(current_leaf(&owner), leaf_before);
    assert_eq!(contents(&owner.messages()), active_before);
    assert_eq!(session_files(&e).len(), before.len() + 1);
}

#[test]
fn fork_at_an_inactive_branch_entry_imports_that_branch_and_not_the_side_question() {
    let e = env();
    let mut owner = new_session(&e);
    owner.append(user("q1")).unwrap();
    owner.append(assistant("a1")).unwrap();
    let c = owner.append(user("main later")).unwrap();
    let d = owner.append(assistant("main answer")).unwrap();
    owner.navigate_to(&c).unwrap();
    owner.append(user("side question")).unwrap();

    let fork = owner.fork_at(&e.layout, Some(&d)).unwrap();

    assert_eq!(
        contents(&fork.messages()),
        ["q1", "a1", "main later", "main answer"]
    );
}

#[test]
fn fork_at_no_target_creates_an_empty_ancestry_and_leaves_the_source_untouched() {
    let e = env();
    let mut owner = new_session(&e);
    owner.append(user("q1")).unwrap();
    owner.append(assistant("a1")).unwrap();
    let path = owner.path().unwrap().to_owned();
    let owner_id = owner.id().to_owned();
    let owner_bytes = fs::read(&path).unwrap();
    let leaf_before = current_leaf(&owner);

    let root = owner.fork_at(&e.layout, None).unwrap();

    assert!(root.messages().is_empty());
    assert_ne!(root.id(), owner_id);
    assert_eq!(fs::read(&path).unwrap(), owner_bytes);
    assert_eq!(current_leaf(&owner), leaf_before);
}

#[test]
fn fork_at_an_unknown_entry_is_an_error_and_creates_no_file() {
    let e = env();
    let mut owner = new_session(&e);
    owner.append(user("q1")).unwrap();
    let before = session_files(&e);

    assert!(owner.fork_at(&e.layout, Some("no-such-entry")).is_err());
    assert_eq!(session_files(&e), before);
}
