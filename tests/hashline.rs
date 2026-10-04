//! Hashline read/edit/write exercised through `tools::execute` on temp workspaces.

use dial::tools::hashline::tag_of;
use dial::tools::{EditMode, ToolOutput, WriteObserver, Workspace, execute, schemas};
use serde_json::json;
use std::fs;
use std::path::Path;
use parking_lot::Mutex;
use std::sync::Arc;

fn workspace() -> (tempfile::TempDir, Workspace) {
    let dir = tempfile::tempdir().unwrap();
    let ws = Workspace::new(dir.path()).unwrap();
    (dir, ws)
}

fn run(ws: &Workspace, name: &str, args: serde_json::Value) -> ToolOutput {
    execute(ws, name, &args, 20)
}

fn read(ws: &Workspace, path: &str) -> ToolOutput {
    run(ws, "read", json!({ "path": path }))
}

fn patch(ws: &Workspace, input: &str) -> ToolOutput {
    run(ws, "edit", json!({ "input": input }))
}

/// The tag in the first `[path#TAG]` header of `text`.
fn tag_in(text: &str, path: &str) -> String {
    let marker = format!("[{path}#");
    let at = text.find(&marker).unwrap_or_else(|| panic!("no header for {path} in {text}"));
    text[at + marker.len()..at + marker.len() + 4].to_owned()
}

fn read_tag(ws: &Workspace, path: &str) -> String {
    let out = read(ws, path);
    assert!(!out.is_error, "{}", out.text);
    tag_in(&out.text, path.split(':').next().unwrap())
}

fn numbered(n: usize) -> String {
    (1..=n).map(|i| format!("l{i}\n")).collect()
}

fn content(dir: &tempfile::TempDir, name: &str) -> String {
    fs::read_to_string(dir.path().join(name)).unwrap()
}

#[test]
fn missing_put_body_prefix_explains_the_correction_without_writing() {
    let (dir, ws) = workspace();
    fs::write(dir.path().join("f.rs"), "original\n").unwrap();
    let tag = read_tag(&ws, "f.rs");
    let bad = patch(&ws, &format!("[f.rs#{tag}]\nPUT 1.=1:\n+new\n    unchanged\n"));
    assert!(bad.is_error && bad.text.contains("missing `+` prefix"), "{}", bad.text);
    assert!(bad.text.contains("+    unchanged"));
    assert_eq!(content(&dir, "f.rs"), "original\n");
    assert!(!patch(&ws, &format!("[f.rs#{tag}]\nPUT 1.=1:\n+new\n+    unchanged\n")).is_error);
    assert_eq!(content(&dir, "f.rs"), "new\n    unchanged\n");
}

#[test]
fn read_prints_tag_header_and_anchored_rows_with_selectors() {
    let (dir, ws) = workspace();
    fs::write(dir.path().join("f.txt"), "alpha\nbeta\ngamma\n").unwrap();
    let out = read(&ws, "f.txt");
    assert!(!out.is_error);
    assert_eq!(out.text, format!("[f.txt#{}]\n1:alpha\n2:beta\n3:gamma\n", tag_of("alpha\nbeta\ngamma\n")));
    assert!(read(&ws, "f.txt:2-3").text.ends_with("2:beta\n3:gamma\n[lines 2-3 of 3]\n"));
    assert!(read(&ws, "f.txt:-1").text.ends_with("]\n3:gamma\n[lines 3 of 3]\n"));
    assert_eq!(read(&ws, "f.txt:raw").text, "alpha\nbeta\ngamma\n");
    assert_eq!(read(&ws, "f.txt:raw:2-2").text, "beta\n");
    assert_eq!(read(&ws, "f.txt:2+1:raw").text, "beta\n");
    let paged = run(&ws, "read", json!({"path": "f.txt", "offset": 2, "limit": 1}));
    assert!(paged.text.contains("2:beta\n") && !paged.text.contains("gamma"));
    assert!(paged.text.ends_with("[lines 2 of 3; continue with `f.txt:3-`]\n"), "{}", paged.text);
    let past = read(&ws, "f.txt:9-");
    assert!(past.text.contains("no lines in that range"), "{}", past.text);
    assert!(read(&ws, "f.txt:0").is_error);
    // A file whose name looks like a selector is read literally.
    fs::write(dir.path().join("weird:5"), "x\n").unwrap();
    assert!(read(&ws, "weird:5").text.contains("1:x"));
    fs::write(dir.path().join("empty"), "").unwrap();
    assert!(read(&ws, "empty").text.contains("(empty file)"));
    fs::write(dir.path().join("bin"), b"a\0b").unwrap();
    assert!(read(&ws, "bin").is_error);
    let listing = read(&ws, ".");
    assert!(listing.text.contains("f.txt") && listing.text.contains("directory"));
}

#[test]
fn read_is_bounded_and_footer_names_the_next_range() {
    let (dir, ws) = workspace();
    let line = "x".repeat(100);
    let body: String = (0..3000).map(|_| format!("{line}\n")).collect();
    fs::write(dir.path().join("big.txt"), &body).unwrap();
    let out = run(&ws, "read", json!({"path": "big.txt", "limit": 10000}));
    assert!(!out.is_error);
    assert!(out.text.len() <= 64 * 1024 + 300, "{}", out.text.len());
    let footer = out.text.lines().last().unwrap();
    assert!(footer.contains("output truncated") && footer.contains("`big.txt:"), "{footer}");
    // Only the displayed lines are editable.
    let tag = tag_in(&out.text, "big.txt");
    let unseen = patch(&ws, &format!("[big.txt#{tag}]\nPUT 3000.=3000:\n+changed\n"));
    assert!(unseen.is_error && unseen.text.contains("have not been shown"), "{}", unseen.text);
    assert!(!patch(&ws, &format!("[big.txt#{tag}]\nPUT 5.=5:\n+changed\n")).is_error);
}

