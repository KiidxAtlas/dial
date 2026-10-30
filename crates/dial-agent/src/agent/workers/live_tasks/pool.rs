//! Reusable worker pools. Queued assignments run through the same `start` path as every other
//! worker, on at most `concurrency` slots. A slot keeps its last worker, so the next item continues
//! that worker's real history. A launch refused before its worker started is terminal, and so is a
//! worker lost mid-run, because its effects are unknown and its item is never replayed.
use super::slot::{SlotRoute, SlotStore};
use super::*;

/// Pool-level assignment fields copied into every item's single-task assignment.
const SHARED_KEYS: &[&str] = &[
    "agent",
    "context",
    "output_schema",
    "max_steps",
    "isolation",
    "model_profile",
    "work_packet",
];
const MAX_ITEMS: usize = 512;
const MAX_POOLS: usize = 64;

/// Owned copy of the caller's launch context. A pool dispatches queued items from `pump`, which has
/// no caller context, so each pool call that has one refreshes the copy.
pub(super) struct PoolLaunch {
    config: Config,
    workspace: Workspace,
    ext: Registry,
    layout: Option<Layout>,
    session_id: String,
    harness: Harness,
    usage: Option<crate::llm::usage::Scope>,
    context_cap: Option<u32>,
    memory: Option<Arc<crate::storage::memory_backend::MemoryRuntime>>,
    plan_mode: bool,
    agents_messages: Vec<Value>,
    handoff: subagents::Handoff,
}

impl PoolLaunch {
    fn capture(ctx: &StartContext<'_>) -> Self {
        Self {
            config: ctx.config.clone(),
            workspace: ctx.workspace.clone(),
            ext: ctx.ext.clone(),
            layout: ctx.layout.cloned(),
            session_id: ctx.session_id.to_owned(),
            harness: ctx.harness.clone(),
            usage: ctx.usage.cloned(),
            context_cap: ctx.context_cap,
            memory: ctx.memory.cloned(),
            plan_mode: ctx.plan_mode,
            agents_messages: ctx.agents_messages.to_vec(),
            handoff: ctx.handoff.clone(),
        }
    }

    fn ctx(&self) -> StartContext<'_> {
        StartContext {
            config: &self.config,
            workspace: &self.workspace,
            ext: &self.ext,
            layout: self.layout.as_ref(),
            session_id: &self.session_id,
            harness: &self.harness,
            usage: self.usage.as_ref(),
            context_cap: self.context_cap,
            memory: self.memory.as_ref(),
            plan_mode: self.plan_mode,
            agents_messages: &self.agents_messages,
            handoff: self.handoff.clone(),
        }
    }
}

/// One pool slot: the id of its last worker, and the retained state its items share.
#[derive(Default)]
struct PoolSlot {
    last: Option<String>,
    store: SlotStore,
}

/// One pool: queued items, the slots that run them, and the terminal record of each item.
pub(super) struct WorkerPool {
    label: String,
    concurrency: usize,
    shared: Value,
    /// Last worker and retained state per slot; a settled slot continues its conversation for the next item.
    slots: Vec<PoolSlot>,
    items: Vec<PoolItem>,
    closed: bool,
    /// Whether the aggregate of the current batch already reached the completed queue.
    announced: bool,
    /// Items before this index already reached an automatic aggregate.
    delivered: usize,
}

struct PoolItem {
    args: Value,
    launch: Arc<PoolLaunch>,
    state: ItemState,
}

enum ItemState {
    Queued,
    Running { job: String, slot: usize },
    Settled(Value),
}

impl WorkerPool {
    fn new(label: String, concurrency: usize, shared: Value) -> Self {
        Self {
            label,
            concurrency,
            shared,
            slots: (0..concurrency).map(|_| PoolSlot::default()).collect(),
            items: Vec::new(),
            closed: false,
            announced: false,
            delivered: 0,
        }
    }

    fn pending(&self) -> usize {
        self.items
            .iter()
            .filter(|item| !matches!(item.state, ItemState::Settled(_)))
            .count()
    }

    fn next_queued(&self) -> Option<usize> {
        self.items
            .iter()
            .position(|item| matches!(item.state, ItemState::Queued))
    }

    fn free_slot(&self) -> Option<usize> {
        (0..self.concurrency).find(|slot| {
            !self.items.iter().any(
                |item| matches!(item.state, ItemState::Running { slot: busy, .. } if busy == *slot),
            )
        })
    }

