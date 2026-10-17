//! Repository metadata for the commit prompt.

use std::path::Path;

use regex::Regex;

use super::stage::GenerationContext;
use crate::tools::git::run_checked;

/// Subjects read for scope and recent-commit context.
const LOG_LIMIT: &str = "100";
/// Recent subjects shown to the model.
const RECENT_COMMITS: usize = 10;
/// Most frequent scopes shown to the model.
const COMMON_SCOPES: usize = 10;

type Frameworks = &'static [(&'static str, &'static str)];

const RUST_FRAMEWORKS: Frameworks = &[
    ("axum", "Axum"),
    ("actix-web", "Actix Web"),
    ("rocket", "Rocket"),
    ("warp", "Warp"),
    ("tide", "Tide"),
    ("poem", "Poem"),
    ("tower-http", "Tower HTTP"),
    ("hyper", "Hyper"),
    ("tokio", "Tokio async runtime"),
    ("bevy", "Bevy game engine"),
    ("iced", "Iced GUI"),
    ("egui", "egui GUI"),
    ("tauri", "Tauri"),
    ("leptos", "Leptos"),
    ("yew", "Yew"),
    ("dioxus", "Dioxus"),
];

const NODE_FRAMEWORKS: Frameworks = &[
    ("next", "Next.js"),
    ("nuxt", "Nuxt"),
    ("@angular/core", "Angular"),
    ("vue", "Vue"),
    ("react", "React"),
    ("svelte", "Svelte"),
    ("solid-js", "SolidJS"),
    ("express", "Express"),
    ("fastify", "Fastify"),
    ("hono", "Hono"),
    ("nestjs", "NestJS"),
    ("@nestjs/core", "NestJS"),
    ("electron", "Electron"),
    ("expo", "Expo"),
    ("react-native", "React Native"),
];

const PYTHON_FRAMEWORKS: Frameworks = &[
    ("fastapi", "FastAPI"),
    ("django", "Django"),
    ("flask", "Flask"),
    ("starlette", "Starlette"),
    ("litestar", "Litestar"),
    ("sanic", "Sanic"),
    ("tornado", "Tornado"),
    ("aiohttp", "aiohttp"),
    ("pytorch", "PyTorch"),
    ("torch", "PyTorch"),
    ("tensorflow", "TensorFlow"),
    ("jax", "JAX"),
    ("transformers", "Hugging Face"),
];

const GO_FRAMEWORKS: Frameworks = &[
    ("github.com/gin-gonic/gin", "Gin"),
    ("github.com/labstack/echo", "Echo"),
    ("github.com/gofiber/fiber", "Fiber"),
    ("github.com/go-chi/chi", "chi"),
    ("github.com/gorilla/mux", "Gorilla Mux"),
    ("connectrpc.com/connect", "Connect"),
    ("google.golang.org/grpc", "gRPC"),
];

/// Language, framework, package manager, and workspace metadata for one repository.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct RepositoryContext {
    language: Option<String>,
    framework: Option<String>,
    package_manager: Option<String>,
    is_monorepo: bool,
    package_count: Option<usize>,
}

/// Detects the repository in Rust, Node, Python, Go priority order.
pub(crate) fn detect(root: &Path) -> RepositoryContext {
    detect_rust(root)
        .or_else(|| detect_node(root))
        .or_else(|| detect_python(root))
        .or_else(|| detect_go(root))
        .unwrap_or_default()
}

/// Formats detected metadata for the analysis prompt.
pub(crate) fn format_context(context: &RepositoryContext) -> Option<String> {
    let mut language = context.language.clone()?;
    if context.is_monorepo {
        language = match context.package_count {
            None => format!("{language} (workspace)"),
            Some(count) => format!("{language} (workspace, {count} packages)"),
        };
    }
    let mut lines = vec![format!("Language: {language}")];
    if let Some(framework) = &context.framework {
        lines.push(format!("Framework: {framework}"));
    }
    if let Some(manager) = &context.package_manager {
        lines.push(format!("Package manager: {manager}"));
    }
    Some(lines.join("\n"))
}

