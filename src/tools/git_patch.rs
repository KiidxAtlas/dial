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
        if keep_marker && let Some(marker) = line.marker {
            body.push_str(marker);
            body.push('\n');
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
mod tests;
