use super::{
    Driver, MemoryModelRunner, Operation, SaveInput, clean,
    config::{HindsightConfig, MemoryBackendId, MemoryScoping, RetainMode},
    files::PrivateDirectory,
    now, redact, visible_text,
};
use crate::{llm, storage::paths::Layout};
use anyhow::{Context, Result, ensure};
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{collections::BTreeSet, io::Read};

struct Hindsight {
    config: HindsightConfig,
    db: Connection,
    banks: BTreeSet<String>,
    model_context: Value,
    models_loaded: bool,
}
pub fn open(layout: &Layout, config: &HindsightConfig) -> Result<Box<dyn Driver>> {
    ensure!(
        !config.endpoint.is_empty(),
        "hindsight requires explicit service endpoint"
    );
    if !config.api_key_env.is_empty() {
        credential(config)?;
    }
    let directory = PrivateDirectory::open(layout, "hindsight")?;
    let db = directory.database("pending.sqlite3")?;
    db.execute_batch("CREATE TABLE IF NOT EXISTS pending(id TEXT PRIMARY KEY,endpoint TEXT NOT NULL,bank TEXT NOT NULL,project TEXT NOT NULL,session TEXT NOT NULL,branch TEXT NOT NULL,revision TEXT NOT NULL,payload TEXT NOT NULL,created INTEGER NOT NULL,state TEXT NOT NULL DEFAULT 'queued');CREATE TABLE IF NOT EXISTS transcripts(endpoint TEXT NOT NULL,bank TEXT NOT NULL,project TEXT NOT NULL,session TEXT NOT NULL,branch TEXT NOT NULL,revision TEXT NOT NULL,turns INTEGER NOT NULL,retained INTEGER NOT NULL DEFAULT 0,content TEXT NOT NULL,PRIMARY KEY(endpoint,bank,project,session,branch));")?;
    Ok(Box::new(Hindsight {
        config: config.clone(),
        db,
        banks: BTreeSet::new(),
        model_context: json!([]),
        models_loaded: false,
    }))
}
fn credential(config: &HindsightConfig) -> Result<Option<String>> {
    if config.api_key_env.is_empty() {
        return Ok(None);
    }
    let token = std::env::var(&config.api_key_env).map_err(|_| {
        anyhow::anyhow!(
            "the Hindsight API key variable {name} (memory.hindsight.api_key_env) is not set; export {name} in the shell before starting dial",
            name = config.api_key_env
        )
    })?;
    ensure!(
        !token.is_empty() && token.bytes().all(|byte| byte.is_ascii_graphic()),
        "the API key is empty or contains characters outside plain ASCII"
    );
    Ok(Some(token))
}
fn encoded(value: &str) -> String {
    url::form_urlencoded::byte_serialize(value.as_bytes()).collect()
}
impl Hindsight {
    fn tags(&self, op: &Operation) -> Option<Vec<String>> {
        (self.config.scoping == MemoryScoping::PerProjectTagged).then(|| {
            vec![format!(
                "project:{:x}",
                Sha256::digest(op.scope.project.as_bytes())
            )]
        })
    }
    fn request(
        &self,
        op: &Operation,
        method: &str,
        path: &str,
        body: &Value,
        purpose: &str,
        model: &mut dyn MemoryModelRunner,
    ) -> Result<Value> {
        op.check()?;
        let token = credential(&self.config)?;
        let url = format!("{}{path}", self.config.endpoint.trim_end_matches('/'));
        let parsed = url::Url::parse(&url)?;
        let local = parsed.host_str().is_some_and(|host| {
            host == "localhost"
                || host
                    .parse::<std::net::IpAddr>()
                    .is_ok_and(|ip| ip.is_loopback())
        });
        let timeout = match purpose {
            "recall" => self.config.recall_timeout_secs,
            "reflect" => self.config.reflect_timeout_secs,
            "retain" => self.config.retain_timeout_secs,
            _ => self.config.request_timeout_secs,
        };
        let _cancellation = op.cancellation_guard();
        let agent = llm::agent_with(
            op.timeout_secs(timeout),
            !local,
            Some(op.cancel.clone()),
            None,
        );
        let auth = token
            .as_ref()
            .map(|value| format!("Bearer {value}"))
            .unwrap_or_default();
        ensure!(
            matches!(method, "GET" | "DELETE" | "PUT" | "POST"),
            "unsupported Hindsight request method"
        );
        let receipt = model.authorize_external(op, purpose)?;
        let result = std::thread::scope(|scope| -> Result<(u16, Vec<u8>)> {
            let (start_send, start_receive) = std::sync::mpsc::sync_channel(0);
            let (send, receive) = std::sync::mpsc::sync_channel(1);
            let worker=scope.spawn(move||{
                let result=(||->Result<(u16,Vec<u8>)>{
                    start_receive.recv().context("owner cancelled Hindsight send")?;
                    let mut response=match method{
                        "GET"=>{let mut request=agent.get(&url);if !auth.is_empty(){request=request.header("Authorization",&auth);}request.call()},
                        "DELETE"=>{let mut request=agent.delete(&url);if !auth.is_empty(){request=request.header("Authorization",&auth);}request.call()},
                        "PUT"=>{let mut request=agent.put(&url).header("Content-Type","application/json");if !auth.is_empty(){request=request.header("Authorization",&auth);}request.send_json(body)},
                        "POST"=>{let mut request=agent.post(&url).header("Content-Type","application/json");if !auth.is_empty(){request=request.header("Authorization",&auth);}request.send_json(body)},
                        _=>unreachable!(),
                    }.map_err(|_|anyhow::anyhow!("Hindsight transport failed or cancelled; credentials and response withheld"))?;
                    let status=response.status().as_u16();let mut bytes=Vec::new();response.body_mut().as_reader().take(1024*1024+1).read_to_end(&mut bytes).context("Hindsight bounded response read failed")?;ensure!(bytes.len()<=1024*1024,"Hindsight response exceeds 1MiB bound");Ok((status,bytes))
                })();let _=send.send(result);
            });
            if let Err(error) = model.external_started(op, receipt) {
                drop(start_send);
                let _ = worker.join();
                return Err(error);
            }
            start_send
                .send(())
                .context("Hindsight owned transport unavailable")?;
            loop {
                if model.heartbeat(op).is_err() {
                    op.cancel.store(true, std::sync::atomic::Ordering::SeqCst);
                }
                match receive.recv_timeout(std::time::Duration::from_millis(50)) {
                    Ok(result) => {
                        worker
                            .join()
                            .map_err(|_| anyhow::anyhow!("Hindsight transport panicked"))?;
                        return result;
                    }
                    Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
                    Err(_) => {
                        let _ = worker.join();
                        anyhow::bail!("Hindsight owned transport exited");
                    }
                }
            }
        });
        let (status, bytes) = match result {
            Ok(result) => result,
            Err(error) => {
                let _ = model.abandon_external(op, receipt);
                return Err(error);
            }
        };
        let value = serde_json::from_slice::<Value>(&bytes).unwrap_or(Value::Null);
        model.account_external(
            op,
            receipt,
            value.get("usage").unwrap_or(&Value::Null),
            (200..300).contains(&status),
        )?;
        op.check()?;
        if method == "DELETE" && status == 404 {
            return Ok(json!({"deleted":false}));
        }
        ensure!(
            (200..300).contains(&status),
            "hindsight {purpose} rejected with HTTP {status}; response body withheld"
        );
        if method == "DELETE" && bytes.is_empty() {
            return Ok(json!({"deleted":true}));
        }
        ensure!(
            !value.is_null(),
            "hindsight {purpose} returned invalid JSON"
        );
        Ok(value)
    }
    fn ensure_bank(
        &mut self,
        op: &Operation,
        bank: &str,
        model: &mut dyn MemoryModelRunner,
    ) -> Result<()> {
        if self.banks.contains(bank) {
            return Ok(());
        }
        self.request(op,"PUT",&format!("/v1/default/banks/{bank}"),&json!({"reflect_mission":self.config.reflect_mission,"retain_mission":self.config.retain_mission}),"bank setup",model)?;
        self.banks.insert(bank.to_owned());
        Ok(())
    }
    fn list_models(
        &self,
        op: &Operation,
        detail: &str,
        model: &mut dyn MemoryModelRunner,
    ) -> Result<Vec<Value>> {
        let mut models = Vec::new();
        loop {
            let response = self.request(
                op,
                "GET",
                &format!(
                    "/v1/default/banks/{}/mental-models?detail={detail}&limit=1000&offset={}",
                    op.scope.retain_bank,
                    models.len()
                ),
                &Value::Null,
                "mental-model list",
                model,
            )?;
            let page = response["items"]
                .as_array()
                .context("hindsight mental-model list missing items")?;
            ensure!(
                models.len() + page.len() <= 10000,
                "hindsight mental-model list exceeds bounded 10000 entries; no partial snapshot installed"
            );
            models.extend(page.iter().cloned());
            if page.len() < 1000 {
                return Ok(models);
            }
        }
    }
    fn seed_models(&mut self, op: &Operation, model: &mut dyn MemoryModelRunner) -> Result<Value> {
        self.ensure_bank(op, &op.scope.retain_bank, model)?;
        let existing = self.list_models(op, "metadata", model)?;
        let seeds: Value = serde_json::from_str(include_str!("hindsight_seeds.json"))?;
        let mode = match self.config.scoping {
            MemoryScoping::Global => "global",
            MemoryScoping::PerProject => "per-project",
            MemoryScoping::PerProjectTagged => "per-project-tagged",
        };
        let mut created = Vec::new();
        for seed in seeds["seeds"]
            .as_array()
            .context("invalid built-in mental-model seeds")?
        {
            if !seed["scopes"]
                .as_array()
                .is_some_and(|scopes| scopes.iter().any(|scope| scope == mode))
            {
                continue;
            }
            let project_tagged = seed["projectTagged"] == true;
            let tags = if project_tagged {
                self.tags(op).unwrap_or_default()
            } else {
                Vec::new()
            };
            let raw_id = seed["id"].as_str().context("seed id missing")?;
            let id = if self.config.scoping == MemoryScoping::PerProjectTagged && project_tagged {
                format!("{raw_id}-{:x}", Sha256::digest(op.scope.project.as_bytes()))
            } else {
                raw_id.to_owned()
            };
            if existing.iter().any(|entry| {
                entry["id"] == id || (entry["id"] == raw_id && entry["tags"] == json!(tags))
            }) {
                continue;
            }
            self.request(op,"POST",&format!("/v1/default/banks/{}/mental-models",op.scope.retain_bank),&json!({"id":id,"name":seed["name"],"source_query":seed["source_query"],"tags":tags,"max_tokens":seed["max_tokens"],"trigger":seed["trigger"]}),"mental-model create",model)?;
            created.push(id);
        }
        Ok(
            json!({"created":created,"async":true,"message":"Create-only source mental models; server background reflect populates content, then refreshes after consolidation"}),
        )
    }
    fn load_models(
        &mut self,
        op: &Operation,
        model: &mut dyn MemoryModelRunner,
        seed: bool,
    ) -> Result<()> {
        if !self.config.mental_models_enabled {
            return Ok(());
        }
        if seed
            && op.authority.task_depth == 0
            && self.config.mental_model_auto_seed
            && !self.models_loaded
        {
            self.seed_models(op, model)?;
        }
        let mut models = self.list_models(op, "content", model)?;
        let visible = self.tags(op);
        models.retain(|entry| {
            let tag_visible = match &visible {
                None => true,
                Some(tags) => entry["tags"].as_array().is_none_or(|values| {
                    values.is_empty()
                        || values.iter().any(|value| {
                            value
                                .as_str()
                                .is_some_and(|tag| tags.iter().any(|active| active == tag))
                        })
                }),
            };
            tag_visible
                && entry["content"]
                    .as_str()
                    .is_some_and(|content| !content.trim().is_empty())
        });
        models.sort_by(|a, b| {
            a["name"]
                .as_str()
                .unwrap_or("")
                .cmp(b["name"].as_str().unwrap_or(""))
        });
        let items:Vec<Value>=models.iter().map(|entry|json!({"id":entry["id"],"name":redact(entry["name"].as_str().unwrap_or("")),"content":redact(entry["content"].as_str().unwrap_or(""))})).collect();
        op.check()?;
        self.model_context = json!(items);
        self.models_loaded = true;
        Ok(())
    }
    fn queue_item(&mut self, op: &Operation, input: &SaveInput) -> Result<String> {
        ensure!(
            !input.global,
            "Hindsight tools do not expose global scope; choose operator bank scoping policy"
        );
        op.check()?;
        let id = format!("{:016x}", rand::random::<u64>());
        let content = clean(&input.content, 1024 * 1024)?;
        let payload = json!({"content":content,"context":input.context,"document_id":id,"tags":self.tags(op),"metadata":{"session_id":op.authority.session_id,"branch_id":op.authority.branch_id,"revision":op.authority.revision,"cwd":op.scope.project,"source":input.source.as_deref().unwrap_or("coding-agent-retain")}});
        self.db.execute("INSERT INTO pending(id,endpoint,bank,project,session,branch,revision,payload,created) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9)",params![id,self.config.endpoint,op.scope.retain_bank,op.scope.project,op.authority.session_id,op.authority.branch_id,op.authority.revision,serde_json::to_string(&payload)?,now()])?;
        Ok(id)
    }
    fn queue_transcript(&mut self, op: &Operation, force: bool) -> Result<()> {
        if op.authority.task_depth > 0 || !self.config.auto_retain {
            return Ok(());
        }
        let row:Option<(i64,i64,String)>=self.db.query_row("SELECT turns,retained,content FROM transcripts WHERE endpoint=?1 AND bank=?2 AND project=?3 AND session=?4 AND branch=?5",params![self.config.endpoint,op.scope.retain_bank,op.scope.project,op.authority.session_id,op.authority.branch_id],|row|Ok((row.get(0)?,row.get(1)?,row.get(2)?))).optional()?;
        let Some((turns, retained, stored)) = row else {
            return Ok(());
        };
        if turns <= retained || (!force && turns - retained < self.config.retain_every_turns as i64)
        {
            return Ok(());
        }
        let messages: Vec<Value> = serde_json::from_str(&stored).context(
            "stored Hindsight transcript must be refreshed from authentic session history",
        )?;
        let start = if self.config.retain_mode == RetainMode::LastTurn {
            retained.saturating_sub(self.config.retain_overlap_turns as i64)
        } else {
            0
        };
        let mut turn = 0;
        let mut content = String::new();
        for message in &messages {
            let role = message["role"].as_str().unwrap_or("");
            if role == "user" {
                turn += 1;
            }
            if turn > start {
                content.push_str(&format!("[role: {role}]\n{}\n", visible_text(message)));
            }
        }
        let branch_digest = format!("{:x}", Sha256::digest(op.authority.branch_id.as_bytes()));
        let id = if self.config.retain_mode == RetainMode::LastTurn {
            format!("{}-{branch_digest}-{turns}", op.authority.session_id)
        } else {
            format!("{}-{branch_digest}", op.authority.session_id)
        };
        let payload = json!({"content":content,"context":self.config.retain_context,"document_id":id,"update_mode":"replace","tags":self.tags(op),"metadata":{"session_id":op.authority.session_id,"branch_id":op.authority.branch_id,"revision":op.authority.revision,"cwd":op.scope.project,"retained_through_user_turn":turns.to_string(),"source":"coding-agent-transcript"}});
        op.check()?;
        let tx = self
            .db
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        tx.execute("INSERT INTO pending(id,endpoint,bank,project,session,branch,revision,payload,created) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9) ON CONFLICT(id) DO UPDATE SET revision=excluded.revision,payload=excluded.payload,created=excluded.created,state='queued'",params![id,self.config.endpoint,op.scope.retain_bank,op.scope.project,op.authority.session_id,op.authority.branch_id,op.authority.revision,serde_json::to_string(&payload)?,now()])?;
        tx.execute("UPDATE transcripts SET retained=?1 WHERE endpoint=?2 AND bank=?3 AND project=?4 AND session=?5 AND branch=?6",params![turns,self.config.endpoint,op.scope.retain_bank,op.scope.project,op.authority.session_id,op.authority.branch_id])?;
        op.check()?;
        tx.commit()?;
        Ok(())
    }
    fn flush(
        &mut self,
        op: &Operation,
        model: &mut dyn MemoryModelRunner,
        retry: bool,
    ) -> Result<usize> {
        // Every queued write for this endpoint and bank is sent, including writes left by earlier
        // sessions and earlier context-history generations, not only the current branch's.
        let rows:Vec<(String,String)>=self.db.prepare("SELECT id,payload FROM pending WHERE endpoint=?1 AND bank=?2 AND (state='queued' OR ?3) ORDER BY created LIMIT 16")?.query_map(params![self.config.endpoint,op.scope.retain_bank,retry],|row|Ok((row.get(0)?,row.get(1)?)))?.collect::<rusqlite::Result<_>>()?;
        if rows.is_empty() {
            return Ok(0);
        }
        self.ensure_bank(op, &op.scope.retain_bank, model)?;
        let items: Vec<Value> = rows
            .iter()
            .map(|(_, payload)| serde_json::from_str(payload))
            .collect::<serde_json::Result<_>>()?;
        for (id, _) in &rows {
            op.check()?;
            self.db
                .execute("UPDATE pending SET state='inflight' WHERE id=?1", [id])?;
        }
        let result = self.request(
            op,
            "POST",
            &format!("/v1/default/banks/{}/memories", op.scope.retain_bank),
            &json!({"items":items,"async":true}),
            "retain",
            model,
        );
        op.check()?;
        match result {
            Ok(_) => {
                for (id, _) in &rows {
                    self.db.execute("DELETE FROM pending WHERE id=?1", [id])?;
                }
                Ok(rows.len())
            }
            Err(error) => {
                for (id, _) in &rows {
                    self.db.execute(
                        "UPDATE pending SET state='failed-or-ambiguous' WHERE id=?1",
                        [id],
                    )?;
                }
                Err(error)
            }
        }
    }
}
impl Driver for Hindsight {
    fn id(&self) -> MemoryBackendId {
        MemoryBackendId::Hindsight
    }
    fn status(&mut self, op: &Operation) -> Result<Value> {
        let pending: i64 = self.db.query_row(
            "SELECT count(*) FROM pending WHERE endpoint=?1 AND bank=?2",
            params![self.config.endpoint, op.scope.retain_bank],
            |row| row.get(0),
        )?;
        let waiting = if pending == 0 {
            "nothing waiting to send".to_owned()
        } else {
            format!("{pending} write(s) waiting to send")
        };
        let message = format!(
            "Hindsight service at {} ({waiting}). Remote writes can continue after a local cancel, and usage may not be free.",
            self.config.endpoint
        );
        Ok(
            json!({"backend":"hindsight","active":true,"writable":true,"searchable":true,"retainBank":op.scope.retain_bank,"recallBanks":op.scope.recall_banks,"scope":op.scope.project,"pending":pending,"mentalModelsLoaded":self.models_loaded,"message":message}),
        )
    }
    fn search(
        &mut self,
        op: &Operation,
        query: &str,
        limit: usize,
        model: &mut dyn MemoryModelRunner,
    ) -> Result<Value> {
        let mut items = Vec::new();
        for bank in &op.scope.recall_banks {
            self.ensure_bank(op, bank, model)?;
            let response=self.request(op,"POST",&format!("/v1/default/banks/{bank}/memories/recall"),&json!({"query":query,"max_tokens":self.config.recall_max_tokens,"budget":self.config.recall_budget,"types":self.config.recall_types,"tags":self.tags(op),"tags_match":"any"}),"recall",model)?;
            for item in response["results"]
                .as_array()
                .context("hindsight recall response missing results")?
            {
                let content = item["text"]
                    .as_str()
                    .context("hindsight recall item missing text")?;
                items.push(json!({"id":item["id"],"content":redact(content),"source":item["source"],"timestamp":item["timestamp"],"score":item["score"],"bank":bank}));
            }
        }
        items.truncate(limit.min(50));
        Ok(json!({"backend":"hindsight","query":query,"count":items.len(),"items":items}))
    }
    fn save(
        &mut self,
        op: &Operation,
        input: &SaveInput,
        _: &mut dyn MemoryModelRunner,
    ) -> Result<Value> {
        let id = self.queue_item(op, input)?;
        Ok(json!({"backend":"hindsight","stored":0,"queued":true,"ids":[id]}))
    }
    fn reflect(
        &mut self,
        op: &Operation,
        query: &str,
        context: Option<&str>,
        model: &mut dyn MemoryModelRunner,
    ) -> Result<Value> {
        self.ensure_bank(op, &op.scope.retain_bank, model)?;
        let response=self.request(op,"POST",&format!("/v1/default/banks/{}/reflect",op.scope.retain_bank),&json!({"query":query,"context":context.map(redact),"budget":self.config.recall_budget,"tags":self.tags(op),"tags_match":"any"}),"reflect",model)?;
        Ok(
            json!({"backend":"hindsight","text":redact(response["text"].as_str().context("hindsight reflection missing text")?)}),
        )
    }
    fn observe(&mut self, op: &Operation, messages: &[Value]) -> Result<()> {
        op.check()?;
        if op.authority.task_depth > 0 || !self.config.auto_retain {
            return Ok(());
        }
        let mut turns = 0;
        let mut transcript = Vec::new();
        for message in messages {
            let role = message["role"].as_str().unwrap_or("");
            if !matches!(role, "user" | "assistant") {
                continue;
            }
            if role == "user" {
                turns += 1;
            }
            transcript.push(json!({"role":role,"content":redact(&visible_text(message))}));
        }
        let content = serde_json::to_string(&transcript)?;
        ensure!(
            content.len() <= 1024 * 1024,
            "Hindsight transcript exceeds 1MiB; nothing silently clipped"
        );
        op.check()?;
        self.db.execute("INSERT INTO transcripts(endpoint,bank,project,session,branch,revision,turns,content) VALUES(?1,?2,?3,?4,?5,?6,?7,?8) ON CONFLICT(endpoint,bank,project,session,branch) DO UPDATE SET revision=excluded.revision,turns=excluded.turns,content=excluded.content",params![self.config.endpoint,op.scope.retain_bank,op.scope.project,op.authority.session_id,op.authority.branch_id,op.authority.revision,turns,content])?;
        self.queue_transcript(op, false)
    }
    fn maintain(&mut self, op: &Operation, model: &mut dyn MemoryModelRunner) -> Result<()> {
        let sent = self.flush(op, model, false)?;
        if self.config.mental_models_enabled && (!self.models_loaded || sent > 0) {
            self.load_models(op, model, true)?;
        }
        Ok(())
    }
    fn enqueue(&mut self, op: &Operation, model: &mut dyn MemoryModelRunner) -> Result<Value> {
        self.queue_transcript(op, true)?;
        let sent = self.flush(op, model, true)?;
        if self.config.mental_models_enabled {
            self.load_models(op, model, true)?;
        }
        Ok(json!({"backend":"hindsight","submitted":sent,"async":true}))
    }
    fn instructions(&mut self, _: &Operation, max: usize) -> Result<Option<String>> {
        if !self.config.mental_models_enabled {
            return Ok(None);
        }
        Ok(super::render_search(
            &json!({"backend":"hindsight","kind":"mental_models","items":self.model_context}),
            max.min(self.config.mental_model_max_render_chars),
        ))
    }
    fn special_command(
        &mut self,
        op: &Operation,
        input: &str,
        model: &mut dyn MemoryModelRunner,
    ) -> Option<Result<Value>> {
        let mut words = input.split_whitespace();
        if words.next() != Some("mm") {
            return None;
        }
        let command = words.next().unwrap_or("list");
        let id = words.next();
        Some((|| {
            self.ensure_bank(op, &op.scope.retain_bank, model)?;
            match command {
                "list" => Ok(json!({"items":self.list_models(op,"metadata",model)?})),
                "seed" => self.seed_models(op, model),
                "show" | "history" | "refresh" | "delete" => {
                    let id = id.context("mental-model command requires id")?;
                    ensure!(
                        id.len() <= 256 && !id.chars().any(char::is_control),
                        "invalid mental-model id"
                    );
                    let mut path = format!(
                        "/v1/default/banks/{}/mental-models/{}",
                        op.scope.retain_bank,
                        encoded(id)
                    );
                    let method = match command {
                        "refresh" => {
                            path.push_str("/refresh");
                            "POST"
                        }
                        "delete" => "DELETE",
                        "history" => {
                            path.push_str("/history");
                            "GET"
                        }
                        _ => "GET",
                    };
                    let result = self.request(
                        op,
                        method,
                        &path,
                        &Value::Null,
                        &format!("mental-model {command}"),
                        model,
                    )?;
                    if matches!(command, "refresh" | "delete") {
                        self.models_loaded = false;
                    }
                    Ok(result)
                }
                _ => anyhow::bail!(
                    "supported /memory mm commands: list,show,history,seed,refresh,delete"
                ),
            }
        })())
    }
    fn clear(&mut self, op: &Operation, _: &mut dyn MemoryModelRunner) -> Result<Value> {
        op.check()?;
        let tx = self
            .db
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        tx.execute("DELETE FROM pending WHERE endpoint=?1 AND bank=?2 AND project=?3 AND session=?4 AND branch=?5",params![self.config.endpoint,op.scope.retain_bank,op.scope.project,op.authority.session_id,op.authority.branch_id])?;
        tx.execute("DELETE FROM transcripts WHERE endpoint=?1 AND bank=?2 AND project=?3 AND session=?4 AND branch=?5",params![self.config.endpoint,op.scope.retain_bank,op.scope.project,op.authority.session_id,op.authority.branch_id])?;
        op.check()?;
        tx.commit()?;
        self.models_loaded = false;
        self.model_context = json!([]);
        Ok(
            json!({"backend":"hindsight","localSessionCleared":true,"remoteCleared":false,"message":"Only local session retention/recall state reset; remote bank not deleted"}),
        )
    }
    fn queue(&mut self, op: &Operation) -> Result<Value> {
        let items:Vec<Value>=self.db.prepare("SELECT id,state,created FROM pending WHERE endpoint=?1 AND bank=?2 ORDER BY created LIMIT 50")?.query_map(params![self.config.endpoint,op.scope.retain_bank],|row|Ok(json!({"id":row.get::<_,String>(0)?,"state":row.get::<_,String>(1)?,"created":row.get::<_,i64>(2)?})))?.collect::<rusqlite::Result<_>>()?;
        Ok(json!({"backend":"hindsight","items":items}))
    }
}

#[cfg(test)]
mod tests;
