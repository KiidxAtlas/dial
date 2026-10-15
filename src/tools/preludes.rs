//! Eval preludes: host capabilities that eval cells call as `__dial_prelude__(name, params)`.
//!
//! Each prelude ships JS and Python sources installed into the cell runtime once per registry
//! generation. JS sources are evaluated in the shared REPL realm, so they must declare globals with
//! `var`, function declarations or `globalThis.x = …` so a reinstall never redeclares a lexical binding.

use std::cell::RefCell;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use parking_lot::RwLock;
use serde_json::{Value, json};

use super::ToolOutput;
use super::ext::ToolCallback;

pub struct PreludeDef {
    pub name: &'static str,
    pub js: &'static str,
    pub py: &'static str,
}

/// Tool dispatch scoped to one prelude call. Calls made through it run the calling cell's
/// `ToolContext` gates (approvals, mutation authorizer, native-effect interception).
pub struct PreludeCall<'a> {
    dispatch: &'a mut dyn FnMut(ToolCallback) -> Result<ToolOutput, String>,
}

impl PreludeCall<'_> {
    pub fn tool(&mut self, name: &str, arguments: Value) -> Result<Value, String> {
        let output = (self.dispatch)(ToolCallback::Call {
            name: name.to_owned(),
            arguments,
        })?;
        if output.is_error {
            return Err(output.text);
        }
        Ok(output.data.unwrap_or_else(|| json!(output.text)))
    }
}

pub type PreludeHandler =
    Arc<dyn Fn(Value, &mut PreludeCall<'_>) -> anyhow::Result<Value> + Send + Sync>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PreludeLanguage {
    Js,
    Python,
}

struct Entry {
    def: PreludeDef,
    handler: PreludeHandler,
}

static REGISTRY: RwLock<Vec<Entry>> = RwLock::new(Vec::new());
static GENERATION: AtomicU64 = AtomicU64::new(0);

thread_local! {
    static DISPLAY: RefCell<String> = const { RefCell::new(String::new()) };
}

/// Registers a prelude. A later registration with the same name replaces the earlier one in place.
pub fn register_prelude(def: PreludeDef, handler: PreludeHandler) {
    let mut registry = REGISTRY.write();
    match registry.iter().position(|entry| entry.def.name == def.name) {
        Some(index) => registry[index] = Entry { def, handler },
        None => registry.push(Entry { def, handler }),
    }
    GENERATION.fetch_add(1, Ordering::AcqRel);
}

/// Concatenated install source for every registered prelude in `language`; empty when none.
pub fn prelude_source_for(language: PreludeLanguage) -> String {
    let registry = REGISTRY.read();
    let (separator, sources): (&str, Vec<&'static str>) = match language {
        PreludeLanguage::Js => ("\n", registry.iter().map(|entry| entry.def.js).collect()),
        PreludeLanguage::Python => ("\n\n", registry.iter().map(|entry| entry.def.py).collect()),
    };
    sources
        .into_iter()
        .map(str::trim)
        .filter(|source| !source.is_empty())
        .collect::<Vec<_>>()
        .join(separator)
}

/// Appends text to the current cell's display buffer; it is prepended to that cell's stdout.
pub fn display(text: String) {
    DISPLAY.with(|buffer| {
        let mut buffer = buffer.borrow_mut();
        buffer.push_str(&text);
        if !text.ends_with('\n') {
            buffer.push('\n');
        }
    });
}

pub(crate) fn generation() -> u64 {
    GENERATION.load(Ordering::Acquire)
}

pub(crate) fn take_display() -> String {
    DISPLAY.with(|buffer| std::mem::take(&mut *buffer.borrow_mut()))
}

pub(crate) fn invoke(
    name: &str,
    parameters: Value,
    dispatch: &mut dyn FnMut(ToolCallback) -> Result<ToolOutput, String>,
) -> Result<Value, String> {
    let handler = REGISTRY
        .read()
        .iter()
        .find(|entry| entry.def.name == name)
        .map(|entry| entry.handler.clone())
        .ok_or_else(|| format!("Eval prelude \"{name}\" is not enabled in the current session."))?;
    let mut call = PreludeCall { dispatch };
    handler(parameters, &mut call).map_err(|error| format!("{error:#}"))
}

