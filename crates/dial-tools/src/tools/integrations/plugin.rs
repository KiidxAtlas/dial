//! Runtime lifecycle plugins. Trusted stdio executables receive JSON-RPC requests and may rewrite
//! event payloads. They do not gain direct access to agent execution or approvals.
use crate::config::hooks::canonical_event;

pub mod installer;
pub mod marketplace;
pub mod protocol;
use protocol::{Action, Negotiated, Slot, Suggestion, UiSlot};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    process::Command,
    sync::{
        Arc, OnceLock,
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc::{SyncSender, TrySendError, sync_channel},
    },
    time::Duration,
};

pub trait Plugin: Send + Sync {
    /// Stateful integrations can supply a fresh implementation for a forked execution owner.
    fn fork_session(&self) -> Option<Arc<dyn Plugin>> {
        None
    }
    fn on_event_with_control(
        &self,
        event: &str,
        payload: &Value,
        interrupt: &AtomicBool,
        obsolete: &(dyn Fn() -> bool + Sync),
    ) -> anyhow::Result<Option<Value>> {
        anyhow::ensure!(!obsolete(), "extension owner is obsolete");
        let result = self.on_event(event, payload, interrupt);
        anyhow::ensure!(
            !obsolete(),
            "extension response belongs to an obsolete owner"
        );
        result
    }
    fn name(&self) -> &str;
    fn events(&self) -> &[String];
    /// Slash commands this plugin answers through [`Plugin::call`].
    fn commands(&self) -> &[String] {
        &[]
    }
    /// A plugin-initiated exchange, such as a command that opens an extension panel.
    fn call(&self, method: &str, _params: Value, _interrupt: &AtomicBool) -> anyhow::Result<Value> {
        anyhow::bail!("extension {} does not handle {method}", self.name())
    }
    /// The protocol and capabilities negotiated at `initialize`. Protocol 1 plugins are legacy.
    fn negotiated(&self) -> Negotiated {
        Negotiated::legacy()
    }
    fn on_event(
        &self,
        event: &str,
        payload: &Value,
        interrupt: &AtomicBool,
    ) -> anyhow::Result<Option<Value>>;
    /// Observational delivery: the whole reply, which may carry declared actions and UI contributions.
    fn observe(
        &self,
        event: &str,
        payload: &Value,
        interrupt: &AtomicBool,
        obsolete: &(dyn Fn() -> bool + Sync),
    ) -> anyhow::Result<Value> {
        self.on_event_with_control(event, payload, interrupt, obsolete)
            .map(|rewrite| rewrite.map_or(Value::Null, |rewrite| json!({"rewrite": rewrite})))
    }
}

#[derive(Debug)]
pub struct Answer {
    pub plugin: String,
    pub result: anyhow::Result<Value>,
}

/// Longest slash-command name an extension may declare; longer names are dropped, not truncated.
const MAX_COMMAND_NAME_CHARS: usize = 48;

/// A command name is a short run of printable, non-space characters without a leading slash.
fn valid_command_name(name: &str) -> bool {
    !name.is_empty()
        && name.chars().count() <= MAX_COMMAND_NAME_CHARS
        && !name.starts_with('/')
        && name.chars().all(|c| c.is_ascii_graphic())
}

/// An action a protocol 2 plugin asked for, waiting for its owner to apply it.
#[derive(Debug, Clone, PartialEq)]
pub struct PluginEffect {
    pub plugin: String,
    pub action: Action,
}

/// Capacity of the observational queue. A full queue drops events with a notice rather than
/// blocking the agent loop.
const OBSERVER_QUEUE_CAPACITY: usize = 256;
/// Notices are diagnostics; a drop storm must not grow them without bound.
const MAX_PENDING_NOTICES: usize = 64;

/// One observational event queued for the background worker.
struct EventJob {
    event: String,
    payload: Value,
    plugins: Vec<Arc<dyn Plugin>>,
    generation: u64,
}

fn push_notice(notices: &parking_lot::Mutex<Vec<String>>, notice: String) {
    let mut notices = notices.lock();
    if notices.len() < MAX_PENDING_NOTICES {
        notices.push(notice);
    }
}

