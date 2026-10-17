//! Agent definitions offered by the `task` tool, and the `[task]` settings that govern them.
//!
//! Sources, in precedence order (the first definition of a name wins; later ones are reported as
//! shadowed warnings):
//! 1. built-in types ([`super::subagents::AGENTS`]); their names are reserved;
//! 2. `[subagents]` presets (custom scouts) and `[agents.<name>]` config entries;
//! 3. Markdown definition files, nearest first: `<project>/.dial/agents/*.md`,
//!    `<project>/.claude/agents/*.md` (each searched from the project root up to its git root),
//!    then `$DIAL_HOME/agents/*.md` (normally `~/.dial/agents`) and `~/.claude/agents/*.md`.
//!
//! A definition file is Markdown with YAML frontmatter; the body is the agent's role prompt:
//!
//! ```markdown
//! ---
//! name: migrator
//! description: applies schema migrations and updates call sites
//! tools: [read, grep, glob, bash, edit, write]   # or "read, grep" (CSV)
//! spawns: [scout]                                # "*" for any agent, [] / omitted for none
//! model: ["@small", "anthropic/claude-sonnet-4-5"] # fallback list; "@role" names a model role
//! thinking_level: low
//! max_steps: 40
//! autoload_skills: [db-migrations]
//! output: { type: object, required: [files], properties: { files: { type: array } } }
//! ---
//! You migrate database schemas. ...
//! ```
//!
//! Definitions are instructions, never permissions: an agent that can change files (`mutates`,
//! inferred from `edit`/`write`/`notebook`/`ast_edit` in `tools`) still needs approval to launch
//! and for each change; read-only agents are refused every operation that needs approval; spawning
//! is bounded by `task.max_recursion_depth`. Claude Code files use Claude tool names (`Read`,
//! `Edit`, …), which are mapped; their `model` aliases are ignored. Symlinked project definition
//! files are skipped so a repository cannot point a definition at files outside it.
use super::subagents::{AGENTS, AgentSpec, builtin};
use crate::config::types::{AgentConfig, Config};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

/// Most definitions loaded from files across all directories.
const MAX_FILE_DEFINITIONS: usize = 64;
const MAX_FILE_BYTES: u64 = 64 * 1024;
const MAX_PROMPT: usize = 32 * 1024;
const MAX_DESCRIPTION: usize = 300;
/// Tools whose presence makes a definition a writer.
const WRITE_TOOLS: &[&str] = &["edit", "write", "notebook", "ast_edit"];

/// `[task]`: subagent orchestration limits.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct TaskConfig {
    /// How many levels deep subagents may start their own subagents: -1 unlimited, 0 none.
    /// The main agent's children are depth 1; with 2 they may spawn one more level.
    pub max_recursion_depth: i32,
    /// Most active worker branches per project in this process (1..=64).
    pub max_concurrency: usize,
    pub isolation_dependencies: bool,
    pub isolation_git: bool,
    pub isolation_dependency_bytes: u64,
    pub isolation_source_bytes: usize,
    pub isolation_source_files: usize,
    /// Per-provider ceiling on in-flight subagent model requests (provider id → limit; 0 or
    /// absent = unlimited). A slot is held only while a request streams, never across a child's
    /// whole run, so deep spawn trees cannot deadlock. A local model server is always 1.
    pub provider_concurrency: BTreeMap<String, usize>,
    /// Load Markdown agent definitions from `.dial/agents` and `~/.dial/agents`.
    pub discover_agents: bool,
    /// Also load Claude Code definitions from `.claude/agents` and `~/.claude/agents`.
    pub claude_agents: bool,
    /// Rejected structured results (`yield` or a JSON answer) a child may correct before failing.
    pub max_schema_retries: u32,
    /// Keep finished subagent transcripts (outside the project) so they can be resumed by id.
    pub persist: bool,
    /// Pending steering messages a running or parked agent may hold (1..=4096).
    pub inbox_limit: usize,
}

