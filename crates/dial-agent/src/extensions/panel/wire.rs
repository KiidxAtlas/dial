//! JSON wire form of panels and outcomes, as runtime extensions send and receive them.
use super::{Element, MAX_ELEMENTS, Outcome, Panel, Value};
use anyhow::{Result, anyhow, bail, ensure};
use serde_json::{Value as Json, json};

/// Reads `{"title", "generation", "elements": [{"type": ...}]}` and validates the result, so an
/// extension's panel is either a checked [`Panel`] or an error that names the bad field.
pub fn panel_from_json(json: &Json) -> Result<Panel> {
    let title = str_field(json, "title")?.to_owned();
    let generation = json
        .get("generation")
        .and_then(Json::as_u64)
        .ok_or_else(|| anyhow!("panel needs a numeric generation"))?;
    let elements = json
        .get("elements")
        .and_then(Json::as_array)
        .ok_or_else(|| anyhow!("panel needs an elements array"))?;
    ensure!(
        elements.len() <= MAX_ELEMENTS,
        "panel has more than {MAX_ELEMENTS} elements"
    );
    let panel = Panel {
        title,
        generation,
        elements: elements
            .iter()
            .map(element_from_json)
            .collect::<Result<_>>()?,
    };
    panel.validate()?;
    Ok(panel)
}

fn element_from_json(json: &Json) -> Result<Element> {
    Ok(match str_field(json, "type")? {
        "text" => Element::Text(str_field(json, "text")?.to_owned()),
        "markdown" => Element::Markdown(str_field(json, "text")?.to_owned()),
        "list" => Element::List(strings(json, "items")?),
        "input" => Element::Input {
            id: str_field(json, "id")?.to_owned(),
            label: str_field(json, "label")?.to_owned(),
            value: json
                .get("value")
                .map_or(Ok(""), |v| {
                    v.as_str()
                        .ok_or_else(|| anyhow!("input value must be a string"))
                })?
                .to_owned(),
        },
        "select" => Element::Select {
            id: str_field(json, "id")?.to_owned(),
            label: str_field(json, "label")?.to_owned(),
            options: strings(json, "options")?,
            selected: match json.get("selected") {
                None => 0,
                Some(v) => v
                    .as_u64()
                    .and_then(|n| usize::try_from(n).ok())
                    .ok_or_else(|| anyhow!("select selected must be an option index"))?,
            },
        },
        "button" => Element::Button {
            id: str_field(json, "id")?.to_owned(),
            label: str_field(json, "label")?.to_owned(),
            primary: json.get("primary").and_then(Json::as_bool).unwrap_or(false),
        },
        other => bail!("unknown panel element {other:?}"),
    })
}

fn str_field<'a>(json: &'a Json, key: &str) -> Result<&'a str> {
    json.get(key)
        .and_then(Json::as_str)
        .ok_or_else(|| anyhow!("panel field {key:?} must be a string"))
}

fn strings(json: &Json, key: &str) -> Result<Vec<String>> {
    json.get(key)
        .and_then(Json::as_array)
        .ok_or_else(|| anyhow!("panel field {key:?} must be an array"))?
        .iter()
        .map(|item| {
            item.as_str()
                .map(str::to_owned)
                .ok_or_else(|| anyhow!("panel field {key:?} must hold strings"))
        })
        .collect()
}

/// The outcome as an extension receives it. Inputs are strings; selects are option indexes.
pub fn outcome_to_json(outcome: &Outcome) -> Json {
    match outcome {
        Outcome::Submitted { button, values } => json!({
            "status": "submitted",
            "button": button,
            "values": values
                .iter()
                .map(|(id, value)| match value {
                    Value::Text(text) => (id.clone(), json!(text)),
                    Value::Choice(index) => (id.clone(), json!(index)),
                })
                .collect::<serde_json::Map<_, _>>(),
        }),
        Outcome::Cancelled => json!({"status": "cancelled"}),
        Outcome::Unsupported(reason) => json!({"status": "unsupported", "reason": reason}),
    }
}