    fn state(&self) -> &'static str {
        if self.pending() > 0 {
            "running"
        } else if self.closed {
            "closed"
        } else if self.items.is_empty() {
            "open"
        } else if self.items.iter().all(|item| {
            matches!(&item.state, ItemState::Settled(record) if record["status"].as_str() == Some("done"))
        }) {
            "done"
        } else {
            "partial"
        }
    }

    /// Validates every item before queueing any, so a bad item leaves the pool unchanged.
    fn enqueue(&mut self, launch: &Arc<PoolLaunch>, tasks: &[Value]) -> Result<(), String> {
        if self.items.len() + tasks.len() > MAX_ITEMS {
            return Err(format!("a pool holds at most {MAX_ITEMS} items"));
        }
        let config = subagents::with_file_agents(&launch.config);
        let mut queued = Vec::with_capacity(tasks.len());
        for task in tasks {
            let args = merged(&self.shared, task)?;
            let request = subagents::parse(&args, &config).map_err(|error| error.to_string())?;
            subagents::check_isolation_backends(&request.jobs, &launch.workspace, &config)?;
            queued.push(args);
        }
        self.items.extend(queued.into_iter().map(|args| PoolItem {
            args,
            launch: launch.clone(),
            state: ItemState::Queued,
        }));
        self.announced = false;
        Ok(())
    }
}

/// The single-task assignment for one item: pool-level fields, overridden by the item's own.
fn merged(shared: &Value, task: &Value) -> Result<Value, String> {
    let fields = task.as_object().ok_or("each pool task must be an object")?;
    let mut args = shared.clone();
    for (key, value) in fields {
        args[key.as_str()] = value.clone();
    }
    Ok(args)
}

fn pool_tasks(args: &Value) -> Result<Vec<Value>, String> {
    match &args["tasks"] {
        Value::Null => Ok(Vec::new()),
        Value::Array(tasks) => Ok(tasks.clone()),
        _ => Err("pool tasks must be an array".into()),
    }
}

/// The job id of the first launched worker in a `start` reply, if any actually started.
fn started_job(reply: &Value) -> Option<String> {
    reply
        .as_array()?
        .iter()
        .find(|entry| entry["status"].as_str() == Some("running"))
        .and_then(|entry| entry["id"].as_str().map(str::to_owned))
}

/// The aggregate a pool delivers once per batch: one line per item outcome.
fn aggregate(mut report: Value) -> Value {
    let lines: Vec<String> = report["items"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|item| {
            let text: String = item["result"]
                .as_str()
                .unwrap_or("")
                .chars()
                .take(240)
                .collect();
            format!(
                "#{} {}: {}",
                item["item"],
                item["status"].as_str().unwrap_or("unknown"),
                text
            )
        })
        .collect();
    let state = report["state"].clone();
    report["status"] = state;
    report["result"] = json!(lines.join("\n"));
    report
}

impl LiveTasks {
    /// Creates a pool, optionally with first items, and starts whatever fits the slot limits.
    pub fn pool_create(&mut self, ctx: StartContext<'_>, args: &Value) -> Result<Value, String> {
        let launch = Arc::new(PoolLaunch::capture(&ctx));
        let concurrency = match args.get("concurrency") {
            None | Some(Value::Null) => launch.config.task.max_concurrency.clamp(1, 4),
            Some(value) => value
                .as_u64()
                .filter(|n| (1..=32).contains(n))
                .ok_or("pool concurrency must be an integer from 1 to 32")?
                as usize,
        };
        let mut shared = json!({});
        for key in SHARED_KEYS {
            if let Some(value) = args.get(*key) {
                shared[*key] = value.clone();
            }
        }
        let tasks = pool_tasks(args)?;
        if self.pools.len() >= MAX_POOLS {
            self.prune_pools();
            if self.pools.len() >= MAX_POOLS {
                return Err("too many worker pools; close finished pools first".into());
            }
        }
        let mut pool = WorkerPool::new(
            args["name"].as_str().unwrap_or("pool").to_owned(),
            concurrency,
            shared,
        );
        pool.enqueue(&launch, &tasks)?;
        self.next_pool += 1;
        let id = format!("pool-{}", self.next_pool);
        self.pools.insert(id.clone(), pool);
        self.advance_pools();
        self.pool_report(&id, false)
    }

    /// Queues more items on an open pool and starts whatever fits.
    pub fn pool_push(&mut self, ctx: StartContext<'_>, args: &Value) -> Result<Value, String> {
        let id = args["id"].as_str().unwrap_or("").to_owned();
        let launch = Arc::new(PoolLaunch::capture(&ctx));
        let tasks = pool_tasks(args)?;
        if tasks.is_empty() {
            return Err("push needs a non-empty tasks array".into());
        }
        let pool = self
            .pools
            .get_mut(&id)
            .ok_or_else(|| format!("unknown pool {id}"))?;
        if pool.closed {
            return Err(format!("pool {id} is closed"));
        }
        pool.enqueue(&launch, &tasks)?;
        self.advance_pools();
        self.pool_report(&id, false)
    }

