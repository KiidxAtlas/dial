//! `/mcp smithery-*` slash commands (`/mcp smithery-search|login|logout`, plus `smithery-install`).
//! Registry logic lives in `tools::mcp::smithery`; this module drives the prompts and config writes.

use super::extensions::{spawn_job, spawn_job_streaming};
use crate::agent::{Agent, Ui};
use crate::app::{App, Bridge, PAINT_EVERY, Shell, Step};
use crate::llm::oauth::util::open_browser;
use crate::storage::settings_store::Scope;
use crate::tools::mcp::smithery::{self, SearchResult};
use anyhow::{Result, anyhow, bail};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Instant;

/// Picker labels longer than this are cut with `...` so the list stays readable.
const LABEL_MAX: usize = 120;

pub(super) fn run(
    shell: &mut Shell,
    app: &App,
    _agent: &mut Agent<'_>,
    name: &str,
    args: &str,
) -> Option<Result<Step>> {
    if name != "mcp" {
        return None;
    }
    let args = args.trim();
    let (sub, rest) = match args.split_once(char::is_whitespace) {
        Some((head, tail)) => (head, tail.trim()),
        None => (args, ""),
    };
    let result = match sub {
        "smithery-search" => search(shell, app, &Resume::new(Op::Search, rest)),
        "smithery-install" => install(shell, app, &Resume::new(Op::Install, rest)),
        "smithery-login" => login(shell, app, rest),
        "smithery-logout" => logout(shell, app),
        _ => return None,
    };
    Some(result)
}

fn user_dir(app: &App) -> &Path {
    Path::new(&app.config.user_dir)
}

fn key_file_hint(app: &App) -> String {
    smithery::key_file(user_dir(app))
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| "smithery.json".to_owned())
}

fn scope_label(scope: &Scope) -> &'static str {
    match scope {
        Scope::Global => "user",
        Scope::Project => "project",
    }
}

/// Runs `f` against a `Ui` bridge over the live shell, the same way `/cleanse` drives its prompts.
fn with_ui<T>(shell: &mut Shell, app: &App, f: impl FnOnce(&mut Bridge<'_>) -> T) -> T {
    let mut bridge = Bridge {
        app,
        shell: &mut *shell,
        interrupt: app.interrupt.clone(),
        started: Instant::now(),
        last_paint: Instant::now() - PAINT_EVERY,
        failed: false,
        pending: Vec::new(),
        next_id: 0,
    };
    bridge.repaint(true);
    f(&mut bridge)
}

/// Picks one of `options` and returns its index; `None` when the user cancelled.
fn pick(
    shell: &mut Shell,
    app: &App,
    title: &str,
    question: &str,
    options: &[String],
) -> Option<usize> {
    let answer = with_ui(shell, app, |ui| ui.ask_user(title, question, options))?;
    options.iter().position(|option| *option == answer)
}

/// Free-text prompt; `None` when the user cancelled.
fn prompt(shell: &mut Shell, app: &App, title: &str, question: &str) -> Option<String> {
    with_ui(shell, app, |ui| ui.ask_user(title, question, &[]))
}

/// A Smithery command that can be re-run once a login finishes.
#[derive(Debug, Clone, Copy)]
enum Op {
    Search,
    Install,
}

impl Op {
    /// Why a login is requested when no key is cached yet.
    fn missing_reason(self) -> &'static str {
        match self {
            Self::Search => "required for smithery-search",
            Self::Install => "required for smithery-install",
        }
    }
}

/// An operation parked while a background job runs. `resume` picks it up when that job finishes.
/// `may_reauth` says whether that run may prompt for login again on 401/403/429; a run retries
/// once, so a run resumed after a re-auth may not re-auth again.
#[derive(Debug, Clone)]
pub(crate) struct Resume {
    op: Op,
    args: String,
    may_reauth: bool,
    /// Slot the registry job fills; `None` when no registry call was started.
    fetched: Option<Slot>,
}

type Slot = Arc<Mutex<Option<Fetched>>>;

impl Resume {
    fn new(op: Op, args: &str) -> Self {
        Self {
            op,
            args: args.to_owned(),
            may_reauth: true,
            fetched: None,
        }
    }

    fn continued(&self, may_reauth: bool) -> Self {
        Self {
            op: self.op,
            args: self.args.clone(),
            may_reauth,
            fetched: None,
        }
    }

