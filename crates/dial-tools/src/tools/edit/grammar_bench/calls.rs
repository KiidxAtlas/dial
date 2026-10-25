//! Renders one case as a correct call in each edit grammar.
//!
//! Every renderer is what a careful caller of that grammar would send for the same intended
//! change, using the grammar's own idioms: hashline addresses the snapshot's line numbers,
//! replace, patch, apply-patch and sloppy anchor on unchanged context lines that are widened until
//! the text is unique in the file, and `ast_edit` expresses whole-word renames structurally.
use super::cases::{Case, Hunk};
use crate::config::types::EditMode;
use serde::Serialize;
use serde_json::{Value, json};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Grammar {
    Hashline,
    Replace,
    ApplyPatch,
    Patch,
    Sloppy,
    AstEdit,
}

impl Grammar {
    pub const ALL: [Grammar; 6] = [
        Grammar::Hashline,
        Grammar::Replace,
        Grammar::ApplyPatch,
        Grammar::Patch,
        Grammar::Sloppy,
        Grammar::AstEdit,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Grammar::Hashline => "hashline",
            Grammar::Replace => "replace",
            Grammar::ApplyPatch => "apply_patch",
            Grammar::Patch => "patch",
            Grammar::Sloppy => "sloppy",
            Grammar::AstEdit => "ast_edit",
        }
    }

    pub fn parse(name: &str) -> Option<Grammar> {
        Grammar::ALL
            .into_iter()
            .find(|grammar| grammar.name() == name)
    }

    /// The `edit.mode` the `edit` tool runs under; `None` for the separate `ast_edit` tool.
    pub fn edit_mode(self) -> Option<EditMode> {
        match self {
            Grammar::Hashline => Some(EditMode::Hashline),
            Grammar::Replace => Some(EditMode::Replace),
            Grammar::ApplyPatch => Some(EditMode::ApplyPatch),
            Grammar::Patch => Some(EditMode::Patch),
            Grammar::Sloppy => Some(EditMode::Sloppy),
            Grammar::AstEdit => None,
        }
    }
}

/// One tool call: the tool name and its JSON arguments.
#[derive(Debug, Clone)]
pub struct Call {
    pub tool: &'static str,
    pub args: Value,
}

impl Call {
    /// Serialized size of the arguments, the payload a model has to emit.
    pub fn bytes(&self) -> usize {
        self.args.to_string().len()
    }
}

/// Contiguous region of the original file that one call edits, as diff rows: `' '` unchanged
/// context, `'-'` removed, `'+'` added.
struct Block {
    ops: Vec<(char, String)>,
}

impl Block {
    fn text(&self, keep: &[char]) -> String {
        self.ops
            .iter()
            .filter(|(op, _)| keep.contains(op))
            .map(|(_, line)| format!("{line}\n"))
            .collect()
    }

    fn old_text(&self) -> String {
        self.text(&[' ', '-'])
    }

    fn new_text(&self) -> String {
        self.text(&[' ', '+'])
    }

    fn diff(&self) -> String {
        let rows: Vec<String> = self
            .ops
            .iter()
            .map(|(op, line)| format!("{op}{line}"))
            .collect();
        format!("@@\n{}", rows.join("\n"))
    }
}

fn occurrences(text: &str, pattern: &str) -> usize {
    text.char_indices()
        .filter(|(at, _)| text[*at..].starts_with(pattern))
        .count()
}

/// Smallest symmetric context around the group's changed lines that occurs exactly once in
/// `text` and reaches a non-blank line on each side (a blank-only anchor is not an anchor; leading
/// context only without `trailing`).
fn window(lines: &[String], text: &str, group: &[&Hunk], trailing: bool) -> (usize, usize) {
    let low = group[0].start;
    let last = group[group.len() - 1];
    let high = last.start + last.remove;
    for context in 1..=lines.len() {
        let start = low.saturating_sub(context);
        let end = if trailing {
            (high + context).min(lines.len())
        } else {
            high
        };
        let old: String = lines[start..end]
            .iter()
            .map(|line| format!("{line}\n"))
            .collect();
        let anchored = |range: std::ops::Range<usize>, edge: bool| {
            edge || lines[range].iter().any(|line| !line.trim().is_empty())
        };
        if !old.is_empty()
            && anchored(start..low, start == 0)
            && anchored(high..end, !trailing || end == lines.len())
            && occurrences(text, &old) == 1
        {
            return (start, end);
        }
    }
    (0, lines.len())
}

fn block_ops(lines: &[String], group: &[&Hunk], (start, end): (usize, usize)) -> Block {
    let mut ops = Vec::new();
    let mut hunks = group.iter().peekable();
    let mut at = start;
    while at < end {
        if let Some(hunk) = hunks.next_if(|hunk| hunk.start == at) {
            ops.extend(lines[at..at + hunk.remove].iter().map(|l| ('-', l.clone())));
            ops.extend(hunk.insert.iter().map(|l| ('+', l.clone())));
            at += hunk.remove;
        } else {
            ops.push((' ', lines[at].clone()));
            at += 1;
        }
    }
    for hunk in hunks {
        ops.extend(hunk.insert.iter().map(|l| ('+', l.clone())));
    }
    Block { ops }
}

