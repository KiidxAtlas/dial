//! Allowlisted desktop helper and handle call chains for direct `computer` calls.

use serde::{Deserialize, Deserializer};
use serde_json::Value;

/// One allowlisted method invocation in a computer call chain.
#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct ComputerCallStep {
    pub method: String,
    #[serde(default, deserialize_with = "null_as_empty")]
    pub args: Vec<Value>,
}

/// Model-supplied `null` for an optional list is the same as omitting it.
fn null_as_empty<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Vec<Value>, D::Error> {
    Ok(Option::<Vec<Value>>::deserialize(deserializer)?.unwrap_or_default())
}

/// Approval tier a direct computer helper needs: inspection reads, input and mutation execute.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ComputerCallPolicy {
    Read,
    Exec,
}

type MethodPolicies = &'static [(&'static str, ComputerCallPolicy)];

use ComputerCallPolicy::{Exec, Read};

/// Helpers callable on the `desktop` root; `window`, `display` and `ref` also anchor one-hop handle chains.
pub const DESKTOP_METHODS: MethodPolicies = &[
    ("capabilities", Read),
    ("displays", Read),
    ("display", Read),
    ("apps.list", Read),
    ("apps.open", Exec),
    ("control.acquire", Exec),
    ("control.release", Exec),
    ("control.state", Read),
    ("holdKeys", Exec),
    ("holdMouse", Exec),
    ("windows", Read),
    ("window", Read),
    ("focusedWindow", Read),
    ("screenshot", Read),
    ("zoom", Read),
    ("click", Exec),
    ("doubleClick", Exec),
    ("move", Exec),
    ("drag", Exec),
    ("scroll", Exec),
    ("type", Exec),
    ("press", Exec),
    ("elementAt", Read),
    ("focusedElement", Read),
    ("ref", Read),
    ("clipboard.read", Read),
    ("clipboard.write", Exec),
];

/// Display handles expose only capture and input, never window-specific AX or activation.
pub const DISPLAY_METHODS: MethodPolicies = &[
    ("screenshot", Read),
    ("zoom", Read),
    ("click", Exec),
    ("doubleClick", Exec),
    ("move", Exec),
    ("drag", Exec),
    ("scroll", Exec),
    ("type", Exec),
    ("press", Exec),
    ("holdKeys", Exec),
    ("holdMouse", Exec),
];

/// Helpers callable on a window handle resolved through `desktop.window(id)`.
pub const WINDOW_METHODS: MethodPolicies = &[
    ("observe", Read),
    ("menu.items", Read),
    ("menu.select", Exec),
    ("bringToCurrentSpace", Exec),
    ("holdKeys", Exec),
    ("holdMouse", Exec),
    ("screenshot", Read),
    ("zoom", Read),
    ("click", Exec),
    ("doubleClick", Exec),
    ("move", Exec),
    ("drag", Exec),
    ("scroll", Exec),
    ("type", Exec),
    ("press", Exec),
    ("raise", Exec),
    ("ax", Read),
    ("find", Read),
    ("ref", Read),
];

/// Helpers callable on an AX element handle resolved through `desktop.ref(ref)`.
pub const ELEMENT_METHODS: MethodPolicies = &[
    ("value", Read),
    ("setValue", Exec),
    ("bounds", Read),
    ("attributes", Read),
    ("actions", Read),
    ("perform", Exec),
    ("press", Exec),
    ("click", Exec),
    ("focus", Exec),
    ("parent", Read),
    ("children", Read),
];

/// Root methods whose result accepts one chained handle call, with the handle label and method table.
fn handle_root(method: &str) -> Option<(&'static str, MethodPolicies)> {
    match method {
        "window" => Some(("window", WINDOW_METHODS)),
        "display" => Some(("display", DISPLAY_METHODS)),
        "ref" => Some(("element", ELEMENT_METHODS)),
        _ => None,
    }
}

fn lookup(methods: MethodPolicies, method: &str) -> Option<ComputerCallPolicy> {
    methods
        .iter()
        .find(|(name, _)| *name == method)
        .map(|(_, p)| *p)
}

