//! Runtime lifecycle plugins. Trusted stdio executables receive JSON-RPC requests and may rewrite
//! event payloads. They do not gain direct access to agent execution or approvals.
use crate::config::hooks::canonical_event;

pub mod installer;
pub mod marketplace;
use serde_json::{Value, json};
use std::{
    path::{Path, PathBuf},
    process::Command,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
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
    fn on_event(
        &self,
        event: &str,
        payload: &Value,
        interrupt: &AtomicBool,
    ) -> anyhow::Result<Option<Value>>;
}

#[derive(Debug)]
pub struct Answer {
    pub plugin: String,
    pub result: anyhow::Result<Value>,
}

#[derive(Default)]
pub struct Host {
    plugins: parking_lot::Mutex<Vec<Arc<dyn Plugin>>>,
    notices: parking_lot::Mutex<Vec<String>>,
    generation: AtomicU64,
    closed: AtomicBool,
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

    fn start_one(
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
        true
    }
    pub fn close(&self) {
        let mut plugins = self.plugins.lock();
        self.closed.store(true, Ordering::Release);
        self.generation.fetch_add(1, Ordering::AcqRel);
        plugins.clear();
    }
    pub fn subscribed(&self, event: &str) -> bool {
        self.plugins
            .lock()
            .iter()
            .any(|p| p.events().iter().any(|name| canonical_event(name) == event))
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
                let mut result = if self.closed.load(Ordering::Acquire) {
                    Err(anyhow::anyhow!("extension owner is closed"))
                } else { plugin.on_event_with_control(event, payload, interrupt, &obsolete).map(|value| value.unwrap_or(Value::Null)) };
                if self.closed.load(Ordering::Acquire) || self.generation.load(Ordering::Acquire) != generation {
                    result = Err(anyhow::anyhow!("extension registrations changed during the request; stale response discarded"));
                }
                Answer { plugin: name, result }
            })
            .collect()
    }
    pub fn take_notices(&self) -> Vec<String> {
        std::mem::take(&mut *self.notices.lock())
    }
}

struct ProcessPlugin {
    name: String,
    program: PathBuf,
    args: Vec<String>,
    env: std::collections::BTreeMap<String, String>,
    events: Vec<String>,
    timeout: Duration,
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
            timeout: Duration::from_secs(timeout_secs.max(1)),
        };
        let response = plugin.request(
            "initialize",
            json!({"protocolVersion":"1","capabilities":{}}),
            None,
        )?;
        let events = response
            .get("events")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        plugin.events = events
            .into_iter()
            .filter_map(|v| v.as_str().map(str::to_owned))
            .collect();
        anyhow::ensure!(
            !plugin.events.is_empty(),
            "initialize response must include an events array"
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
            timeout: self.timeout,
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
}

#[cfg(test)]
mod ownership_tests;