#[test]
fn ranged_read_returns_only_those_rows_and_records_only_them_as_seen() {
    let (dir, ws) = workspace();
    fs::write(dir.path().join("f.txt"), numbered(100)).unwrap();
    let out = read(&ws, "f.txt:5-6,90-91");
    assert!(!out.is_error, "{}", out.text);
    assert_eq!(
        out.text,
        format!("[f.txt#{}]\n5:l5\n6:l6\n…\n90:l90\n91:l91\n[lines 5-6, 90-91 of 100]\n", tag_of(&numbered(100)))
    );
    let tag = tag_in(&out.text, "f.txt");
    // Lines between and around the ranges were never shown.
    for unseen in [4, 7, 50, 92] {
        let refused = patch(&ws, &format!("[f.txt#{tag}]\nPUT {unseen}.={unseen}:\n+changed\n"));
        assert!(refused.is_error && refused.text.contains("have not been shown"), "line {unseen}: {}", refused.text);
    }
    assert_eq!(content(&dir, "f.txt"), numbered(100));
    assert!(!patch(&ws, &format!("[f.txt#{tag}]\nPUT 6.=6:\n+six\n")).is_error);
    assert!(content(&dir, "f.txt").contains("\nsix\n"));
}

#[test]
fn ranged_reads_clip_at_eof_and_tail_reads_the_end() {
    let (dir, ws) = workspace();
    fs::write(dir.path().join("f.txt"), numbered(10)).unwrap();
    assert!(read(&ws, "f.txt:8-99").text.ends_with("8:l8\n9:l9\n10:l10\n[lines 8-10 of 10]\n"));
    assert!(read(&ws, "f.txt:11-20").text.contains("(no lines in that range; file has 10 lines)"));
    assert!(read(&ws, "f.txt:-3").text.ends_with("8:l8\n9:l9\n10:l10\n[lines 8-10 of 10]\n"));
    assert!(read(&ws, "f.txt:-99").text.contains("[lines 1-10 of 10]"));
    // Offset/limit and a selector are alternatives, not a combination.
    let both = run(&ws, "read", json!({"path": "f.txt:2-3", "offset": 2}));
    assert!(both.is_error && both.text.contains("not both"), "{}", both.text);
    // A read that shows nothing records nothing editable.
    fs::write(dir.path().join("g.txt"), numbered(10)).unwrap();
    let tag = read_tag(&ws, "g.txt:11-20");
    let refused = patch(&ws, &format!("[g.txt#{tag}]\nPUT 1.=1:\n+x\n"));
    assert!(refused.is_error && refused.text.contains("have not been shown"), "{}", refused.text);
}

#[test]
fn every_put_and_cut_form_applies_against_the_original_numbering() {
    let (dir, ws) = workspace();
    fs::write(dir.path().join("f.txt"), numbered(10)).unwrap();
    let tag = read_tag(&ws, "f.txt");
    let out = patch(
        &ws,
        &format!(
            "[f.txt#{tag}]\nPUT 2.=3:\n+two\n+three\n+three-b\nPUT <1:\n+head\nPUT >5:\n+after5\nPUT >$:\n+tail\nCUT 7.=8\n"
        ),
    );
    assert!(!out.is_error, "{}", out.text);
    assert_eq!(
        content(&dir, "f.txt"),
        "head\nl1\ntwo\nthree\nthree-b\nl4\nl5\nafter5\nl6\nl9\nl10\ntail\n"
    );
    let fresh = tag_of(&content(&dir, "f.txt"));
    assert!(out.text.starts_with(&format!("[f.txt#{fresh}]\n")), "{}", out.text);
    assert!(out.text.contains("3:two") && out.text.contains("5:three-b"), "{}", out.text);
    assert!(!out.text.contains("Warnings"));
}

#[test]
fn body_escapes_blank_rows_wrapper_and_trailing_newline_handling() {
    let (dir, ws) = workspace();
    fs::write(dir.path().join("f.txt"), "a\nb").unwrap();
    let tag = read_tag(&ws, "f.txt");
    let out = patch(
        &ws,
        &format!("*** Begin Patch\n[f.txt#{tag}]\nPUT >$:\n++plus\n+-minus\n+\n+end\n*** End Patch\n"),
    );
    assert!(!out.is_error, "{}", out.text);
    assert_eq!(content(&dir, "f.txt"), "a\nb\n+plus\n-minus\n\nend");
    fs::write(dir.path().join("e.txt"), "").unwrap();
    let tag = read_tag(&ws, "e.txt");
    assert!(!patch(&ws, &format!("[e.txt#{tag}]\nPUT >$:\n+x\n")).is_error);
    assert_eq!(content(&dir, "e.txt"), "x\n");
}