/// Applies one reply. UI contributions replace their slot (an empty list clears it); other
/// actions queue as effects. Only protocol 2 plugins may act; refusals become notices.
fn absorb_reply(
    notices: &parking_lot::Mutex<Vec<String>>,
    effects: &parking_lot::Mutex<Vec<PluginEffect>>,
    slots: &parking_lot::Mutex<BTreeMap<(String, Slot), Vec<String>>>,
    plugin: &str,
    negotiated: &Negotiated,
    reply: &Value,
) {
    if negotiated.version < 2 {
        return;
    }
    let (actions, refused) = protocol::parse_actions(negotiated, reply);
    for notice in refused {
        push_notice(notices, format!("plugin {plugin}: {notice}"));
    }
    for action in actions {
        match action {
            Action::Ui(UiSlot { slot, lines }) => {
                let key = (plugin.to_owned(), slot);
                let mut slots = slots.lock();
                if lines.is_empty() {
                    slots.remove(&key);
                } else {
                    slots.insert(key, lines);
                }
            }
            action => effects.lock().push(PluginEffect {
                plugin: plugin.to_owned(),
                action,
            }),
        }
    }
}

#[derive(Default)]
pub struct Host {
    plugins: parking_lot::Mutex<Vec<Arc<dyn Plugin>>>,
    notices: Arc<parking_lot::Mutex<Vec<String>>>,
    generation: Arc<AtomicU64>,
    closed: Arc<AtomicBool>,
    effects: Arc<parking_lot::Mutex<Vec<PluginEffect>>>,
    slots: Arc<parking_lot::Mutex<BTreeMap<(String, Slot), Vec<String>>>>,
    observer: OnceLock<Option<SyncSender<EventJob>>>,
}
impl Host {
    pub fn register(&self, plugin: Arc<dyn Plugin>) -> anyhow::Result<()> {
        anyhow::ensure!(
            !self.closed.load(Ordering::Acquire),
            "extension owner is closed"
        );
        anyhow::ensure!(
            !plugin.name().trim().is_empty(),
            "plugin name cannot be empty"
        );
        let mut plugins = self.plugins.lock();
        anyhow::ensure!(
            !self.closed.load(Ordering::Acquire),
            "extension owner is closed"
        );
        anyhow::ensure!(
            !plugins.iter().any(|p| p.name() == plugin.name()),
            "duplicate plugin name"
        );
        plugins.push(plugin);
        self.generation.fetch_add(1, Ordering::AcqRel);
        Ok(())
    }
    /// Start explicitly configured operator-trusted plugin executables. Each request is a bounded
    /// JSON-RPC exchange on stdin/stdout; stderr is kept separate from protocol data.
    pub fn load_configured(&self, config: &crate::config::types::Config) {
        if !config.extensions.enabled {
            return;
        }
        // Installed plugins (`dial plugin install`) run alongside the configured ones.
        match installer::PluginStore::new(config.user_dir.join("plugins")).enabled_runtimes() {
            Ok(runtimes) => {
                // Each plugin starts on its own, so one broken install leaves its siblings running.
                for runtime in runtimes {
                    match runtime {
                        Ok((plugin, program, args, env)) => self.start_one(
                            &plugin.name,
                            &program,
                            &args,
                            &env,
                            config.extensions.timeout_secs,
                        ),
                        Err(error) => self
                            .notices
                            .lock()
                            .push(format!("installed plugins: {error:#}")),
                    }
                }
            }
            Err(error) => self
                .notices
                .lock()
                .push(format!("installed plugins: {error:#}")),
        }
        for (name, spec) in &config.extensions.plugins {
            if !spec.enabled {
                continue;
            }
            self.start_one(
                name,
                &spec.program,
                &spec.args,
                &spec.env,
                config.extensions.timeout_secs,
            );
        }
    }