/// Edit regions with unique context; hunks whose context windows touch share one region.
fn blocks(case: &Case, trailing: bool) -> Vec<Block> {
    let text = case.original();
    let mut groups: Vec<Vec<&Hunk>> = case.hunks.iter().map(|hunk| vec![hunk]).collect();
    groups.sort_by_key(|group| group[0].start);
    loop {
        let windows: Vec<(usize, usize)> = groups
            .iter()
            .map(|group| window(&case.lines, &text, group, trailing))
            .collect();
        match (1..groups.len()).find(|&i| windows[i - 1].1 >= windows[i].0) {
            Some(i) => {
                let tail = groups.remove(i);
                groups[i - 1].extend(tail);
            }
            None => {
                return groups
                    .iter()
                    .zip(windows)
                    .map(|(group, span)| block_ops(&case.lines, group, span))
                    .collect();
            }
        }
    }
}

fn hashline_input(case: &Case, tag: &str) -> String {
    let mut input = format!("[{}#{tag}]\n", case.file);
    let mut hunks: Vec<&Hunk> = case.hunks.iter().collect();
    hunks.sort_by_key(|hunk| hunk.start);
    for hunk in hunks {
        let first = hunk.start + 1;
        let last = hunk.start + hunk.remove;
        let rows: String = hunk
            .insert
            .iter()
            .map(|line| format!("+{line}\n"))
            .collect();
        if hunk.remove == 0 && hunk.start == 0 {
            input.push_str(&format!("PUT <1:\n{rows}"));
        } else if hunk.remove == 0 {
            input.push_str(&format!("PUT >{}:\n{rows}", hunk.start));
        } else if hunk.insert.is_empty() {
            input.push_str(&format!("CUT {first}.={last}\n"));
        } else {
            input.push_str(&format!("PUT {first}.={last}:\n{rows}"));
        }
    }
    input
}

fn sloppy_input(case: &Case) -> String {
    let mut input = format!("*** Edit File: {}", case.file);
    if let Some((old, new)) = &case.rename {
        return format!("{input} all\n*** Find\n{old}\n*** Replace\n{new}\n");
    }
    input.push('\n');
    if let [hunk] = case.hunks.as_slice()
        && hunk.remove == 0
        && let [block] = blocks(case, false).as_slice()
    {
        input.push_str(&format!(
            "*** Find\n{}*** Insert After\n{}",
            block.text(&[' ']),
            block.text(&['+'])
        ));
        return input;
    }
    for block in blocks(case, true) {
        input.push_str(&format!(
            "*** Find\n{}*** Replace\n{}",
            block.old_text(),
            block.new_text()
        ));
    }
    input
}

/// The call for `grammar`, or `None` when the grammar cannot express the case (`ast_edit` only
/// renames). `read` is the model-visible output of reading the file, which supplies the
/// hashline tag.
pub fn render(case: &Case, grammar: Grammar, read: &str) -> Option<Call> {
    let path = case.file.as_str();
    let edit = |args: Value| Call { tool: "edit", args };
    Some(match grammar {
        Grammar::Hashline => {
            let marker = format!("[{path}#");
            let at = read.find(&marker)? + marker.len();
            let tag = read.get(at..at + 4)?;
            edit(json!({"input": hashline_input(case, tag)}))
        }
        Grammar::Replace => match &case.rename {
            Some((old, new)) => edit(
                json!({"path": path, "old_string": old, "new_string": new, "replace_all": true}),
            ),
            None => {
                let [block] = blocks(case, true).try_into().ok()?;
                edit(
                    json!({"path": path, "old_string": block.old_text(), "new_string": block.new_text()}),
                )
            }
        },
        Grammar::Patch => {
            let diff: Vec<String> = blocks(case, true).iter().map(Block::diff).collect();
            edit(json!({"path": path, "edits": [{"op": "update", "diff": diff.join("\n")}]}))
        }
        Grammar::ApplyPatch => {
            let diff: Vec<String> = blocks(case, true).iter().map(Block::diff).collect();
            edit(json!({"input": format!(
                "*** Begin Patch\n*** Update File: {path}\n{}\n*** End Patch\n",
                diff.join("\n")
            )}))
        }
        Grammar::Sloppy => edit(json!({"input": sloppy_input(case)})),
        Grammar::AstEdit => {
            let (old, new) = case.rename.as_ref()?;
            Call {
                tool: "ast_edit",
                args: json!({"pattern": old, "rewrite": new, "path": path, "dry_run": false}),
            }
        }
    })
}