#[test]
fn registers_move_text_across_sections_files_and_calls() {
    let (dir, ws) = workspace();
    fs::write(dir.path().join("a.txt"), "a1\na2\na3\n").unwrap();
    fs::write(dir.path().join("b.txt"), "b1\nb2\n").unwrap();
    let (ta, tb) = (read_tag(&ws, "a.txt"), read_tag(&ws, "b.txt"));
    // Named cut in one section feeds a paste in a later section.
    let out = patch(&ws, &format!("[a.txt#{ta}]\nCUT 2.=3 @mv\n[b.txt#{tb}]\nPUT >$ @mv\n"));
    assert!(!out.is_error, "{}", out.text);
    assert_eq!(content(&dir, "a.txt"), "a1\n");
    assert_eq!(content(&dir, "b.txt"), "b1\nb2\na2\na3\n");
    // The named register survives the call; pasting does not consume it.
    let ta = tag_in(&out.text, "a.txt");
    let again = patch(&ws, &format!("[a.txt#{ta}]\nPUT <1 @mv\nPUT >1 @mv\n"));
    assert!(!again.is_error, "{}", again.text);
    assert_eq!(content(&dir, "a.txt"), "a2\na3\na1\na2\na3\n");
    let ta = tag_in(&again.text, "a.txt");
    let replaced = patch(&ws, &format!("[a.txt#{ta}]\nPUT 3.=3 @mv\nPUT >$:\n+z\n"));
    assert!(!replaced.is_error, "{}", replaced.text);
    assert_eq!(content(&dir, "a.txt"), "a2\na3\na2\na3\na2\na3\nz\n");
    // Anonymous register: batch-local.
    let ta = tag_in(&replaced.text, "a.txt");
    let anon = patch(&ws, &format!("[a.txt#{ta}]\nCUT 1.=2\nPUT >$\n"));
    assert!(!anon.is_error, "{}", anon.text);
    assert_eq!(content(&dir, "a.txt"), "a2\na3\na2\na3\nz\na2\na3\n");
    let ta = tag_in(&anon.text, "a.txt");
    let empty = patch(&ws, &format!("[a.txt#{ta}]\nPUT >$\n"));
    assert!(empty.is_error && empty.text.contains("anonymous register is empty"), "{}", empty.text);
    let unknown = patch(&ws, &format!("[a.txt#{ta}]\nPUT >$ @nope\n"));
    assert!(unknown.is_error && unknown.text.contains("unknown register @nope"), "{}", unknown.text);
    // A paste cannot precede its cut.
    let order = patch(&ws, &format!("[a.txt#{ta}]\nPUT >$ @late\nCUT 1.=1 @late\n"));
    assert!(order.is_error);
}

#[test]
fn block_anchors_on_braces_indentation_and_markdown() {
    let (dir, ws) = workspace();
    let rust = "use x;\n\nfn one(a: i32) -> i32 {\n    let s = \"}\";\n    a\n}\n\nfn two() {}\n";
    fs::write(dir.path().join("m.rs"), rust).unwrap();
    let tag = read_tag(&ws, "m.rs");
    let out = patch(&ws, &format!("[m.rs#{tag}]\nPUT 3*:\n+fn one() -> i32 {{\n+    1\n+}}\n"));
    assert!(!out.is_error, "{}", out.text);
    assert_eq!(content(&dir, "m.rs"), "use x;\n\nfn one() -> i32 {\n    1\n}\n\nfn two() {}\n");
    let tag = tag_in(&out.text, "m.rs");
    let single = patch(&ws, &format!("[m.rs#{tag}]\nCUT 7*\n"));
    assert!(single.is_error && single.text.contains("7.=7"), "{}", single.text);
    let cut = patch(&ws, &format!("[m.rs#{tag}]\nCUT 3*\nPUT >7*:\n+// after two\n"));
    assert!(!cut.is_error, "{}", cut.text);
    assert_eq!(content(&dir, "m.rs"), "use x;\n\n\nfn two() {}\n// after two\n");
    assert!(cut.text.contains("Warnings") && cut.text.contains("`>7*` did not resolve"), "{}", cut.text);

    fs::write(dir.path().join("p.py"), "@cache\ndef greet(name):\n    print(name)\n\ngreet('w')\n").unwrap();
    let tag = read_tag(&ws, "p.py");
    let out = patch(&ws, &format!("[p.py#{tag}]\nPUT 1*:\n+@cache\n+def greet(name):\n+    print(f'hi {{name}}')\n"));
    assert!(!out.is_error, "{}", out.text);
    assert_eq!(content(&dir, "p.py"), "@cache\ndef greet(name):\n    print(f'hi {name}')\n\ngreet('w')\n");

    fs::write(dir.path().join("d.md"), "# T\nintro\n## A\na\n### A1\ndeep\n## B\nb\n").unwrap();
    let tag = read_tag(&ws, "d.md");
    let out = patch(&ws, &format!("[d.md#{tag}]\nCUT 3*\n"));
    assert!(!out.is_error, "{}", out.text);
    assert_eq!(content(&dir, "d.md"), "# T\nintro\n## B\nb\n");
}