    /// The result stored by the finished job, if this operation started one.
    fn take_fetched(&mut self) -> Option<Fetched> {
        let slot = self.fetched.take()?;
        let mut stored = slot.lock().ok()?;
        stored.take()
    }
}

/// A registry call that runs on a background job. It never touches the UI.
enum Request {
    Search {
        keyword: String,
        limit: usize,
        semantic: bool,
    },
    Details {
        path: String,
    },
}

impl Request {
    fn run(self, user_dir: &Path, reason: &str, may_reauth: bool) -> Fetched {
        match self {
            Self::Search {
                keyword,
                limit,
                semantic,
            } => Fetched::Search(registry_call(user_dir, reason, may_reauth, |api_key| {
                let opts = smithery::SearchOptions {
                    limit,
                    api_key: api_key.map(str::to_owned),
                    semantic,
                };
                smithery::search(&keyword, &opts)
            })),
            Self::Details { path } => {
                Fetched::Details(registry_call(user_dir, reason, may_reauth, |api_key| {
                    smithery::fetch_details(&path, api_key)
                }))
            }
        }
    }
}

/// What a registry call produced, as the UI thread needs it.
#[derive(Debug)]
enum Outcome<T> {
    Done(T),
    /// The call needs a login first; the UI thread starts it.
    Login {
        reason: String,
        may_reauth: bool,
    },
    Failed(smithery::SmitheryError),
}

/// A finished registry call, stored by its job for `resume`.
#[derive(Debug)]
enum Fetched {
    Search(Outcome<Vec<SearchResult>>),
    Details(Outcome<Option<Value>>),
}

/// Runs a registry call under the login and re-auth policy without prompting. A login the call
/// would need comes back as `Outcome::Login` instead of being started here.
fn registry_call<T>(
    user_dir: &Path,
    reason: &str,
    may_reauth: bool,
    op: impl FnMut(Option<&str>) -> Result<T, smithery::SmitheryError>,
) -> Outcome<T> {
    let mut wanted = None;
    let result = smithery::run_with_reauth(user_dir, reason, may_reauth, op, |why, again| {
        wanted = Some((why.to_owned(), again));
        Ok(smithery::Reauth::Pending)
    });
    match (result, wanted) {
        (Ok(Some(value)), _) => Outcome::Done(value),
        (Ok(None), Some((reason, may_reauth))) => Outcome::Login { reason, may_reauth },
        (Ok(None), None) => Outcome::Failed(smithery::SmitheryError {
            message: "Smithery login state was lost.".to_owned(),
            status: 0,
        }),
        (Err(error), _) => Outcome::Failed(error),
    }
}

/// Starts `request` as the background job for `parked`. The job stores its result in the parked
/// slot, and its completion re-enters through `resume`.
fn spawn_fetch(shell: &mut Shell, app: &App, mut parked: Resume, request: Request) {
    if shell.auth_job.is_some() {
        shell.note(
            "MCP",
            "Another background operation is still running; try again when it finishes.",
        );
        return;
    }
    let slot: Slot = Arc::new(Mutex::new(None));
    parked.fetched = Some(slot.clone());
    let user_dir = user_dir(app).to_path_buf();
    let reason = parked.op.missing_reason();
    let may_reauth = parked.may_reauth;
    shell.smithery_resume = Some(parked);
    spawn_job(shell, "Smithery", move |_cancel| {
        let fetched = request.run(&user_dir, reason, may_reauth);
        let mut stored = slot
            .lock()
            .map_err(|_| anyhow!("Smithery result slot is poisoned"))?;
        *stored = Some(fetched);
        Ok("Smithery registry request finished.".to_owned())
    });
}

/// Maps a registry failure to the user-facing message for `action`.
fn registry_error(app: &App, error: &smithery::SmitheryError, action: &str) -> anyhow::Error {
    match error.status {
        401 | 403 => anyhow!(
            "Smithery authentication required. Run /mcp smithery-login or add an API key to {}.",
            key_file_hint(app)
        ),
        429 => anyhow!(
            "Smithery {action} failed: rate limited by Smithery (429). Wait a moment and try again."
        ),
        _ => anyhow!("Smithery {action} failed: {}", error.message),
    }
}

