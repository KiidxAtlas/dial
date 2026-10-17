//! Contains third-party MIT-licensed code; see THIRD_PARTY_NOTICES.
//! Dial retains strict cell identity, metadata and stale-output policy.
//! Jupyter notebook (`.ipynb`) codec: projects cells onto an editable text
//! form (`# %% [type] cell:N` markers). Persistence uses the guarded raw-span
//! carrier, preserving metadata/numeric lexemes and one-space fresh JSON.
//!
//! Notebook cell edits for `.ipynb` documents.

use std::{collections::HashSet, fmt::Write as _, path::Path, sync::LazyLock};

use regex::Regex;
use serde_json::{Map, Value};
use std::borrow::Cow;

/// Notebook decode/encode failure with model-facing diagnostics.
#[derive(Debug, thiserror::Error)]
pub enum NotebookError {
    #[error("Invalid JSON in notebook: {display}")]
    InvalidJson { display: String },
    #[error("Invalid notebook structure (expected object): {display}")]
    NotObject { display: String },
    #[error("Invalid notebook structure (missing cells array): {display}")]
    MissingCells { display: String },
    #[error("Invalid notebook cell {index} in {display}")]
    InvalidCell { index: usize, display: String },
    #[error(
        "Invalid notebook editable representation for {display}: expected first line to be \"# %% \
		 [code] cell:0\", \"# %% [markdown] cell:0\", or \"# %% [raw] cell:0\"."
    )]
    InvalidEditableText { display: String },
    #[error("{message}")]
    Policy { message: String },
}

/// A cell type supported by the editable notebook projection.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NotebookCellType {
    /// An executable code cell.
    Code,
    /// A Markdown cell.
    Markdown,
    /// A raw-text cell.
    Raw,
}

impl NotebookCellType {
    fn parse(value: &str) -> Option<Self> {
        match value {
            "code" => Some(Self::Code),
            "markdown" => Some(Self::Markdown),
            "raw" => Some(Self::Raw),
            _ => None,
        }
    }

    const fn as_str(self) -> &'static str {
        match self {
            Self::Code => "code",
            Self::Markdown => "markdown",
            Self::Raw => "raw",
        }
    }
}

/// One cell parsed from the editable notebook projection.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ParsedVirtualCell {
    /// Parsed cell kind.
    pub cell_type: NotebookCellType,
    /// Original cell index, when the marker named one.
    pub cell_index: Option<usize>,
    /// Unescaped cell source.
    pub source: String,
}

static CELL_MARKER_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^# %% \[(code|markdown|raw)\](?: cell:(\d+))?$")
        .expect("valid notebook cell marker regex")
});
static ESCAPABLE_MARKER_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^# %%+ \[(?:code|markdown|raw)\](?: cell:\d+)?$")
        .expect("valid notebook escape regex")
});
static ESCAPED_MARKER_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^# %%%+ \[(?:code|markdown|raw)\](?: cell:\d+)?$")
        .expect("valid notebook unescape regex")
});

fn escape_marker_like_source_lines(source: &str) -> Cow<'_, str> {
    if !source.contains("# %%") {
        return Cow::Borrowed(source);
    }
    Cow::Owned(
        source
            .split('\n')
            .map(|line| {
                if ESCAPABLE_MARKER_RE.is_match(line) {
                    line.replacen("# %", "# %%", 1)
                } else {
                    line.to_owned()
                }
            })
            .collect::<Vec<_>>()
            .join("\n"),
    )
}

fn unescape_marker_like_line(line: &str) -> String {
    if ESCAPED_MARKER_RE.is_match(line) {
        line.replacen("# %%", "# %", 1)
    } else {
        line.to_owned()
    }
}

/// True when the extension is `.ipynb` (case-insensitive).
pub fn is_notebook_path(path: &Path) -> bool {
    path.extension()
        .is_some_and(|extension| extension.eq_ignore_ascii_case("ipynb"))
}

/// Split notebook source text into nbformat `source` lines (each keeps its
/// trailing `\n`; a final unterminated line is kept as-is).
pub fn split_notebook_source(content: &str) -> Vec<String> {
    if content.is_empty() {
        Vec::new()
    } else {
        content.split_inclusive('\n').map(str::to_owned).collect()
    }
}

