//! REST and WebSocket I/O for GitLab Duo Agent workflows. Wire shapes live in
//! `wire.rs`; this module only moves bytes and enforces timeouts/interrupts.

use std::net::{TcpStream, ToSocketAddrs};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail, ensure};
use serde_json::{Value, json};
use tungstenite::client::IntoClientRequest;
use tungstenite::stream::MaybeTlsStream;
use tungstenite::{Message, WebSocket};

use crate::llm::errors::{Classified, ErrorClass};
use crate::llm::{Interrupted, host_of, is_local_host};

use super::wire;

/// Socket read slice; each slice re-checks the interrupt flag.
const POLL: Duration = Duration::from_millis(100);
/// Silence longer than this ends the turn as a transient failure.
pub(super) const IDLE_TIMEOUT: Duration = Duration::from_secs(90);
/// Best-effort stop calls never wait longer than this.
pub(super) const STOP_TIMEOUT_SECS: u64 = 5;
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(30);

pub(super) type Socket = WebSocket<MaybeTlsStream<TcpStream>>;

/// A GitLab Duo project the workflow runs against.
#[derive(Debug)]
pub(super) struct Project {
    pub(super) id: String,
    pub(super) namespace_id: String,
    pub(super) root_namespace_id: String,
}

/// Credentials and endpoint shared by every request in one exchange.
pub(super) struct Api<'a> {
    pub(super) base: &'a str,
    pub(super) token: &'a str,
    pub(super) interrupt: &'a Arc<AtomicBool>,
    pub(super) timeout: u64,
}

/// Direct-access bearer for the workflow socket.
pub(super) struct Access {
    pub(super) token: String,
    pub(super) headers: Vec<(String, String)>,
}

impl Api<'_> {
    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base.trim_end_matches('/'))
    }

    fn agent(&self, url: &str, interrupt: Option<Arc<AtomicBool>>, timeout: u64) -> ureq::Agent {
        let local = host_of(url).is_some_and(|host| is_local_host(&host));
        crate::llm::agent_with(timeout, !local, interrupt, None)
    }

    /// Resolve the workflow project: the explicit path when given, otherwise the
    /// first project the token can develop against (Developer+).
    pub(super) fn project_for(&self, explicit: Option<&str>) -> Result<Project> {
        if let Some(path) = explicit {
            let data = self.get_json(
                &format!("/api/v4/projects/{}", encode_component(path)),
                None,
            )?;
            return self.project_from(&data);
        }
        let query = "membership=true&include_subgroups=true&per_page=1&min_access_level=30\
                     &order_by=last_activity_at&sort=desc";
        let data = self.get_json(&format!("/api/v4/projects?{query}"), None)?;
        let project = data
            .as_array()
            .and_then(|list| list.first())
            .context("GitLab returned no project with Developer access for this token")?;
        self.project_from(project)
    }

    /// Read ids from a project response. A nested project's root namespace is the
    /// top-level group named by the namespace's `full_path`.
    fn project_from(&self, project: &Value) -> Result<Project> {
        let id = id_text(project.get("id")).context("GitLab project response omitted id")?;
        let namespace_id = id_text(project.pointer("/namespace/id"))
            .context("GitLab project response omitted namespace.id")?;
        let top_group = project
            .pointer("/namespace/full_path")
            .and_then(Value::as_str)
            .and_then(|path| path.split_once('/'))
            .map(|(top, _)| top.to_owned());
        let root_group_id = match top_group {
            Some(top) => {
                let data =
                    self.get_json(&format!("/api/v4/groups/{}", encode_component(&top)), None)?;
                id_text(data.get("id")).context("GitLab group response omitted id")?
            }
            None => namespace_id.clone(),
        };
        Ok(Project {
            id,
            namespace_id,
            root_namespace_id: root_namespace(&root_group_id),
        })
    }

    /// Exchange the user token for a workflow-scoped bearer.
    pub(super) fn direct_access(
        &self,
        root_namespace_id: &str,
        project_id: &str,
    ) -> Result<Access> {
        let body = json!({
            "workflow_definition": wire::WORKFLOW_DEFINITION,
            "root_namespace_id": root_namespace_id,
            "project_id": project_id,
        });
        let data = self.post_json("/api/v4/ai/duo_workflows/direct_access", &body)?;
        let token = data["token"]
            .as_str()
            .or_else(|| data.pointer("/gitlab_rails/token").and_then(Value::as_str))
            .filter(|token| !token.is_empty())
            .context("GitLab direct-access response omitted token")?
            .to_owned();
        let headers = data
            .pointer("/duo_workflow_service/headers")
            .and_then(Value::as_object)
            .into_iter()
            .flatten()
            .filter_map(|(name, value)| value.as_str().map(|v| (name.clone(), v.to_owned())))
            .collect();
        Ok(Access { token, headers })
    }

    /// Create a workflow record; returns its id.
    pub(super) fn create_workflow(&self, project_id: &str, goal: &str) -> Result<String> {
        let body = json!({
            "workflow_definition": wire::WORKFLOW_DEFINITION,
            "environment": "ide",
            "allow_agent_to_request_user": false,
            "agent_privileges": [6],
            "pre_approved_agent_privileges": [6],
            "requires_duo_cli_enabled": false,
            "project_id": project_id,
            "goal": goal,
        });
        let data = self.post_json("/api/v4/ai/duo_workflows/workflows", &body)?;
        ["id", "workflow_id", "workflowId"]
            .iter()
            .find_map(|key| id_text(data.get(*key)))
            .context("GitLab workflow create response omitted id")
    }

    /// Best-effort stop. Errors are ignored: the workflow is abandoned either way.
    pub(super) fn stop_workflow(&self, workflow_id: &str) {
        let url = self.url(&format!("/api/v4/ai/duo_workflows/workflows/{workflow_id}"));
        let Ok(bytes) = serde_json::to_vec(&json!({ "status_event": "stop" })) else {
            return;
        };
        let _ = self
            .agent(&url, None, self.timeout.min(STOP_TIMEOUT_SECS))
            .patch(&url)
            .header("authorization", &format!("Bearer {}", self.token))
            .header("content-type", "application/json")
            .send(&bytes);
    }

    fn get_json(&self, path: &str, interrupt: Option<Arc<AtomicBool>>) -> Result<Value> {
        let url = self.url(path);
        let response = self
            .agent(&url, interrupt, self.timeout)
            .get(&url)
            .header("authorization", &format!("Bearer {}", self.token))
            .call()
            .map_err(|error| crate::llm::transport_error(error, &url, Some(self.token)))?;
        read_json(response, self.token)
    }

    fn post_json(&self, path: &str, body: &Value) -> Result<Value> {
        let url = self.url(path);
        let response = crate::llm::send_with_headers(
            &url,
            Some(self.token),
            body,
            self.timeout,
            None,
            Some(Arc::clone(self.interrupt)),
            &[],
        )?;
        read_json(response, self.token)
    }
}