/// `/mcp smithery-search <keyword>`: starts the registry search. The picker and deploy follow in
/// `finish`, once the job has stored its result.
fn search(shell: &mut Shell, app: &App, parked: &Resume) -> Result<Step> {
    let parsed = smithery::parse_search(&parked.args)?;
    let request = Request::Search {
        keyword: parsed.keyword,
        limit: parsed.limit,
        semantic: parsed.semantic,
    };
    spawn_fetch(shell, app, parked.clone(), request);
    Ok(Step::Done)
}

/// `/mcp smithery-install <@namespace/server|keyword>`: a qualified name starts a details lookup, a
/// keyword starts a registry search. `finish` deploys the result.
fn install(shell: &mut Shell, app: &App, parked: &Resume) -> Result<Step> {
    let parsed = smithery::parse_install(&parked.args)?;
    let request = if parsed.name.contains('/') {
        let qualified = smithery::normalize_qualified_name(&parsed.name);
        Request::Details {
            path: qualified.trim_start_matches('@').to_owned(),
        }
    } else {
        Request::Search {
            keyword: parsed.name,
            limit: smithery::DEFAULT_LIMIT,
            semantic: false,
        }
    };
    spawn_fetch(shell, app, parked.clone(), request);
    Ok(Step::Done)
}

/// Continues a parked operation with the result its job stored.
fn finish(
    shell: &mut Shell,
    app: &App,
    agent: &mut Agent<'_>,
    parked: &Resume,
    fetched: Fetched,
) -> Result<Step> {
    match (parked.op, fetched) {
        (Op::Search, Fetched::Search(outcome)) => {
            let parsed = smithery::parse_search(&parked.args)?;
            let Some(results) = settle(shell, app, parked, outcome, "search")? else {
                return Ok(Step::Done);
            };
            choose_and_deploy(
                shell,
                app,
                agent,
                &parsed.keyword,
                results,
                parsed.scope,
                &BTreeMap::new(),
            )
        }
        (Op::Install, Fetched::Search(outcome)) => {
            let parsed = smithery::parse_install(&parked.args)?;
            let Some(results) = settle(shell, app, parked, outcome, "search")? else {
                return Ok(Step::Done);
            };
            choose_and_deploy(
                shell,
                app,
                agent,
                &parsed.name,
                results,
                parsed.scope,
                &parsed.sets,
            )
        }
        (Op::Install, Fetched::Details(outcome)) => {
            let parsed = smithery::parse_install(&parked.args)?;
            let qualified = smithery::normalize_qualified_name(&parsed.name);
            let Some(found) = settle(shell, app, parked, outcome, "install")? else {
                return Ok(Step::Done);
            };
            let Some(details) = found else {
                bail!("No Smithery server found for {qualified}.");
            };
            let entry = json!({"id": qualified, "qualifiedName": qualified});
            let result = smithery::to_search_result(&entry, &details)
                .ok_or_else(|| anyhow!("Smithery server {qualified} has no usable connection."))?;
            deploy(shell, app, agent, &result, parsed.scope, &parsed.sets)
        }
        _ => bail!("Smithery result does not match the parked operation."),
    }
}

/// Turns a registry outcome into its value. A login the call needs is started, and the parked
/// operation waits for it.
fn settle<T>(
    shell: &mut Shell,
    app: &App,
    parked: &Resume,
    outcome: Outcome<T>,
    action: &str,
) -> Result<Option<T>> {
    match outcome {
        Outcome::Done(value) => Ok(Some(value)),
        Outcome::Login { reason, may_reauth } => {
            start_login(shell, app, &reason, Some(parked.continued(may_reauth)))?;
            Ok(None)
        }
        Outcome::Failed(error) => Err(registry_error(app, &error, action)),
    }
}

/// Shows the results as a picker and deploys the chosen one.
fn choose_and_deploy(
    shell: &mut Shell,
    app: &App,
    agent: &mut Agent<'_>,
    keyword: &str,
    results: Vec<SearchResult>,
    scope: Scope,
    sets: &BTreeMap<String, String>,
) -> Result<Step> {
    if results.is_empty() {
        shell.note(
            "MCP",
            format!("No Smithery results found for \"{keyword}\"."),
        );
        return Ok(Step::Done);
    }
    let labels: Vec<String> = results
        .iter()
        .enumerate()
        .map(|(i, r)| {
            let label = format!(
                "{}. {} ({}, uses {})",
                i + 1,
                r.display_name,
                r.transport,
                r.use_count
            );
            if label.chars().count() > LABEL_MAX {
                let cut: String = label.chars().take(LABEL_MAX - 3).collect();
                format!("{cut}...")
            } else {
                label
            }
        })
        .collect();
    let title = format!("Registry results for \"{keyword}\"");
    let Some(index) = pick(shell, app, &title, "Select a server to deploy.", &labels) else {
        shell.note("MCP", "MCP Smithery selection cancelled.");
        return Ok(Step::Done);
    };
    deploy(shell, app, agent, &results[index], scope, sets)
}