#[test]
fn stale_tag_recovers_when_the_change_is_elsewhere_and_refuses_when_it_intersects() {
    let (dir, ws) = workspace();
    fs::write(dir.path().join("f.txt"), numbered(12)).unwrap();
    let tag = read_tag(&ws, "f.txt");
    // Someone prepends two lines and edits line 11: the edit on line 3 still lands on `l3`.
    fs::write(dir.path().join("f.txt"), format!("p1\np2\n{}", numbered(12).replace("l11", "L11"))).unwrap();
    let out = patch(&ws, &format!("[f.txt#{tag}]\nPUT 3.=3:\n+THREE\n"));
    assert!(!out.is_error, "{}", out.text);
    assert_eq!(
        content(&dir, "f.txt"),
        format!("p1\np2\nl1\nl2\nTHREE\n{}", "l4\nl5\nl6\nl7\nl8\nl9\nl10\nL11\nl12\n")
    );
    assert!(out.text.contains("Warnings") && out.text.contains("stale"), "{}", out.text);
    assert!(out.text.contains("5:THREE"), "preview uses the new numbering: {}", out.text);

    // Now an intersecting change: the model's anchor sits on a changed line.
    fs::write(dir.path().join("g.txt"), numbered(12)).unwrap();
    let tag = read_tag(&ws, "g.txt");
    let mut changed = numbered(12).replace("l5\n", "FIVE\n");
    changed.push_str("l13\n");
    fs::write(dir.path().join("g.txt"), &changed).unwrap();
    let refused = patch(&ws, &format!("[g.txt#{tag}]\nPUT 4.=6:\n+x\n"));
    assert!(refused.is_error, "{}", refused.text);
    assert!(refused.text.contains("cannot be re-anchored") && refused.text.contains("5:FIVE"), "{}", refused.text);
    assert_eq!(content(&dir, "g.txt"), changed, "file untouched");
    // The mismatch reply carries a fresh, immediately usable tag for the lines it showed.
    let fresh = tag_in(&refused.text, "g.txt");
    assert_eq!(fresh, tag_of(&changed));
    let retry = patch(&ws, &format!("[g.txt#{fresh}]\nPUT 5.=5:\n+five\n"));
    assert!(!retry.is_error, "{}", retry.text);
    assert!(content(&dir, "g.txt").contains("\nfive\n"));
    // An insert squeezed between changed neighbours is ambiguous and refused too.
    fs::write(dir.path().join("h.txt"), "a\nb\nc\n").unwrap();
    let tag = read_tag(&ws, "h.txt");
    fs::write(dir.path().join("h.txt"), "a\nNEW\nb\nc\n").unwrap();
    assert!(patch(&ws, &format!("[h.txt#{tag}]\nPUT >1:\n+x\n")).is_error);
    assert!(!patch(&ws, &format!("[h.txt#{tag}]\nPUT >3:\n+x\n")).is_error);
}

#[test]
fn unknown_tags_and_unseen_lines_are_rejected_with_guidance() {
    let (dir, ws) = workspace();
    fs::write(dir.path().join("f.txt"), numbered(20)).unwrap();
    let never_read = patch(&ws, &format!("[f.txt#{}]\nPUT 1.=1:\n+x\n", tag_of(&numbered(20))));
    assert!(never_read.is_error && never_read.text.contains("read it"), "{}", never_read.text);
    let real = read_tag(&ws, "f.txt:1-5");
    let bogus = if real == "0000" { "FFFF" } else { "0000" };
    let unknown = patch(&ws, &format!("[f.txt#{bogus}]\nPUT 1.=1:\n+x\n"));
    assert!(unknown.is_error && unknown.text.contains("unknown snapshot tag"), "{}", unknown.text);
    let unseen = patch(&ws, &format!("[f.txt#{real}]\nPUT 3.=10:\n+x\n"));
    assert!(unseen.is_error && unseen.text.contains("`f.txt:6-10`"), "{}", unseen.text);
    let tail = patch(&ws, &format!("[f.txt#{real}]\nPUT >$:\n+x\n"));
    assert!(tail.is_error && tail.text.contains("have not been shown"), "{}", tail.text);
    let beyond = patch(&ws, &format!("[f.txt#{real}]\nPUT 1.=99:\n+x\n"));
    assert!(beyond.is_error && beyond.text.contains("beyond the end"), "{}", beyond.text);
    assert_eq!(content(&dir, "f.txt"), numbered(20));
    // Reading the missing range makes it editable under the same tag.
    assert_eq!(read_tag(&ws, "f.txt:6-10"), real);
    assert!(!patch(&ws, &format!("[f.txt#{real}]\nPUT 3.=10:\n+x\n")).is_error);
    let recovered = patch(&ws, &format!("[f.txt#{real}]\nPUT 1.=1:\n+one\n"));
    assert!(!recovered.is_error && recovered.text.contains("stale"), "line 1 is unchanged since the snapshot: {}", recovered.text);
}

#[test]
fn edits_keep_earlier_seen_lines_editable_and_write_is_immediately_editable() {
    let (dir, ws) = workspace();
    fs::write(dir.path().join("f.txt"), numbered(30)).unwrap();
    let tag = read_tag(&ws, "f.txt");
    let first = patch(&ws, &format!("[f.txt#{tag}]\nPUT 2.=2:\n+two\n"));
    assert!(!first.is_error, "{}", first.text);
    let fresh = tag_in(&first.text, "f.txt");
    let second = patch(&ws, &format!("[f.txt#{fresh}]\nPUT 25.=25:\n+twenty-five\n"));
    assert!(!second.is_error, "no re-read needed for lines already seen: {}", second.text);

    let wrote = run(&ws, "write", json!({"path": "w.txt", "content": "one\ntwo\nthree\n"}));
    assert!(!wrote.is_error && wrote.text.contains("created"), "{}", wrote.text);
    let tag = tag_in(&wrote.text, "w.txt");
    assert_eq!(tag, tag_of("one\ntwo\nthree\n"));
    let out = patch(&ws, &format!("[w.txt#{tag}]\nPUT >3:\n+four\n"));
    assert!(!out.is_error, "{}", out.text);
    assert_eq!(content(&dir, "w.txt"), "one\ntwo\nthree\nfour\n");
}

