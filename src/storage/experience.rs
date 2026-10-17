//! Project-scoped, bounded experience from observed calls and passing executable checks.
//! Retrieval is advisory. Recipes recheck each step's hashes and use ordinary tool approvals.
use crate::config::paths::Layout;
use crate::storage::memory::screen;
use crate::tools::Workspace;
use anyhow::{Result, ensure};
use parking_lot::Mutex;
use rusqlite::{Connection, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::Read,
    path::{Component, Path},
    time::Duration,
};

use crate::platform::fs as platform_fs;

pub const MAX_STEPS: usize = 16;
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Step {
    pub tool: String,
    pub args: Value,
    pub sources: BTreeMap<String, Option<String>>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Recipe {
    pub objective: String,
    pub model: String,
    pub session: String,
    pub steps: Vec<Step>,
    pub checks: Vec<String>,
    pub lesson: Option<String>,
}
pub struct Experience {
    conn: Mutex<Connection>,
}

fn digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}
pub fn safe_path(path: &str) -> bool {
    !path.is_empty()
        && Path::new(path)
            .components()
            .all(|part| matches!(part, Component::Normal(_)))
        && !path.split('/').any(|part| {
            part == ".dial" || part == ".git" || part == "private" || part.starts_with(".env")
        })
}
pub fn source_hash(ws: &Workspace, path: &str) -> Result<Option<String>> {
    ensure!(safe_path(path), "source path must be relative and public");
    let target = ws.resolve(path).map_err(anyhow::Error::msg)?;
    match fs::metadata(&target) {
        Ok(meta) => {
            ensure!(
                meta.is_file() && meta.len() <= 1024 * 1024,
                "source exceeds experience budget"
            );
            let mut bytes = Vec::new();
            fs::File::open(target)?
                .take(1024 * 1024 + 1)
                .read_to_end(&mut bytes)?;
            ensure!(bytes.len() <= 1024 * 1024, "source grew beyond budget");
            Ok(Some(digest(&bytes)))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}
pub fn paths(tool: &str, args: &Value) -> Vec<String> {
    let mut paths = Vec::new();
    if let Some(path) = args["path"].as_str() {
        let path = path.split(':').next().unwrap_or(path);
        if safe_path(path) {
            paths.push(path.into());
        }
    }
    if tool == "edit"
        && let Some(input) = args["input"].as_str()
    {
        for line in input.lines().filter(|line| line.starts_with('[')) {
            if let Some((path, _)) = line.trim_start_matches('[').rsplit_once('#')
                && safe_path(path)
            {
                paths.push(path.into());
            }
        }
    }
    paths.sort();
    paths.dedup();
    paths
}
pub fn preconditions(ws: &Workspace, tool: &str, args: &Value) -> BTreeMap<String, Option<String>> {
    let paths = paths(tool, args);
    if paths.len() > 4 {
        return BTreeMap::new();
    }
    paths
        .into_iter()
        .map(|path| source_hash(ws, &path).map(|hash| (path, hash)))
        .collect::<Result<_>>()
        .unwrap_or_default()
}
pub fn applicable(ws: &Workspace, sources: &BTreeMap<String, Option<String>>) -> bool {
    !sources.is_empty()
        && sources.len() <= 4
        && sources
            .iter()
            .all(|(path, hash)| source_hash(ws, path).is_ok_and(|current| &current == hash))
}
pub fn terms(text: &str) -> BTreeSet<String> {
    text.split(|c: char| !c.is_alphanumeric() && c != '_')
        .filter(|s| s.len() > 2)
        .take(128)
        .map(str::to_lowercase)
        .collect()
}
pub fn failure_kind(text: &str) -> &'static str {
    let text = text.to_lowercase();
    if text.contains("syntax") || text.contains("parse") {
        "syntax"
    } else if text.contains("not found") || text.contains("no such file") {
        "missing_source"
    } else if text.contains("changed") || text.contains("stale") {
        "stale_source"
    } else if text.contains("invalid") || text.contains("required") {
        "tool_contract"
    } else if text.contains("denied") || text.contains("approval") {
        "permission"
    } else {
        "execution"
    }
}
impl Experience {
    pub fn open(layout: &Layout) -> Result<Self> {
        layout.ensure_project_outside_storage()?;
        match platform_fs::create_dir_owner_only(
            fs::DirBuilder::new().recursive(true),
            &layout.state_dir,
        ) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error.into()),
        }
        let state_meta = fs::symlink_metadata(&layout.state_dir)?;
        ensure!(
            state_meta.is_dir() && !state_meta.file_type().is_symlink(),
            "state directory must be a regular directory"
        );
        platform_fs::set_owner_only(&layout.state_dir)?;
        let path = layout.state_dir.join("experience.sqlite3");
        let mut options = fs::OpenOptions::new();
        options.create(true).read(true).write(true);
        let _file = platform_fs::open_owner_only(&options, &path)?;
        platform_fs::set_owner_only(&path)?;
        let conn = Connection::open(&path)?;
        conn.busy_timeout(Duration::from_millis(200))?;
        conn.execute_batch("PRAGMA journal_mode=WAL; CREATE TABLE IF NOT EXISTS experience (
            id TEXT PRIMARY KEY, kind TEXT NOT NULL, payload TEXT NOT NULL, model TEXT NOT NULL,
            impressions INTEGER NOT NULL DEFAULT 0, uses INTEGER NOT NULL DEFAULT 0,
            verified_uses INTEGER NOT NULL DEFAULT 0, at INTEGER NOT NULL);
            CREATE TABLE IF NOT EXISTS tool_outcomes(model TEXT NOT NULL,tool TEXT NOT NULL,ok INTEGER NOT NULL DEFAULT 0,errors INTEGER NOT NULL DEFAULT 0,PRIMARY KEY(model,tool));")?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }
    fn put(&self, kind: &str, model: &str, payload: Value) -> Result<String> {
        let raw = payload.to_string();
        ensure!(raw.len() <= 64 * 1024, "experience record exceeds budget");
        screen_record(&payload)?;
        let id = digest(format!("{kind}:{raw}").as_bytes());
        let conn = self.conn.lock();
        let at = i64::try_from(crate::learn::improve::now()).unwrap_or(i64::MAX);
        conn.execute(
            "INSERT INTO experience(id,kind,payload,model,at) VALUES(?1,?2,?3,?4,?5) ON CONFLICT(id) DO UPDATE SET at=excluded.at",
            params![id, kind, raw, model, at],
        )?;
        conn.execute("DELETE FROM experience WHERE id IN (SELECT id FROM experience ORDER BY at DESC LIMIT -1 OFFSET 256)",[])?;
        Ok(id)
    }
    pub fn recipe(&self, recipe: &Recipe) -> Result<String> {
        ensure!(
            !recipe.steps.is_empty()
                && recipe.steps.len() <= MAX_STEPS
                && !recipe.checks.is_empty(),
            "recipe requires observed steps and passing checks"
        );
        validate_recipe(recipe)?;
        self.put("recipe", &recipe.model, serde_json::to_value(recipe)?)
    }
    pub fn failure(
        &self,
        model: &str,
        tool: &str,
        args: &Value,
        sources: &BTreeMap<String, Option<String>>,
        kind: &str,
    ) -> Result<()> {
        if sources.is_empty() {
            return Ok(());
        }
        // Store structured conditions, never raw stderr, credentials, or executable advice from output.
        self.put("failure",model,json!({"tool":tool,"fields":args.as_object().map(|v|v.keys().collect::<Vec<_>>()),"sources":sources,"kind":kind}))?;
        Ok(())
    }
    pub fn observe_tool(&self, model: &str, tool: &str, ok: bool) -> Result<()> {
        self.conn.lock().execute("INSERT INTO tool_outcomes(model,tool,ok,errors) VALUES(?1,?2,?3,?4) ON CONFLICT(model,tool) DO UPDATE SET ok=ok+excluded.ok,errors=errors+excluded.errors",params![model,tool,i64::from(ok),i64::from(!ok)])?;
        Ok(())
    }
    pub fn tool_scores(&self, model: &str) -> Result<BTreeMap<String, f64>> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare("SELECT tool,ok,errors FROM tool_outcomes WHERE model=?1")?;
        Ok(stmt
            .query_map([model], |r| {
                let ok = r.get::<_, i64>(1)?;
                let errors = r.get::<_, i64>(2)?;
                Ok((r.get(0)?, (ok + 1) as f64 / (ok + errors + 2) as f64))
            })?
            .collect::<rusqlite::Result<_>>()?)
    }
    pub fn get_recipe(&self, id: &str) -> Result<Recipe> {
        let raw: String = self.conn.lock().query_row(
            "SELECT payload FROM experience WHERE id=?1 AND kind='recipe'",
            [id],
            |r| r.get(0),
        )?;
        let recipe: Recipe = serde_json::from_str(&raw)?;
        screen_record(&serde_json::from_str(&raw)?)?;
        validate_recipe(&recipe)?;
        Ok(recipe)
    }
    pub fn recall(
        &self,
        query: &str,
        model: &str,
        ws: &Workspace,
        budget: usize,
    ) -> Result<(String, Vec<String>)> {
        let candidates = self.inspect()?;
        let query = terms(query);
        let mut ranked = Vec::new();
        let mut cheap = candidates["entries"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|entry| screen_record(&entry["payload"]).is_ok())
            .map(|entry| {
                (
                    terms(&entry["payload"].to_string())
                        .intersection(&query)
                        .count(),
                    entry,
                )
            })
            .collect::<Vec<_>>();
        cheap.sort_by_key(|(score, _)| std::cmp::Reverse(*score));
        let mut hashes = BTreeMap::new();
        let mut remaining = 4 * 1024 * 1024u64;
        for (_, entry) in cheap.into_iter().take(8) {
            let payload = &entry["payload"];
            let overlap = terms(&payload.to_string()).intersection(&query).count();
            if overlap == 0 {
                continue;
            }
            let conditions: BTreeMap<String, Option<String>> = if entry["kind"] == "failure" {
                serde_json::from_value(payload["sources"].clone()).unwrap_or_default()
            } else {
                payload["steps"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|step| {
                        serde_json::from_value::<BTreeMap<String, Option<String>>>(
                            step["sources"].clone(),
                        )
                        .ok()
                    })
                    .fold(BTreeMap::new(), |mut all, step| {
                        for (path, hash) in step {
                            all.entry(path).or_insert(hash);
                        }
                        all
                    })
            };
            let exact = !conditions.is_empty()
                && conditions.len() <= 16
                && conditions.iter().all(|(path, expected)| {
                    if let Some(hash) = hashes.get(path) {
                        return hash == expected;
                    }
                    let Some(target) = ws.resolve(path).ok() else {
                        return false;
                    };
                    let size = fs::metadata(target).map(|m| m.len()).unwrap_or(0);
                    if size > remaining {
                        return false;
                    }
                    remaining -= size;
                    match source_hash(ws, path) {
                        Ok(hash) => {
                            let matches = &hash == expected;
                            hashes.insert(path.clone(), hash);
                            matches
                        }
                        Err(_) => false,
                    }
                });
            if entry["kind"] == "failure" && !exact {
                continue;
            }
            let utility = (entry["verified_uses"].as_f64().unwrap_or(0.0) + 1.0)
                / (entry["uses"].as_f64().unwrap_or(0.0) + 2.0);
            let rank = overlap as f64 + utility + if entry["model"] == model { 0.25 } else { 0.0 };
            let id = entry["id"].as_str().unwrap_or("").to_owned();
            let text = if entry["kind"] == "recipe" {
                format!(
                    "Recipe {id}: {} · source state {} · steps {} · checks {} · lesson {}",
                    payload["objective"],
                    if exact {
                        "matches initial preconditions"
                    } else {
                        "analogous; cannot replay until preconditions match"
                    },
                    payload["steps"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .map(|s| s["tool"].as_str().unwrap_or("?"))
                        .collect::<Vec<_>>()
                        .join(" → "),
                    payload["checks"],
                    payload["lesson"]
                )
            } else {
                format!(
                    "Failure condition {id}: {} failed as {} with fields {} under source hashes {}. Recheck prerequisites before repeating.",
                    payload["tool"], payload["kind"], payload["fields"], payload["sources"]
                )
            };
            ranked.push((rank, id, text));
        }
        ranked.sort_by(|a, b| b.0.total_cmp(&a.0));
        let mut text = String::new();
        let mut ids = Vec::new();
        for (_, id, line) in ranked.into_iter().take(3) {
            if budget != 0 && text.len() + line.len() + 1 > budget {
                continue;
            }
            text.push_str(&line);
            text.push('\n');
            ids.push(id);
        }
        let conn = self.conn.lock();
        for id in &ids {
            conn.execute(
                "UPDATE experience SET impressions=impressions+1 WHERE id=?1",
                [id],
            )?;
        }
        Ok((text, ids))
    }
    pub fn credit(&self, id: &str, verified: bool) -> Result<()> {
        self.conn.lock().execute(
            "UPDATE experience SET uses=uses+1,verified_uses=verified_uses+?2 WHERE id=?1",
            params![id, i64::from(verified)],
        )?;
        Ok(())
    }
    pub fn inspect(&self) -> Result<Value> {
        let conn = self.conn.lock();
        let mut stmt=conn.prepare("SELECT id,kind,payload,model,impressions,uses,verified_uses FROM experience ORDER BY at DESC LIMIT 256")?;
        let entries=stmt.query_map([],|r|Ok(json!({"id":r.get::<_,String>(0)?,"kind":r.get::<_,String>(1)?,"payload":serde_json::from_str::<Value>(&r.get::<_,String>(2)?).unwrap_or(Value::Null),"model":r.get::<_,String>(3)?,"impressions":r.get::<_,i64>(4)?,"uses":r.get::<_,i64>(5)?,"verified_uses":r.get::<_,i64>(6)?})))?.collect::<rusqlite::Result<Vec<_>>>()?;
        let mut outcomes = conn.prepare(
            "SELECT model,tool,ok,errors FROM tool_outcomes ORDER BY ok+errors DESC LIMIT 64",
        )?;
        let outcomes=outcomes.query_map([],|r|Ok(json!({"model":r.get::<_,String>(0)?,"tool":r.get::<_,String>(1)?,"successes":r.get::<_,i64>(2)?,"errors":r.get::<_,i64>(3)?})))?.collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(
            json!({"tool_outcomes":outcomes,"entries":entries,"scope":"project-scoped observed experience; utility is observed reuse, not causal attribution; memories are advisory"}),
        )
    }
}