/// Prompts for a server name and the schema inputs not given by `sets`, then writes the server config.
fn deploy(
    shell: &mut Shell,
    app: &App,
    agent: &mut Agent<'_>,
    result: &SearchResult,
    scope: Scope,
    sets: &BTreeMap<String, String>,
) -> Result<Step> {
    let base = smithery::to_config_name(&result.qualified_name);
    let default_name =
        smithery::unique_name(&base, |candidate| app.config.mcp.contains_key(candidate));
    let name = loop {
        let question = format!("Server name for deploy (default: {default_name})");
        let Some(answer) = prompt(shell, app, "MCP deploy", &question) else {
            shell.note("MCP", "MCP deploy cancelled.");
            return Ok(Step::Done);
        };
        let answer = answer.trim();
        let candidate = if answer.is_empty() {
            default_name.clone()
        } else {
            answer.to_owned()
        };
        if app.config.mcp.contains_key(&candidate) {
            shell.note(
                "MCP",
                format!(
                    "Server \"{candidate}\" already exists in {} config.",
                    scope_label(&scope)
                ),
            );
            continue;
        }
        break candidate;
    };

    let mut values = sets.clone();
    for input in &result.inputs {
        if values.contains_key(&input.key) {
            continue;
        }
        let requirement = if input.required {
            "required"
        } else {
            "optional"
        };
        let label = input
            .key
            .split(['_', '-'])
            .filter(|part| !part.is_empty())
            .collect::<Vec<_>>()
            .join(" ");
        let mut question = format!("{label} ({requirement})");
        if let Some(description) = &input.description {
            question.push_str(&format!(" - {description}"));
        }
        if !input.enum_values.is_empty() {
            question.push_str(&format!(" [one of: {}]", input.enum_values.join(", ")));
        }
        if let Some(default) = &input.default {
            question.push_str(&format!(" [default: {default}]"));
        }
        let Some(answer) = prompt(shell, app, "MCP deploy", &question) else {
            shell.note("MCP", "MCP deploy cancelled.");
            return Ok(Step::Done);
        };
        let answer = answer.trim();
        if answer.is_empty() {
            if input.required && input.default.is_none() {
                shell.note(
                    "MCP",
                    format!("Missing required value for \"{}\".", input.key),
                );
                return Ok(Step::Done);
            }
            continue;
        }
        values.insert(input.key.clone(), answer.to_owned());
    }

    let server = smithery::plan_install(result, &values)?;
    let change = smithery::config_change(&name, &server)?;
    let step = shell.persist(app, agent, &[change], scope)?;
    shell.note(
        "MCP",
        format!("Added MCP server {name} ({} config).", scope_label(&scope)),
    );
    Ok(step)
}

fn login(shell: &mut Shell, app: &App, args: &str) -> Result<Step> {
    let flag_key = smithery::parse_login(args)?;
    if let Some(key) = flag_key {
        let path = smithery::key_file(user_dir(app))?;
        spawn_job(shell, "Smithery", move |_cancel| {
            smithery::validate_key(&key)?;
            smithery::save_key(&path, &key)?;
            Ok("Smithery API key saved.".to_owned())
        });
        return Ok(Step::Done);
    }
    start_login(shell, app, smithery::LOGIN_REASON, None)?;
    Ok(Step::Done)
}