impl Default for TaskConfig {
    fn default() -> Self {
        Self {
            max_recursion_depth: 2,
            max_concurrency: 32,
            isolation_dependencies: true,
            isolation_git: true,
            isolation_dependency_bytes: 4 * 1024 * 1024 * 1024,
            isolation_source_bytes: 512 * 1024 * 1024,
            isolation_source_files: 100_000,
            provider_concurrency: BTreeMap::new(),
            discover_agents: true,
            claude_agents: true,
            max_schema_retries: 3,
            persist: true,
            inbox_limit: 256,
        }
    }
}

impl TaskConfig {
    pub fn validate(&self) -> anyhow::Result<()> {
        anyhow::ensure!(
            (1..=1_000_000).contains(&self.isolation_source_files)
                && (1..=2_147_483_648).contains(&self.isolation_source_bytes),
            "invalid isolation source limits"
        );
        anyhow::ensure!(
            self.isolation_dependency_bytes <= 64 * 1024 * 1024 * 1024,
            "isolation dependency limit exceeds 64 GiB"
        );
        anyhow::ensure!(
            (-1..=16).contains(&self.max_recursion_depth),
            "task.max_recursion_depth must be -1 (unlimited) or 0..16"
        );
        anyhow::ensure!(
            (1..=64).contains(&self.max_concurrency),
            "task.max_concurrency must be 1..64"
        );
        anyhow::ensure!(
            self.max_schema_retries <= 10,
            "task.max_schema_retries must be 0..10"
        );
        anyhow::ensure!(
            (1..=4096).contains(&self.inbox_limit),
            "task.inbox_limit must be 1..4096"
        );
        for (provider, limit) in &self.provider_concurrency {
            anyhow::ensure!(
                !provider.trim().is_empty() && *limit <= 1024,
                "task.provider_concurrency.{provider:?} must be 0 (unlimited)..1024"
            );
        }
        Ok(())
    }

    /// Whether an agent at `depth` (main agent 0, its children 1) may still start subagents.
    pub fn can_spawn_at(&self, depth: u32) -> bool {
        self.max_recursion_depth < 0 || i64::from(depth) < i64::from(self.max_recursion_depth)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Source {
    Builtin,
    Preset,
    Config,
    Project,
    ClaudeProject,
    User,
    ClaudeUser,
}

impl Source {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Builtin => "builtin",
            Self::Preset => "preset",
            Self::Config => "config",
            Self::Project => "project",
            Self::ClaudeProject => "claude-project",
            Self::User => "user",
            Self::ClaudeUser => "claude-user",
        }
    }
    fn claude(self) -> bool {
        matches!(self, Self::ClaudeProject | Self::ClaudeUser)
    }
    fn project(self) -> bool {
        matches!(self, Self::Project | Self::ClaudeProject)
    }
}

/// Which agents an agent may start (the `spawns` frontmatter field).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Spawns {
    None,
    All,
    Only(Vec<String>),
}

impl Spawns {
    pub fn from_static(list: &[&str]) -> Self {
        Self::from_list(list.iter().map(|s| (*s).to_owned()).collect())
    }
    pub fn from_list(list: Vec<String>) -> Self {
        let list: Vec<String> = list
            .into_iter()
            .map(|s| s.trim().to_owned())
            .filter(|s| !s.is_empty())
            .collect();
        if list.iter().any(|s| s == "*") {
            Self::All
        } else if list.is_empty() {
            Self::None
        } else {
            Self::Only(list)
        }
    }
    pub fn allows(&self, name: &str) -> bool {
        match self {
            Self::None => false,
            Self::All => true,
            Self::Only(list) => list.iter().any(|allowed| allowed == name),
        }
    }
}