#[test]
fn no_op_edits_report_without_writing_and_the_third_repeat_errors() {
    let (dir, ws) = workspace();
    fs::write(dir.path().join("a.txt"), "a\nb\n").unwrap();
    fs::write(dir.path().join("b.txt"), "x\ny\n").unwrap();
    let ta = read_tag(&ws, "a.txt");
    let same = format!("[a.txt#{ta}]\nPUT 1.=1:\n+a\n");
    for _ in 0..2 {
        let out = patch(&ws, &same);
        assert!(!out.is_error && out.text.contains("No changes"), "{}", out.text);
    }
    let third = patch(&ws, &same);
    assert!(third.is_error && third.text.contains("3 times"), "{}", third.text);
    // A different edit resets the counter.
    let real = patch(&ws, &format!("[a.txt#{ta}]\nPUT 2.=2:\n+B\n"));
    assert!(!real.is_error);
    // Multi-section: one no-op rejects the batch and nothing is written.
    let tb = read_tag(&ws, "b.txt");
    let ta = tag_in(&real.text, "a.txt");
    let batch = patch(&ws, &format!("[a.txt#{ta}]\nPUT 1.=1:\n+A\n[b.txt#{tb}]\nPUT 1.=1:\n+x\n"));
    assert!(batch.is_error && batch.text.contains("b.txt") && batch.text.contains("nothing was written"), "{}", batch.text);
    assert_eq!(content(&dir, "a.txt"), "a\nB\n");
}

#[test]
fn a_bad_later_section_leaves_earlier_files_untouched() {
    let (dir, ws) = workspace();
    fs::write(dir.path().join("a.txt"), "a\n").unwrap();
    fs::write(dir.path().join("b.txt"), "b\n").unwrap();
    let (ta, tb) = (read_tag(&ws, "a.txt"), read_tag(&ws, "b.txt"));
    for bad in [
        format!("[b.txt#{tb}]\nPUT 9.=9:\n+x\n"),
        format!("[missing.txt#{tb}]\nPUT 1.=1:\n+x\n"),
        format!("[b.txt#{tb}]\nPUT 1.=1:\n+x\nPUT 1.=1:\n+y\n"),
        format!("[b.txt#{tb}]\nBOGUS\n"),
    ] {
        let out = patch(&ws, &format!("[a.txt#{ta}]\nPUT 1.=1:\n+changed\n{bad}"));
        assert!(out.is_error, "{bad}");
        assert_eq!(content(&dir, "a.txt"), "a\n", "{bad}: {}", out.text);
        assert_eq!(content(&dir, "b.txt"), "b\n");
    }
    // Same-path sections merge so original numbering applies to both.
    let merged = patch(&ws, &format!("[a.txt#{ta}]\nPUT >1:\n+two\n[a.txt#{ta}]\nPUT <1:\n+zero\n"));
    assert!(!merged.is_error, "{}", merged.text);
    assert_eq!(content(&dir, "a.txt"), "zero\na\ntwo\n");
    let conflicting = patch(&ws, &format!("[b.txt#{tb}]\nPUT 1.=1:\n+x\n[b.txt#{}]\nPUT >1:\n+y\n", tag_of("zzz")));
    assert!(conflicting.is_error && conflicting.text.contains("same tag"), "{}", conflicting.text);
}

#[test]
fn overlapping_and_reversed_ranges_are_rejected() {
    let (dir, ws) = workspace();
    fs::write(dir.path().join("f.txt"), numbered(6)).unwrap();
    let tag = read_tag(&ws, "f.txt");
    for bad in [
        "PUT 2.=4:\n+x\nPUT 3.=3:\n+y\n",
        "PUT 2.=4:\n+x\nCUT 4.=5\n",
        "PUT 4.=2:\n+x\n",
        "PUT 2.=4:\n+x\nPUT >3:\n+y\n",
    ] {
        let out = patch(&ws, &format!("[f.txt#{tag}]\n{bad}"));
        assert!(out.is_error, "{bad}: {}", out.text);
        assert_eq!(content(&dir, "f.txt"), numbered(6));
    }
    // Adjacent edits and inserts on range boundaries are fine.
    let ok = patch(&ws, &format!("[f.txt#{tag}]\nPUT 2.=3:\n+x\nPUT >3:\n+after\nPUT <2:\n+before\nPUT 4.=4:\n+y\n"));
    assert!(!ok.is_error, "{}", ok.text);
    assert_eq!(content(&dir, "f.txt"), "l1\nbefore\nx\nafter\ny\nl5\nl6\n");
}