    pub fn start_one(
        &self,
        name: &str,
        program: &Path,
        args: &[String],
        env: &std::collections::BTreeMap<String, String>,
        timeout_secs: u64,
    ) {
        match ProcessPlugin::start(name, program, args, env, timeout_secs) {
            Ok(plugin) => {
                if let Err(error) = self.register(Arc::new(plugin)) {
                    self.notices
                        .lock()
                        .push(format!("plugin {name}: {error:#}"));
                }
            }
            Err(error) => self
                .notices
                .lock()
                .push(format!("plugin {name} failed to start: {error:#}")),
        }
    }
    pub fn fork_session(&self) -> Self {
        Self {
            plugins: parking_lot::Mutex::new(
                self.plugins
                    .lock()
                    .iter()
                    .map(|plugin| plugin.fork_session().unwrap_or_else(|| Arc::clone(plugin)))
                    .collect(),
            ),
            ..Self::default()
        }
    }
    pub fn unregister(&self, name: &str) -> bool {
        let mut plugins = self.plugins.lock();
        let before = plugins.len();
        plugins.retain(|plugin| plugin.name() != name);
        if plugins.len() == before {
            return false;
        }
        self.generation.fetch_add(1, Ordering::AcqRel);
        self.slots
            .lock()
            .retain(|(owner, _), _| owner.as_str() != name);
        true
    }
    pub fn close(&self) {
        let mut plugins = self.plugins.lock();
        self.closed.store(true, Ordering::Release);
        self.generation.fetch_add(1, Ordering::AcqRel);
        plugins.clear();
        self.effects.lock().clear();
        self.slots.lock().clear();
    }
    pub fn subscribed(&self, event: &str) -> bool {
        self.plugins
            .lock()
            .iter()
            .any(|p| p.events().iter().any(|name| canonical_event(name) == event))
    }
    /// The loaded extension that answers the slash `command`, if any.
    pub fn command_owner(&self, command: &str) -> Option<String> {
        self.plugins
            .lock()
            .iter()
            .find(|p| p.commands().iter().any(|c| c == command))
            .map(|p| p.name().to_owned())
    }
    /// Every slash command the loaded extensions answer, with the owning extension's name. Names
    /// come from extension protocol text, so only short, plain words are kept.
    pub fn command_names(&self) -> Vec<(String, String)> {
        self.plugins
            .lock()
            .iter()
            .flat_map(|p| {
                let owner = p.name().to_owned();
                p.commands()
                    .iter()
                    .filter(|c| valid_command_name(c))
                    .map(move |c| (c.clone(), owner.clone()))
                    .collect::<Vec<_>>()
            })
            .collect()
    }
    /// One bounded JSON-RPC exchange with the named extension. Fails if it was unloaded, so a
    /// request started for a panel cannot reach an extension that has since gone away.
    pub fn request(
        &self,
        plugin: &str,
        method: &str,
        params: Value,
        interrupt: &AtomicBool,
    ) -> anyhow::Result<Value> {
        anyhow::ensure!(
            !self.closed.load(Ordering::Acquire),
            "extension owner is closed"
        );
        let target = self
            .plugins
            .lock()
            .iter()
            .find(|p| p.name() == plugin)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("extension {plugin} is not loaded"))?;
        let negotiated = target.negotiated();
        let value = target.call(method, params, interrupt)?;
        absorb_reply(
            &self.notices,
            &self.effects,
            &self.slots,
            plugin,
            &negotiated,
            &value,
        );
        Ok(value)
    }
    pub fn dispatch(&self, event: &str, payload: &Value, interrupt: &AtomicBool) -> Vec<Answer> {
        let (plugins, generation) = {
            let plugins = self.plugins.lock();
            (plugins.clone(), self.generation.load(Ordering::Acquire))
        };
        let obsolete = || {
            self.closed.load(Ordering::Acquire)
                || self.generation.load(Ordering::Acquire) != generation
        };
        plugins
            .into_iter()
            .filter(|p| p.events().iter().any(|name| canonical_event(name) == event))
            .take_while(|_| !interrupt.load(Ordering::Acquire))
            .map(|plugin| {
                let name = plugin.name().to_owned();
                let mut reply = if self.closed.load(Ordering::Acquire) {
                    Err(anyhow::anyhow!("extension owner is closed"))
                } else {
                    plugin.observe(event, payload, interrupt, &obsolete)
                };
                if self.closed.load(Ordering::Acquire)
                    || self.generation.load(Ordering::Acquire) != generation
                {
                    reply = Err(anyhow::anyhow!(
                        "extension registrations changed during the request; stale response discarded"
                    ));
                }
                let result = reply.map(|reply| {
                    absorb_reply(
                        &self.notices,
                        &self.effects,
                        &self.slots,
                        &name,
                        &plugin.negotiated(),
                        &reply,
                    );
                    reply.get("rewrite").cloned().unwrap_or(Value::Null)
                });
                Answer { plugin: name, result }
            })
            .collect()
    }
    /// Whether a loaded protocol 2 plugin subscribes to the observed event `event`. Emitters use
    /// this to skip building payloads nobody receives.
    pub fn observes(&self, event: &str) -> bool {
        !delivered_by_hook(event)
            && self
                .plugins
                .lock()
                .iter()
                .any(|p| p.negotiated().version >= 2 && p.events().iter().any(|name| name == event))
    }
    pub fn take_notices(&self) -> Vec<String> {
        std::mem::take(&mut *self.notices.lock())
    }

    /// Queues an observational event for the background worker, in order. The caller never waits
    /// for plugins. Blocking events must use [`Host::dispatch`] instead, because their reply can
    /// change agent behavior.
    pub fn emit(&self, event: &str, payload: &Value) {
        // Aliases of blocking hooks (`tool_call`, `tool_result`, `message_end`, `input`, ...) already
        // reach every subscriber through `dispatch`; observing them too would deliver twice.
        if self.closed.load(Ordering::Acquire) || delivered_by_hook(event) {
            return;
        }
        let plugins: Vec<Arc<dyn Plugin>> = self
            .plugins
            .lock()
            .iter()
            .filter(|p| p.negotiated().version >= 2 && p.events().iter().any(|name| name == event))
            .cloned()
            .collect();
        if plugins.is_empty() {
            return;
        }
        let generation = self.generation.load(Ordering::Acquire);
        let Some(sender) = self.observer.get_or_init(|| self.spawn_observer()) else {
            return;
        };
        let job = EventJob {
            event: event.to_owned(),
            payload: payload.clone(),
            plugins,
            generation,
        };
        match sender.try_send(job) {
            Ok(()) => {}
            Err(TrySendError::Full(_)) => push_notice(
                &self.notices,
                format!("extension event {event} dropped: observer queue is full"),
            ),
            Err(TrySendError::Disconnected(_)) => push_notice(
                &self.notices,
                format!("extension event {event} dropped: observer stopped"),
            ),
        }
    }
    fn spawn_observer(&self) -> Option<SyncSender<EventJob>> {
        let (sender, receiver) = sync_channel::<EventJob>(OBSERVER_QUEUE_CAPACITY);
        let closed = Arc::clone(&self.closed);
        let generation = Arc::clone(&self.generation);
        let notices = Arc::clone(&self.notices);
        let effects = Arc::clone(&self.effects);
        let slots = Arc::clone(&self.slots);
        let spawned = std::thread::Builder::new()
            .name("dial-plugin-events".into())
            .spawn(move || {
                let never = AtomicBool::new(false);
                for job in receiver {
                    let obsolete = || {
                        closed.load(Ordering::Acquire)
                            || generation.load(Ordering::Acquire) != job.generation
                    };
                    for plugin in &job.plugins {
                        if obsolete() {
                            break;
                        }
                        let name = plugin.name().to_owned();
                        let negotiated = plugin.negotiated();
                        match plugin.observe(&job.event, &job.payload, &never, &obsolete) {
                            Ok(reply) if !obsolete() => {
                                absorb_reply(&notices, &effects, &slots, &name, &negotiated, &reply)
                            }
                            Ok(_) => {}
                            Err(error) if !obsolete() => push_notice(
                                &notices,
                                format!("plugin {name} event {}: {error:#}", job.event),
                            ),
                            Err(_) => {}
                        }
                    }
                }
            });
        match spawned {
            Ok(_) => Some(sender),
            Err(error) => {
                push_notice(
                    &self.notices,
                    format!("extension observer failed to start: {error}"),
                );
                None
            }
        }
    }
    /// Actions from loaded plugins waiting for their owner to apply. Actions from an extension
    /// unloaded since they were queued are dropped.
    pub fn take_effects(&self) -> Vec<PluginEffect> {
        let loaded: Vec<String> = self
            .plugins
            .lock()
            .iter()
            .map(|p| p.name().to_owned())
            .collect();
        std::mem::take(&mut *self.effects.lock())
            .into_iter()
            .filter(|effect| loaded.contains(&effect.plugin))
            .collect()
    }
    /// Current UI contributions, by plugin and slot.
    pub fn ui_slots(&self) -> Vec<(String, UiSlot)> {
        self.slots
            .lock()
            .iter()
            .map(|((plugin, slot), lines)| {
                (
                    plugin.clone(),
                    UiSlot {
                        slot: *slot,
                        lines: lines.clone(),
                    },
                )
            })
            .collect()
    }
    /// Suggestions from loaded extensions that declared `autocomplete`, capped in total. Each call
    /// is bounded by the plugin timeout, so interactive callers must run it off the input thread.
    pub fn autocomplete(&self, query: &str, interrupt: &AtomicBool) -> Vec<Suggestion> {
        let targets: Vec<Arc<dyn Plugin>> = self
            .plugins
            .lock()
            .iter()
            .filter(|p| p.negotiated().has(protocol::CAP_AUTOCOMPLETE))
            .cloned()
            .collect();
        let mut out = Vec::new();
        for plugin in targets {
            if interrupt.load(Ordering::Acquire) {
                break;
            }
            match plugin.call("autocomplete", json!({"query": query}), interrupt) {
                Ok(reply) => out.extend(protocol::parse_suggestions(&reply)),
                Err(error) => push_notice(
                    &self.notices,
                    format!("plugin {} autocomplete: {error:#}", plugin.name()),
                ),
            }
        }
        out.truncate(protocol::MAX_AUTOCOMPLETE_ITEMS);
        out
    }
}

