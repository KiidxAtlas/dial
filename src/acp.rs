//! Agent Client Protocol v1 over stdio JSON-RPC 2.0.
//! Uses the RPC actor in-process; client filesystem, terminal and MCP bridges are not installed.
use crate::{config::types::Config, storage::paths::Layout};
use anyhow::{Context, Result, ensure};
use base64::{Engine, engine::general_purpose::STANDARD};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::{HashMap, HashSet},
    fs,
    io::Read,
    path::{Path, PathBuf},
};

use crate::platform;

pub fn run(layout: Layout, config: Config) -> Result<()> {
    crate::rpc::run_acp(layout, config)
}

struct Pending {
    method: String,
    session: Option<String>,
}
/// One live call owns its wire identity and output. Names alone cannot correlate overlapping calls.
struct ActiveTool {
    id: String,
    name: String,
    call_id: Option<String>,
    output: String,
    pending: bool,
    /// Approval that created this placeholder; cleared when a call card adopts it, removed when decided.
    approval: Option<String>,
}
pub(crate) struct Adapter {
    layout: Layout,
    initialized: bool,
    image_input: bool,
    pending: HashMap<String, Pending>,
    sessions: HashSet<String>,
    active: Option<String>,
    permissions: HashMap<String, String>,
    tools: Vec<ActiveTool>,
    next_tool: u64,
    modes: HashMap<String, bool>,
}

fn key(id: &Value) -> String {
    id.to_string()
}
fn response(id: &Value, result: Value) -> Value {
    json!({"jsonrpc":"2.0","id":id,"result":result})
}
fn error(id: &Value, code: i32, message: impl std::fmt::Display) -> Value {
    json!({"jsonrpc":"2.0","id":id,"error":{"code":code,"message":message.to_string()}})
}
fn text_content(text: &Value) -> Value {
    json!({"type":"text","text":text})
}
/// Standard ACP `_meta` for text chunks from notice, background and fatal events. The chunk text is
/// unchanged; `dial` carries Dial's source fields and includes each optional one only when present.
fn source_meta(event: &Value) -> Value {
    let mut dial = json!({"event":event["event"]});
    for field in ["kind", "lane", "tokens", "name", "replace"] {
        if !event[field].is_null() {
            dial[field] = event[field].clone();
        }
    }
    json!({"dial":dial})
}
/// ACP stop reason for a finished prompt. Interruption stays `cancelled` and an exhausted step budget
/// is `max_turn_requests`. Other outcomes keep `end_turn`, with the authoritative Dial outcome and
/// report in `_meta.dial`, so failed, blocked and incomplete turns remain distinguishable.
fn prompt_stop(value: &Value) -> Value {
    if value["interrupted"] == true {
        return json!({"stopReason":"cancelled"});
    }
    let outcome = value["outcome"].as_str().unwrap_or_default();
    let stop = if value["step_limit"] == true {
        "max_turn_requests"
    } else {
        "end_turn"
    };
    if outcome == "completed" && stop == "end_turn" {
        return json!({"stopReason":stop});
    }
    let mut dial = json!({"outcome":outcome});
    if let Some(report) = value["text"].as_str() {
        dial["report"] = json!(report);
    }
    json!({"stopReason":stop,"_meta":{"dial":dial}})
}
/// ACP plan entry for one todo task. The `Status` match is exhaustive, so a new status cannot
/// silently render as pending. Abandoned and blocked tasks keep their state and note in `_meta.dial`.
fn plan_entry(task: &Value) -> Value {
    use crate::agent::todo::Status;
    let raw = match task["status"].as_str() {
        Some("done") => "completed",
        Some("running") => "in_progress",
        Some(status) => status,
        None => "",
    };
    let status = serde_json::from_value::<Status>(json!(raw)).ok();
    let (acp, dial_status) = match status {
        Some(Status::Pending) => ("pending", None),
        Some(Status::InProgress) => ("in_progress", None),
        Some(Status::Completed) => ("completed", None),
        // Abandoned work closes out the plan but is not completed work.
        Some(Status::Abandoned) => ("completed", Some("abandoned")),
        // Blocked work is still outstanding; its blocker is the note.
        Some(Status::Blocked) => ("pending", Some("blocked")),
        None => ("pending", Some(raw)),
    };
    let mut entry = json!({"content":task["content"],"priority":"medium","status":acp});
    if let Some(dial_status) = dial_status {
        let mut dial = json!({"status":dial_status});
        if let Some(note) = task["note"].as_str() {
            dial["note"] = json!(note);
        }
        entry["_meta"] = json!({"dial":dial});
    }
    entry
}
fn tool_kind(name: &str) -> &'static str {
    match name {
        "read" => "read",
        "edit" | "write" => "edit",
        "bash" | "eval" => "execute",
        "grep" | "glob" | "find" => "search",
        "web_search" | "web_fetch" => "fetch",
        "todo" => "think",
        "exit_plan" => "switch_mode",
        _ => "other",
    }
}
fn mode_state(plan: bool) -> Value {
    json!({"currentModeId":if plan {"plan"} else {"default"},"availableModes":[
        {"id":"default","name":"Agent","description":"Operator-configured approval and sandbox policy"},
        {"id":"plan","name":"Plan","description":"Read-only exploration; implementation still requires operator permission"}
    ]})
}
fn config_options(plan: bool) -> Value {
    json!([{"id":"mode","name":"Mode","category":"mode","type":"select","currentValue":if plan {"plan"} else {"default"},
        "options":[{"value":"default","name":"Agent"},{"value":"plan","name":"Plan"}]}])
}