#[test]
fn move_and_remove_files() {
    let (dir, ws) = workspace();
    fs::write(dir.path().join("a.txt"), "a\nb\n").unwrap();
    fs::write(dir.path().join("other.txt"), "o\n").unwrap();
    let ta = read_tag(&ws, "a.txt");
    let out = patch(&ws, &format!("[a.txt#{ta}]\nPUT 2.=2:\n+B\nMV \"sub dir/b.txt\"\n"));
    assert!(!out.is_error, "{}", out.text);
    assert!(!dir.path().join("a.txt").exists());
    assert_eq!(content(&dir, "sub dir/b.txt"), "a\nB\n");
    let moved = tag_in(&out.text, "sub dir/b.txt");
    assert_eq!(moved, tag_of("a\nB\n"));
    // The destination is immediately editable with the tag from the move result.
    assert!(!patch(&ws, &format!("[sub dir/b.txt#{moved}]\nPUT >2:\n+c\n")).is_error);
    // Overwriting an existing destination is allowed; moving onto yourself or out is not.
    let to = read_tag(&ws, "other.txt");
    let tb = read_tag(&ws, "sub dir/b.txt");
    assert!(patch(&ws, &format!("[sub dir/b.txt#{tb}]\nMV sub dir/b.txt\n")).is_error);
    assert!(patch(&ws, &format!("[sub dir/b.txt#{tb}]\nMV ../escape.txt\n")).is_error);
    assert!(patch(&ws, &format!("[sub dir/b.txt#{tb}]\nMV sub dir\n")).is_error);
    assert!(patch(&ws, &format!("[sub dir/b.txt#{tb}]\nMV other.txt\n[other.txt#{to}]\nPUT 1.=1:\n+z\n")).is_error);
    assert!(!patch(&ws, &format!("[sub dir/b.txt#{tb}]\nMV other.txt\n")).is_error);
    assert_eq!(content(&dir, "other.txt"), "a\nB\nc\n");
    assert!(!dir.path().join("sub dir/b.txt").exists());

    let to = read_tag(&ws, "other.txt");
    assert!(patch(&ws, &format!("[other.txt#{to}]\nPUT 1.=1:\n+x\nREM\n")).is_error, "REM cannot mix with edits");
    let rem = patch(&ws, &format!("[other.txt#{to}]\nREM\n"));
    assert!(!rem.is_error && rem.text.contains("deleted other.txt"), "{}", rem.text);
    assert!(!dir.path().join("other.txt").exists());
    let again = patch(&ws, &format!("[other.txt#{to}]\nREM\n"));
    assert!(again.is_error && again.text.contains("no such file"), "{}", again.text);
    // A file that changed since the read is not deleted blindly.
    fs::write(dir.path().join("late.txt"), "1\n").unwrap();
    let tl = read_tag(&ws, "late.txt");
    fs::write(dir.path().join("late.txt"), "2\n").unwrap();
    assert!(patch(&ws, &format!("[late.txt#{tl}]\nREM\n")).is_error);
    assert!(dir.path().join("late.txt").exists());
}

#[test]
fn paths_cannot_escape_the_workspace_through_edit_write_or_mv() {
    let (dir, ws) = workspace();
    let outside = tempfile::tempdir().unwrap();
    let canary = outside.path().join("canary");
    fs::write(&canary, "secret\n").unwrap();
    fs::write(dir.path().join("in.txt"), "in\n").unwrap();
    std::os::unix::fs::symlink(outside.path(), dir.path().join("dlink")).unwrap();
    std::os::unix::fs::symlink(&canary, dir.path().join("flink")).unwrap();
    let tag = read_tag(&ws, "in.txt");
    let abs = canary.display().to_string();
    for target in [abs.as_str(), "../canary", "dlink/canary", "flink"] {
        let rem = patch(&ws, &format!("[{target}#{tag}]\nREM\n"));
        assert!(rem.is_error && !rem.text.contains("secret"), "{target}: {}", rem.text);
        assert!(read(&ws, target).is_error);
        assert!(run(&ws, "write", json!({"path": target, "content": "pwned"})).is_error);
        let mv = patch(&ws, &format!("[in.txt#{tag}]\nMV {target}\n"));
        assert!(mv.is_error, "{target}: {}", mv.text);
    }
    assert_eq!(fs::read_to_string(&canary).unwrap(), "secret\n");
    assert_eq!(content(&dir, "in.txt"), "in\n");
}

#[derive(Default)]
struct Recorder {
    /// (kind, absolute path, file content at the moment of the call)
    calls: Mutex<Vec<(&'static str, std::path::PathBuf, Option<String>)>>,
    fail_suffix: Option<&'static str>,
}

impl Recorder {
    fn note(&self, kind: &'static str, abs: &Path) -> anyhow::Result<()> {
        self.calls.lock().push((kind, abs.to_path_buf(), fs::read_to_string(abs).ok()));
        match self.fail_suffix {
            Some(suffix) if abs.to_string_lossy().ends_with(suffix) => anyhow::bail!("checkpoint refused {suffix}"),
            _ => Ok(()),
        }
    }
}

impl WriteObserver for Recorder {
    fn before_write(&self, abs: &Path) -> anyhow::Result<()> {
        self.note("write", abs)
    }
    fn before_delete(&self, abs: &Path) -> anyhow::Result<()> {
        self.note("delete", abs)
    }
}

#[test]
fn observer_sees_every_mutation_with_canonical_path_before_it_happens() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let recorder = Arc::new(Recorder::default());
    let ws = Workspace::new(dir.path()).unwrap().with_observer(recorder.clone());
    fs::write(root.join("a.txt"), "a\n").unwrap();
    fs::write(root.join("b.txt"), "b\n").unwrap();
    fs::write(root.join("c.txt"), "c\n").unwrap();
    let (ta, tb, tc) = (read_tag(&ws, "a.txt"), read_tag(&ws, "b.txt"), read_tag(&ws, "c.txt"));