/// One resolved agent type: everything a child run needs to know about its role.
#[derive(Clone, Debug)]
pub struct AgentDef {
    pub name: String,
    pub description: String,
    /// Role-specific instructions appended to the common subagent preamble.
    pub prompt: String,
    /// An explicit tool allowlist that also narrows extension tools (`tools` in a definition).
    pub restrict: Option<Vec<String>>,
    pub max_steps: u32,
    /// May change files. Launching one needs approval; without isolation they run one at a time.
    pub mutates: bool,
    pub spawns: Spawns,
    /// Model fallback list: `@role`, `provider/model[:level]` or a bare model name.
    pub model: Vec<String>,
    /// Thinking level (`off`, `minimal`, `low`, `medium`, `high`, `xhigh`, `max`).
    pub thinking: Option<String>,
    /// Default output schema when the caller passes none.
    pub output: Option<Value>,
    pub autoload_skills: Vec<String>,
    pub source: Source,
    pub path: Option<PathBuf>,
}

impl AgentDef {
    pub fn from_builtin(spec: &AgentSpec) -> Self {
        Self {
            name: spec.name.to_owned(),
            description: spec.description.to_owned(),
            prompt: spec.prompt.to_owned(),
            restrict: None,
            max_steps: spec.max_steps,
            mutates: spec.mutates,
            spawns: Spawns::from_static(spec.spawns),
            model: Vec::new(),
            thinking: None,
            output: None,
            autoload_skills: Vec::new(),
            source: Source::Builtin,
            path: None,
        }
    }

    fn preset(name: &str, text: &str) -> Option<Self> {
        let mut def = Self::from_builtin(builtin("scout")?);
        def.name = name.to_owned();
        def.description = "custom read-only preset (scout with extra instructions)".into();
        def.prompt = format!("{}\n{}", def.prompt, text.trim());
        def.source = Source::Preset;
        Some(def)
    }

    fn from_config(name: &str, agent: &AgentConfig) -> Result<Self, String> {
        let base = builtin(&agent.base)
            .ok_or_else(|| format!("custom agent {name:?}: unknown base {:?}", agent.base))?;
        let mut def = Self::from_builtin(base);
        def.name = name.to_owned();
        if !agent.description.is_empty() {
            def.description = agent.description.clone();
        } else {
            def.description = format!("custom {} agent", base.name);
        }
        def.prompt = format!("{}\n{}", base.prompt, agent.prompt.trim());
        def.source = Source::Config;
        if let Some(tools) = &agent.tools {
            def.restrict = Some(tools.clone());
        }
        if let Some(steps) = agent.max_steps {
            def.max_steps = steps;
        }
        if let Some(spawns) = &agent.spawns {
            def.spawns = Spawns::from_list(spawns.clone());
        }
        def.model = agent.model.clone();
        def.thinking = agent.thinking_level.clone();
        def.output = agent.output.clone();
        def.autoload_skills = agent.autoload_skills.clone();
        Ok(def)
    }
}

/// Validate one `[agents.<name>]` entry (called from `Config::validate`).
pub fn validate_config_agent(name: &str, agent: &AgentConfig) -> anyhow::Result<()> {
    let base = builtin(&agent.base);
    anyhow::ensure!(
        base.is_some(),
        "custom agent {name:?} must be based on one of: {}",
        AGENTS.iter().map(|a| a.name).collect::<Vec<_>>().join(", ")
    );
    let base = base.expect("checked");
    anyhow::ensure!(
        !agent.prompt.trim().is_empty() && agent.prompt.len() <= 16384,
        "custom agent {name:?} needs a prompt of 1..16384 bytes"
    );
    anyhow::ensure!(
        agent.description.len() <= 200,
        "custom agent {name:?} description is too long"
    );
    for tool in agent.tools.iter().flatten() {
        anyhow::ensure!(
            base.tools.contains(&tool.as_str()),
            "custom agent {name:?}: tool {tool:?} is not available to a {} agent",
            agent.base
        );
    }
    anyhow::ensure!(
        agent.model.len() <= 8,
        "custom agent {name:?}: at most 8 model entries"
    );
    for model in &agent.model {
        anyhow::ensure!(
            !model.trim().is_empty() && model.len() <= 512,
            "custom agent {name:?}: model entries must be 1..512 bytes"
        );
    }
    if let Some(level) = &agent.thinking_level {
        anyhow::ensure!(
            valid_thinking_level(level),
            "custom agent {name:?}: unknown thinking_level {level:?}"
        );
    }
    if let Some(schema) = &agent.output {
        super::output_schema::check_schema(schema).map_err(|why| {
            anyhow::anyhow!("custom agent {name:?}: invalid output schema: {why}")
        })?;
    }
    for spawn in agent.spawns.iter().flatten() {
        anyhow::ensure!(
            spawn == "*" || valid_name(spawn),
            "custom agent {name:?}: invalid spawns entry {spawn:?}"
        );
    }
    Ok(())
}