fn source_to_text(source: Option<&Value>) -> Cow<'_, str> {
    match source {
        Some(Value::String(source)) => Cow::Borrowed(source),
        Some(Value::Array(lines)) => Cow::Owned(
            lines
                .iter()
                .map(|line| line.as_str().expect("validated notebook source"))
                .collect(),
        ),
        _ => Cow::Borrowed(""),
    }
}

fn validate_notebook<'a>(
    value: &'a Value,
    display: &str,
) -> Result<&'a Map<String, Value>, NotebookError> {
    super::super::notebook::validate(value).map_err(|message| NotebookError::Policy { message })?;
    let object = value.as_object().ok_or_else(|| NotebookError::NotObject {
        display: display.to_owned(),
    })?;
    let cells = object
        .get("cells")
        .and_then(Value::as_array)
        .ok_or_else(|| NotebookError::MissingCells {
            display: display.to_owned(),
        })?;
    for (index, cell) in cells.iter().enumerate() {
        let valid = cell
            .as_object()
            .and_then(|cell| cell.get("cell_type"))
            .and_then(Value::as_str)
            .and_then(NotebookCellType::parse)
            .is_some();
        if !valid {
            return Err(NotebookError::InvalidCell {
                index,
                display: display.to_owned(),
            });
        }
    }
    Ok(object)
}

pub fn is_cell_marker(line: &str) -> bool {
    CELL_MARKER_RE.is_match(line)
}
/// Decode notebook JSON into the editable text form.
pub fn notebook_to_editable_text(json: &str, display: &str) -> Result<String, NotebookError> {
    let json = json.strip_prefix('\u{feff}').unwrap_or(json);
    let notebook: Value = serde_json::from_str(json).map_err(|_| NotebookError::InvalidJson {
        display: display.to_owned(),
    })?;
    notebook_value_to_editable_text(&notebook, display)
}
pub fn notebook_value_to_editable_text(
    notebook: &Value,
    display: &str,
) -> Result<String, NotebookError> {
    let object = validate_notebook(notebook, display)?;
    let cells = object
        .get("cells")
        .and_then(Value::as_array)
        .expect("validated cells");
    let mut text = String::new();
    for (index, cell) in cells.iter().enumerate() {
        if index > 0 {
            text.push('\n');
        }
        let kind = cell["cell_type"].as_str().expect("validated cell type");
        write!(text, "# %% [{kind}] cell:{index}").expect("writing to String cannot fail");
        let source = source_to_text(cell.get("source"));
        let source = super::text::normalize_to_lf(&source);
        if !source.is_empty() {
            text.push('\n');
            text.push_str(&escape_marker_like_source_lines(&source));
        }
    }
    Ok(text)
}

/// Parse an editable notebook projection into virtual cells.
pub fn parse_notebook_editable_text(
    text: &str,
    display: &str,
) -> Result<Vec<ParsedVirtualCell>, NotebookError> {
    let mut cells = Vec::new();
    let mut current: Option<(NotebookCellType, Option<usize>, Vec<String>)> = None;
    let lines: Vec<&str> = if text.is_empty() {
        Vec::new()
    } else {
        text.split('\n').collect()
    };

    for line in lines {
        if let Some(captures) = CELL_MARKER_RE.captures(line) {
            if let Some((cell_type, cell_index, source_lines)) = current.take() {
                cells.push(ParsedVirtualCell {
                    cell_type,
                    cell_index,
                    source: source_lines.join("\n"),
                });
            }
            let cell_type =
                NotebookCellType::parse(captures.get(1).expect("cell type capture").as_str())
                    .expect("validated marker type");
            let cell_index = captures
                .get(2)
                .map(|value| {
                    value
                        .as_str()
                        .parse::<usize>()
                        .map_err(|_| NotebookError::Policy {
                            message: format!("Invalid cell index in {display}"),
                        })
                })
                .transpose()?;
            current = Some((cell_type, cell_index, Vec::new()));
        } else if let Some((_, _, source_lines)) = current.as_mut() {
            source_lines.push(unescape_marker_like_line(line));
        } else {
            return Err(NotebookError::InvalidEditableText {
                display: display.to_owned(),
            });
        }
    }
    if let Some((cell_type, cell_index, source_lines)) = current {
        cells.push(ParsedVirtualCell {
            cell_type,
            cell_index,
            source: source_lines.join("\n"),
        });
    }
    Ok(cells)
}