/// Map a numeric or string id to text.
fn id_text(value: Option<&Value>) -> Option<String> {
    match value? {
        Value::String(text) if !text.is_empty() => Some(text.clone()),
        Value::Number(number) => Some(number.to_string()),
        _ => None,
    }
}

/// Read a JSON body or surface the bounded HTTP error.
fn read_json(mut response: ureq::http::Response<ureq::Body>, token: &str) -> Result<Value> {
    let status = response.status().as_u16();
    let delay = crate::llm::retry::server_delay(response.headers());
    let limit = if (200..300).contains(&status) {
        crate::llm::MAX_RESPONSE_BYTES
    } else {
        crate::llm::ERROR_EXCERPT_BYTES
    };
    let bytes = crate::llm::read_bounded(response.body_mut().as_reader(), limit)?;
    if !(200..300).contains(&status) {
        return Err(crate::llm::http_error(status, &bytes, Some(token), delay));
    }
    ensure!(
        !bytes.truncated,
        "GitLab Duo Agent REST response exceeded the configured limit"
    );
    serde_json::from_slice(&bytes.data).context("GitLab Duo Agent returned invalid JSON")
}

/// Open the workflow socket and send the start request.
pub(super) fn connect(
    api: &Api<'_>,
    access: &Access,
    project: &Project,
    model: &str,
) -> Result<Socket> {
    let base = url::Url::parse(api.base).context("invalid GitLab base URL")?;
    let mut url = base.clone();
    url.set_path("/api/v4/ai/duo_workflows/ws");
    url.set_scheme(if base.scheme() == "https" {
        "wss"
    } else {
        "ws"
    })
    .map_err(|_| anyhow!("invalid GitLab WebSocket scheme"))?;
    let root = root_namespace(&project.namespace_id);
    url.query_pairs_mut()
        .append_pair("project_id", &project.id)
        .append_pair("namespace_id", &project.namespace_id)
        .append_pair("root_namespace_id", &root)
        .append_pair("user_selected_model_identifier", model)
        .append_pair("workflow_definition", wire::WORKFLOW_DEFINITION);
    let mut request = url.as_str().into_client_request()?;
    let headers = request.headers_mut();
    headers.insert("authorization", format!("Bearer {}", access.token).parse()?);
    headers.insert("x-gitlab-client-type", wire::CLIENT_TYPE.parse()?);
    headers.insert(
        "x-gitlab-language-server-version",
        wire::LANGUAGE_SERVER_VERSION.parse()?,
    );
    headers.insert(
        "user-agent",
        format!("dial/{}", env!("CARGO_PKG_VERSION")).parse()?,
    );
    headers.insert("origin", base.origin().ascii_serialization().parse()?);
    headers.insert("x-gitlab-project-id", project.id.parse()?);
    headers.insert("x-gitlab-namespace-id", project.namespace_id.parse()?);
    headers.insert("x-gitlab-root-namespace-id", root.parse()?);
    for (name, value) in &access.headers {
        let name = tungstenite::http::HeaderName::from_bytes(name.as_bytes())?;
        headers.insert(name, value.parse()?);
    }
    let tcp = open_tcp(&base)?;
    tcp.set_read_timeout(Some(HANDSHAKE_TIMEOUT))?;
    tcp.set_write_timeout(Some(HANDSHAKE_TIMEOUT))?;
    tcp.set_nodelay(true)?;
    let (mut socket, _) = tungstenite::client_tls_with_config(request, tcp, None, None).map_err(
        |error| match error {
            tungstenite::handshake::HandshakeError::Failure(error) => anyhow::Error::from(error),
            tungstenite::handshake::HandshakeError::Interrupted(_) => {
                anyhow!("GitLab Duo Agent WebSocket handshake interrupted")
            }
        },
    )?;

    set_poll(&mut socket)?;
    Ok(socket)
}