/// Whether `event` is another name for a blocking hook, so [`Host::dispatch`] already delivers it.
fn delivered_by_hook(event: &str) -> bool {
    canonical_event(event) != event
}

struct ProcessPlugin {
    name: String,
    program: PathBuf,
    args: Vec<String>,
    env: std::collections::BTreeMap<String, String>,
    events: Vec<String>,
    commands: Vec<String>,
    timeout: Duration,
    negotiated: Negotiated,
}
impl ProcessPlugin {
    fn start(
        name: &str,
        program: &Path,
        args: &[String],
        env: &std::collections::BTreeMap<String, String>,
        timeout_secs: u64,
    ) -> anyhow::Result<Self> {
        let mut plugin = Self {
            name: name.into(),
            program: program.into(),
            args: args.into(),
            env: env.clone(),
            events: Vec::new(),
            commands: Vec::new(),
            timeout: Duration::from_secs(timeout_secs.max(1)),
            negotiated: Negotiated::legacy(),
        };
        let response = plugin.request("initialize", protocol::initialize_params(), None)?;
        plugin.negotiated = protocol::negotiate(&response)?;
        let events = response
            .get("events")
            .and_then(Value::as_array)
            .map_or(&[][..], Vec::as_slice);
        plugin.events = events
            .iter()
            .filter_map(|v| v.as_str().map(str::to_owned))
            .collect();
        let commands = response
            .get("commands")
            .and_then(Value::as_array)
            .map_or(&[][..], Vec::as_slice);
        plugin.commands = commands
            .iter()
            .map(|v| {
                v.as_str()
                    .filter(|c| valid_command_name(c))
                    .map(str::to_owned)
                    .ok_or_else(|| {
                        anyhow::anyhow!("command names must be plain ASCII words of at most 48 bytes without a leading slash")
                    })
            })
            .collect::<anyhow::Result<_>>()?;
        anyhow::ensure!(
            !plugin.events.is_empty() || !plugin.commands.is_empty(),
            "initialize response must register events or commands"
        );
        Ok(plugin)
    }
    fn request(
        &self,
        method: &str,
        params: Value,
        interrupt: Option<&dyn Fn() -> bool>,
    ) -> anyhow::Result<Value> {
        const MAX_REPLY_BYTES: usize = 1024 * 1024;
        let request = json!({"jsonrpc":"2.0","id":1,"method":method,"params":params});
        let mut body = serde_json::to_vec(&request)?;
        body.push(b'\n');
        let mut command = Command::new(&self.program);
        command.args(&self.args).envs(&self.env);
        let never = || false;
        let mut policy = crate::runtime::process::Policy::bounded(self.timeout, MAX_REPLY_BYTES);
        policy.fail_on_output_limit = true;
        let output = crate::runtime::process::run_with_control(
            command,
            Some(body),
            policy,
            interrupt.unwrap_or(&never),
            &mut |_, _| {},
        )
        .map_err(anyhow::Error::msg)?;
        anyhow::ensure!(
            output.success(),
            "{}",
            output.failure("plugin request", self.timeout)
        );
        anyhow::ensure!(
            !output.output_incomplete,
            "plugin output stayed open after exit"
        );
        let output = output.stdout;
        let value: Value = serde_json::from_slice(&output)?;
        anyhow::ensure!(
            value.get("jsonrpc").is_none_or(|version| version == "2.0"),
            "unsupported plugin JSON-RPC version"
        );
        anyhow::ensure!(
            value.get("id").is_none_or(|id| id == 1),
            "plugin response does not match its request"
        );
        if let Some(error) = value.get("error") {
            anyhow::bail!("JSON-RPC error: {error}");
        }
        Ok(value.get("result").cloned().unwrap_or(Value::Null))
    }
}