/// Answers the `prelude` and `display` bridge callbacks; `None` for any other callback kind.
pub(crate) fn bridge_reply(
    request: &Value,
    dispatch: &mut dyn FnMut(ToolCallback) -> Result<ToolOutput, String>,
) -> Option<Result<Value, String>> {
    match request.get("callback").and_then(Value::as_str)? {
        "prelude" => {
            let Some(name) = request.get("name").and_then(Value::as_str) else {
                return Some(Err("malformed prelude name".into()));
            };
            let parameters = request.get("parameters").cloned().unwrap_or(Value::Null);
            Some(Ok(match invoke(name, parameters, dispatch) {
                Ok(data) => json!({"text":"","data":data,"is_error":false}),
                Err(text) => json!({"text":text,"data":null,"is_error":true}),
            }))
        }
        "display" => {
            let text = request
                .get("text")
                .and_then(Value::as_str)
                .unwrap_or_default();
            display(text.to_owned());
            Some(Ok(json!({"text":"","data":null,"is_error":false})))
        }
        _ => None,
    }
}

#[cfg(test)]
mod computer_tests {
    use super::*;

    fn constant(value: i64) -> PreludeHandler {
        Arc::new(
            move |_: Value, _: &mut PreludeCall<'_>| -> anyhow::Result<Value> { Ok(json!(value)) },
        )
    }

    fn computer_no_dispatch(_: ToolCallback) -> Result<ToolOutput, String> {
        Err("no dispatch in this test".into())
    }

    fn computer_mutation_dispatch(
        approve: bool,
    ) -> impl FnMut(ToolCallback) -> Result<ToolOutput, String> {
        move |callback| match callback {
            ToolCallback::Call { name, .. } if name == "computer_test_mutate" => Ok(if approve {
                ToolOutput {
                    execution: Default::default(),
                    text: "done".into(),
                    is_error: false,
                    data: Some(json!("done")),
                }
            } else {
                ToolOutput {
                    execution: Default::default(),
                    text: "denied by confirm".into(),
                    is_error: true,
                    data: None,
                }
            }),
            other => Err(format!("unexpected callback {other:?}")),
        }
    }

    #[test]
    fn computer_prelude_missing_is_explicit() {
        let error = invoke(
            "computer_test_missing_prelude",
            json!({}),
            &mut computer_no_dispatch,
        )
        .unwrap_err();
        assert_eq!(
            error,
            "Eval prelude \"computer_test_missing_prelude\" is not enabled in the current session."
        );
    }

    #[test]
    fn computer_prelude_registration_replaces_by_name() {
        register_prelude(
            PreludeDef {
                name: "computer_test_replace",
                js: "var a = 1;",
                py: "a = 1",
            },
            constant(1),
        );
        register_prelude(
            PreludeDef {
                name: "computer_test_replace",
                js: "var a = 2;",
                py: "a = 2",
            },
            constant(2),
        );
        assert_eq!(
            invoke(
                "computer_test_replace",
                json!({}),
                &mut computer_no_dispatch
            )
            .unwrap(),
            json!(2)
        );
        assert_eq!(
            prelude_source_for(PreludeLanguage::Python)
                .matches("a = 2")
                .count(),
            1
        );
    }

    #[test]
    fn computer_prelude_mutation_follows_dispatch_approval() {
        register_prelude(
            PreludeDef {
                name: "computer_test_mutate_prelude",
                js: "",
                py: "",
            },
            Arc::new(|_: Value, call: &mut PreludeCall<'_>| {
                call.tool("computer_test_mutate", json!({"action": "click"}))
                    .map_err(anyhow::Error::msg)
            }),
        );
        let mut approved = computer_mutation_dispatch(true);
        assert_eq!(
            invoke("computer_test_mutate_prelude", json!({}), &mut approved).unwrap(),
            json!("done")
        );
        let mut denied = computer_mutation_dispatch(false);
        assert_eq!(
            invoke("computer_test_mutate_prelude", json!({}), &mut denied).unwrap_err(),
            "denied by confirm"
        );
    }

    #[test]
    fn computer_display_appends_lines() {
        display("first".into());
        display("second\n".into());
        assert_eq!(take_display(), "first\nsecond\n");
        assert_eq!(take_display(), "");
    }
}
