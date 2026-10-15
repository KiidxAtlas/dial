//! Commit input compatibility over the shared prompt renderer.
use std::collections::BTreeMap;

/// Template input: text, a list, or a record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Value {
    Text(String),
    List(Vec<Value>),
    Map(BTreeMap<String, Value>),
}

impl Value {
    pub(crate) fn text(text: impl Into<String>) -> Self {
        Self::Text(text.into())
    }

    /// Build a record from `(key, value)` pairs.
    pub(crate) fn map<const N: usize>(pairs: [(&str, Value); N]) -> Self {
        Self::Map(pairs.into_iter().map(|(k, v)| (k.to_owned(), v)).collect())
    }

    fn json(&self) -> serde_json::Value {
        match self {
            Self::Text(text) => serde_json::Value::String(text.clone()),
            Self::List(items) => serde_json::Value::Array(items.iter().map(Self::json).collect()),
            Self::Map(items) => serde_json::Value::Object(
                items
                    .iter()
                    .map(|(key, value)| (key.clone(), value.json()))
                    .collect(),
            ),
        }
    }
}

pub(crate) fn render(template: &str, context: &Value) -> Result<String, String> {
    crate::runtime::template::render_raw(template, &context.json())
        .map_err(|error| error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn variables_and_if_blocks_render() {
        let context = Value::map([("name", Value::text("dial")), ("extra", Value::text(""))]);
        let out = render(
            "hi {{name}}\n{{#if extra}}\nno\n{{/if}}\n{{#if name}}\nyes\n{{/if}}\n",
            &context,
        )
        .unwrap();
        assert_eq!(out, "hi dial\nyes\n");
    }

    #[test]
    fn each_and_list_render_nested_records() {
        let context = Value::map([(
            "entries",
            Value::List(vec![Value::map([
                ("path", Value::text("CHANGELOG.md")),
                (
                    "sections",
                    Value::List(vec![Value::map([
                        ("name", Value::text("Added")),
                        (
                            "items",
                            Value::List(vec![Value::text("one"), Value::text("two")]),
                        ),
                    ])]),
                ),
            ])]),
        )]);
        let template = "{{#each entries}}\n### {{path}}\n{{#each sections}}\n{{name}}:\n{{#list items prefix=\"- \" join=\"\\n\"}}{{this}}{{/list}}\n{{/each}}\n{{/each}}\n";
        let out = render(template, &context).unwrap();
        assert_eq!(out, "### CHANGELOG.md\nAdded:\n- one\n- two\n");
    }

    #[test]
    fn else_branch_renders_when_falsy() {
        let context = Value::map([("goal", Value::text(""))]);
        let out = render("{{#if goal}}\nset\n{{else}}\ndefault\n{{/if}}\n", &context).unwrap();
        assert_eq!(out, "default\n");
    }

    #[test]
    fn unclosed_block_is_an_error() {
        assert!(render("{{#if x}}", &Value::map([])).is_err());
    }
}
