//! Concurrent startup has two consumers: a complete, config-ordered load for explicit queries,
//! and independently publishable groups for the registry's passive turn boundary.
use super::{Config, Connected, Duration, McpLoad, add_tools, connect, http, names};
use anyhow::Result;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;

#[derive(Default)]
struct Attempt {
    result: OnceLock<Result<Connected>>,
    /// A partial pass can be handed to a rebuilt registry. Replay its tools but not its notices.
    reported: AtomicBool,
}

pub(super) struct Attempts {
    servers: BTreeMap<String, Attempt>,
    groups: Vec<Vec<String>>,
}

impl Attempts {
    pub(super) fn new(config: &Config) -> Self {
        let servers: BTreeMap<_, _> = config
            .mcp
            .iter()
            .filter(|(_, s)| s.enabled)
            .map(|(name, _)| (name.clone(), Attempt::default()))
            .collect();
        // Prefix overlap is transitive for publication: "a" can collide with both "a__b"
        // and "a__c" even though the latter two cannot collide with one another.
        let mut prefixes: Vec<_> = servers
            .keys()
            .map(|name| (names::namespace_prefix(name), name))
            .collect();
        prefixes.sort_unstable();
        let mut groups: Vec<Vec<String>> = Vec::new();
        let mut iter = prefixes.into_iter().peekable();
        while let Some((prefix, first)) = iter.next() {
            let mut group = vec![first.clone()];
            while let Some((_, next)) = iter.next_if(|(key, _)| key.starts_with(&prefix)) {
                group.push(next.clone());
            }
            group.sort_unstable(); // Released name allocation uses unsanitized config key order.
            groups.push(group);
        }
        Self { servers, groups }
    }

    pub(super) fn run(&self, config: &Config, timeout: Duration, cancel: &AtomicBool) {
        thread::scope(|scope| {
            let mut threads = Vec::new();
            for (name, attempt) in &self.servers {
                let thread = thread::Builder::new()
                    .name(format!("mcp-{name}-connect"))
                    .spawn_scoped(scope, move || {
                        let server_cfg = &config.mcp[name];
                        let result = if server_cfg.sandbox {
                            match crate::tools::sandbox_spec_for_mcp(
                                &config.sandbox,
                                Some((&config.state_dir, &config.user_dir)),
                            ) {
                                Ok(spec) => connect(
                                    name,
                                    server_cfg,
                                    &config.project_root,
                                    &config.user_dir,
                                    Some(spec),
                                    timeout,
                                    &|| cancel.load(Ordering::SeqCst),
                                ),
                                Err(error) => Err(error.context("building MCP sandbox policy")),
                            }
                        } else {
                            connect(
                                name,
                                server_cfg,
                                &config.project_root,
                                &config.user_dir,
                                None,
                                timeout,
                                &|| cancel.load(Ordering::SeqCst),
                            )
                        };
                        let _ = attempt.result.set(result);
                    });
                match thread {
                    Ok(handle) => threads.push((attempt, handle)),
                    Err(error) => {
                        let _ = attempt.result.set(Err(error.into()));
                    }
                }
            }
            for (attempt, handle) in threads {
                if handle.join().is_err() {
                    let _ = attempt
                        .result
                        .set(Err(anyhow::anyhow!("connection thread panicked")));
                }
            }
        });
    }

    /// Called only after the coordinator has settled. If it panicked partway through spawning
    /// workers, finish the missing slots as failures and keep the servers that did connect.
    pub(super) fn finish(&self) -> McpLoad {
        self.snapshot(|_| true)
    }

    /// Normally a finished prefetch can be adopted unchanged. Only a partly published handoff
    /// needs its startup diagnostics filtered before it becomes the next registry's ready load.
    pub(super) fn handed_over_load(&self) -> Option<McpLoad> {
        self.servers
            .values()
            .any(|a| a.reported.load(Ordering::Relaxed))
            .then(|| self.snapshot(|a| !a.reported.swap(true, Ordering::Relaxed)))
    }

    fn snapshot(&self, report: impl Fn(&Attempt) -> bool) -> McpLoad {
        let mut loaded = McpLoad::default();
        for (name, attempt) in &self.servers {
            let _ = attempt
                .result
                .get_or_init(|| Err(anyhow::anyhow!("connection thread panicked")));
            attempt.apply(name, &mut loaded, report(attempt));
        }
        loaded
    }

    /// Only publish groups with all identities known, never timing-dependent winners of a name
    /// collision. `seen` belongs to the consuming registry (clones and handoffs can catch up).
    pub(super) fn publish(&self, seen: &mut BTreeSet<String>, delta: &mut McpLoad) -> bool {
        let before = seen.len();
        for group in &self.groups {
            if seen.contains(&group[0])
                || !group
                    .iter()
                    .all(|name| self.servers[name].result.get().is_some())
            {
                continue;
            }
            for name in group {
                let attempt = &self.servers[name];
                attempt.apply(name, delta, !attempt.reported.swap(true, Ordering::Relaxed));
                seen.insert(name.clone());
            }
        }
        seen.len() != before
    }
}

impl Attempt {
    fn apply(&self, name: &str, loaded: &mut McpLoad, report: bool) {
        let before = loaded.notices.len();
        match self.result.get().expect("attempt settled") {
            Ok(connected) => add_tools(loaded, name, connected.clone()),
            Err(error) => {
                if error
                    .downcast_ref::<http::AuthRequired>()
                    .is_some_and(http::AuthRequired::oauth_sign_in)
                {
                    loaded.sign_in.push(name.to_owned());
                }
                loaded.notices.push(format!("mcp `{name}`: {error:#}"));
            }
        }
        if !report {
            loaded.notices.truncate(before);
        }
    }
}