#[cfg(test)]
mod tests;
impl Plugin for ProcessPlugin {
    fn fork_session(&self) -> Option<Arc<dyn Plugin>> {
        Some(Arc::new(Self {
            name: self.name.clone(),
            program: self.program.clone(),
            args: self.args.clone(),
            env: self.env.clone(),
            events: self.events.clone(),
            commands: self.commands.clone(),
            timeout: self.timeout,
            negotiated: self.negotiated.clone(),
        }))
    }
    fn on_event_with_control(
        &self,
        event: &str,
        payload: &Value,
        interrupt: &AtomicBool,
        obsolete: &(dyn Fn() -> bool + Sync),
    ) -> anyhow::Result<Option<Value>> {
        let response = self.request(
            "event",
            json!({"event":event,"payload":payload}),
            Some(&|| interrupt.load(Ordering::Acquire) || obsolete()),
        )?;
        Ok(response.get("rewrite").cloned())
    }
    fn observe(
        &self,
        event: &str,
        payload: &Value,
        interrupt: &AtomicBool,
        obsolete: &(dyn Fn() -> bool + Sync),
    ) -> anyhow::Result<Value> {
        anyhow::ensure!(!obsolete(), "extension owner is obsolete");
        self.request(
            "event",
            json!({"event":event,"payload":payload}),
            Some(&|| interrupt.load(Ordering::Acquire) || obsolete()),
        )
    }
    fn negotiated(&self) -> Negotiated {
        self.negotiated.clone()
    }
    fn name(&self) -> &str {
        &self.name
    }
    fn events(&self) -> &[String] {
        &self.events
    }
    fn on_event(
        &self,
        event: &str,
        payload: &Value,
        interrupt: &AtomicBool,
    ) -> anyhow::Result<Option<Value>> {
        anyhow::ensure!(!interrupt.load(Ordering::Acquire), "interrupted");
        let response = self.request(
            "event",
            json!({"event":event,"payload":payload}),
            Some(&|| interrupt.load(Ordering::Acquire)),
        )?;
        Ok(response.get("rewrite").cloned())
    }
    fn commands(&self) -> &[String] {
        &self.commands
    }
    fn call(&self, method: &str, params: Value, interrupt: &AtomicBool) -> anyhow::Result<Value> {
        self.request(method, params, Some(&|| interrupt.load(Ordering::Acquire)))
    }
}

#[cfg(test)]
mod ownership_tests;
