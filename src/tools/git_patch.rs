//! Line-level staging over one file's unified diff. Pure text transforms: no git I/O happens here.
//! A selection is a set of indices into one hunk's body lines: every ` `, `+` and `-` line, numbered in
//! order. `\ No newline` markers are not numbered; each is copied after the line it follows, and dropped
//! together with that line when the line itself is not emitted.
//!
//! Forward (`reverse == false`): selected `+` and `-` lines are kept, unselected `+` lines are dropped and
//! unselected `-` lines become context. Apply the result with `git apply --cached` to stage.
//!
//! Reverse (`reverse == true`): `+` and `-` roles are swapped before the same selection logic runs, so the
//! indices still name the lines the user sees. The result targets the new side of the diff and is applied
//! directly, without `-R`: `--cached` to unstage, the worktree to discard.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Context,
    Add,
    Del,
}

impl Kind {
    fn flipped(self) -> Self {
        match self {
            Kind::Context => Kind::Context,
            Kind::Add => Kind::Del,
            Kind::Del => Kind::Add,
        }
    }
}

struct BodyLine<'a> {
    kind: Kind,
    /// Line content without its one-byte prefix.
    text: &'a str,
    /// The `\ No newline at end of file` marker that followed this line, verbatim.
    marker: Option<&'a str>,
}

struct Hunk<'a> {
    /// Byte span of the whole hunk (header through its last body line) within the file patch.
    start: usize,
    end: usize,
    old_start: u64,
    new_start: u64,
    /// Text after the closing `@@` of the header (usually empty or a function name), kept verbatim.
    trailer: &'a str,
    lines: Vec<BodyLine<'a>>,
}

/// Build a patch that applies only the selected body lines of hunk `hunk` of `file_patch`.
///
/// `file_patch` is one file's `git diff` text. The file header is copied verbatim; the output contains
/// only the chosen hunk, with its counts recomputed from the emitted body and its original start values
/// kept (swapped for `reverse`). Returns `None` when the hunk does not exist or the selection contains no
/// changed line.
pub(crate) fn line_patch(
    file_patch: &str,
    hunk: usize,
    selected: &[usize],
    reverse: bool,
) -> Option<String> {
    let (header, hunks) = parse(file_patch)?;
    let hunk = hunks.get(hunk)?;
    let mut chosen = vec![false; hunk.lines.len()];
    for &index in selected {
        if let Some(slot) = chosen.get_mut(index) {
            *slot = true;
        }
    }
    let (old_start, new_start) = if reverse {
        (hunk.new_start, hunk.old_start)
    } else {
        (hunk.old_start, hunk.new_start)
    };
    let mut body = String::new();
    let (mut old_len, mut new_len) = (0u64, 0u64);
    let mut changed = false;
    for (line, &picked) in hunk.lines.iter().zip(&chosen) {
        let kind = if reverse {
            line.kind.flipped()
        } else {
            line.kind
        };
        let (prefix, keep_marker) = match (kind, picked) {
            (Kind::Context, _) => (' ', true),
            (Kind::Add, true) => ('+', true),
            (Kind::Add, false) => continue,
            (Kind::Del, true) => ('-', true),
            // An unselected removal stays in the file, so it becomes context. Its marker described the
            // removed side only, so it is dropped rather than attached to a line both sides share.
            (Kind::Del, false) => (' ', false),
        };
        match prefix {
            ' ' => {
                old_len += 1;
                new_len += 1;
            }
            '+' => new_len += 1,
            _ => old_len += 1,
        }
        changed |= picked && kind != Kind::Context;
        body.push(prefix);
        body.push_str(line.text);
        body.push('\n');
        if keep_marker {
            if let Some(marker) = line.marker {
                body.push_str(marker);
                body.push('\n');
            }
        }
    }
    if !changed {
        return None;
    }
    let mut out = String::with_capacity(header.len() + body.len() + 64);
    out.push_str(header);
    out.push_str(&format!(
        "@@ -{old_start},{old_len} +{new_start},{new_len} @@{}\n",
        hunk.trailer
    ));
    out.push_str(&body);
    Some(out)
}

/// Split one file's `git diff` text into its header (everything before the first hunk, verbatim) and
/// each hunk's text verbatim, header line through last body line. Returns `None` when there is no hunk.
pub(crate) fn file_header_and_hunks(file_patch: &str) -> Option<(String, Vec<String>)> {
    let (header, hunks) = parse(file_patch)?;
    let texts: Vec<String> = hunks
        .iter()
        .map(|hunk| file_patch[hunk.start..hunk.end].to_owned())
        .collect();
    Some((header.to_owned(), texts))
}

