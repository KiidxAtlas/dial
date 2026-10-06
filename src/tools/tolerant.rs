//! Accept an `edit` whose `old` differs from the file only in whitespace.
//!
//! The commonest wasted round for a weaker model is a block copied with the wrong indentation or a stray trailing
//! space: the text is right, the edit fails, and the retry costs a whole generation. This accepts such a block, but
//! only when the match is unambiguous and uniform:
//!
//!  * every line equals the file's line once leading and trailing whitespace is ignored (same text, same order);
//!  * exactly one place in the file matches;
//!  * the indentation differs by one common prefix on every non-blank line, so `new` can be shifted by the same
//!    prefix and keep its own relative indentation.
//!
//! Anything else (two candidates, a line whose text differs, indentation that moved unevenly) is refused and falls
//! back to the byte-exact path with its diagnostics. There is no similarity score and no guessing at renamed text.

/// The file's text with the matched block replaced.
pub(super) struct Found {
    pub updated: String,
    /// 1-based inclusive line range that matched.
    pub lines: (usize, usize),
    /// How `new` was re-indented, for the message back to the model.
    pub reindent: Option<String>,
}

pub(super) enum Outcome {
    Unique(Found),
    /// Several blocks match once whitespace is ignored; the 1-based starting lines.
    Ambiguous(Vec<usize>),
    None,
}

struct Line<'a> {
    text: &'a str,
    start: usize,
    /// End of the text excluding its line terminator, and end including it.
    end: usize,
    end_with_eol: usize,
    eol: &'a str,
}

fn split_lines(text: &str) -> Vec<Line<'_>> {
    let mut lines = Vec::new();
    let mut start = 0;
    for piece in text.split_inclusive('\n') {
        let body = piece.strip_suffix('\n').unwrap_or(piece);
        let (body, eol) = match body.strip_suffix('\r') {
            Some(body) if piece.ends_with('\n') => (body, "\r\n"),
            _ if piece.ends_with('\n') => (body, "\n"),
            _ => (body, ""),
        };
        lines.push(Line {
            text: body,
            start,
            end: start + body.len(),
            end_with_eol: start + piece.len(),
            eol,
        });
        start += piece.len();
    }
    lines
}

fn indent(line: &str) -> &str {
    &line[..line.len() - line.trim_start().len()]
}

/// `Some(prefix)` when `file_indent` is `old_indent` with `prefix` added in front, or `Some("-N")` when it is
/// shorter by the first N characters of `old_indent`.
#[derive(Clone, PartialEq)]
enum Shift {
    None,
    Add(String),
    Remove(String),
}

fn shift_between(old_indent: &str, file_indent: &str) -> Option<Shift> {
    if old_indent == file_indent {
        return Some(Shift::None);
    }
    if let Some(prefix) = file_indent.strip_suffix(old_indent) {
        return Some(Shift::Add(prefix.to_owned()));
    }
    old_indent
        .strip_suffix(file_indent)
        .map(|prefix| Shift::Remove(prefix.to_owned()))
}

fn apply_shift(line: &str, shift: &Shift) -> String {
    if line.trim().is_empty() {
        return line.to_owned();
    }
    match shift {
        Shift::None => line.to_owned(),
        Shift::Add(prefix) => format!("{prefix}{line}"),
        Shift::Remove(prefix) => line
            .strip_prefix(prefix.as_str())
            .unwrap_or(line)
            .to_owned(),
    }
}

pub(super) fn find(text: &str, old: &str, new: &str) -> Outcome {
    let old_lines: Vec<&str> = old.lines().collect();
    if old_lines.iter().all(|l| l.trim().is_empty()) {
        return Outcome::None;
    }
    let file = split_lines(text);
    if old_lines.len() > file.len() {
        return Outcome::None;
    }
    let mut hits: Vec<usize> = Vec::new();
    for at in 0..=file.len() - old_lines.len() {
        if old_lines
            .iter()
            .zip(&file[at..])
            .all(|(o, f)| o.trim() == f.text.trim())
        {
            hits.push(at);
        }
    }
    match hits.as_slice() {
        [] => Outcome::None,
        [at] => {
            build(text, &file, *at, &old_lines, old, new).map_or(Outcome::None, Outcome::Unique)
        }
        many => Outcome::Ambiguous(many.iter().map(|at| at + 1).collect()),
    }
}