/// Login flow: browser sign-in first, a pasted key when the browser flow cannot run. The session
/// request and the browser wait run in one background job; if that job fails, the event loop calls
/// `browser_failed`.
fn start_login(
    shell: &mut Shell,
    app: &App,
    reason: &str,
    resume: Option<Resume>,
) -> Result<smithery::Reauth> {
    shell.note(
        "MCP",
        format!(
            "Smithery authentication required ({reason}).\nIf browser auth fails, you can paste an API key."
        ),
    );
    if shell.auth_job.is_some() {
        shell.note(
            "MCP",
            "Another background operation is still running; try again when it finishes.",
        );
        return Ok(smithery::Reauth::Declined);
    }
    let path = smithery::key_file(user_dir(app))?;
    let base = smithery::auth_base();
    shell.smithery_resume = resume;
    spawn_job_streaming(shell, "Smithery", move |cancel, progress| {
        let session = smithery::create_auth_session(&base)?;
        if open_browser(&session.auth_url) {
            progress(
                "Opened the Smithery authorization page in your browser.\nWaiting for approval (up to 5 minutes).",
            );
        } else {
            progress(&format!(
                "Open this URL to authorize Smithery: {}\nWaiting for approval (up to 5 minutes).",
                session.auth_url
            ));
        }
        let key = smithery::await_api_key(
            &base,
            &session.session_id,
            smithery::POLL_INTERVAL,
            smithery::AUTH_DEADLINE,
            &cancel,
        )?;
        smithery::validate_key(&key)?;
        smithery::save_key(&path, &key)?;
        Ok("Smithery API key saved.".to_owned())
    });
    Ok(smithery::Reauth::Pending)
}

/// Called by the event loop when a browser sign-in job fails: warns, then asks for a pasted key.
/// A key pasted that way re-runs the operation that was parked behind the login.
pub(crate) fn browser_failed(
    shell: &mut Shell,
    app: &App,
    agent: &mut Agent<'_>,
    error: &anyhow::Error,
) -> Result<Step> {
    match fallback_to_paste(shell, app, error) {
        smithery::Reauth::Saved => resume(shell, app, agent),
        _ => {
            shell.smithery_resume = None;
            Ok(Step::Done)
        }
    }
}

/// Called by the event loop when a background job succeeds: continues the parked operation. A
/// registry result stored by its job is shown or deployed here; a login success re-runs the
/// operation. Failures are noted rather than raised, because this runs from the event loop.
pub(crate) fn resume(shell: &mut Shell, app: &App, agent: &mut Agent<'_>) -> Result<Step> {
    let Some(mut parked) = shell.smithery_resume.take() else {
        return Ok(Step::Done);
    };
    let result = match parked.take_fetched() {
        Some(fetched) => finish(shell, app, agent, &parked, fetched),
        None => match parked.op {
            Op::Search => search(shell, app, &parked),
            Op::Install => install(shell, app, &parked),
        },
    };
    match result {
        Ok(step) => Ok(step),
        Err(error) => {
            shell.note("MCP", format!("{error:#}"));
            Ok(Step::Done)
        }
    }
}

/// Browser-failure fallback: warns, then runs the pasted-key prompt.
fn fallback_to_paste(shell: &mut Shell, app: &App, error: &anyhow::Error) -> smithery::Reauth {
    shell.note(
        "MCP",
        format!("Browser authorization failed: {error:#}. Falling back to API key."),
    );
    match paste_login(shell, app) {
        Ok(reauth) => reauth,
        Err(error) => {
            shell.note("MCP", format!("{error:#}"));
            smithery::Reauth::Declined
        }
    }
}

/// Asks until a key validates, then caches it.
fn paste_login(shell: &mut Shell, app: &App) -> Result<smithery::Reauth> {
    let path = smithery::key_file(user_dir(app))?;
    let saved = {
        let mut ui = PasteUi {
            shell: &mut *shell,
            app,
        };
        smithery::paste_key_login(&path, &mut ui, smithery::validate_key)?
    };
    shell.note(
        "MCP",
        if saved {
            "Smithery API key saved."
        } else {
            "Smithery login cancelled."
        },
    );
    Ok(if saved {
        smithery::Reauth::Saved
    } else {
        smithery::Reauth::Declined
    })
}

/// Terminal side of the pasted-key prompt: asks through the MCP bridge and notes rejected entries.
struct PasteUi<'a> {
    shell: &'a mut Shell,
    app: &'a App,
}

impl smithery::KeyPrompt for PasteUi<'_> {
    fn ask(&mut self, question: &str) -> Option<String> {
        prompt(&mut *self.shell, self.app, "MCP login", question)
    }
    fn error(&mut self, message: &str) {
        self.shell.note("MCP", message);
    }
}

fn logout(shell: &mut Shell, app: &App) -> Result<Step> {
    let path = smithery::key_file(user_dir(app))?;
    let text = if smithery::clear_key(&path)? {
        "Smithery API key removed."
    } else {
        "No cached Smithery API key found."
    };
    shell.note("MCP", text);
    Ok(Step::Done)
}