fn proof(value: &Value) -> bool {
    value.is_null()
        || value
            .as_str()
            .is_some_and(|s| s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit()))
}
fn redact_proofs(value: &mut Value) -> Result<()> {
    if let Some(sources) = value.get_mut("sources").and_then(Value::as_object_mut) {
        for (path, hash) in sources {
            ensure!(safe_path(path) && proof(hash), "invalid source proof");
            *hash = json!("source fingerprint");
        }
    }
    Ok(())
}
/// Screen text and executable arguments while recognizing machine-generated source fingerprints.
/// Hash fields are checked structurally, never used as an exemption for source text or arguments.
pub(crate) fn screen_record(payload: &Value) -> Result<()> {
    let mut screened = payload.clone();
    redact_proofs(&mut screened)?;
    if let Some(steps) = screened.get_mut("steps").and_then(Value::as_array_mut) {
        for step in steps {
            redact_proofs(step)?;
        }
    }
    if let Some(observations) = screened
        .get_mut("observations")
        .and_then(Value::as_array_mut)
    {
        for observation in observations {
            redact_proofs(observation)?;
            for field in ["id", "digest"] {
                if let Some(value) = observation.get_mut(field) {
                    ensure!(proof(value), "invalid observation fingerprint");
                    *value = json!("observation fingerprint");
                }
            }
        }
    }
    if let Some(hypotheses) = screened.get_mut("hypotheses").and_then(Value::as_array_mut) {
        for hypothesis in hypotheses {
            if let Some(value) = hypothesis.get_mut("evidence") {
                ensure!(proof(value), "invalid evidence fingerprint");
                *value = json!("observation fingerprint");
            }
        }
    }
    screen(&screened.to_string()).map_err(anyhow::Error::msg)
}