/// Custom definitions accept the same disabled-thinking spelling as the CLI and
/// retain the existing exact effort values (including none and ultra).
pub(super) fn valid_thinking_level(level: &str) -> bool {
    level == "off"
        || serde_json::from_value::<crate::llm::profiles::ReasoningEffort>(Value::String(
            level.to_owned(),
        ))
        .is_ok()
}

/// Agent names may use ASCII letters, digits, `_` and `-` (1..64 bytes).
pub fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
        && !matches!(name.to_ascii_lowercase().as_str(), "main" | "sub")
}

/// Every agent the `task` tool can launch in this configuration.
#[derive(Clone, Debug, Default)]
pub struct Catalog {
    agents: Vec<Arc<AgentDef>>,
    pub warnings: Vec<String>,
}

impl Catalog {
    pub fn load(config: &Config) -> Self {
        let mut catalog = Self::default();
        for spec in AGENTS {
            catalog.push(AgentDef::from_builtin(spec));
        }
        for (name, text) in &config.subagents {
            if let Some(def) = AgentDef::preset(name, text) {
                catalog.push(def);
            }
        }
        for (name, agent) in &config.agents {
            match AgentDef::from_config(name, agent) {
                Ok(def) => catalog.push(def),
                Err(why) => catalog.warnings.push(why),
            }
        }
        if config.task.discover_agents {
            let mut loaded = 0usize;
            for (dir, source) in directories(config) {
                for result in load_dir(&dir, source) {
                    match result {
                        Ok(def) if loaded < MAX_FILE_DEFINITIONS => {
                            loaded += 1;
                            catalog.push(def);
                        }
                        Ok(def) => catalog.warnings.push(format!(
                            "agent definition {} skipped: at most {MAX_FILE_DEFINITIONS} definition files are loaded",
                            def.path.as_deref().map(Path::display).map(|d| d.to_string()).unwrap_or_default()
                        )),
                        Err(why) => catalog.warnings.push(why),
                    }
                }
            }
        }
        register(&catalog);
        catalog
    }

    fn push(&mut self, def: AgentDef) {
        if let Some(existing) = self.agents.iter().find(|a| a.name == def.name) {
            if def.source != Source::Builtin {
                self.warnings.push(format!(
                    "agent {:?} from {} is shadowed by the {} definition",
                    def.name,
                    def.path
                        .as_deref()
                        .map(|p| p.display().to_string())
                        .unwrap_or_else(|| def.source.as_str().into()),
                    existing.source.as_str()
                ));
            }
            return;
        }
        self.agents.push(Arc::new(def));
    }

    #[cfg(test)]
    pub fn get(&self, name: &str) -> Option<&Arc<AgentDef>> {
        self.agents.iter().find(|a| a.name == name)
    }

    pub fn iter(&self) -> impl Iterator<Item = &Arc<AgentDef>> {
        self.agents.iter()
    }
}

/// Process-wide record of which agent names are writers, so `task` approval (which sees only the
/// call arguments) can tell a custom writer from a read-only agent. Fail-closed: a name ever seen
/// as a writer in any loaded catalog needs approval, and an unknown name is treated as a writer.
#[derive(Default)]
struct Kinds {
    read_only: BTreeSet<String>,
    writers: BTreeSet<String>,
}