fn describe(methods: MethodPolicies) -> String {
    methods
        .iter()
        .map(|(name, _)| *name)
        .collect::<Vec<_>>()
        .join(", ")
}

/// Validates a call chain against the allowlist. Error messages use fixed wording.
pub fn validate_chain(chain: &[ComputerCallStep]) -> Result<(), String> {
    if chain.is_empty() {
        return Err("Action 'call' requires a non-empty 'chain'.".into());
    }
    if chain.len() > 2 {
        return Err(
            "Call chains support one handle hop at most; use computer.run(fn) for longer sequences."
                .into(),
        );
    }
    let root = &chain[0];
    if lookup(DESKTOP_METHODS, &root.method).is_none() {
        return Err(format!(
            "Unknown desktop method \"{}\". Desktop helpers support: {}.",
            root.method,
            describe(DESKTOP_METHODS)
        ));
    }
    if chain.len() == 1 {
        return Ok(());
    }
    let Some((label, methods)) = handle_root(&root.method) else {
        return Err(format!(
            "Only desktop.window(id)/desktop.display(id)/desktop.ref(ref) results accept a chained call; got desktop.{}().",
            root.method
        ));
    };
    let step = &chain[1];
    if lookup(methods, &step.method).is_none() {
        let mut cap = label.chars();
        let capitalized = match cap.next() {
            Some(first) => first.to_uppercase().collect::<String>() + cap.as_str(),
            None => String::new(),
        };
        return Err(format!(
            "Unknown {label} method \"{}\". {capitalized} handles support: {}.",
            step.method,
            describe(methods)
        ));
    }
    Ok(())
}

/// Whether every step of a direct computer call is inspection-only, so the run needs read approval.
pub fn is_read_only_computer_call(chain: &[ComputerCallStep]) -> Result<bool, String> {
    validate_chain(chain)?;
    let root = &chain[0];
    if chain.len() == 1 {
        return Ok(lookup(DESKTOP_METHODS, &root.method) == Some(Read));
    }
    let (_, methods) = handle_root(&root.method).ok_or_else(|| {
        format!(
            "Only handle roots accept a chained call; got {}.",
            root.method
        )
    })?;
    Ok(lookup(methods, &chain[1].method) == Some(Read))
}

/// Renders a call chain (`id(5).click()`) with arguments as JavaScript literals.
pub fn render_call_chain(chain: &[ComputerCallStep]) -> String {
    chain
        .iter()
        .map(|step| {
            let args = step
                .args
                .iter()
                .map(render_run_arg)
                .collect::<Vec<_>>()
                .join(", ");
            format!("{}({args})", step.method)
        })
        .collect::<Vec<_>>()
        .join(".")
}

/// Renders JSON data without interpreting object keys as JavaScript syntax.
pub fn render_run_arg(value: &Value) -> String {
    let json = value.to_string();
    if value.is_object() || value.is_array() {
        // Object literals give `__proto__` special semantics, even when quoted.
        // Parsing also preserves these keys in nested objects and arrays.
        format!("JSON.parse({})", Value::String(json))
    } else {
        json
    }
}

/// Renders a function invocation with the requested run scope and positional arguments.
pub fn render_function_run(fn_source: &str, scope_names: &[&str], args: &[Value]) -> String {
    let scope = scope_names.join(", ");
    let rendered_args: String = args
        .iter()
        .map(|v| format!(", {}", render_run_arg(v)))
        .collect();
    format!("return await ({fn_source})({{ {scope} }}{rendered_args});")
}

/// Renders a validated direct computer call as the JavaScript body run by the worker.
pub fn render_computer_call(chain: &[ComputerCallStep]) -> Result<String, String> {
    validate_chain(chain)?;
    let root = &chain[0];
    if chain.len() == 1 {
        return Ok(format!(
            "return await desktop.{};",
            render_call_chain(std::slice::from_ref(root))
        ));
    }
    Ok(format!(
        "return await (await desktop.{}).{};",
        render_call_chain(std::slice::from_ref(root)),
        render_call_chain(std::slice::from_ref(&chain[1]))
    ))
}

#[cfg(test)]
mod computer_call_tests;