fn build(
    text: &str,
    file: &[Line],
    at: usize,
    old_lines: &[&str],
    old: &str,
    new: &str,
) -> Option<Found> {
    let block = &file[at..at + old_lines.len()];
    // One uniform shift for every non-blank line, or no tolerance at all.
    let mut shift: Option<Shift> = None;
    for (o, f) in old_lines.iter().zip(block) {
        if o.trim().is_empty() {
            continue;
        }
        let this = shift_between(indent(o), indent(f.text))?;
        match &shift {
            None => shift = Some(this),
            Some(first) if *first == this => {}
            Some(_) => return None,
        }
    }
    let shift = shift.unwrap_or(Shift::None);
    let eol = block
        .iter()
        .find(|l| !l.eol.is_empty())
        .map_or("\n", |l| l.eol);
    let mut replacement = new
        .lines()
        .map(|l| apply_shift(l, &shift))
        .collect::<Vec<_>>()
        .join(eol);
    let last = block.last()?;
    let wants_newline = old.ends_with('\n');
    let end = if wants_newline {
        last.end_with_eol
    } else {
        last.end
    };
    if wants_newline && !last.eol.is_empty() && !replacement.is_empty() {
        replacement.push_str(eol);
    }
    let mut updated = String::with_capacity(text.len() + replacement.len());
    updated.push_str(&text[..block[0].start]);
    updated.push_str(&replacement);
    updated.push_str(&text[end..]);
    let reindent = match &shift {
        Shift::None => None,
        Shift::Add(prefix) => Some(format!(
            "`new` was indented by {} more to match the file",
            describe(prefix)
        )),
        Shift::Remove(prefix) => Some(format!(
            "`new` was outdented by {} to match the file",
            describe(prefix)
        )),
    };
    Some(Found {
        updated,
        lines: (at + 1, at + old_lines.len()),
        reindent,
    })
}

fn describe(prefix: &str) -> String {
    let tabs = prefix.chars().filter(|c| *c == '\t').count();
    let spaces = prefix.chars().filter(|c| *c == ' ').count();
    match (tabs, spaces) {
        (0, n) => format!("{n} space{}", if n == 1 { "" } else { "s" }),
        (n, 0) => format!("{n} tab{}", if n == 1 { "" } else { "s" }),
        (t, s) => format!("{t} tab(s) and {s} space(s)"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unique(text: &str, old: &str, new: &str) -> Found {
        match find(text, old, new) {
            Outcome::Unique(found) => found,
            Outcome::Ambiguous(l) => panic!("ambiguous {l:?}"),
            Outcome::None => panic!("no match"),
        }
    }

    #[test]
    fn a_block_with_the_wrong_indentation_matches_and_new_is_shifted_with_it() {
        let text = "fn f() {\n        if x {\n            go();\n        }\n}\n";
        // The model wrote the block at 4 spaces instead of 8.
        let found = unique(
            text,
            "    if x {\n        go();\n    }",
            "    if x {\n        stop();\n        go();\n    }",
        );
        assert_eq!(
            found.updated,
            "fn f() {\n        if x {\n            stop();\n            go();\n        }\n}\n"
        );
        assert_eq!(found.lines, (2, 4));
        assert!(found.reindent.unwrap().contains("4 spaces more"));
    }

    #[test]
    fn tabs_added_in_front_are_applied_to_new_too() {
        let found = unique(
            "a\n\t\tx = 1;\n\t\ty = 2;\nb\n",
            "x = 1;\ny = 2;",
            "x = 10;\ny = 2;",
        );
        assert_eq!(found.updated, "a\n\t\tx = 10;\n\t\ty = 2;\nb\n");
    }

    #[test]
    fn trailing_whitespace_and_a_missing_final_newline_are_tolerated() {
        let found = unique("a\nvalue = 1;   \nb\n", "value = 1;\n", "value = 2;\n");
        assert_eq!(found.updated, "a\nvalue = 2;\nb\n");
        assert!(found.reindent.is_none());
    }

    #[test]
    fn crlf_files_keep_their_line_endings() {
        let found = unique(
            "a\r\n    x = 1;\r\n    y = 2;\r\nb\r\n",
            "x = 1;\ny = 2;",
            "x = 1;\nz = 3;\ny = 2;",
        );
        assert_eq!(
            found.updated,
            "a\r\n    x = 1;\r\n    z = 3;\r\n    y = 2;\r\nb\r\n"
        );
    }

    #[test]
    fn a_block_at_the_end_of_a_file_without_a_newline_does_not_gain_one() {
        let found = unique("a\n  tail();", "tail();", "tail2();");
        assert_eq!(found.updated, "a\n  tail2();");
    }

    #[test]
    fn two_matches_are_reported_not_chosen() {
        match find("  x();\nmid\n    x();\n", "x();", "y();") {
            Outcome::Ambiguous(lines) => assert_eq!(lines, vec![1, 3]),
            _ => panic!("must be ambiguous"),
        }
    }

    #[test]
    fn different_text_is_never_matched() {
        assert!(matches!(
            find("    a();\n    b();\n", "a();\nc();", "x"),
            Outcome::None
        ));
        assert!(matches!(find("    a();\n", "A();", "x"), Outcome::None));
        // Same lines in a different order are different text.
        assert!(matches!(find("a\nb\n", "b\na", "x"), Outcome::None));
    }

    #[test]
    fn indentation_that_moved_unevenly_is_refused() {
        // Line one is 4 deeper in the file, line two is 2 deeper: no single shift explains it.
        assert!(matches!(
            find("    a();\n  b();\n", "a();\nb();", "x();"),
            Outcome::None
        ));
    }

    #[test]
    fn blank_lines_inside_the_block_are_allowed_and_a_blank_only_old_is_not() {
        let found = unique("  a();\n\n  b();\n", "a();\n\nb();", "a();\n\nc();");
        assert_eq!(found.updated, "  a();\n\n  c();\n");
        assert!(matches!(find("a\n\nb\n", "\n\n", "x"), Outcome::None));
    }
}