    assert!(!patch(&ws, &format!("[a.txt#{ta}]\nPUT 1.=1:\n+A\n")).is_error);
    assert!(!run(&ws, "write", json!({"path": "new/w.txt", "content": "w"})).is_error);
    assert!(!patch(&ws, &format!("[b.txt#{tb}]\nMV moved.txt\n[c.txt#{tc}]\nREM\n")).is_error);

    let calls = recorder.calls.lock().clone();
    let summary: Vec<_> = calls.iter().map(|(k, p, c)| (*k, p.strip_prefix(&root).unwrap().to_owned(), c.clone())).collect();
    let rel = |s: &str| std::path::PathBuf::from(s);
    assert_eq!(
        summary,
        vec![
            ("write", rel("a.txt"), Some("a\n".to_owned())),
            ("write", rel("new/w.txt"), None),
            ("write", rel("moved.txt"), None),
            ("delete", rel("b.txt"), Some("b\n".to_owned())),
            ("delete", rel("c.txt"), Some("c\n".to_owned())),
        ]
    );
}

#[test]
fn an_erroring_observer_aborts_the_mutation_and_the_whole_batch() {
    let dir = tempfile::tempdir().unwrap();
    let recorder = Arc::new(Recorder { fail_suffix: Some("b.txt"), ..Recorder::default() });
    let ws = Workspace::new(dir.path()).unwrap().with_observer(recorder);
    fs::write(dir.path().join("a.txt"), "a\n").unwrap();
    fs::write(dir.path().join("b.txt"), "b\n").unwrap();
    let (ta, tb) = (read_tag(&ws, "a.txt"), read_tag(&ws, "b.txt"));
    let out = patch(&ws, &format!("[a.txt#{ta}]\nPUT 1.=1:\n+A\n[b.txt#{tb}]\nPUT 1.=1:\n+B\n"));
    assert!(out.is_error && out.text.contains("blocked") && out.text.contains("checkpoint refused"), "{}", out.text);
    assert_eq!(content(&dir, "a.txt"), "a\n", "first file must not be written either");
    assert_eq!(content(&dir, "b.txt"), "b\n");
    let single = patch(&ws, &format!("[b.txt#{tb}]\nREM\n"));
    assert!(single.is_error && dir.path().join("b.txt").exists());
    let mv = patch(&ws, &format!("[a.txt#{ta}]\nMV b.txt\n"));
    assert!(mv.is_error && dir.path().join("a.txt").exists());
    let write = run(&ws, "write", json!({"path": "b.txt", "content": "z"}));
    assert!(write.is_error);
    assert_eq!(content(&dir, "b.txt"), "b\n");
    assert!(!dir.path().join("sub").exists());
    let write = run(&ws, "write", json!({"path": "sub/b.txt", "content": "z"}));
    assert!(write.is_error && !dir.path().join("sub").exists(), "no directories created before the observer approves");
}

#[test]
fn crlf_files_keep_their_line_endings() {
    let (dir, ws) = workspace();
    fs::write(dir.path().join("w.txt"), "a\r\nb\r\nc\r\n").unwrap();
    let out = read(&ws, "w.txt");
    assert!(out.text.contains("\n2:b\n"), "{:?}", out.text);
    assert_eq!(tag_in(&out.text, "w.txt"), tag_of("a\nb\nc\n"));
    let tag = tag_in(&out.text, "w.txt");
    assert!(!patch(&ws, &format!("[w.txt#{tag}]\nPUT 2.=2:\n+X\n+Y\n")).is_error);
    assert_eq!(fs::read(dir.path().join("w.txt")).unwrap(), b"a\r\nX\r\nY\r\nc\r\n");
}

#[test]
fn argument_and_size_limits() {
    let (dir, ws) = workspace();
    assert!(run(&ws, "edit", json!({})).text.contains("missing required argument"));
    assert!(run(&ws, "edit", json!({"input": 5})).text.contains("must be a string"));
    assert!(patch(&ws, &"x".repeat(256 * 1024 + 1)).text.contains("limit"));
    fs::write(dir.path().join("huge.txt"), vec![b'a'; 4 * 1024 * 1024 + 1]).unwrap();
    let out = read(&ws, "huge.txt");
    assert!(out.is_error && out.text.contains("raw:A-B"), "{}", out.text);
    let raw = read(&ws, "huge.txt:raw:1-1");
    assert!(!raw.is_error && raw.text.starts_with("aaa"), "{}", &raw.text[..50.min(raw.text.len())]);
    let edit = patch(&ws, &format!("[huge.txt#{}]\nPUT 1.=1:\n+x\n", tag_of("a")));
    assert!(edit.is_error);
}

