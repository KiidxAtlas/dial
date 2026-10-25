//! `/graphify [tree|html|callflow]` renders the project's Graphify knowledge graph with Graphify's
//! own exporters and opens the page in the browser.
//!
//! The graph is `graphify-out/graph.json` under the project root, or the directory `$GRAPHIFY_OUT`
//! names, resolved the way Graphify resolves it. The exporter runs as `uvx --from graphifyy
//! graphify …` in a background job whose output streams into the transcript. This command only
//! renders an existing graph: it never builds one, so it starts no model or network extraction.
use super::CommandSpec;
use super::maintenance::spawn_process;
use crate::app::{App, Shell, Step};
use anyhow::{Result, ensure};
use std::ffi::{OsStr, OsString};
use std::path::{Component, Path, PathBuf};

pub(super) const COMMANDS: &[CommandSpec] = &[CommandSpec {
    name: "graphify",
    description: "open the project's Graphify knowledge graph in the browser (default: tree)",
    args_hint: "[tree|html|callflow]",
    subcommands: &["tree", "html", "callflow"],
}];

const TITLE: &str = "Graphify";
const USAGE: &str = "usage: /graphify [tree|html|callflow]";
/// Builds or refreshes the graph locally with tree-sitter; no API key, nothing leaves the machine.
const BUILD_HINT: &str = "uvx --from graphifyy graphify extract . --code-only";

/// Rendering touches neither the agent nor the conversation, so it runs at once even during a turn.
pub(super) fn run_live(
    shell: &mut Shell,
    app: &App,
    name: &str,
    args: &str,
) -> Option<Result<Step>> {
    (name == "graphify").then(|| {
        view(shell, &app.layout.project_root, args)?;
        Ok(Step::Done)
    })
}

/// A Graphify exporter that writes one self-contained HTML page.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum View {
    /// Collapsible D3 tree by directory; readable at any graph size.
    Tree,
    /// Interactive force graph; graphs above 5000 nodes become one node per community.
    Html,
    /// Mermaid architecture and call-flow diagrams.
    Callflow,
}

impl View {
    fn parse(arg: &str) -> Option<Self> {
        match arg {
            "" | "tree" => Some(Self::Tree),
            "html" => Some(Self::Html),
            "callflow" => Some(Self::Callflow),
            _ => None,
        }
    }

    /// The page the exporter writes into the output directory. `export html` has no output flag
    /// and always writes `graph.html` beside the graph, which is that same directory.
    fn file(self) -> &'static str {
        match self {
            Self::Tree => "GRAPH_TREE.html",
            Self::Html => "graph.html",
            Self::Callflow => "callflow.html",
        }
    }

    /// Exporter arguments after `graphify`, always naming the graph explicitly.
    fn args(self, graph: &Path, page: &Path) -> Vec<OsString> {
        let mut args: Vec<OsString> = match self {
            Self::Tree => vec!["tree".into()],
            Self::Html => vec!["export".into(), "html".into()],
            Self::Callflow => vec!["export".into(), "callflow-html".into()],
        };
        args.extend(["--graph".into(), graph.into()]);
        if self != Self::Html {
            args.extend(["--output".into(), page.into()]);
        }
        args
    }
}

/// Graphify's output directory for `root`: `$GRAPHIFY_OUT` when it is set, non-blank and does not
/// name the project or one of its parents (Graphify ignores those values too), else
/// `graphify-out`. A relative value is relative to the project root.
fn out_dir(root: &Path, env: Option<&OsStr>) -> PathBuf {
    let value = env
        .map(|value| value.to_string_lossy().trim().to_owned())
        .filter(|value| !value.is_empty() && !names_root_or_parent(Path::new(value)));
    root.join(value.as_deref().unwrap_or("graphify-out"))
}

/// True when `path` normalizes lexically to `.` or to a run of `..` components.
fn names_root_or_parent(path: &Path) -> bool {
    let mut parts: Vec<Component<'_>> = Vec::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir if matches!(parts.last(), Some(Component::Normal(_))) => {
                parts.pop();
            }
            other => parts.push(other),
        }
    }
    parts
        .iter()
        .all(|component| matches!(component, Component::ParentDir))
}

fn view(shell: &mut Shell, root: &Path, args: &str) -> Result<()> {
    let Some(view) = View::parse(args.trim()) else {
        shell.note(TITLE, USAGE);
        return Ok(());
    };
    let out = out_dir(root, std::env::var_os("GRAPHIFY_OUT").as_deref());
    let graph = out.join("graph.json");
    if !graph.is_file() {
        shell.note(
            TITLE,
            format!(
                "No graph at {}. Build it in the project first: {BUILD_HINT}",
                graph.display()
            ),
        );
        return Ok(());
    }
    // `uvx` is resolved by the spawn itself (including Windows `PATHEXT`); a missing uv surfaces
    // as the job's "not found on PATH" error.
    let page = out.join(view.file());
    let mut argv: Vec<OsString> = vec!["--from".into(), "graphifyy".into(), "graphify".into()];
    argv.extend(view.args(&graph, &page));
    let shown = std::iter::once("uvx".into())
        .chain(argv.iter().map(|arg| arg.to_string_lossy().into_owned()))
        .collect::<Vec<String>>()
        .join(" ");
    spawn_process(
        shell,
        TITLE,
        "uvx".into(),
        argv,
        root.to_path_buf(),
        shown,
        move || {
            ensure!(
                page.is_file(),
                "Graphify finished without writing {}",
                page.display()
            );
            Ok(
                if crate::llm::oauth::util::open_browser(&page.to_string_lossy()) {
                    format!("Opened {} in the browser.", page.display())
                } else {
                    format!("Wrote {}; open it in a browser.", page.display())
                },
            )
        },
    );
    Ok(())
}

#[cfg(test)]
mod tests;