/// GraphQL group id form of a numeric namespace id, as the server expects.
pub(super) fn root_namespace(namespace_id: &str) -> String {
    format!("gid://gitlab/Group/{namespace_id}")
}

/// Percent-encode one path segment. `/` becomes `%2F`, as GitLab expects for
/// full project and group paths.
fn encode_component(text: &str) -> String {
    text.bytes()
        .map(|byte| match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                char::from(byte).to_string()
            }
            _ => format!("%{byte:02X}"),
        })
        .collect()
}

fn open_tcp(base: &url::Url) -> Result<TcpStream> {
    let host = base.host_str().context("GitLab base URL has no host")?;
    let host = host.trim_start_matches('[').trim_end_matches(']');
    let port = base
        .port_or_known_default()
        .context("GitLab base URL has no port")?;
    let mut last = None;
    for addr in (host, port).to_socket_addrs()? {
        match TcpStream::connect_timeout(&addr, HANDSHAKE_TIMEOUT) {
            Ok(stream) => return Ok(stream),
            Err(error) => last = Some(error),
        }
    }
    match last {
        Some(error) => Err(error.into()),
        None => bail!("GitLab host resolved to no addresses"),
    }
}

fn set_poll(socket: &mut Socket) -> Result<()> {
    let tcp = match socket.get_mut() {
        MaybeTlsStream::Plain(tcp) => tcp,
        MaybeTlsStream::Rustls(tls) => &mut tls.sock,
        _ => bail!("unsupported GitLab Duo Agent TLS transport"),
    };
    tcp.set_read_timeout(Some(POLL))?;
    tcp.set_write_timeout(Some(HANDSHAKE_TIMEOUT))?;
    Ok(())
}

/// Send one JSON message on the socket.
pub(super) fn send(socket: &mut Socket, payload: &Value) -> Result<()> {
    socket
        .send(Message::Text(payload.to_string().into()))
        .context("GitLab Duo Agent WebSocket send failed")
}

/// Wait for the next JSON server event. Honours the interrupt flag every poll
/// slice and fails after `IDLE_TIMEOUT` of silence.
pub(super) fn next_event(socket: &mut Socket, interrupt: &AtomicBool) -> Result<Value> {
    let mut idle_since = Instant::now();
    loop {
        if interrupt.load(Ordering::SeqCst) {
            return Err(Interrupted.into());
        }
        if idle_since.elapsed() > IDLE_TIMEOUT {
            return Err(Classified::new(
                ErrorClass::Transient,
                "GitLab Duo Agent workflow went silent",
            )
            .into());
        }
        match socket.read() {
            Ok(Message::Text(text)) => {
                if let Ok(event) = serde_json::from_str::<Value>(text.as_str()) {
                    return Ok(event);
                }
                idle_since = Instant::now();
            }
            Ok(Message::Close(_)) => bail!("GitLab Duo Agent closed the workflow socket"),
            Ok(_) => idle_since = Instant::now(),
            Err(tungstenite::Error::Io(error))
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) => {}
            Err(error) => return Err(error).context("GitLab Duo Agent WebSocket read failed"),
        }
    }
}