    /// Closes a pool: queued items settle as cancelled and running workers are interrupted.
    pub fn pool_close(&mut self, id: &str) -> Result<Value, String> {
        let pool = self
            .pools
            .get_mut(id)
            .ok_or_else(|| format!("unknown pool {id}"))?;
        pool.closed = true;
        let mut running = Vec::new();
        for item in &mut pool.items {
            if matches!(item.state, ItemState::Queued) {
                item.state = ItemState::Settled(json!({
                    "status": "cancelled",
                    "result": "pool closed before this item started",
                    "data": {"dial_effects": "none"},
                }));
            } else if let ItemState::Running { job, .. } = &item.state {
                running.push(job.clone());
            }
        }
        for job in running {
            let _ = self.cancel(&job);
        }
        self.pool_report(id, false)
    }

    pub fn is_pool(&self, id: &str) -> bool {
        self.pools.contains_key(id)
    }

    /// Pool items are announced through their aggregate, not as duplicate per-job results.
    pub(super) fn pool_owns_job(&self, id: &str) -> bool {
        self.pools.values().any(|pool| {
            pool.items.iter().any(|item| match &item.state {
                ItemState::Running { job, .. } => job == id,
                ItemState::Settled(record) => record["id"].as_str() == Some(id),
                ItemState::Queued => false,
            })
        })
    }

    /// The pool's state and item outcomes. `settled_only` omits items that are still queued or running.
    pub fn pool_report(&self, id: &str, settled_only: bool) -> Result<Value, String> {
        self.report_from(id, settled_only, 0)
    }

    /// Like `pool_report`, limited to items at or after index `from`.
    fn report_from(&self, id: &str, settled_only: bool, from: usize) -> Result<Value, String> {
        let pool = self
            .pools
            .get(id)
            .ok_or_else(|| format!("unknown pool {id}"))?;
        let mut counts: BTreeMap<String, usize> = BTreeMap::new();
        let mut items = Vec::new();
        for (index, item) in pool.items.iter().enumerate().skip(from) {
            let view = self.item_view(index + 1, item);
            let key = view["status"].as_str().unwrap_or("unknown").to_owned();
            *counts.entry(key).or_insert(0) += 1;
            if !settled_only || matches!(item.state, ItemState::Settled(_)) {
                items.push(view);
            }
        }
        Ok(json!({
            "id": id,
            "name": pool.label,
            "state": pool.state(),
            "concurrency": pool.concurrency,
            "closed": pool.closed,
            "counts": counts,
            "items": items,
        }))
    }

    /// Pools with unfinished items, listed next to jobs so a turn knows to keep waiting.
    pub(super) fn pool_entries(&self) -> Vec<Value> {
        self.pools
            .iter()
            .filter(|(_, pool)| pool.pending() > 0)
            .map(|(id, pool)| {
                json!({
                    "id": id,
                    "name": pool.label,
                    "status": "running",
                    "state": "running",
                    "pending": pool.pending(),
                })
            })
            .collect()
    }

    /// Ids of pools with queued or running items.
    pub(super) fn pending_pool_ids(&self) -> Vec<String> {
        self.pools
            .iter()
            .filter(|(_, pool)| pool.pending() > 0)
            .map(|(id, _)| id.clone())
            .collect()
    }

    /// Whether a pool's unfinished items keep it from being complete.
    pub(super) fn pool_pending(&self, id: Option<&str>) -> bool {
        self.pools
            .iter()
            .any(|(name, pool)| id.is_none_or(|id| id == name) && pool.pending() > 0)
    }

    /// Settles finished items, starts queued ones within the slot and live-job limits, and announces
    /// each batch's aggregate once. Called from every pump.
    pub(super) fn advance_pools(&mut self) {
        self.settle_pools();
        self.dispatch_pools();
        self.announce_pools();
    }

    fn item_view(&self, seq: usize, item: &PoolItem) -> Value {
        let mut view = match &item.state {
            ItemState::Queued => json!({
                "status": "queued",
                "task": item.args["task"],
            }),
            ItemState::Running { job, .. } => self
                .jobs
                .get(job)
                .map_or_else(|| json!({"id": job, "status": "running"}), JobState::status),
            ItemState::Settled(record) => record.clone(),
        };
        view["item"] = json!(seq);
        view
    }