fn detect_rust(root: &Path) -> Option<RepositoryContext> {
    let content = optional_text(&root.join("Cargo.toml"))?;
    let is_monorepo = content.contains("[workspace]");
    let package_count = if is_monorepo {
        members_count(&content)
    } else {
        None
    };
    Some(RepositoryContext {
        language: Some("Rust".to_owned()),
        framework: detect_framework(&content, RUST_FRAMEWORKS),
        package_manager: Some("cargo".to_owned()),
        is_monorepo,
        package_count,
    })
}

/// Counts the quoted entries of `members = [ ... ]`.
fn members_count(content: &str) -> Option<usize> {
    let members = Regex::new(r"(?s)members\s*=\s*\[([^\]]*)\]").ok()?;
    let quoted = Regex::new(r#""[^"]+""#).ok()?;
    let body = members.captures(content)?.get(1)?.as_str();
    Some(quoted.find_iter(body).count())
}

fn detect_node(root: &Path) -> Option<RepositoryContext> {
    let content = optional_text(&root.join("package.json"))?;
    let data: serde_json::Value = serde_json::from_str(&content).unwrap_or_default();
    let dependencies = node_dependency_names(&data, &content);
    let is_typescript = dependencies.iter().any(|name| name == "typescript")
        || root.join("tsconfig.json").is_file();
    let package_manager = if root.join("pnpm-lock.yaml").is_file() {
        "pnpm"
    } else if root.join("yarn.lock").is_file() {
        "yarn"
    } else if root.join("bun.lockb").is_file() || root.join("bun.lock").is_file() {
        "bun"
    } else {
        "npm"
    };
    let workspaces = data.get("workspaces").filter(|value| !value.is_null());
    let is_monorepo = workspaces.is_some_and(|value| value != &serde_json::Value::Bool(false))
        || root.join("pnpm-workspace.yaml").is_file();
    Some(RepositoryContext {
        language: Some(
            if is_typescript {
                "TypeScript"
            } else {
                "JavaScript"
            }
            .to_owned(),
        ),
        framework: dependency_framework(&dependencies, NODE_FRAMEWORKS),
        package_manager: Some(package_manager.to_owned()),
        is_monorepo,
        package_count: workspaces.and_then(workspace_count),
    })
}

fn workspace_count(workspaces: &serde_json::Value) -> Option<usize> {
    if let Some(items) = workspaces.as_array() {
        return Some(items.len());
    }
    workspaces
        .get("packages")
        .and_then(serde_json::Value::as_array)
        .map(Vec::len)
}

/// Dependency names from the four dependency maps. Without any, quoted keys in the raw text
/// are used.
fn node_dependency_names(data: &serde_json::Value, content: &str) -> Vec<String> {
    let mut names = Vec::new();
    for field in [
        "dependencies",
        "devDependencies",
        "peerDependencies",
        "optionalDependencies",
    ] {
        if let Some(map) = data.get(field).and_then(serde_json::Value::as_object) {
            names.extend(map.keys().cloned());
        }
    }
    if names.is_empty()
        && let Ok(keys) = Regex::new(r#""([@\w./-]+)"\s*:"#)
    {
        names.extend(keys.captures_iter(content).map(|c| c[1].to_owned()));
    }
    names
}

fn detect_python(root: &Path) -> Option<RepositoryContext> {
    let pyproject = optional_text(&root.join("pyproject.toml"));
    let setup = optional_text(&root.join("setup.py"));
    let requirements = optional_text(&root.join("requirements.txt"));
    if pyproject.is_none() && setup.is_none() && requirements.is_none() {
        return None;
    }
    let tool = pyproject.as_deref().unwrap_or_default();
    let package_manager = if tool.contains("[tool.poetry") {
        "poetry"
    } else if tool.contains("[tool.uv") || root.join("uv.lock").is_file() {
        "uv"
    } else if tool.contains("[tool.pdm") || root.join("pdm.lock").is_file() {
        "pdm"
    } else if root.join("Pipfile").is_file() {
        "pipenv"
    } else {
        "pip"
    };
    let text = [
        pyproject.as_deref(),
        requirements.as_deref(),
        setup.as_deref(),
    ]
    .into_iter()
    .flatten()
    .collect::<Vec<_>>()
    .join("\n")
    .to_lowercase();
    Some(RepositoryContext {
        language: Some("Python".to_owned()),
        framework: detect_framework(&text, PYTHON_FRAMEWORKS),
        package_manager: Some(package_manager.to_owned()),
        is_monorepo: pyproject.is_some()
            && (tool.contains("[tool.uv.workspace") || tool.contains("[tool.pdm.workspace")),
        package_count: None,
    })
}

fn detect_go(root: &Path) -> Option<RepositoryContext> {
    let content = optional_text(&root.join("go.mod"))?;
    Some(RepositoryContext {
        language: Some("Go".to_owned()),
        framework: detect_framework(&content, GO_FRAMEWORKS),
        package_manager: Some("go mod".to_owned()),
        is_monorepo: false,
        package_count: None,
    })
}

fn detect_framework(content: &str, frameworks: Frameworks) -> Option<String> {
    let lower = content.to_lowercase();
    frameworks
        .iter()
        .find(|(needle, _)| lower.contains(*needle))
        .map(|(_, name)| (*name).to_owned())
}

fn dependency_framework(dependencies: &[String], frameworks: Frameworks) -> Option<String> {
    frameworks
        .iter()
        .find(|(name, _)| dependencies.iter().any(|dep| dep == *name))
        .map(|(_, label)| (*label).to_owned())
}

fn optional_text(path: &Path) -> Option<String> {
    std::fs::read_to_string(path).ok()
}

/// Project names for the prompt: the repository directory, plus the single Python package
/// directory at its root when there is exactly one.
pub(crate) fn project_names(root: &Path) -> Vec<String> {
    let mut names = Vec::new();
    if let Some(base) = root.file_name().and_then(|name| name.to_str()) {
        names.push(base.to_owned());
    }
    if let Ok(entries) = std::fs::read_dir(root) {
        let mut packages = Vec::new();
        for entry in entries.flatten() {
            let is_dir = entry.file_type().is_ok_and(|kind| kind.is_dir());
            if is_dir && entry.path().join("__init__.py").is_file() {
                packages.push(entry.file_name().to_string_lossy().into_owned());
            }
        }
        if packages.len() == 1 {
            names.push(packages.remove(0));
        }
    }
    let mut unique: Vec<String> = Vec::new();
    for name in names {
        if !unique.contains(&name) {
            unique.push(name);
        }
    }
    unique
}

/// Builds the generation context for the repository at `root`.
/// A failing `git log` yields no history rather than an error.
pub(crate) fn generation_context(root: &Path) -> GenerationContext {
    let log =
        run_checked(root, &["log", "-n", LOG_LIMIT, "--format=%s"], false).unwrap_or_default();
    let subjects: Vec<&str> = log.lines().collect();
    let recent = subjects
        .iter()
        .take(RECENT_COMMITS)
        .copied()
        .collect::<Vec<_>>()
        .join("\n");
    let scopes = common_scopes(&subjects);
    GenerationContext {
        user_context: None,
        recent_commits: (!recent.is_empty()).then_some(recent),
        common_scopes: (!scopes.is_empty()).then_some(scopes),
        project_context: format_context(&detect(root)),
        project_names: project_names(root),
    }
}

/// Top scopes of `type(scope): subject` lines, most frequent first, formatted `scope (count)`.
/// Ties keep first-seen order.
pub(crate) fn common_scopes(subjects: &[&str]) -> String {
    let mut counts: Vec<(&str, usize)> = Vec::new();
    for subject in subjects {
        let Some(colon) = subject.find(':') else {
            continue;
        };
        let prefix = &subject[..colon];
        let Some(start) = prefix.find('(') else {
            continue;
        };
        let Some(end) = prefix[start + 1..]
            .find(')')
            .map(|offset| start + 1 + offset)
        else {
            continue;
        };
        let commit_type = &prefix[..start];
        let scope = &prefix[start + 1..end];
        let suffix = &prefix[end + 1..];
        if commit_type.is_empty()
            || commit_type.contains(char::is_whitespace)
            || scope.trim().is_empty()
            || !(suffix.is_empty() || suffix == "!")
        {
            continue;
        }
        match counts.iter_mut().find(|(name, _)| *name == scope) {
            Some((_, count)) => *count += 1,
            None => counts.push((scope, 1)),
        }
    }
    counts.sort_by_key(|entry| std::cmp::Reverse(entry.1));
    counts
        .iter()
        .take(COMMON_SCOPES)
        .map(|(scope, count)| format!("{scope} ({count})"))
        .collect::<Vec<_>>()
        .join(", ")
}

#[cfg(test)]
mod tests;