static KINDS: Mutex<Option<Kinds>> = Mutex::new(None);

fn register(catalog: &Catalog) {
    let mut kinds = KINDS.lock().unwrap_or_else(|e| e.into_inner());
    let kinds = kinds.get_or_insert_with(Kinds::default);
    for agent in &catalog.agents {
        if agent.mutates {
            kinds.writers.insert(agent.name.clone());
        } else {
            kinds.read_only.insert(agent.name.clone());
        }
    }
}

/// Whether launching `name` can change files (built-in types are fixed; others fail closed).
pub fn launches_writer(name: &str) -> bool {
    if let Some(spec) = builtin(name) {
        return spec.mutates;
    }
    let kinds = KINDS.lock().unwrap_or_else(|e| e.into_inner());
    match kinds.as_ref() {
        Some(kinds) => kinds.writers.contains(name) || !kinds.read_only.contains(name),
        None => true,
    }
}

/// Definition directories, highest precedence first.
fn directories(config: &Config) -> Vec<(PathBuf, Source)> {
    let mut out = Vec::new();
    if config.project_root.is_absolute() {
        let mut project = Vec::new();
        let mut claude = Vec::new();
        // Nearest first, up to the git root; without a git root, only the project itself.
        let root = &config.project_root;
        let git_root = root.ancestors().find(|dir| dir.join(".git").exists());
        let span: Vec<&Path> = match git_root {
            Some(top) => root
                .ancestors()
                .take_while(|dir| dir.starts_with(top))
                .collect(),
            None => vec![root.as_path()],
        };
        for current in span {
            project.push((current.join(".dial").join("agents"), Source::Project));
            if config.task.claude_agents {
                claude.push((
                    current.join(".claude").join("agents"),
                    Source::ClaudeProject,
                ));
            }
        }
        out.extend(project);
        out.extend(claude);
    }
    if config.user_dir.is_absolute() {
        out.push((config.user_dir.join("agents"), Source::User));
        if config.task.claude_agents
            && let Some(home) = std::env::var_os("HOME").filter(|h| !h.is_empty())
        {
            out.push((
                PathBuf::from(home).join(".claude").join("agents"),
                Source::ClaudeUser,
            ));
        }
    }
    let mut seen = BTreeSet::new();
    out.retain(|(dir, _)| seen.insert(dir.clone()));
    out
}

fn load_dir(dir: &Path, source: Source) -> Vec<Result<AgentDef, String>> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut files: Vec<PathBuf> = entries
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "md"))
        .collect();
    files.sort();
    files
        .into_iter()
        .filter_map(|path| {
            let meta = std::fs::symlink_metadata(&path).ok()?;
            if meta.file_type().is_symlink() {
                if source.project() {
                    return Some(Err(format!(
                        "agent definition {} skipped: project definitions may not be symlinks",
                        path.display()
                    )));
                }
                if !std::fs::metadata(&path).is_ok_and(|m| m.is_file()) {
                    return None;
                }
            } else if !meta.is_file() {
                return None;
            }
            Some(load_file(&path, source))
        })
        .collect()
}

fn load_file(path: &Path, source: Source) -> Result<AgentDef, String> {
    let fail = |why: String| format!("agent definition {}: {why}", path.display());
    let size = std::fs::metadata(path)
        .map_err(|e| fail(e.to_string()))?
        .len();
    if size > MAX_FILE_BYTES {
        return Err(fail(format!("larger than {MAX_FILE_BYTES} bytes")));
    }
    let text = std::fs::read_to_string(path).map_err(|e| fail(e.to_string()))?;
    let stem = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or_default()
        .to_owned();
    let mut def = parse_definition(&text, &stem, source).map_err(fail)?;
    def.path = Some(path.to_path_buf());
    Ok(def)
}