fn validate_recipe(recipe: &Recipe) -> Result<()> {
    ensure!(
        !recipe.steps.is_empty()
            && recipe.steps.len() <= MAX_STEPS
            && !recipe.checks.is_empty()
            && recipe.checks.len() <= 8,
        "recipe requires bounded observed steps and checks"
    );
    ensure!(
        recipe.objective.len() <= 4096 && recipe.checks.iter().all(|s| s.len() <= 2048),
        "recipe metadata exceeds budget"
    );
    for step in &recipe.steps {
        ensure!(
            matches!(
                step.tool.as_str(),
                "read" | "grep" | "glob" | "write" | "ast_search"
            ),
            "unsupported recipe operation"
        );
        ensure!(
            !step.sources.is_empty()
                && step.sources.len() <= 4
                && step.sources.keys().all(|p| safe_path(p)),
            "recipe requires complete bounded source preconditions"
        );
        ensure!(
            step.args["path"]
                .as_str()
                .is_some_and(|path| !paths(&step.tool, &step.args).is_empty()
                    && step
                        .sources
                        .contains_key(path.split(':').next().unwrap_or(path))),
            "recipe inspection requires explicit source scope"
        );
        ensure!(
            paths(&step.tool, &step.args)
                .iter()
                .all(|p| step.sources.contains_key(p)),
            "recipe source conditions do not cover arguments"
        );
        if step.tool == "write" {
            ensure!(
                step.args["path"]
                    .as_str()
                    .is_some_and(|p| step.sources.contains_key(p))
                    && step.args["content"]
                        .as_str()
                        .is_some_and(|s| s.len() <= 1024 * 1024),
                "invalid recipe write"
            );
        }
    }
    Ok(())
}
pub fn render(report: &Value) -> String {
    let entries = report["entries"]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or(&[]);
    let mut out = format!(
        "Project experience: {} records. No model loaded.\nObserved reuse counts are not causal accuracy scores.\n\n",
        entries.len()
    );
    for entry in entries.iter().take(20) {
        let payload = &entry["payload"];
        out.push_str(&format!(
            "{} {}\n  {}\n  shown {} · used {} · passed configured checks {}\n",
            entry["kind"].as_str().unwrap_or("?"),
            entry["id"].as_str().unwrap_or("?"),
            if entry["kind"] == "recipe" {
                payload["objective"].as_str().unwrap_or("").to_owned()
            } else {
                format!("{}: {} (source-scoped)", payload["tool"], payload["kind"])
            },
            entry["impressions"],
            entry["uses"],
            entry["verified_uses"]
        ));
    }
    out.push_str("\n/experience ledger shows this session's observations and hypotheses. Full export: dial experience --json.\nRecipes require matching source hashes, current configured behavioral checks and normal tool approvals.\nConfigure verification commands and mark their names behavioral in /settings → Tools → Verification.\n");
    out
}

#[cfg(test)]
mod tests;