/// Parse a single file's patch into its header and hunks. Hunks end at the first line that is not part of
/// a hunk body (for example a following `diff --git` line). `None` when the text is malformed or has no hunk.
fn parse(file_patch: &str) -> Option<(&str, Vec<Hunk<'_>>)> {
    let mut hunks: Vec<Hunk<'_>> = Vec::new();
    let mut current: Option<Hunk<'_>> = None;
    let mut offset = 0;
    for raw in file_patch.split_inclusive('\n') {
        let line = raw.strip_suffix('\n').unwrap_or(raw);
        if let Some(rest) = line.strip_prefix("@@") {
            if let Some(mut done) = current.take() {
                done.end = offset;
                hunks.push(done);
            }
            let (old_start, new_start, trailer) = parse_head(rest)?;
            current = Some(Hunk {
                start: offset,
                end: file_patch.len(),
                old_start,
                new_start,
                trailer,
                lines: Vec::new(),
            });
        } else {
            let body_hunk = if is_body_line(line) {
                current.as_mut()
            } else {
                None
            };
            if let Some(hunk) = body_hunk {
                match line.bytes().next() {
                    Some(b'\\') => {
                        if let Some(last) = hunk.lines.last_mut() {
                            last.marker = Some(line);
                        }
                    }
                    first => {
                        let kind = match first {
                            Some(b'+') => Kind::Add,
                            Some(b'-') => Kind::Del,
                            _ => Kind::Context,
                        };
                        hunk.lines.push(BodyLine {
                            kind,
                            text: line.get(1..).unwrap_or(""),
                            marker: None,
                        });
                    }
                }
            } else if let Some(mut done) = current.take() {
                done.end = offset;
                hunks.push(done);
            }
        }
        offset += raw.len();
    }
    if let Some(mut done) = current.take() {
        done.end = file_patch.len();
        hunks.push(done);
    }
    let first = hunks.first()?.start;
    Some((&file_patch[..first], hunks))
}

fn is_body_line(line: &str) -> bool {
    matches!(line.bytes().next(), None | Some(b' ' | b'+' | b'-' | b'\\'))
}

/// Parse the text after `@@` of a hunk header: ` -old[,n] +new[,n] @@trailer`. Returns the two start
/// lines and the trailer. The lengths are ignored because they are recomputed from the body.
fn parse_head(rest: &str) -> Option<(u64, u64, &str)> {
    let body = rest.strip_prefix(' ')?;
    let close = body.find(" @@")?;
    let (ranges, trailer) = (&body[..close], &body[close + 3..]);
    let (old, new) = ranges.split_once(' ')?;
    Some((
        range_start(old.strip_prefix('-')?)?,
        range_start(new.strip_prefix('+')?)?,
        trailer,
    ))
}