/// Create a new nbformat cell with default metadata and code-cell state.
pub fn create_notebook_cell(cell_type: NotebookCellType, source: &str) -> Value {
    let mut cell = Map::new();
    cell.insert(
        "id".into(),
        Value::String(format!("dial-{:032x}", rand::random::<u128>())),
    );
    cell.insert("cell_type".into(), Value::String(cell_type.as_str().into()));
    cell.insert("metadata".into(), Value::Object(Map::new()));
    cell.insert(
        "source".into(),
        Value::Array(
            split_notebook_source(source)
                .into_iter()
                .map(Value::String)
                .collect(),
        ),
    );
    if cell_type == NotebookCellType::Code {
        cell.insert("execution_count".into(), Value::Null);
        cell.insert("outputs".into(), Value::Array(Vec::new()));
    }
    Value::Object(cell)
}

/// Create an empty nbformat 4.5 notebook document.
pub fn create_empty_notebook() -> Value {
    let mut notebook = Map::new();
    notebook.insert("cells".into(), Value::Array(Vec::new()));
    notebook.insert("metadata".into(), Value::Object(Map::new()));
    notebook.insert("nbformat".into(), Value::from(4));
    notebook.insert("nbformat_minor".into(), Value::from(5));
    Value::Object(notebook)
}

/// Apply editable text while preserving all unrelated document and reused-cell values.
pub struct EditedNotebook {
    pub document: Value,
    pub origins: Vec<Option<usize>>,
}
pub fn apply_notebook_editable_text(
    notebook: &Value,
    text: &str,
    display: &str,
    new_notebook: bool,
) -> Result<EditedNotebook, NotebookError> {
    let object = validate_notebook(notebook, display)?;
    let original_cells = object
        .get("cells")
        .and_then(Value::as_array)
        .expect("validated cells");
    let parsed_cells = parse_notebook_editable_text(text, display)?;
    let mut used_original_cells = HashSet::new();
    let mut next_cells = Vec::with_capacity(parsed_cells.len());
    let mut origins = Vec::with_capacity(parsed_cells.len());

    for (position, parsed) in parsed_cells.into_iter().enumerate() {
        let original = match parsed.cell_index {
            Some(index) if index < original_cells.len() && used_original_cells.insert(index) => {
                Some(&original_cells[index])
            }
            Some(index) if new_notebook && original_cells.is_empty() && index == position => None,
            Some(index) => {
                return Err(NotebookError::Policy {
                    message: format!(
                        "Cell marker cell:{index} in {display} is duplicate or not an authentic original cell; omit the index for a new cell"
                    ),
                });
            }
            None => None,
        };
        origins.push(if original.is_some() {
            parsed.cell_index
        } else {
            None
        });
        if let Some(original) = original {
            let mut cell = original.as_object().expect("validated cell").clone();
            cell.insert(
                "cell_type".into(),
                Value::String(parsed.cell_type.as_str().into()),
            );
            let old_source = source_to_text(original.get("source"));
            let changed = super::text::normalize_to_lf(&old_source).as_ref() != parsed.source
                || original["cell_type"].as_str() != Some(parsed.cell_type.as_str());
            if changed {
                let persisted = super::text::restore_line_endings_like(&old_source, &parsed.source);
                cell.insert(
                    "source".into(),
                    Value::Array(
                        split_notebook_source(&persisted)
                            .into_iter()
                            .map(Value::String)
                            .collect(),
                    ),
                );
            }
            if parsed.cell_type == NotebookCellType::Code {
                if changed {
                    cell.insert("execution_count".into(), Value::Null);
                    cell.insert("outputs".into(), Value::Array(Vec::new()));
                }
            } else if original["cell_type"] == "code" {
                cell.remove("execution_count");
                cell.remove("outputs");
            }
            next_cells.push(Value::Object(cell));
        } else {
            next_cells.push(create_notebook_cell(parsed.cell_type, &parsed.source));
        }
    }

    let mut next_notebook = object.clone();
    next_notebook.insert("cells".into(), Value::Array(next_cells));
    let output = Value::Object(next_notebook);
    super::super::notebook::validate(&output)
        .map_err(|message| NotebookError::Policy { message })?;
    Ok(EditedNotebook {
        document: output,
        origins,
    })
}