impl Adapter {
    pub(crate) fn new(layout: Layout) -> Self {
        Self {
            layout,
            initialized: false,
            image_input: false,
            pending: HashMap::new(),
            sessions: HashSet::new(),
            active: None,
            permissions: HashMap::new(),
            tools: Vec::new(),
            next_tool: 0,
            modes: HashMap::new(),
        }
    }
    /// Return an actor request and immediate wire responses. Client responses are permission-only.
    pub(crate) fn receive(&mut self, wire: Value) -> (Option<Value>, Vec<Value>) {
        let id = wire.get("id").cloned().unwrap_or(Value::Null);
        if !wire.is_object()
            || wire["jsonrpc"] != "2.0"
            || (!id.is_null() && !id.is_string() && !id.is_number())
        {
            return (
                None,
                vec![error(&Value::Null, -32600, "invalid JSON-RPC 2.0 envelope")],
            );
        }
        if wire.get("method").is_none() {
            let Some(approval) = self.permissions.remove(&key(&id)) else {
                return (None, Vec::new());
            };
            let outcome = &wire["result"]["outcome"];
            let decision = if wire.get("error").is_none() && outcome["outcome"] == "selected" {
                match outcome["optionId"].as_str() {
                    Some("allow") => "allow",
                    Some("always") => "always",
                    _ => "deny",
                }
            } else {
                "deny"
            };
            return (
                Some(
                    json!({"id":null,"method":"approve","params":{"approval_id":approval,"decision":decision}}),
                ),
                Vec::new(),
            );
        }
        let Some(method) = wire["method"].as_str() else {
            return (None, vec![error(&id, -32600, "method must be a string")]);
        };
        let notification = wire.get("id").is_none();
        if method == "$/cancel_request" {
            return (None, Vec::new());
        }
        if notification && method != "session/cancel" {
            return (None, Vec::new());
        }
        if !notification && id.is_null() {
            return (
                None,
                vec![error(&id, -32600, "request id must be a string or number")],
            );
        }
        // Check correlation before preparation: prompt preparation can persist attachments, and
        // cancellation must not consume an existing prompt's response slot.
        if !notification && self.pending.contains_key(&key(&id)) {
            return (
                None,
                vec![error(&id, -32602, "request id is already in flight")],
            );
        }
        let result = self.prepare(method, &wire["params"], &id);
        match result {
            Ok((request, replies)) => (request, replies),
            Err(e) => (
                None,
                if notification {
                    Vec::new()
                } else {
                    vec![error(&id, -32602, format!("{e:#}"))]
                },
            ),
        }
    }
    fn prepare(
        &mut self,
        method: &str,
        params: &Value,
        id: &Value,
    ) -> Result<(Option<Value>, Vec<Value>)> {
        if method == "initialize" {
            ensure!(!self.initialized, "already initialized");
            ensure!(
                params["protocolVersion"].as_u64().is_some(),
                "protocolVersion must be an integer"
            );
            // The capabilities are client offers, not requests. Do not silently claim their bridges.
            self.initialized = true;
            return Ok((
                None,
                vec![response(
                    id,
                    json!({"protocolVersion":1,
                        "agentInfo":{"name":"dial","title":"Dial","version":crate::build_info::VERSION},
                        "authMethods":[{"id":"local","name":"Existing Dial provider configuration","description":"Configure credentials outside ACP; no terminal or OAuth authentication bridge"}],
                        "agentCapabilities":{"loadSession":true,"promptCapabilities":{"embeddedContext":true,"image":self.image_input,"audio":false},
                            "mcpCapabilities":{"http":false,"sse":false},"sessionCapabilities":{"list":{},"fork":{},"resume":{},"close":{}}},
                        "_meta":{"dial":{"cwd":self.layout.project_root,"clientBridges":[],"permissionRequests":"required for operator ask policy; failure denies","questionInput":{"supported":false,"reason":"No custom human-input capability is negotiated; permission responses are not question answers"},"cwdPolicy":"one launched project; start dial -C <cwd> acp for another project"}}
                    }),
                )],
            ));
        }
        ensure!(self.initialized, "initialize must precede session methods");
        if method == "authenticate" {
            ensure!(
                params["methodId"] == "local",
                "unsupported authentication method; only existing local configuration is supported"
            );
            return Ok((None, vec![response(id, json!({}))]));
        }
        ensure!(params.is_object(), "params must be an object");
        let mut actor_params = json!({"_acp":true});
        let session = params["sessionId"].as_str().map(str::to_owned);
        let actor_method = match method {
            "session/new" | "session/load" | "session/resume" | "session/fork" => {
                self.cwd(params["cwd"].as_str().context("cwd is required")?)?;
                let servers = params["mcpServers"]
                    .as_array()
                    .context("mcpServers must be an array")?;
                ensure!(
                    servers.is_empty(),
                    "client MCP servers are unsupported; configure operator-owned Dial adapters instead"
                );
                if let Some(dirs) = params.get("additionalDirectories") {
                    ensure!(
                        dirs.as_array().is_some_and(|d| d.is_empty()),
                        "additionalDirectories client bridge is unsupported"
                    );
                }
                actor_params["_replay"] = json!(method == "session/load");
                match method {
                    "session/new" => "new_session",
                    "session/fork" => "fork_session",
                    _ => "resume",
                }
            }
            "session/list" => {
                if let Some(cwd) = params["cwd"].as_str() {
                    self.cwd(cwd)?;
                }
                ensure!(
                    params["cwd"].is_null() || params["cwd"].is_string(),
                    "cwd must be a string"
                );
                ensure!(
                    params["cursor"].is_null() || params["cursor"].is_string(),
                    "cursor must be a string"
                );
                if let Some(dirs) = params.get("additionalDirectories") {
                    ensure!(
                        dirs.as_array().is_some_and(|d| d.is_empty()),
                        "additionalDirectories client bridge is unsupported"
                    );
                }
                actor_params["cursor"] = params["cursor"].clone();
                "list_sessions"
            }
            "session/prompt" => {
                ensure!(
                    session
                        .as_deref()
                        .is_some_and(|s| self.sessions.contains(s)),
                    "session is not open"
                );
                let blocks = params["prompt"]
                    .as_array()
                    .context("prompt must be a content-block array")?;
                let (text, message) = self.prompt(blocks)?;
                actor_params["text"] = json!(text);
                actor_params["_message"] = message;
                "send"
            }
            "session/cancel" => {
                let session = session.as_deref().context("sessionId is required")?;
                ensure!(self.sessions.contains(session), "session is not open");
                // The core has only one running turn. Never cancel another session's turn.
                if self.active.as_deref() != Some(session) {
                    return Ok((
                        None,
                        if id.is_null() {
                            Vec::new()
                        } else {
                            vec![response(id, json!({}))]
                        },
                    ));
                }
                self.permissions.clear();
                "abort"
            }
            "session/close" => {
                actor_params["_active"] = json!(session.as_deref() == self.active.as_deref());
                "close_session"
            }
            "session/set_mode" | "session/set_config_option" => {
                if method == "session/set_config_option" {
                    ensure!(
                        params["configId"] == "mode",
                        "only the mode config option is supported"
                    );
                }
                let mode = if method == "session/set_mode" {
                    &params["modeId"]
                } else {
                    &params["value"]
                };
                ensure!(
                    mode == "plan" || mode == "default",
                    "mode must be plan or default"
                );
                actor_params["enabled"] = json!(mode == "plan");
                "plan"
            }
            _ => {
                return Ok((
                    None,
                    vec![error(
                        id,
                        -32601,
                        format!(
                            "unsupported ACP method {method}; client filesystem/terminal/auth/MCP bridges are not implemented"
                        ),
                    )],
                ));
            }
        };
        if actor_method != "new_session" && actor_method != "list_sessions" {
            let session = session.as_deref().context("sessionId is required")?;
            if !matches!(actor_method, "resume" | "fork_session") {
                ensure!(self.sessions.contains(session), "session is not open");
            }
            actor_params["session_id"] = json!(session);
        }
        if !id.is_null() {
            self.pending.insert(
                key(id),
                Pending {
                    method: method.into(),
                    session,
                },
            );
        }
        Ok((
            Some(json!({"id":id,"method":actor_method,"params":actor_params})),
            Vec::new(),
        ))
    }
    fn cwd(&self, cwd: &str) -> Result<()> {
        ensure!(Path::new(cwd).is_absolute(), "cwd must be absolute");
        ensure!(
            fs::canonicalize(cwd)? == self.layout.project_root,
            "cwd differs from launched project; launch dial -C <cwd> acp"
        );
        self.layout.ensure_project_outside_storage()
    }
    fn prompt(&self, blocks: &[Value]) -> Result<(String, Value)> {
        ensure!(blocks.len() <= 64, "at most 64 prompt content blocks");
        let mut text = String::new();
        let mut images = Vec::new();
        for block in blocks {
            let part = match block["type"].as_str() {
                Some("text") => block["text"]
                    .as_str()
                    .context("text block requires text")?
                    .to_owned(),
                Some("image") => {
                    images.push(self.image(&block["data"], &block["mimeType"])?);
                    "[Image attached]".to_owned()
                }
                Some("resource") => {
                    let resource = &block["resource"];
                    let uri = resource["uri"]
                        .as_str()
                        .context("embedded resource requires uri")?;
                    if let Some(body) = resource["text"].as_str() {
                        format!("Referenced resource {uri}:\n{body}")
                    } else if resource["mimeType"]
                        .as_str()
                        .is_some_and(|m| m.starts_with("image/"))
                    {
                        images.push(self.image(&resource["blob"], &resource["mimeType"])?);
                        format!("[Image resource attached: {uri}]")
                    } else {
                        let bytes = STANDARD.decode(
                            resource["blob"]
                                .as_str()
                                .context("resource requires text or base64 blob")?,
                        )?;
                        format!(
                            "Referenced resource {uri}:\n{}",
                            String::from_utf8(bytes)
                                .context("non-image resource blob must be UTF-8")?
                        )
                    }
                }
                Some("resource_link") => {
                    let uri = block["uri"]
                        .as_str()
                        .context("resource_link requires uri")?;
                    let path = self.file_uri(uri)?;
                    let file = self.resource_file(&path)?;
                    ensure!(
                        file.metadata()?.is_file(),
                        "resource must be a regular project file"
                    );
                    if crate::llm::images::is_image(&path) {
                        let mut bytes = Vec::new();
                        file.take(crate::llm::images::MAX_IMAGE_BYTES + 1)
                            .read_to_end(&mut bytes)?;
                        images.push(self.image_bytes(bytes, None)?);
                        format!("[Image resource attached: {uri}]")
                    } else {
                        let mut bytes = Vec::new();
                        file.take(1024 * 1024 + 1).read_to_end(&mut bytes)?;
                        ensure!(
                            bytes.len() <= 1024 * 1024,
                            "resource exceeds 1 MiB; embed a selected excerpt instead"
                        );
                        format!(
                            "Referenced resource {uri}:\n{}",
                            String::from_utf8(bytes).context(
                                "resource is not UTF-8; embed a supported image instead"
                            )?
                        )
                    }
                }
                _ => anyhow::bail!(
                    "unsupported content block (audio and remote/client resource bridges are not supported)"
                ),
            };
            if !text.is_empty() {
                text.push_str("\n\n");
            }
            text.push_str(&part);
            ensure!(text.len() <= 262_144, "expanded prompt exceeds 256 KiB");
        }
        ensure!(!text.trim().is_empty(), "prompt is empty");
        let mut message = json!({"role":"user","content":text});
        if !images.is_empty() {
            message["dial_images"] = json!(images);
        }
        Ok((text, message))
    }
    fn file_uri(&self, uri: &str) -> Result<PathBuf> {
        let url = url::Url::parse(uri)?;
        ensure!(
            url.scheme() == "file" && url.query().is_none() && url.fragment().is_none(),
            "resource links must be local file:// URLs; client/remote resource fetching is unsupported"
        );
        let path = url
            .to_file_path()
            .map_err(|_| anyhow::anyhow!("invalid local file URI"))?
            .canonicalize()?;
        ensure!(
            path.starts_with(&self.layout.project_root),
            "resource resolves outside project; embed its content explicitly instead"
        );
        Ok(path)
    }
    /// Open by walking held directory handles without following replacement reparse points.
    /// A running project tool must not race a resource read into operator-private files.
    fn resource_file(&self, path: &Path) -> Result<fs::File> {
        let relative = path.strip_prefix(&self.layout.project_root)?;
        Ok(platform::fs::open_beneath(
            &self.layout.project_root,
            relative,
            false,
        )?)
    }