/// Split `---\n<yaml>\n---\n<body>`; a file without frontmatter is all body.
fn split_frontmatter(text: &str) -> Result<(Option<&str>, &str), String> {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let Some(rest) = text
        .strip_prefix("---\n")
        .or_else(|| text.strip_prefix("---\r\n"))
    else {
        return Ok((None, text));
    };
    let mut offset = 0;
    for line in rest.split_inclusive('\n') {
        if line.trim_end_matches(['\r', '\n']) == "---" {
            return Ok((Some(&rest[..offset]), &rest[offset + line.len()..]));
        }
        offset += line.len();
    }
    if rest[offset..].trim_end() == "---" {
        return Ok((Some(&rest[..offset]), ""));
    }
    Err("frontmatter is not closed with `---`".into())
}

/// A string, a list of strings, or a comma-separated string.
fn list(value: Option<&Value>, key: &str) -> Result<Option<Vec<String>>, String> {
    match value {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(text)) => Ok(Some(
            text.split(',')
                .map(|s| s.trim().to_owned())
                .filter(|s| !s.is_empty())
                .collect(),
        )),
        Some(Value::Array(items)) => items
            .iter()
            .map(|item| match item {
                Value::String(s) => Ok(s.trim().to_owned()),
                _ => Err(format!("`{key}` entries must be strings")),
            })
            .collect::<Result<Vec<_>, _>>()
            .map(|items| Some(items.into_iter().filter(|s| !s.is_empty()).collect())),
        Some(_) => Err(format!("`{key}` must be a string or a list of strings")),
    }
}

/// Claude Code tool names → Dial tool names.
fn dial_tool(name: &str) -> String {
    match name {
        "Read" | "LS" => "read".into(),
        "Write" => "write".into(),
        "Edit" | "MultiEdit" => "edit".into(),
        "Glob" => "glob".into(),
        "Grep" => "grep".into(),
        "Bash" => "bash".into(),
        "WebFetch" => "web_fetch".into(),
        "WebSearch" => "web_search".into(),
        "NotebookEdit" | "NotebookRead" => "notebook".into(),
        "Task" | "Agent" => "task".into(),
        "TodoWrite" => "todo".into(),
        other if other.starts_with("mcp__") => other.into(),
        other => other.to_ascii_lowercase(),
    }
}

