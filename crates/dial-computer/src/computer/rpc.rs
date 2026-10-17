use std::fmt;

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::computer::desktop::DesktopSession;
use crate::computer::desktop::applications::{ApplicationOpenOptions, ApplicationQuery};
use crate::computer::desktop::error::{DesktopError, ErrorCode};
use crate::computer::desktop::types::{
    AxQuery, AxSnapshotOptions, CaptureCaps, CaptureRegion, DesktopPoint, HoldOptions,
    PointerOptions,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ComputerError {
    pub name: String,
    pub message: String,
    pub stack: Option<String>,
    pub is_tool_error: bool,
    pub is_abort: bool,
}

pub type ComputerResult<T> = Result<T, ComputerError>;

impl ComputerError {
    pub fn tool(message: impl Into<String>) -> Self {
        Self {
            name: "ToolError".into(),
            message: message.into(),
            stack: None,
            is_tool_error: true,
            is_abort: false,
        }
    }

    pub fn abort(message: impl Into<String>) -> Self {
        Self {
            name: "ToolAbortError".into(),
            message: message.into(),
            stack: None,
            is_tool_error: false,
            is_abort: true,
        }
    }

    pub fn internal(message: impl Into<String>) -> Self {
        Self {
            name: "Error".into(),
            message: message.into(),
            stack: None,
            is_tool_error: false,
            is_abort: false,
        }
    }

    pub fn to_payload(&self) -> Value {
        let mut payload = json!({
            "name": self.name,
            "message": self.message,
            "isToolError": self.is_tool_error,
            "isAbort": self.is_abort,
        });
        if let Some(stack) = &self.stack {
            payload["stack"] = json!(stack);
        }
        payload
    }

    pub fn from_payload(value: &Value) -> Self {
        Self {
            name: value
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or("Error")
                .to_owned(),
            message: value
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("Computer worker error")
                .to_owned(),
            stack: value
                .get("stack")
                .and_then(Value::as_str)
                .map(str::to_owned),
            is_tool_error: value
                .get("isToolError")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            is_abort: value
                .get("isAbort")
                .and_then(Value::as_bool)
                .unwrap_or(false),
        }
    }
}

impl fmt::Display for ComputerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for ComputerError {}

pub fn desktop_error(error: DesktopError) -> ComputerError {
    let is_abort = matches!(error.code, ErrorCode::Cancelled | ErrorCode::Closed);
    ComputerError {
        name: "Error".into(),
        message: error.message,
        stack: None,
        is_tool_error: false,
        is_abort,
    }
}

fn parse<T: DeserializeOwned>(method: &str, args: Value) -> ComputerResult<T> {
    let args = if args.is_null() {
        Value::Object(Default::default())
    } else {
        args
    };
    serde_json::from_value(args)
        .map_err(|e| ComputerError::tool(format!("Invalid arguments for {method}: {e}")))
}

fn out<T: Serialize>(value: T) -> ComputerResult<Value> {
    serde_json::to_value(value).map_err(|e| ComputerError::internal(e.to_string()))
}

fn ok<T: Serialize>(result: Result<T, DesktopError>) -> ComputerResult<Value> {
    out(result.map_err(desktop_error)?)
}

fn unit(result: Result<(), DesktopError>) -> ComputerResult<Value> {
    result.map(|()| Value::Null).map_err(desktop_error)
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ListApplicationsArgs {
    options: Option<ApplicationQuery>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct OpenApplicationArgs {
    id: String,
    options: Option<ApplicationOpenOptions>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ObserveArgs {
    target: String,
    caps: Option<CaptureCaps>,
    ax_options: Option<AxSnapshotOptions>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct MenuItemsArgs {
    target: String,
    path: Option<Vec<String>>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct MenuSelectArgs {
    target: String,
    path: Vec<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct WindowIdArgs {
    window_id: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct HoldKeysArgs {
    target: String,
    keys: Vec<String>,
    options: HoldOptions,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct HoldMouseArgs {
    target: String,
    x: f64,
    y: f64,
    options: HoldOptions,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CaptureArgs {
    target: String,
    caps: Option<CaptureCaps>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CaptureRegionArgs {
    target: String,
    region: CaptureRegion,
    caps: Option<CaptureCaps>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct PointArgs {
    target: String,
    x: f64,
    y: f64,
    opts: Option<PointerOptions>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct DragArgs {
    target: String,
    path: Vec<DesktopPoint>,
    opts: Option<PointerOptions>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ScrollArgs {
    target: String,
    x: f64,
    y: f64,
    dx: f64,
    dy: f64,
    opts: Option<PointerOptions>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct TypeTextArgs {
    target: String,
    text: String,
    opts: Option<PointerOptions>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct KeyChordArgs {
    target: String,
    keys: Vec<String>,
    opts: Option<PointerOptions>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct AxSnapshotArgs {
    target: String,
    opts: Option<AxSnapshotOptions>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct AxQueryArgs {
    target: String,
    query: AxQuery,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct AxElementAtArgs {
    target: String,
    x: f64,
    y: f64,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ReferenceArgs {
    reference: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct AxPerformArgs {
    reference: String,
    action: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct AxSetValueArgs {
    reference: String,
    value: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct AxClickArgs {
    reference: String,
    opts: Option<PointerOptions>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ClipboardWriteArgs {
    text: String,
}

#[cfg(target_os = "macos")]
fn clipboard_candidates(write: bool) -> Vec<(&'static str, Vec<&'static str>)> {
    vec![if write {
        ("pbcopy", vec![])
    } else {
        ("pbpaste", vec![])
    }]
}

#[cfg(target_os = "linux")]
fn clipboard_candidates(write: bool) -> Vec<(&'static str, Vec<&'static str>)> {
    let mut list = Vec::new();
    if std::env::var_os("WAYLAND_DISPLAY").is_some() {
        list.push(if write {
            ("wl-copy", vec![])
        } else {
            ("wl-paste", vec!["--no-newline"])
        });
    }
    list.push(if write {
        ("xclip", vec!["-selection", "clipboard", "-i"])
    } else {
        ("xclip", vec!["-selection", "clipboard", "-o"])
    });
    list.push(if write {
        ("xsel", vec!["--clipboard", "--input"])
    } else {
        ("xsel", vec!["--clipboard", "--output"])
    });
    list
}

#[cfg(target_os = "windows")]
fn clipboard_candidates(write: bool) -> Vec<(&'static str, Vec<&'static str>)> {
    vec![if write {
        (
            "powershell",
            vec![
                "-NoProfile",
                "-NonInteractive",
                "-Command",
                "[Console]::In.ReadToEnd() | Set-Clipboard",
            ],
        )
    } else {
        (
            "powershell",
            vec![
                "-NoProfile",
                "-NonInteractive",
                "-Command",
                "Get-Clipboard -Raw",
            ],
        )
    }]
}

#[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
fn clipboard_candidates(_write: bool) -> Vec<(&'static str, Vec<&'static str>)> {
    Vec::new()
}

fn run_clipboard(program: &str, args: &[&str], stdin: Option<&str>) -> Result<String, String> {
    use std::io::Write;
    use std::process::{Command, Stdio};
    let mut child = Command::new(program)
        .args(args)
        .stdin(if stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(if stdin.is_some() {
            Stdio::null()
        } else {
            Stdio::piped()
        })
        .stderr(if stdin.is_some() {
            Stdio::null()
        } else {
            Stdio::piped()
        })
        .spawn()
        .map_err(|e| format!("{program}: {e}"))?;
    let written = match (stdin, child.stdin.take()) {
        (Some(text), Some(mut pipe)) => pipe.write_all(text.as_bytes()),
        _ => Ok(()),
    };
    let output = child
        .wait_with_output()
        .map_err(|e| format!("{program}: {e}"))?;
    written.map_err(|e| format!("{program}: {e}"))?;
    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    } else {
        Err(format!(
            "{program} exited with {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        ))
    }
}

fn clipboard_run(write: bool, stdin: Option<&str>) -> ComputerResult<String> {
    let mut last_error = None;
    for (program, args) in clipboard_candidates(write) {
        match run_clipboard(program, &args, stdin) {
            Ok(text) => return Ok(text),
            Err(error) => last_error = Some(error),
        }
    }
    Err(ComputerError::tool(last_error.unwrap_or_else(|| {
        "Clipboard is not supported on this platform".to_owned()
    })))
}

fn clipboard_read() -> ComputerResult<Value> {
    out(clipboard_run(false, None)?)
}

fn clipboard_write(text: &str) -> ComputerResult<Value> {
    clipboard_run(true, Some(text))?;
    Ok(Value::Null)
}

pub fn dispatch(session: &DesktopSession, method: &str, args: Value) -> ComputerResult<Value> {
    match method {
        "capabilities" => ok(session.capabilities()),
        "listDisplays" => ok(session.list_displays()),
        "listWindows" => ok(session.list_windows()),
        "listApplications" => {
            let a: ListApplicationsArgs = parse(method, args)?;
            ok(session.list_applications(a.options))
        }
        "openApplication" => {
            let a: OpenApplicationArgs = parse(method, args)?;
            ok(session.open_application(a.id, a.options))
        }
        "observe" => {
            let a: ObserveArgs = parse(method, args)?;
            ok(session.observe(a.target, a.caps, a.ax_options))
        }
        "menuItems" => {
            let a: MenuItemsArgs = parse(method, args)?;
            ok(session.menu_items(a.target, a.path))
        }
        "menuSelect" => {
            let a: MenuSelectArgs = parse(method, args)?;
            unit(session.menu_select(a.target, a.path))
        }
        "bringToCurrentSpace" => {
            let a: WindowIdArgs = parse(method, args)?;
            unit(session.bring_to_current_space(a.window_id))
        }
        "holdKeys" => {
            let a: HoldKeysArgs = parse(method, args)?;
            unit(session.hold_keys(a.target, a.keys, a.options))
        }
        "holdMouse" => {
            let a: HoldMouseArgs = parse(method, args)?;
            unit(session.hold_mouse(a.target, a.x, a.y, a.options))
        }
        "acquireControl" => ok(session.acquire_control()),
        "releaseControl" => {
            session.release_control();
            Ok(Value::Null)
        }
        "controlState" => out(session.control_state()),
        "retire" => {
            session.retire();
            Ok(Value::Null)
        }
        "cancel" => {
            session.cancel();
            Ok(Value::Null)
        }
        "capture" => {
            let a: CaptureArgs = parse(method, args)?;
            ok(session.capture(a.target, a.caps))
        }
        "captureRegion" => {
            let a: CaptureRegionArgs = parse(method, args)?;
            ok(session.capture_region(a.target, a.region, a.caps))
        }
        "click" => {
            let a: PointArgs = parse(method, args)?;
            unit(session.click(a.target, a.x, a.y, a.opts))
        }
        "moveMouse" => {
            let a: PointArgs = parse(method, args)?;
            unit(session.move_mouse(a.target, a.x, a.y, a.opts))
        }
        "drag" => {
            let a: DragArgs = parse(method, args)?;
            unit(session.drag(a.target, a.path, a.opts))
        }
        "scroll" => {
            let a: ScrollArgs = parse(method, args)?;
            unit(session.scroll(a.target, a.x, a.y, a.dx, a.dy, a.opts))
        }
        "typeText" => {
            let a: TypeTextArgs = parse(method, args)?;
            unit(session.type_text(a.target, a.text, a.opts))
        }
        "keyChord" => {
            let a: KeyChordArgs = parse(method, args)?;
            unit(session.key_chord(a.target, a.keys, a.opts))
        }
        "raiseWindow" => {
            let a: WindowIdArgs = parse(method, args)?;
            unit(session.raise_window(a.window_id))
        }
        "axSnapshot" => {
            let a: AxSnapshotArgs = parse(method, args)?;
            ok(session.ax_snapshot(a.target, a.opts))
        }
        "axQuery" => {
            let a: AxQueryArgs = parse(method, args)?;
            ok(session.ax_query(a.target, a.query))
        }
        "axElementAt" => {
            let a: AxElementAtArgs = parse(method, args)?;
            ok(session.ax_element_at(a.target, a.x, a.y))
        }
        "axFocused" => ok(session.ax_focused()),
        "axNode" => {
            let a: ReferenceArgs = parse(method, args)?;
            ok(session.ax_node(a.reference))
        }
        "axAttributes" => {
            let a: ReferenceArgs = parse(method, args)?;
            ok(session.ax_attributes(a.reference))
        }
        "axChildren" => {
            let a: ReferenceArgs = parse(method, args)?;
            ok(session.ax_children(a.reference))
        }
        "axParent" => {
            let a: ReferenceArgs = parse(method, args)?;
            ok(session.ax_parent(a.reference))
        }
        "axPerform" => {
            let a: AxPerformArgs = parse(method, args)?;
            unit(session.ax_perform(a.reference, a.action))
        }
        "axSetValue" => {
            let a: AxSetValueArgs = parse(method, args)?;
            unit(session.ax_set_value(a.reference, a.value))
        }
        "axFocus" => {
            let a: ReferenceArgs = parse(method, args)?;
            unit(session.ax_focus(a.reference))
        }
        "axClick" => {
            let a: AxClickArgs = parse(method, args)?;
            unit(session.ax_click(a.reference, a.opts))
        }
        "clipboardRead" => clipboard_read(),
        "clipboardWrite" => {
            let a: ClipboardWriteArgs = parse(method, args)?;
            clipboard_write(&a.text)
        }
        other => Err(ComputerError::tool(format!(
            "Unknown computer native method: {other}"
        ))),
    }
}

#[cfg(test)]
mod computer_rpc_tests;