    fn image(&self, data: &Value, mime: &Value) -> Result<Value> {
        let mime = mime.as_str().context("image requires mimeType")?;
        let data = data.as_str().context("image requires base64 data")?;
        ensure!(data.len() <= 7 * 1024 * 1024, "encoded image exceeds limit");
        self.image_bytes(STANDARD.decode(data)?, Some(mime))
    }
    fn image_bytes(&self, bytes: Vec<u8>, requested_mime: Option<&str>) -> Result<Value> {
        ensure!(
            self.image_input,
            "the configured model does not support image input"
        );
        ensure!(
            bytes.len() as u64 <= crate::llm::images::MAX_IMAGE_BYTES,
            "image exceeds 5 MiB"
        );
        let (mime, ext) = match image::guess_format(&bytes)? {
            image::ImageFormat::Png => ("image/png", "png"),
            image::ImageFormat::Jpeg => ("image/jpeg", "jpg"),
            image::ImageFormat::Gif => ("image/gif", "gif"),
            image::ImageFormat::WebP => ("image/webp", "webp"),
            _ => anyhow::bail!("unsupported image format"),
        };
        ensure!(
            requested_mime.is_none_or(|requested| requested == mime),
            "image bytes do not match mimeType"
        );
        let hash = format!("{:x}", Sha256::digest(&bytes));
        let dir = self.layout.state_dir.join("acp-images");
        platform::fs::create_dir_owner_only(&fs::DirBuilder::new().recursive(true), &dir)?;
        ensure!(
            dir.canonicalize()?
                .starts_with(self.layout.state_dir.canonicalize()?),
            "image storage escapes private state"
        );
        let path = dir.join(format!("{hash}.{ext}"));
        // Atomic and always rewritten: a torn file from an earlier crash must never be reused.
        crate::storage::settings_store::write_private(&path, &bytes)?;
        let reference = crate::llm::images::user_attachment(&self.layout.project_root, &path)?;
        ensure!(
            reference["sha256"].as_str() == Some(hash.as_str()),
            "stored attachment content differs"
        );
        Ok(reference)
    }
    fn update(&self, update: Value) -> Option<Value> {
        self.active.as_ref().map(|session|json!({"jsonrpc":"2.0","method":"session/update","params":{"sessionId":session,"update":update}}))
    }
    fn replay(&self, message: &Value) -> Vec<Value> {
        let role = message["role"].as_str().unwrap_or("");
        let mut updates = Vec::new();
        if matches!(role, "user" | "assistant") {
            let kind = if role == "user" {
                "user_message_chunk"
            } else {
                "agent_message_chunk"
            };
            if let Some(text) = message["content"].as_str() {
                if let Some(update) =
                    self.update(json!({"sessionUpdate":kind,"content":{"type":"text","text":text}}))
                {
                    updates.push(update);
                }
            }
            for reference in message["dial_images"].as_array().into_iter().flatten() {
                let content = match crate::llm::images::verified_bytes(reference) {
                    Ok((bytes, mime)) => {
                        json!({"type":"image","mimeType":mime,"data":STANDARD.encode(bytes)})
                    }
                    Err(e) => {
                        json!({"type":"text","text":format!("[Stored image unavailable: {e:#}]")})
                    }
                };
                if let Some(update) = self.update(json!({"sessionUpdate":kind,"content":content})) {
                    updates.push(update);
                }
            }
        }
        for call in message["tool_calls"].as_array().into_iter().flatten() {
            let name = call["function"]["name"].as_str().unwrap_or("tool");
            let args = call["function"]["arguments"]
                .as_str()
                .and_then(|s| serde_json::from_str::<Value>(s).ok())
                .unwrap_or_else(|| call["function"]["arguments"].clone());
            if let Some(update)=self.update(json!({"sessionUpdate":"tool_call","toolCallId":call["id"],"title":name,"kind":tool_kind(name),"status":"in_progress","rawInput":args})){updates.push(update);}
        }
        if role == "tool" {
            let mut content =
                vec![json!({"type":"content","content":text_content(&message["content"])})];
            for reference in message["dial_images"].as_array().into_iter().flatten() {
                let image = match crate::llm::images::verified_bytes(reference) {
                    Ok((bytes, mime)) => {
                        json!({"type":"image","mimeType":mime,"data":STANDARD.encode(bytes)})
                    }
                    Err(e) => {
                        json!({"type":"text","text":format!("[Stored image unavailable: {e:#}]")})
                    }
                };
                content.push(json!({"type":"content","content":image}));
            }
            if let Some(update)=self.update(json!({"sessionUpdate":"tool_call_update","toolCallId":message["tool_call_id"],
                "status":if message["dial_evidence"]["is_error"]==true {"failed"} else {"completed"},
                "content":content})){updates.push(update);}
        }
        updates
    }

