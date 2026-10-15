//! Versioned judge questions; instructions and criteria are reviewable independently of callers.
use anyhow::{Context, Result, ensure};
use serde::Deserialize;
use std::{collections::BTreeMap, sync::LazyLock};

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QuestionSpec {
    schema_version: u32,
    pub instructions: String,
    pub criteria: BTreeMap<String, String>,
    pub required: Vec<String>,
}
impl QuestionSpec {
    pub fn parse(source: &str) -> Result<Self> {
        let specification: Self = serde_json::from_str(source)?;
        ensure!(
            specification.schema_version == 1,
            "unsupported judge-question version"
        );
        ensure!(
            !specification.instructions.trim().is_empty(),
            "judge instructions are empty"
        );
        ensure!(
            specification
                .criteria
                .iter()
                .all(|(label, meaning)| !label.is_empty() && !meaning.trim().is_empty()),
            "empty judge criterion"
        );
        Ok(specification)
    }
    pub fn render(&self, values: &serde_json::Value) -> Result<String> {
        for path in &self.required {
            let mut value = values;
            for part in path.split('.') {
                value = value
                    .get(part)
                    .with_context(|| format!("missing prompt input {path}"))?;
            }
            ensure!(!value.is_null(), "missing prompt input {path}");
        }
        crate::runtime::template::render_raw(&self.instructions, values)
    }
}
static QUESTIONS: LazyLock<BTreeMap<&'static str, QuestionSpec>> = LazyLock::new(|| {
    [
        (
            "completion-review",
            include_str!("questions/completion-review.json"),
        ),
        (
            "failure-escalation",
            include_str!("questions/failure-escalation.json"),
        ),
    ]
    .into_iter()
    .map(|(name, source)| {
        (
            name,
            QuestionSpec::parse(source).expect("bundled judge question is valid"),
        )
    })
    .collect()
});
pub fn instructions(name: &str) -> &'static str {
    &QUESTIONS
        .get(name)
        .expect("caller names a registered judge question")
        .instructions
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn prompt_inputs_are_validated_before_render_and_schema_changes_fail_closed() {
        let spec = QuestionSpec::parse(r#"{"schema_version":1,"instructions":"Review {{target}}","criteria":{},"required":["target"]}"#).unwrap();
        assert!(spec.render(&serde_json::json!({})).is_err());
        assert_eq!(
            spec.render(&serde_json::json!({"target":"src/a.rs"}))
                .unwrap(),
            "Review src/a.rs"
        );
        assert!(
            QuestionSpec::parse(
                r#"{"schema_version":2,"instructions":"x","criteria":{},"required":[]}"#
            )
            .is_err()
        );
    }
}