pub(crate) fn parse_definition(text: &str, stem: &str, source: Source) -> Result<AgentDef, String> {
    let (frontmatter, body) = split_frontmatter(text)?;
    let fields: serde_json::Map<String, Value> = match frontmatter {
        None => serde_json::Map::new(),
        Some(yaml) if yaml.trim().is_empty() => serde_json::Map::new(),
        Some(yaml) => {
            let parsed: serde_norway::Value = serde_norway::from_str(yaml)
                .map_err(|e| format!("invalid YAML frontmatter: {e}"))?;
            match serde_json::to_value(parsed).map_err(|e| format!("frontmatter: {e}"))? {
                Value::Object(map) => map,
                Value::Null => serde_json::Map::new(),
                _ => return Err("frontmatter must be a mapping".into()),
            }
        }
    };
    let get = |keys: &[&str]| keys.iter().find_map(|key| fields.get(*key));
    let string = |keys: &[&str]| -> Result<Option<String>, String> {
        match get(keys) {
            None | Some(Value::Null) => Ok(None),
            Some(Value::String(s)) => Ok(Some(s.trim().to_owned())),
            Some(Value::Number(n)) => Ok(Some(n.to_string())),
            Some(Value::Bool(b)) => Ok(Some(b.to_string())),
            Some(_) => Err(format!("`{}` must be a string", keys[0])),
        }
    };
    let name = string(&["name"])?.unwrap_or_else(|| stem.to_owned());
    if !valid_name(&name) {
        return Err(format!(
            "invalid agent name {name:?} (letters, digits, `_`, `-`; at most 64; not main/sub)"
        ));
    }
    if builtin(&name).is_some() {
        return Err(format!(
            "agent name {name:?} is reserved for a built-in agent type"
        ));
    }
    let description = string(&["description"])?
        .filter(|d| !d.is_empty())
        .ok_or("frontmatter needs a `description`")?;
    let description: String = description.chars().take(MAX_DESCRIPTION).collect();
    let prompt = body.trim().to_owned();
    if prompt.is_empty() {
        return Err("the definition body (the agent's instructions) is empty".into());
    }
    if prompt.len() > MAX_PROMPT {
        return Err(format!("the definition body exceeds {MAX_PROMPT} bytes"));
    }
    let tools = list(get(&["tools"]), "tools")?.map(|tools| {
        let mut out: Vec<String> = Vec::new();
        for tool in tools {
            let tool = if source.claude() {
                dial_tool(&tool)
            } else {
                tool
            };
            if !out.contains(&tool) {
                out.push(tool);
            }
        }
        out
    });
    let explicit_mutates = match get(&["mutates"]) {
        None | Some(Value::Null) => None,
        Some(Value::Bool(b)) => Some(*b),
        Some(_) => return Err("`mutates` must be true or false".into()),
    };
    let lists_task = tools
        .as_ref()
        .is_some_and(|t| t.iter().any(|n| n == "task"));
    let spawns = match get(&["spawns"]) {
        None | Some(Value::Null) if lists_task => Spawns::All,
        None | Some(Value::Null) => Spawns::None,
        Some(Value::Bool(true)) => Spawns::All,
        Some(Value::Bool(false)) => Spawns::None,
        value => Spawns::from_list(list(value, "spawns")?.unwrap_or_default()),
    };
    if let Spawns::Only(names) = &spawns
        && let Some(bad) = names.iter().find(|n| !valid_name(n))
    {
        return Err(format!("invalid `spawns` entry {bad:?}"));
    }
    let writes = tools
        .as_ref()
        .is_some_and(|t| t.iter().any(|n| WRITE_TOOLS.contains(&n.as_str())));
    if explicit_mutates == Some(false) && writes {
        return Err("`mutates: false` conflicts with write tools in `tools`".into());
    }
    let mutates = explicit_mutates.unwrap_or(writes);
    let restrict = tools.map(|t| {
        t.into_iter()
            .filter(|n| !matches!(n.as_str(), "task" | "yield"))
            .collect::<Vec<_>>()
    });
    let model = if source.claude() {
        Vec::new()
    } else {
        list(get(&["model"]), "model")?.unwrap_or_default()
    };
    if model.len() > 8 || model.iter().any(|m| m.len() > 512) {
        return Err("`model` takes at most 8 entries of at most 512 bytes".into());
    }
    let thinking =
        string(&["thinking_level", "thinkingLevel", "thinking"])?.filter(|t| !t.is_empty());
    if let Some(level) = &thinking
        && !valid_thinking_level(level)
    {
        return Err(format!("unknown thinking level {level:?}"));
    }
    let output = match get(&["output", "output_schema"]) {
        None | Some(Value::Null) => None,
        Some(schema) => {
            super::output_schema::check_schema(schema)
                .map_err(|why| format!("invalid `output` schema: {why}"))?;
            Some(schema.clone())
        }
    };
    let max_steps = match get(&["max_steps", "maxSteps"]) {
        None | Some(Value::Null) => 0,
        Some(value) => value
            .as_u64()
            .and_then(|n| u32::try_from(n).ok())
            .ok_or("`max_steps` must be a nonnegative integer")?,
    };
    let autoload_skills = list(
        get(&["autoload_skills", "autoloadSkills", "skills"]),
        "autoload_skills",
    )?
    .unwrap_or_default();
    if autoload_skills.len() > 16 {
        return Err("at most 16 `autoload_skills`".into());
    }
    Ok(AgentDef {
        name,
        description,
        prompt,
        restrict,
        max_steps,
        mutates,
        spawns,
        model,
        thinking,
        output,
        autoload_skills,
        source,
        path: None,
    })
}

#[cfg(test)]
mod tests;