#[test]
fn schemas_depend_on_the_edit_mode() {
    let hashline = schemas(&["read", "edit", "write"], EditMode::Hashline).unwrap();
    let replace = schemas(&["read", "edit", "write"], EditMode::Replace).unwrap();
    assert_eq!(hashline[1]["function"]["parameters"]["required"], json!(["input"]));
    assert_eq!(replace[1]["function"]["parameters"]["required"], json!(["path", "old", "new"]));
    let description = hashline[1]["function"]["description"].as_str().unwrap();
    for needle in ["[PATH#TAG]", "PUT N.=M:", "CUT", "REM", "MV", "@tmp", "Example"] {
        assert!(description.contains(needle), "missing {needle}");
    }
    // Replace mode keeps the plain numbered read.
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("f.txt"), "x\n").unwrap();
    let ws = Workspace::new(dir.path()).unwrap().with_edit_mode(EditMode::Replace);
    assert_eq!(read(&ws, "f.txt").text, "     1\tx\n");
    assert!(run(&ws, "edit", json!({"input": "[f.txt#0000]\nREM\n"})).is_error);
}

/// End-to-end: the way a model drives the tools, printed for human inspection.
#[test]
fn smoke_read_edit_round_trip() {
    let (dir, ws) = workspace();
    let source = "fn main() {\n    let a = 1;\n    let b = 2;\n    println!(\"{}\", a + b);\n}\n\nfn helper() -> i32 {\n    7\n}\n";
    fs::write(dir.path().join("main.rs"), source).unwrap();
    let r1 = read(&ws, "main.rs");
    println!("--- read ---\n{}", r1.text);
    let tag = tag_in(&r1.text, "main.rs");
    let e1 = patch(
        &ws,
        &format!(
            "[main.rs#{tag}]\nPUT 3.=3:\n+    let b = 20;\nPUT 7*:\n+fn helper() -> i32 {{\n+    42\n+}}\nPUT >$:\n+\n+// done\n"
        ),
    );
    println!("--- edit ---\n{}", e1.text);
    assert!(!e1.is_error, "{}", e1.text);
    let expected = "fn main() {\n    let a = 1;\n    let b = 20;\n    println!(\"{}\", a + b);\n}\n\nfn helper() -> i32 {\n    42\n}\n\n// done\n";
    assert_eq!(content(&dir, "main.rs"), expected);
    let tag = tag_in(&e1.text, "main.rs");
    let e2 = patch(&ws, &format!("[main.rs#{tag}]\nCUT 1*\n"));
    println!("--- edit 2 ---\n{}", e2.text);
    assert!(!e2.is_error, "{}", e2.text);
    assert_eq!(content(&dir, "main.rs"), "\nfn helper() -> i32 {\n    42\n}\n\n// done\n");
    let r2 = read(&ws, "main.rs");
    assert!(r2.text.contains(&format!("[main.rs#{}]", tag_of(&content(&dir, "main.rs")))));
}

#[test]
fn hostile_numbers_and_emptied_files_error_cleanly_instead_of_panicking() {
    let (dir, ws) = workspace();
    fs::write(dir.path().join("f.txt"), "a\nb\n").unwrap();
    let tag = read_tag(&ws, "f.txt");
    fs::write(dir.path().join("f.txt"), "").unwrap();
    let out = patch(&ws, &format!("[f.txt#{tag}]\nPUT >$:\n+x\n"));
    assert!(out.is_error && !out.text.contains("internal error"), "{}", out.text);
    assert_eq!(content(&dir, "f.txt"), "");
    let max = usize::MAX;
    let huge = patch(&ws, &format!("[f.txt#{tag}]\nPUT {max}*:\n+x\nPUT <{max}:\n+y\nCUT 1.={max}\n"));
    assert!(huge.is_error && !huge.text.contains("internal error"), "{}", huge.text);
    fs::write(dir.path().join("g.txt"), "a\nb\n").unwrap();
    let out = read(&ws, &format!("g.txt:{max}+{max}"));
    assert!(!out.text.contains("internal error"), "{}", out.text);
    let out = read(&ws, &format!("g.txt:1-{max}"));
    assert!(out.text.contains("2:b"), "{}", out.text);
}

#[test]
fn an_off_by_one_range_reports_the_neighbouring_line_it_deleted() {
    // Observed with a local model: `PUT 2.=3` meant "replace line 2 with two lines" and silently
    // deleted line 3. The edit result must name the deleted line so the model can repair it at once.
    let (dir, ws) = workspace();
    fs::write(dir.path().join("f.txt"), "let a = 1;\nlet term = enter();\nlet dark = probe();\nlet keys = load();\n").unwrap();
    let tag = read_tag(&ws, "f.txt");
    let out = patch(&ws, &format!("[f.txt#{tag}]\nPUT 2.=3:\n+let mut term = enter();\n+term.clear();\n"));
    assert!(!out.is_error, "{}", out.text);
    assert!(out.text.contains("removed lines not present in your new text"), "{}", out.text);
    assert!(out.text.contains("3: let dark = probe();"), "{}", out.text);
    assert!(out.text.contains("PUT >N:"), "the message must point at the insert syntax: {}", out.text);
    // A replacement that keeps every old line (re-indented or moved) reports nothing.
    let tag = read_tag(&ws, "f.txt");
    let kept = patch(&ws, &format!("[f.txt#{tag}]\nPUT 1.=1:\n+let a = 2;\n"));
    assert!(kept.text.contains("removed lines") && kept.text.contains("1: let a = 1;"), "{}", kept.text);
    let tag = read_tag(&ws, "f.txt");
    let same = patch(&ws, &format!("[f.txt#{tag}]\nPUT 4.=4:\n+    let keys = load();\n"));
    assert!(!same.text.contains("removed lines"), "{}", same.text);
}