fn range_start(spec: &str) -> Option<u64> {
    spec.split(',').next()?.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::process::Command;
    use std::time::{SystemTime, UNIX_EPOCH};

    const FILE_HEADER: &str =
        "diff --git a/f.txt b/f.txt\nindex 1111111..2222222 100644\n--- a/f.txt\n+++ b/f.txt\n";

    fn patch(body: &str) -> String {
        format!("{FILE_HEADER}{body}")
    }

    /// Body indices of the changed lines in hunk 0, numbered the same way `line_patch` numbers them.
    fn change_indices(file_patch: &str) -> Vec<usize> {
        let (_, hunks) = parse(file_patch).expect("patch parses");
        hunks[0]
            .lines
            .iter()
            .enumerate()
            .filter(|(_, line)| line.kind != Kind::Context)
            .map(|(index, _)| index)
            .collect()
    }

    /// Scratch directory removed on drop, so a failing test still cleans up.
    struct Scratch(PathBuf);

    impl Scratch {
        fn new(name: &str) -> Self {
            let nanos = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("system clock after epoch")
                .as_nanos();
            let dir = std::env::temp_dir().join(format!(
                "dial-git-patch-{name}-{}-{nanos}",
                std::process::id()
            ));
            fs::create_dir_all(&dir).expect("create scratch dir");
            Self(dir)
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn git(repo: &Path, args: &[&str]) -> String {
        let out = Command::new("git")
            .current_dir(repo)
            .args([
                "-c",
                "user.name=dial",
                "-c",
                "user.email=dial@example.invalid",
                "-c",
                "commit.gpgsign=false",
            ])
            .args(args)
            .output()
            .expect("git must be installed to run this test");
        assert!(
            out.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8(out.stdout).expect("git output is UTF-8")
    }

    fn assert_applies(repo: &Path, scratch: &Path, patch_text: &str) {
        let patch_file = scratch.join("p.patch");
        fs::write(&patch_file, patch_text).expect("write patch file");
        let out = Command::new("git")
            .current_dir(repo)
            .args(["apply", "--cached", "--check"])
            .arg(&patch_file)
            .output()
            .expect("run git apply");
        assert!(
            out.status.success(),
            "git apply --check rejected:\n{patch_text}\n{}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    #[test]
    fn single_added_line_selected() {
        let file = patch("@@ -1,2 +1,4 @@\n a\n+b\n+c\n d\n");
        assert_eq!(
            line_patch(&file, 0, &[1], false).unwrap(),
            patch("@@ -1,2 +1,3 @@\n a\n+b\n d\n")
        );
    }

    #[test]
    fn single_removed_line_selected() {
        let file = patch("@@ -1,3 +1,2 @@\n a\n-b\n c\n");
        assert_eq!(
            line_patch(&file, 0, &[1], false).unwrap(),
            patch("@@ -1,3 +1,2 @@\n a\n-b\n c\n")
        );
    }

    #[test]
    fn unselected_removal_becomes_context() {
        let file = patch("@@ -1,4 +1,2 @@\n a\n-b\n-c\n d\n");
        assert_eq!(
            line_patch(&file, 0, &[2], false).unwrap(),
            patch("@@ -1,4 +1,3 @@\n a\n b\n-c\n d\n")
        );
    }

    #[test]
    fn reverse_mode_swaps_roles() {
        let file = patch("@@ -1,3 +1,3 @@\n a\n-old\n+new\n c\n");
        // Reverse-selecting the removal re-adds `old`; the unselected addition `new` stays as context.
        assert_eq!(
            line_patch(&file, 0, &[1], true).unwrap(),
            patch("@@ -1,3 +1,4 @@\n a\n+old\n new\n c\n")
        );
        // Reverse-selecting the addition removes `new`; the unselected removal `old` is dropped.
        assert_eq!(
            line_patch(&file, 0, &[2], true).unwrap(),
            patch("@@ -1,3 +1,2 @@\n a\n-new\n c\n")
        );
    }

    #[test]
    fn header_recount_with_multiple_hunks() {
        let file = patch("@@ -10,2 +10,3 @@\n a\n+x\n b\n@@ -40,4 +41,2 @@\n c\n-d\n e\n-f\n");
        assert_eq!(
            line_patch(&file, 1, &[1], false).unwrap(),
            patch("@@ -40,4 +41,3 @@\n c\n-d\n e\n f\n")
        );
    }

    #[test]
    fn empty_selection_returns_none() {
        let file = patch("@@ -1,3 +1,2 @@\n a\n-b\n c\n");
        assert_eq!(line_patch(&file, 0, &[], false), None);
        assert_eq!(line_patch(&file, 0, &[9], false), None);
        // Context-only selection touches no change.
        assert_eq!(line_patch(&file, 0, &[0, 2], false), None);
        assert!(line_patch(&file, 0, &[1], true).is_some());
        assert_eq!(line_patch(&file, 3, &[1], false), None);
    }

    #[test]
    fn no_newline_marker_preserved() {
        let body = "@@ -1,2 +1,2 @@\n a\n-old\n\\ No newline at end of file\n+new\n\\ No newline at end of file\n";
        let file = patch(body);
        assert_eq!(line_patch(&file, 0, &[1, 2], false).unwrap(), file);
        // Only the removal is selected: its marker stays with it, the dropped addition's marker goes.
        assert_eq!(
            line_patch(&file, 0, &[1], false).unwrap(),
            patch("@@ -1,2 +1,1 @@\n a\n-old\n\\ No newline at end of file\n")
        );
    }

    #[test]
    fn header_and_hunks_split_verbatim() {
        let file = patch("@@ -1,2 +1,3 @@ fn a()\n a\n+b\n c\n@@ -40,4 +41,2 @@\n c\n-d\n e\n-f\n");
        let (header, hunks) = file_header_and_hunks(&file).expect("hunks present");
        assert_eq!(header, FILE_HEADER);
        assert_eq!(
            hunks,
            vec![
                "@@ -1,2 +1,3 @@ fn a()\n a\n+b\n c\n".to_owned(),
                "@@ -40,4 +41,2 @@\n c\n-d\n e\n-f\n".to_owned(),
            ]
        );
    }

    #[test]
    fn selected_lines_pass_git_apply_check() {
        let scratch = Scratch::new("apply-check");
        let repo = scratch.0.join("repo");
        fs::create_dir_all(&repo).expect("create repo dir");
        git(&repo, &["init", "-q"]);
        fs::write(repo.join("f.txt"), "1\n2\n3\n4\n5\n").expect("write base");
        git(&repo, &["add", "f.txt"]);
        git(&repo, &["commit", "-q", "-m", "base"]);
        fs::write(repo.join("f.txt"), "1\nTWO\n3\n5\nsix\n").expect("write edit");

        // Stage: the diff is against the index, which still equals HEAD.
        let unstaged = git(&repo, &["diff", "--no-color", "--", "f.txt"]);
        let changes = change_indices(&unstaged);
        assert_eq!(changes.len(), 4);
        let mut selections = vec![changes.clone()];
        selections.extend(changes.iter().map(|&index| vec![index]));
        for selected in &selections {
            let patch_text =
                line_patch(&unstaged, 0, selected, false).expect("selection has changes");
            assert_applies(&repo, &scratch.0, &patch_text);
        }

        // Unstage: stage everything, then revert selected lines against the index.
        git(&repo, &["add", "f.txt"]);
        let staged = git(&repo, &["diff", "--cached", "--no-color", "--", "f.txt"]);
        let staged_changes = change_indices(&staged);
        assert_eq!(staged_changes.len(), 4);
        let mut reverse_selections = vec![staged_changes.clone()];
        reverse_selections.extend(staged_changes.iter().map(|&index| vec![index]));
        for selected in &reverse_selections {
            let patch_text = line_patch(&staged, 0, selected, true).expect("selection has changes");
            assert_applies(&repo, &scratch.0, &patch_text);
        }
    }
}
