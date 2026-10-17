//! Contains third-party MIT-licensed code; see THIRD_PARTY_NOTICES.
//! Pure string algorithms only; Dial owns path authority, witnesses and filesystem transactions.
use super::{error::EditError, text::js_trim};
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Operation {
    Create,
    Delete,
    Update,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApplyPatchEntry {
    pub path: String,
    pub op: Operation,
    pub rename: Option<String>,
    pub diff: Option<String>,
}
const BEGIN_PATCH_MARKER: &str = "*** Begin Patch";
const END_PATCH_MARKER: &str = "*** End Patch";
const ADD_FILE_MARKER: &str = "*** Add File: ";
const DELETE_FILE_MARKER: &str = "*** Delete File: ";
const UPDATE_FILE_MARKER: &str = "*** Update File: ";
const MOVE_TO_MARKER: &str = "*** Move to: ";
fn parse_error(message: impl Into<String>, line: Option<usize>) -> EditError {
    let message = message.into();
    EditError::parse(match line {
        Some(line) => format!("Line {line}: {message}"),
        None => message,
    })
}

fn parse_with_options(
    patch_text: &str,
    streaming: bool,
) -> Result<Vec<ApplyPatchEntry>, EditError> {
    let mut lines = js_trim(patch_text).split('\n').collect::<Vec<_>>();
    if lines.len() >= 2
        && matches!(lines[0], "<<EOF" | "<<'EOF'" | "<<\"EOF\"")
        && js_trim(lines[lines.len() - 1]) == "EOF"
    {
        lines = lines[1..lines.len() - 1].to_vec();
    }
    if lines.is_empty()
        || lines
            .first()
            .is_none_or(|line| js_trim(line) != BEGIN_PATCH_MARKER)
    {
        return if streaming {
            Ok(Vec::new())
        } else {
            Err(parse_error(
                "The first line of the patch must be '*** Begin Patch'",
                None,
            ))
        };
    }
    let has_end = lines
        .last()
        .is_some_and(|line| js_trim(line) == END_PATCH_MARKER);
    if !has_end && !streaming {
        return Err(parse_error(
            "The last line of the patch must be '*** End Patch'",
            None,
        ));
    }
    let end = if has_end {
        lines.len() - 1
    } else {
        lines.len()
    };
    let mut index = 1;
    let mut output = Vec::new();
    while index < end {
        if js_trim(lines[index]).is_empty() {
            index += 1;
            continue;
        }
        let first = js_trim(lines[index]);
        if let Some(path) = first.strip_prefix(ADD_FILE_MARKER) {
            index += 1;
            let mut content = String::new();
            while index < end {
                let Some(line) = lines[index].strip_prefix('+') else {
                    break;
                };
                content.push_str(line);
                content.push('\n');
                index += 1;
            }
            output.push(ApplyPatchEntry {
                path: path.to_owned(),
                op: Operation::Create,
                rename: None,
                diff: Some(content),
            });
            continue;
        }
        if let Some(path) = first.strip_prefix(DELETE_FILE_MARKER) {
            output.push(ApplyPatchEntry {
                path: path.to_owned(),
                op: Operation::Delete,
                rename: None,
                diff: None,
            });
            index += 1;
            continue;
        }
        if let Some(path) = first.strip_prefix(UPDATE_FILE_MARKER) {
            let path = path.to_owned();
            index += 1;
            let mut rename = None;
            if index < end
                && let Some(destination) = lines[index].strip_prefix(MOVE_TO_MARKER)
            {
                rename = Some(destination.to_owned());
                index += 1;
            }
            let body_start = index;
            while index < end {
                let line = lines[index];
                if line.starts_with("*** Add File:")
                    || line.starts_with("*** Delete File:")
                    || line.starts_with("*** Update File:")
                {
                    break;
                }
                index += 1;
            }
            if body_start == index {
                if streaming {
                    output.push(ApplyPatchEntry {
                        path,
                        op: Operation::Update,
                        rename,
                        diff: Some(String::new()),
                    });
                    continue;
                }
                return Err(parse_error(
                    format!("Update file hunk for path '{path}' is empty"),
                    Some(body_start + 1),
                ));
            }
            output.push(ApplyPatchEntry {
                path,
                op: Operation::Update,
                rename,
                diff: Some(lines[body_start..index].join("\n")),
            });
            continue;
        }
        if streaming {
            break;
        }
        return Err(parse_error(
            format!(
                "'{first}' is not a valid hunk header. Valid hunk headers: '*** Add File: {{path}}', \
				 '*** Delete File: {{path}}', '*** Update File: {{path}}'"
            ),
            Some(index + 1),
        ));
    }
    Ok(output)
}

/// Parse a complete Codex `apply_patch` envelope.
pub fn parse_apply_patch(patch_text: &str) -> Result<Vec<ApplyPatchEntry>, EditError> {
    parse_with_options(patch_text, false)
}

/// Best-effort parser for a partially streamed envelope.
pub fn parse_apply_patch_streaming(patch_text: &str) -> Result<Vec<ApplyPatchEntry>, EditError> {
    parse_with_options(patch_text, true)
}