    pub(crate) fn emit(&mut self, event: Value) -> Vec<Value> {
        if let Some(id) = event
            .get("id")
            .filter(|_| event.get("result").is_some() || event.get("error").is_some())
        {
            let Some(pending) = self.pending.remove(&key(id)) else {
                return Vec::new();
            };
            if let Some(e) = event.get("error") {
                return vec![error(
                    id,
                    -32603,
                    e["message"].as_str().unwrap_or("actor error"),
                )];
            }
            let value = &event["result"];
            let mut updates = Vec::new();
            let result = match pending.method.as_str() {
                "session/new" | "session/load" | "session/resume" | "session/fork" => {
                    let session = value["session_id"].as_str().unwrap_or("").to_owned();
                    self.sessions.insert(session.clone());
                    self.active = Some(session.clone());
                    self.tools.clear();
                    let plan = value["plan_mode"].as_bool().unwrap_or(false);
                    self.modes.insert(session.clone(), plan);
                    let mut result =
                        json!({"modes":mode_state(plan),"configOptions":config_options(plan)});
                    if matches!(pending.method.as_str(), "session/new" | "session/fork") {
                        result["sessionId"] = json!(session);
                    }
                    result
                }
                "session/list" => value.clone(),
                "session/prompt" => {
                    self.permissions.clear();
                    let plan = value["plan_mode"].as_bool().unwrap_or(false);
                    if let Some(session) = self.active.clone() {
                        if self.modes.insert(session, plan) != Some(plan) {
                            if let Some(update)=self.update(json!({"sessionUpdate":"current_mode_update","currentModeId":if plan {"plan"} else {"default"}})){updates.push(update);}
                            if let Some(update)=self.update(json!({"sessionUpdate":"config_option_update","configOptions":config_options(plan)})){updates.push(update);}
                        }
                    }
                    prompt_stop(value)
                }
                "session/close" => {
                    if let Some(session) = pending.session {
                        self.sessions.remove(&session);
                        self.modes.remove(&session);
                        if self.active.as_ref() == Some(&session) {
                            self.active = None;
                            self.tools.clear();
                            self.permissions.clear();
                        }
                    }
                    json!({})
                }
                "session/set_mode" | "session/set_config_option" => {
                    let plan = value["enabled"].as_bool().unwrap_or(false);
                    if let Some(session) = pending.session {
                        self.modes.insert(session.clone(), plan);
                        self.active = Some(session);
                    }
                    if let Some(update)=self.update(json!({"sessionUpdate":"current_mode_update","currentModeId":if plan {"plan"} else {"default"}})){updates.push(update);}
                    if let Some(update)=self.update(json!({"sessionUpdate":"config_option_update","configOptions":config_options(plan)})){updates.push(update);}
                    if pending.method == "session/set_config_option" {
                        json!({"configOptions":config_options(plan)})
                    } else {
                        json!({})
                    }
                }
                _ => json!({}),
            };
            // Session history belongs before load's response; new sessions have no early updates.
            updates.push(response(id, result));
            return updates;
        }
        let update = match event["event"].as_str().unwrap_or("") {
            "ready" => {
                self.image_input = event["image_input"].as_bool().unwrap_or(false);
                return Vec::new();
            }
            "replay_message" => return self.replay(&event["message"]),
            "active_session" => {
                self.active = event["session_id"].as_str().map(str::to_owned);
                self.tools.clear();
                return Vec::new();
            }
            "text" => {
                json!({"sessionUpdate":"agent_message_chunk","content":text_content(&event["delta"])})
            }
            "reasoning" => {
                json!({"sessionUpdate":"agent_thought_chunk","content":text_content(&event["delta"])})
            }
            "approval" => {
                let Some(session) = &self.active else {
                    return Vec::new();
                };
                let Some(approval) = event["approval_id"].as_str() else {
                    return Vec::new();
                };
                let id = format!("dial-permission-{approval}");
                self.permissions.insert(key(&json!(id)), approval.into());
                let name = event["tool"].as_str().unwrap_or("tool");
                let call_id = event["call_id"].as_str();
                // Attach to the call card with this call ID. Without one, add a placeholder that the
                // call's own tool_call adopts, or that `approval_resolved` removes once decided.
                let existing = call_id.and_then(|call| {
                    self.tools
                        .iter()
                        .rev()
                        .find(|tool| tool.call_id.as_deref() == Some(call))
                        .map(|tool| tool.id.clone())
                });
                let tool = match existing {
                    Some(tool) => tool,
                    None => {
                        self.tools.push(ActiveTool {
                            id: approval.into(),
                            name: name.into(),
                            call_id: call_id.map(str::to_owned),
                            output: String::new(),
                            pending: true,
                            approval: Some(approval.into()),
                        });
                        approval.into()
                    }
                };
                return vec![
                    json!({"jsonrpc":"2.0","id":id,"method":"session/request_permission","params":{"sessionId":session,
                    "toolCall":{"toolCallId":tool,"title":event["summary"],"kind":tool_kind(name),"status":"pending"},
                    "options":[{"optionId":"allow","name":"Allow once","kind":"allow_once"},{"optionId":"always","name":"Allow always","kind":"allow_always"},{"optionId":"deny","name":"Deny","kind":"reject_once"}]}}),
                ];
            }
            "approval_resolved" => {
                if let Some(approval) = event["approval_id"].as_str() {
                    // A decided request is no longer pending: drop its reply mapping and any placeholder.
                    self.permissions
                        .remove(&key(&json!(format!("dial-permission-{approval}"))));
                    self.tools
                        .retain(|tool| tool.approval.as_deref() != Some(approval));
                }
                return Vec::new();
            }
            "tool_call" => {
                let name = event["name"].as_str().unwrap_or("tool");
                let call_id = event["call_id"].as_str().map(str::to_owned);
                let id = if let Some(tool) = self.tools.iter_mut().rev().find(|tool| {
                    tool.pending
                        && tool.name == name
                        && match (&tool.call_id, &call_id) {
                            (Some(own), Some(new)) => own == new,
                            (Some(_), None) => false,
                            (None, _) => true,
                        }
                }) {
                    tool.pending = false;
                    tool.call_id = call_id;
                    tool.approval = None;
                    tool.id.clone()
                } else {
                    self.next_tool += 1;
                    let id = format!("dial-tool-{}", self.next_tool);
                    self.tools.push(ActiveTool {
                        id: id.clone(),
                        name: name.into(),
                        call_id,
                        output: String::new(),
                        pending: false,
                        approval: None,
                    });
                    id
                };
                json!({"sessionUpdate":"tool_call","toolCallId":id,"title":event["summary"],"kind":tool_kind(name),"status":"in_progress","rawInput":{"tool":name,"summary":event["summary"]}})
            }
            "tool_result" | "tool_output" => {
                let name = event["name"].as_str().unwrap_or("tool");
                let position =
                    self.tools
                        .iter()
                        .rposition(|tool| match event["call_id"].as_str() {
                            Some(id) => tool.call_id.as_deref() == Some(id),
                            None => tool.name == name,
                        });
                let Some(position) = position else {
                    return Vec::new();
                };
                let id = self.tools[position].id.clone();
                let partial = event["event"] == "tool_output";
                let text = if partial {
                    let text = &mut self.tools[position].output;
                    if let Some(delta) = event["delta"].as_str() {
                        text.push_str(delta);
                    }
                    if text.len() > 24 * 1024 {
                        *text = crate::agent::guard::clip_middle(text, 24 * 1024);
                    }
                    text.clone()
                } else {
                    event["text"].as_str().unwrap_or("").to_owned()
                };
                if !partial {
                    self.tools.remove(position);
                }
                json!({"sessionUpdate":"tool_call_update","toolCallId":id,"status":if partial {"in_progress"} else if event["is_error"]==true {"failed"} else {"completed"},"content":[{"type":"content","content":{"type":"text","text":text}}],"rawOutput":event.get("data")})
            }
            "todo" => {
                let entries: Vec<_> = event["tasks"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .map(plan_entry)
                    .collect();
                json!({"sessionUpdate":"plan","entries":entries})
            }
            "notice" | "background" | "fatal" => {
                json!({"sessionUpdate":"agent_message_chunk","content":text_content(&event["message"]),"_meta":source_meta(&event)})
            }
            _ => return Vec::new(),
        };
        self.update(update).into_iter().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::ui_event::{EventKind, UiEvent};

    /// Session updates for one active session, after each event passes through the adapter.
    fn session_updates(events: Vec<Value>) -> Vec<Value> {
        let project = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        let layout = Layout::with_user_dir(project.path(), home.path().to_path_buf()).unwrap();
        let mut adapter = Adapter::new(layout);
        adapter.emit(json!({"event":"active_session","session_id":"s1"}));
        events
            .into_iter()
            .flat_map(|event| adapter.emit(event))
            .map(|message| {
                assert_eq!(message["method"], "session/update");
                assert_eq!(message["params"]["sessionId"], "s1");
                message["params"]["update"].clone()
            })
            .collect()
    }

    #[test]
    fn overlapping_same_name_calls_keep_their_own_results() {
        let updates = session_updates(vec![
            json!({"event":"tool_call","call_id":"a","name":"read","summary":"first"}),
            json!({"event":"tool_call","call_id":"b","name":"read","summary":"second"}),
            json!({"event":"tool_output","call_id":"a","name":"read","delta":"partial a"}),
            json!({"event":"tool_output","call_id":"b","name":"read","delta":"partial b"}),
            json!({"event":"tool_result","call_id":"b","name":"read","text":"second result","is_error":true}),
            json!({"event":"tool_result","call_id":"a","name":"read","text":"first result","data":{"answer":1}}),
        ]);
        assert_eq!(updates.len(), 6);
        let a = &updates[0]["toolCallId"];
        let b = &updates[1]["toolCallId"];
        assert_ne!(a, b);
        for index in [2, 5] {
            assert_eq!(&updates[index]["toolCallId"], a);
        }
        for index in [3, 4] {
            assert_eq!(&updates[index]["toolCallId"], b);
        }
        assert_eq!(updates[2]["content"][0]["content"]["text"], "partial a");
        assert_eq!(updates[3]["content"][0]["content"]["text"], "partial b");
        assert_eq!(updates[4]["status"], "failed");
        assert_eq!(updates[5]["status"], "completed");
        assert_eq!(updates[5]["rawOutput"], json!({"answer":1}));
    }

    #[test]
    fn approval_and_legacy_output_follow_the_call_and_session_lifetime() {
        let project = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        let layout = Layout::with_user_dir(project.path(), home.path().to_path_buf()).unwrap();
        let mut adapter = Adapter::new(layout);
        adapter.emit(json!({"event":"active_session","session_id":"s1"}));
        let approval = adapter.emit(json!({"event":"approval","approval_id":"p1","tool":"bash"}));
        let id = approval[0]["params"]["toolCall"]["toolCallId"].clone();
        let call = adapter.emit(json!({"event":"tool_call","call_id":"a","name":"bash"}));
        assert_eq!(call[0]["params"]["update"]["toolCallId"], id);
        let partial = adapter.emit(json!({"event":"tool_output","name":"bash","delta":"output"}));
        assert_eq!(partial[0]["params"]["update"]["toolCallId"], id);
        assert!(
            adapter
                .emit(json!({"event":"tool_result","call_id":"unknown","name":"bash"}))
                .is_empty()
        );
        let result =
            adapter.emit(json!({"event":"tool_result","call_id":"a","name":"bash","text":"done"}));
        assert_eq!(result[0]["params"]["update"]["toolCallId"], id);
        assert!(adapter.tools.is_empty());
        adapter.emit(json!({"event":"tool_call","call_id":"old","name":"read"}));
        adapter.emit(json!({"event":"active_session","session_id":"s2"}));
        assert!(adapter.tools.is_empty());
        assert!(
            adapter
                .emit(json!({"event":"tool_result","call_id":"old","name":"read"}))
                .is_empty()
        );
    }

    #[test]
    fn typed_notice_keeps_text_chunk_and_carries_dial_source_meta() {
        let event = crate::rpc::typed_notice(
            &UiEvent::new(EventKind::Compaction, "History compacted").with_tokens(900, 120),
        );
        let updates = session_updates(vec![event]);
        assert_eq!(updates.len(), 1);
        assert_eq!(updates[0]["sessionUpdate"], "agent_message_chunk");
        assert_eq!(
            updates[0]["content"],
            json!({"type":"text","text":"History compacted"})
        );
        assert_eq!(
            updates[0]["_meta"]["dial"],
            json!({"event":"notice","kind":"compaction","lane":"divider","tokens":[900,120]})
        );
    }

    #[test]
    fn plain_notice_background_and_fatal_omit_absent_fields() {
        let updates = session_updates(vec![
            json!({"event":"notice","message":"plain"}),
            json!({"event":"background","message":"started"}),
            json!({"event":"background","name":"job","message":"partial","replace":true}),
            json!({"event":"background","name":"job","message":"append","replace":false}),
            json!({"event":"fatal","message":"boom"}),
        ]);
        assert_eq!(updates.len(), 5);
        assert_eq!(updates[0]["content"]["text"], "plain");
        assert_eq!(updates[0]["_meta"]["dial"], json!({"event":"notice"}));
        assert_eq!(updates[1]["_meta"]["dial"], json!({"event":"background"}));
        assert_eq!(
            updates[2]["_meta"]["dial"],
            json!({"event":"background","name":"job","replace":true})
        );
        assert_eq!(
            updates[3]["_meta"]["dial"],
            json!({"event":"background","name":"job","replace":false})
        );
        assert_eq!(updates[4]["content"]["text"], "boom");
        assert_eq!(updates[4]["_meta"]["dial"], json!({"event":"fatal"}));
    }
    #[test]
    fn cancellation_cannot_steal_a_prompt_response_and_notifications_have_no_reply() {
        let project = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        let layout = Layout::with_user_dir(project.path(), home.path().to_path_buf()).unwrap();
        let mut adapter = Adapter::new(layout);
        adapter.initialized = true;
        adapter.sessions.extend(["s1".into(), "s2".into()]);
        adapter.active = Some("s1".into());
        let (prompt, replies) = adapter.receive(json!({"jsonrpc":"2.0","id":1,
            "method":"session/prompt","params":{"sessionId":"s1","prompt":[{"type":"text","text":"hello"}]}}));
        assert_eq!(prompt.unwrap()["method"], "send");
        assert!(replies.is_empty());
        adapter
            .permissions
            .insert("permission".into(), "approval".into());
        let (request, replies) = adapter.receive(json!({"jsonrpc":"2.0","id":1,
            "method":"session/cancel","params":{"sessionId":"s1"}}));
        assert!(request.is_none());
        assert!(replies[0].get("error").is_some());
        assert_eq!(adapter.pending["1"].method, "session/prompt");
        assert_eq!(adapter.permissions.len(), 1);

        let (request, replies) = adapter.receive(json!({"jsonrpc":"2.0","id":2,
            "method":"session/cancel","params":{"sessionId":"s1"}}));
        assert_eq!(request.unwrap()["method"], "abort");
        assert!(replies.is_empty());
        assert!(adapter.permissions.is_empty());
        assert_eq!(
            adapter.emit(json!({"id":2,"result":{"requested":true}})),
            vec![response(&json!(2), json!({}))]
        );

        let (request, replies) = adapter.receive(json!({"jsonrpc":"2.0",
            "method":"session/cancel","params":{"sessionId":"s1"}}));
        assert_eq!(request.unwrap()["method"], "abort");
        assert!(replies.is_empty());
        assert!(
            adapter
                .emit(json!({"id":null,"result":{"requested":true}}))
                .is_empty()
        );
        let prompt_reply = adapter.emit(json!({"id":1,"result":{"interrupted":true}}));
        assert_eq!(
            prompt_reply.last().unwrap(),
            &response(&json!(1), json!({"stopReason":"cancelled"}))
        );
        assert!(adapter.pending.is_empty());

        let (request, replies) = adapter.receive(json!({"jsonrpc":"2.0","id":3,
            "method":"session/cancel","params":{"sessionId":"s2"}}));
        assert!(request.is_none());
        assert_eq!(replies, vec![response(&json!(3), json!({}))]);
    }

    #[test]
    fn prompt_stop_reasons_distinguish_outcomes() {
        assert_eq!(
            prompt_stop(&json!({"outcome":"completed","text":"done"})),
            json!({"stopReason":"end_turn"})
        );
        assert_eq!(
            prompt_stop(&json!({"interrupted":true,"outcome":"interrupted"})),
            json!({"stopReason":"cancelled"})
        );
        for outcome in ["failed", "blocked", "needs_input", "incomplete"] {
            let stop = prompt_stop(&json!({"outcome":outcome,"text":"report"}));
            assert_eq!(stop["stopReason"], "end_turn", "{outcome}");
            assert_eq!(stop["_meta"]["dial"]["outcome"], outcome);
            assert_eq!(stop["_meta"]["dial"]["report"], "report");
        }
        let limited =
            prompt_stop(&json!({"outcome":"incomplete","step_limit":true,"text":"Stopped"}));
        assert_eq!(limited["stopReason"], "max_turn_requests");
        assert_eq!(limited["_meta"]["dial"]["outcome"], "incomplete");
    }

    #[test]
    fn plan_entries_map_every_todo_status_without_silent_pending() {
        let entry =
            |status: &str| plan_entry(&json!({"content":"task","status":status,"note":"why"}));
        assert_eq!(entry("pending")["status"], "pending");
        assert_eq!(entry("in_progress")["status"], "in_progress");
        assert_eq!(entry("completed")["status"], "completed");
        assert_eq!(entry("completed")["_meta"], Value::Null);
        assert_eq!(entry("abandoned")["status"], "completed");
        assert_eq!(
            entry("abandoned")["_meta"]["dial"],
            json!({"status":"abandoned","note":"why"})
        );
        assert_eq!(entry("blocked")["status"], "pending");
        assert_eq!(
            entry("blocked")["_meta"]["dial"],
            json!({"status":"blocked","note":"why"})
        );
    }

    #[test]
    fn approvals_attach_by_call_id_and_expire_when_decided() {
        let project = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        let layout = Layout::with_user_dir(project.path(), home.path().to_path_buf()).unwrap();
        let mut adapter = Adapter::new(layout);
        adapter.emit(json!({"event":"active_session","session_id":"s1"}));
        adapter.emit(json!({"event":"tool_call","call_id":"a","name":"bash"}));
        adapter.emit(json!({"event":"tool_call","call_id":"b","name":"bash"}));
        // Same-name calls: the approval for call "a" attaches to its own card, not the latest one.
        let attached = adapter
            .emit(json!({"event":"approval","approval_id":"p1","tool":"bash","call_id":"a"}));
        assert_eq!(
            attached[0]["params"]["toolCall"]["toolCallId"],
            "dial-tool-1"
        );
        // An approval without a call card gets a placeholder, which a same-name call with another
        // call ID must not adopt, and which the decision removes.
        let orphan = adapter
            .emit(json!({"event":"approval","approval_id":"p2","tool":"bash","call_id":"zz"}));
        assert_eq!(orphan[0]["params"]["toolCall"]["toolCallId"], "p2");
        let call = adapter.emit(json!({"event":"tool_call","call_id":"c","name":"bash"}));
        assert_eq!(call[0]["params"]["update"]["toolCallId"], "dial-tool-3");
        assert_eq!(adapter.tools.len(), 4);
        adapter.emit(json!({"event":"approval_resolved","approval_id":"p2"}));
        assert_eq!(adapter.tools.len(), 3);
        assert!(
            !adapter
                .permissions
                .contains_key(&key(&json!("dial-permission-p2")))
        );
    }
}
