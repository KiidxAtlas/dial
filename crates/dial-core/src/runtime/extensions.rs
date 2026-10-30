//! Version-one, bounded extension contributions shared by the host and presentation layers.
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::BTreeSet;

fn object() -> Value {
    json!({})
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Contributions {
    pub views: Vec<View>,
    #[serde(default = "object")]
    pub settings: Value,
    pub commands: Vec<Command>,
    pub agents: Vec<Agent>,
    pub workflows: Vec<Workflow>,
    pub context_sources: Vec<ContextSource>,
    pub renderers: Vec<Renderer>,
    pub keybindings: Vec<Keybinding>,
    pub presets: Vec<Preset>,
}
impl Default for Contributions {
    fn default() -> Self {
        Self {
            views: Vec::new(),
            settings: object(),
            commands: Vec::new(),
            agents: Vec::new(),
            workflows: Vec::new(),
            context_sources: Vec::new(),
            renderers: Vec::new(),
            keybindings: Vec::new(),
            presets: Vec::new(),
        }
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct View {
    pub id: String,
    pub title: String,
    pub location: Location,
    pub elements: Vec<Component>,
}
/// A revisioned view owned by one extension, never a cross-extension update grant.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct OwnedView {
    pub plugin: String,
    pub view: View,
    pub revision: u64,
}
pub type ViewDefinition = View;
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Location {
    Sidebar,
    Tab,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Command {
    pub name: String,
    pub description: String,
    #[serde(default = "object")]
    pub parameters: Value,
    pub keybinding: Option<String>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Agent {
    pub id: String,
    pub title: String,
    pub model: Option<String>,
    #[serde(default)]
    pub tools: Vec<String>,
    pub guidance: String,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Workflow {
    pub id: String,
    pub title: String,
    pub steps: Vec<WorkflowStep>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkflowStep {
    pub id: String,
    pub title: String,
    pub kind: StepKind,
    pub name: Option<String>,
    pub arguments: Option<serde_json::Map<String, Value>>,
    pub prompt: Option<String>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StepKind {
    Tool,
    Agent,
    Approval,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ContextSource {
    pub id: String,
    pub title: String,
    pub description: String,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Renderer {
    pub id: String,
    pub title: String,
    pub fields: Vec<Field>,
    pub markdown: Option<String>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Field {
    pub label: String,
    pub path: String,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Keybinding {
    pub key: String,
    pub command: String,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Preset {
    pub id: String,
    pub title: String,
    pub values: serde_json::Map<String, Value>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum Component {
    Text {
        text: String,
    },
    Markdown {
        text: String,
    },
    Code {
        text: String,
        language: String,
    },
    Diff {
        before: String,
        after: String,
        path: String,
    },
    List {
        items: Vec<ListItem>,
    },
    Table {
        columns: Vec<Column>,
        rows: Vec<Row>,
    },
    Tree {
        nodes: Vec<TreeNode>,
    },
    Tabs {
        tabs: Vec<Tab>,
    },
    Progress {
        value: f64,
        total: f64,
        label: String,
    },
    Button {
        id: String,
        label: String,
    },
    Input {
        id: String,
        label: String,
        value: String,
    },
    Select {
        id: String,
        label: String,
        options: Vec<SelectOption>,
        value: String,
    },
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ListItem {
    pub id: String,
    pub label: String,
    pub description: Option<String>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Column {
    pub id: String,
    pub label: String,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Row {
    pub id: String,
    pub cells: Vec<String>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TreeNode {
    pub id: String,
    pub label: String,
    #[serde(default)]
    pub children: Vec<TreeNode>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Tab {
    pub id: String,
    pub title: String,
    pub elements: Vec<Component>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SelectOption {
    pub value: String,
    pub label: String,
}

pub fn validate_id(id: &str) -> Result<()> {
    ensure!(
        !id.is_empty()
            && id.len() <= 64
            && id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"_.-".contains(&b)),
        "invalid extension identifier {id:?}"
    );
    Ok(())
}

impl Contributions {
    pub fn parse(value: Value) -> Result<Self> {
        let value = if value.is_null() { object() } else { value };
        ensure!(
            serde_json::to_vec(&value)?.len() <= 1 << 20,
            "contributions exceed 1 MiB"
        );
        validate_json(&value, 0)?;
        let result: Self = serde_json::from_value(value)?;
        ensure!(
            result.settings.is_object(),
            "settings schema must be an object"
        );
        for ids in [
            result
                .views
                .iter()
                .map(|v| v.id.as_str())
                .collect::<Vec<_>>(),
            result.commands.iter().map(|v| v.name.as_str()).collect(),
            result.agents.iter().map(|v| v.id.as_str()).collect(),
            result.workflows.iter().map(|v| v.id.as_str()).collect(),
            result
                .context_sources
                .iter()
                .map(|v| v.id.as_str())
                .collect(),
            result.renderers.iter().map(|v| v.id.as_str()).collect(),
            result.presets.iter().map(|v| v.id.as_str()).collect(),
        ] {
            unique(ids)?;
        }
        ensure!(result.keybindings.len() <= 64, "too many keybindings");
        for view in &result.views {
            view.validate()?;
        }
        for workflow in &result.workflows {
            unique(workflow.steps.iter().map(|s| s.id.as_str()).collect())?;
            for step in &workflow.steps {
                match step.kind {
                    StepKind::Tool | StepKind::Agent => {
                        validate_id(step.name.as_deref().unwrap_or_default())?;
                    }
                    StepKind::Approval => ensure!(
                        step.prompt.as_ref().is_some_and(|s| !s.is_empty()),
                        "approval step requires prompt"
                    ),
                }
            }
        }
        for binding in &result.keybindings {
            validate_id(&binding.command)?;
            ensure!(
                !binding.key.is_empty() && binding.key.len() <= 64,
                "invalid keybinding"
            );
        }
        super::schema::check_schema(&result.settings).map_err(anyhow::Error::msg)?;
        for command in &result.commands {
            super::schema::check_schema(&command.parameters).map_err(anyhow::Error::msg)?;
        }
        Ok(result)
    }
}
fn unique(ids: Vec<&str>) -> Result<()> {
    ensure!(ids.len() <= 64, "registration limit is 64");
    let mut seen = BTreeSet::new();
    for id in ids {
        validate_id(id)?;
        ensure!(seen.insert(id), "duplicate extension identifier {id}");
    }
    Ok(())
}
fn validate_json(value: &Value, depth: usize) -> Result<()> {
    ensure!(depth <= 32, "contribution nesting exceeds 32");
    match value {
        Value::String(s) => ensure!(
            !s.chars().any(|c| c.is_control() && c != '\n' && c != '\t'),
            "extension text contains control characters"
        ),
        Value::Array(a) => {
            for v in a {
                validate_json(v, depth + 1)?;
            }
        }
        Value::Object(o) => {
            for (k, v) in o {
                ensure!(!k.chars().any(char::is_control), "invalid object key");
                validate_json(v, depth + 1)?;
            }
        }
        _ => {}
    }
    Ok(())
}
impl View {
    pub fn validate(&self) -> Result<()> {
        validate_id(&self.id)?;
        ensure!(
            !self.title.is_empty()
                && self.title.chars().count() <= 128
                && !self.title.chars().any(char::is_control),
            "view title must be 1..128 control-free characters"
        );
        let mut ids = BTreeSet::new();
        component_ids(&self.elements, &mut ids)?;
        let value = serde_json::to_value(self)?;
        validate_json(&value, 0)?;
        ensure!(
            serde_json::to_vec(&value)?.len() <= 65536,
            "view exceeds 64 KiB"
        );
        let mut count = 0;
        components(&self.elements, 1, &mut count)
    }
}
fn node(depth: usize, count: &mut usize) -> Result<()> {
    *count += 1;
    ensure!(
        depth <= 8 && *count <= 256,
        "view exceeds depth 8 or 256 nodes"
    );
    Ok(())
}
fn trees(nodes: &[TreeNode], depth: usize, count: &mut usize) -> Result<()> {
    for n in nodes {
        node(depth, count)?;
        validate_id(&n.id)?;
        trees(&n.children, depth + 1, count)?;
    }
    Ok(())
}
fn components(elements: &[Component], depth: usize, count: &mut usize) -> Result<()> {
    for element in elements {
        node(depth, count)?;
        match element {
            Component::Tree { nodes } => trees(nodes, depth + 1, count)?,
            Component::Tabs { tabs } => {
                for tab in tabs {
                    node(depth + 1, count)?;
                    validate_id(&tab.id)?;
                    components(&tab.elements, depth + 2, count)?;
                }
            }
            Component::List { items } => {
                for item in items {
                    node(depth + 1, count)?;
                    validate_id(&item.id)?;
                }
            }
            Component::Table { columns, rows } => {
                unique(columns.iter().map(|c| c.id.as_str()).collect())?;
                for row in rows {
                    node(depth + 1, count)?;
                    validate_id(&row.id)?;
                    ensure!(
                        row.cells.len() == columns.len(),
                        "table row width differs from columns"
                    );
                }
            }
            Component::Progress { value, total, .. } => ensure!(
                value.is_finite()
                    && total.is_finite()
                    && *total > 0.0
                    && *value >= 0.0
                    && value <= total,
                "invalid progress range"
            ),
            Component::Button { id, .. } | Component::Input { id, .. } => validate_id(id)?,
            Component::Select {
                id, options, value, ..
            } => {
                validate_id(id)?;
                ensure!(
                    options.len() <= 64 && options.iter().any(|o| &o.value == value),
                    "invalid select options/value"
                );
            }
            _ => {}
        }
    }
    Ok(())
}
fn insert_id(id: &str, ids: &mut BTreeSet<String>) -> Result<()> {
    validate_id(id)?;
    ensure!(ids.insert(id.to_owned()), "duplicate view identifier {id}");
    Ok(())
}
fn tree_ids(nodes: &[TreeNode], ids: &mut BTreeSet<String>) -> Result<()> {
    for n in nodes {
        insert_id(&n.id, ids)?;
        tree_ids(&n.children, ids)?;
    }
    Ok(())
}
fn component_ids(elements: &[Component], ids: &mut BTreeSet<String>) -> Result<()> {
    for element in elements {
        match element {
            Component::Button { id, .. }
            | Component::Input { id, .. }
            | Component::Select { id, .. } => insert_id(id, ids)?,
            Component::List { items } => {
                for item in items {
                    insert_id(&item.id, ids)?;
                }
            }
            Component::Table { rows, .. } => {
                for row in rows {
                    insert_id(&row.id, ids)?;
                }
            }
            Component::Tree { nodes } => tree_ids(nodes, ids)?,
            Component::Tabs { tabs } => {
                unique(tabs.iter().map(|t| t.id.as_str()).collect())?;
                for tab in tabs {
                    component_ids(&tab.elements, ids)?;
                }
            }
            _ => {}
        }
    }
    Ok(())
}
#[cfg(test)]
mod tests;
