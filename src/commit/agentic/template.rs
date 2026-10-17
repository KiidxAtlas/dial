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
mod tests;