    fn settle_pools(&mut self) {
        let jobs = &self.jobs;
        for pool in self.pools.values_mut() {
            for item in &mut pool.items {
                let (job, slot) = match &item.state {
                    ItemState::Running { job, slot } => (job.clone(), *slot),
                    _ => continue,
                };
                let settled = jobs
                    .get(&job)
                    .is_none_or(|state| !state.running() && state.worker.is_none());
                if !settled {
                    continue;
                }
                let mut record = jobs.get(&job).map_or_else(
                    || json!({"id": job, "status": "failed", "result": "worker record missing"}),
                    JobState::status,
                );
                // A lost worker's effects are unknown: the item is terminal and never replayed.
                let lost = record["result"].as_str() == Some(WORKER_LOST);
                if lost {
                    record["status"] = json!("lost");
                    if let Some(entry) = pool.slots.get_mut(slot) {
                        entry.last = None;
                    }
                }
                item.state = ItemState::Settled(record);
            }
        }
    }

    fn dispatch_pools(&mut self) {
        let ids: Vec<String> = self
            .pools
            .iter()
            .filter(|(_, pool)| !pool.closed && pool.next_queued().is_some())
            .map(|(id, _)| id.clone())
            .collect();
        for id in ids {
            while let Some(pool) = self.pools.get(&id) {
                let (Some(seq), Some(slot)) = (pool.next_queued(), pool.free_slot()) else {
                    break;
                };
                // Each item dispatches with the launch it was queued with, never a later push's.
                let launch = pool.items[seq].launch.clone();
                let limit = launch
                    .config
                    .subagent_max_live_jobs
                    .min(launch.config.task.max_concurrency);
                let live = self
                    .jobs
                    .values()
                    .filter(|job| job.running() || job.worker.is_some())
                    .count();
                if live >= limit {
                    return;
                }
                let prev = pool.slots[slot].last.clone();
                let store = pool.slots[slot].store.clone();
                let args = pool.items[seq].args.clone();
                match self.launch_item(&launch, &args, store, prev.is_some()) {
                    Ok(job) => {
                        if let Some(pool) = self.pools.get_mut(&id) {
                            pool.slots[slot].last = Some(job.clone());
                            pool.items[seq].state = ItemState::Running { job, slot };
                        }
                    }
                    Err(reason) => {
                        // Refused before any worker ran (validation, lease, model profile): no
                        // effect can exist, so the item is terminal and never retried. Capacity
                        // never reaches here because the live check above keeps it queued.
                        let Some(pool) = self.pools.get_mut(&id) else {
                            break;
                        };
                        pool.items[seq].state = ItemState::Settled(json!({
                            "status": "failed",
                            "result": format!("never started: {reason}"),
                            "data": {"dial_effects": "none"},
                        }));
                        continue;
                    }
                }
            }
        }
    }

    /// Starts one item on `store`, the slot's retained state. A slot that already ran a worker
    /// continues that worker's backend and conversation; a fresh slot starts its first worker.
    fn launch_item(
        &mut self,
        launch: &PoolLaunch,
        args: &Value,
        store: SlotStore,
        continued: bool,
    ) -> Result<String, String> {
        let continuation = continued.then(|| {
            format!(
                "New assignment from the pool:\n{}\nSolution space: {}",
                args["task"].as_str().unwrap_or(""),
                args["solution_space"].as_str().unwrap_or("")
            )
        });
        let route = SlotRoute {
            store,
            continuation,
        };
        let reply = self.start_inner(launch.ctx(), args, Some(route))?;
        started_job(&reply).ok_or_else(|| {
            reply
                .as_array()
                .and_then(|entries| entries.first())
                .and_then(|entry| entry["result"].as_str())
                .unwrap_or("worker did not start")
                .to_owned()
        })
    }

    fn announce_pools(&mut self) {
        let mut ready = Vec::new();
        for (id, pool) in self.pools.iter_mut() {
            if pool.announced || pool.pending() > 0 || (pool.items.is_empty() && !pool.closed) {
                continue;
            }
            pool.announced = true;
            // The automatic aggregate covers only items no earlier batch delivered.
            ready.push((id.clone(), pool.delivered));
            pool.delivered = pool.items.len();
        }
        for (id, from) in ready {
            if let Ok(report) = self.report_from(&id, false, from) {
                self.completed.push(aggregate(report));
            }
        }
    }

    /// Drops the oldest finished pools whose aggregate was already announced.
    fn prune_pools(&mut self) {
        let excess = (self.pools.len() + 1).saturating_sub(MAX_POOLS);
        let finished: Vec<String> = self
            .pools
            .iter()
            .filter(|(_, pool)| pool.announced && pool.pending() == 0)
            .map(|(id, _)| id.clone())
            .take(excess)
            .collect();
        for id in finished {
            self.pools.remove(&id);
        }
    }
}

#[cfg(test)]
mod tests;
