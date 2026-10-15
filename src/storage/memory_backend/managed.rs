use super::{Operation, clean, files::PrivateDirectory, redact};
use crate::storage::{
    paths::Layout,
    skills::{self, Scope, Skill},
};
use anyhow::{Context, Result, ensure};
use rusqlite::{OptionalExtension, params};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    cell::{Cell, RefCell},
    collections::{BTreeMap, BTreeSet},
    path::PathBuf,
};
pub fn directory_path(layout: &Layout) -> PathBuf {
    layout
        .user_dir
        .join("memory-backends")
        .join("managed-skills")
        .join(format!(
            "{:x}",
            Sha256::digest(layout.project_root.as_os_str().as_encoded_bytes())
        ))
}
fn root(layout: &Layout) -> Result<PrivateDirectory> {
    PrivateDirectory::open(layout, "managed-skills")?.child(
        directory_path(layout)
            .file_name()
            .context("managed root missing")?
            .to_str()
            .context("managed root must be UTF-8")?,
    )
}
fn existing_root(layout: &Layout) -> Result<Option<PrivateDirectory>> {
    let Some(base) = PrivateDirectory::existing(layout, "managed-skills")? else {
        return Ok(None);
    };
    match base.existing_child(
        directory_path(layout)
            .file_name()
            .context("managed root missing")?
            .to_str()
            .context("managed root must be UTF-8")?,
    ) {
        Ok(value) => Ok(Some(value)),
        Err(error)
            if error
                .downcast_ref::<std::io::Error>()
                .is_some_and(|error| error.kind() == std::io::ErrorKind::NotFound) =>
        {
            Ok(None)
        }
        Err(error) => Err(error),
    }
}
fn name(value: &str) -> Result<()> {
    ensure!(
        !value.is_empty()
            && value.len() <= 64
            && value
                .bytes()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'-')
            && value.as_bytes()[0] != b'-',
        "managed skill name must be lowercase kebab-case, at most64bytes"
    );
    Ok(())
}
fn asset_path(value: &str) -> Result<()> {
    ensure!(
        !value.is_empty()
            && value.len() <= 512
            && value.split('/').all(|part| !part.is_empty()
                && part != "."
                && part != ".."
                && !part.contains('\\')
                && !part.chars().any(char::is_control)),
        "managed asset escapes private skill"
    );
    Ok(())
}
fn database(directory: &PrivateDirectory) -> Result<rusqlite::Connection> {
    let db = directory.database("skills.sqlite3")?;
    db.execute_batch("CREATE TABLE IF NOT EXISTS skills(name TEXT PRIMARY KEY,description TEXT NOT NULL,body TEXT NOT NULL,generated INTEGER NOT NULL,files TEXT NOT NULL,revision INTEGER NOT NULL,version TEXT NOT NULL);")?;
    Ok(db)
}
fn manifests_db(db: &rusqlite::Connection) -> Result<BTreeMap<String, (i64, Value)>> {
    let rows: Vec<(String, i64, String)> = db
        .prepare("SELECT name,revision,files FROM skills")?
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?
        .collect::<rusqlite::Result<_>>()?;
    rows.into_iter()
        .map(|(name, revision, files)| Ok((name, (revision, serde_json::from_str(&files)?))))
        .collect()
}
fn manifests(layout: &Layout) -> Result<BTreeMap<String, (i64, Value)>> {
    let Some(directory) = existing_root(layout)? else {
        return Ok(BTreeMap::new());
    };
    let Some(db) = directory.database_readonly("skills.sqlite3")? else {
        return Ok(BTreeMap::new());
    };
    manifests_db(&db)
}
fn affected_targets(layout: &Layout, manual: Option<&BTreeSet<&str>>) -> Result<Vec<PathBuf>> {
    let Some(directory) = existing_root(layout)? else {
        return Ok(Vec::new());
    };
    let Some(db) = directory.database_readonly("skills.sqlite3")? else {
        return Ok(Vec::new());
    };
    let rows: Vec<(String, String, String, bool)> = db
        .prepare("SELECT name,version,files,generated FROM skills")?
        .query_map([], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
        })?
        .collect::<rusqlite::Result<_>>()?;
    let mut targets = Vec::new();
    for (skill, version, files, generated) in rows {
        if !manual.map_or(generated, |names| names.contains(skill.as_str())) {
            continue;
        }
        name(&skill)?;
        ensure!(
            version.len() == 32 && version.bytes().all(|byte| byte.is_ascii_hexdigit()),
            "invalid immutable managed generation"
        );
        let base = directory
            .path()
            .join("generations")
            .join(version)
            .join(skill);
        targets.push(base.clone());
        targets.push(base.join("SKILL.md"));
        let files: Value = serde_json::from_str(&files)?;
        for bucket in ["scripts", "templates", "examples"] {
            if let Some(files) = files[bucket].as_array() {
                for file in files {
                    let path = file["path"]
                        .as_str()
                        .context("managed asset path missing")?;
                    asset_path(path)?;
                    targets.push(base.join(bucket).join(path));
                }
            }
        }
    }
    Ok(targets)
}
pub fn consolidation_context(layout: &Layout) -> Result<Vec<Value>> {
    let Some(directory) = existing_root(layout)? else {
        return Ok(Vec::new());
    };
    let Some(db) = directory.database_readonly("skills.sqlite3")? else {
        return Ok(Vec::new());
    };
    let values:Vec<Value>=db.prepare("SELECT name,description,body,generated FROM skills ORDER BY name")?.query_map([],|row|Ok(json!({"name":row.get::<_,String>(0)?,"description":row.get::<_,String>(1)?,"body":row.get::<_,String>(2)?,"generated":row.get::<_,bool>(3)?})))?.collect::<rusqlite::Result<_>>()?;
    Ok(values)
}
pub struct Publication {
    path: PathBuf,
    generation: String,
    targets: Vec<PathBuf>,
    previous: BTreeMap<String, (i64, Value)>,
    staging: RefCell<Option<PrivateDirectory>>,
    committed: Cell<bool>,
}
impl Drop for Publication {
    fn drop(&mut self) {
        if !self.committed.get() {
            if let Some(generations) = self.staging.get_mut().take() {
                let _ = generations.remove_tree(&self.generation);
            }
        }
    }
}
impl Publication {
    pub fn targets(&self) -> &[PathBuf] {
        &self.targets
    }
    fn stage(
        &self,
        directory: &PrivateDirectory,
        op: &Operation,
        skill_name: &str,
        description: &str,
        body: &str,
        files: &Value,
    ) -> Result<()> {
        op.check()?;
        let generations = directory.child("generations")?;
        *self.staging.borrow_mut() = Some(generations.clone());
        let skill = generations.child(&self.generation)?.child(skill_name)?;
        skill.write(
            "SKILL.md",
            format!(
                "---\nname: {skill_name}\ndescription: {}\n---\n\n{body}\n",
                serde_json::to_string(description)?
            )
            .as_bytes(),
        )?;
        for bucket in ["scripts", "templates", "examples"] {
            if let Some(entries) = files[bucket].as_array() {
                let base = skill.child(bucket)?;
                for entry in entries {
                    let relative = entry["path"]
                        .as_str()
                        .context("managed asset path missing")?;
                    asset_path(relative)?;
                    let mut parts = relative.split('/').peekable();
                    let mut parent = base.clone();
                    while let Some(part) = parts.next() {
                        op.check()?;
                        if parts.peek().is_some() {
                            parent = parent.child(part)?;
                        } else {
                            parent.write(
                                part,
                                redact(
                                    entry["content"]
                                        .as_str()
                                        .context("managed asset content missing")?,
                                )
                                .as_bytes(),
                            )?;
                        }
                    }
                }
            }
        }
        op.check()?;
        Ok(())
    }
    pub fn write(&self, layout: &Layout, op: &Operation, skill: &Value) -> Result<()> {
        op.check()?;
        ensure!(
            directory_path(layout) == self.path,
            "managed publication changed operator root"
        );
        let value = skill["name"].as_str().context("learn skill name missing")?;
        name(value)?;
        ensure!(
            !skills::discover_all(layout)
                .iter()
                .any(|authored| authored.name == value),
            "managed skill {value} conflicts with authored skill; authored files untouched"
        );
        let description = clean(
            skill["description"]
                .as_str()
                .context("learn skill description missing")?,
            1024,
        )?;
        ensure!(
            !description.contains('\n'),
            "skill description must be one line"
        );
        let body = clean(
            skill["body"].as_str().context("learn skill body missing")?,
            65536,
        )?;
        let directory = root(layout)?;
        let mut db = database(&directory)?;
        let tx = db.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let revision: Option<i64> = tx
            .query_row(
                "SELECT revision FROM skills WHERE name=?1",
                [value],
                |row| row.get(0),
            )
            .optional()?;
        ensure!(
            revision == self.previous.get(value).map(|(revision, _)| *revision),
            "managed skill changed after target planning; memory saved, skill not overwritten"
        );
        match skill["action"].as_str() {
            Some("create") => ensure!(revision.is_none(), "managed skill exists; use update"),
            Some("update") => ensure!(revision.is_some(), "managed skill missing; use create"),
            _ => anyhow::bail!("learn skill action must be create or update"),
        }
        let files = self
            .previous
            .get(value)
            .map(|(_, files)| files.clone())
            .unwrap_or_else(|| json!({}));
        self.stage(&directory, op, value, &description, &body, &files)?;
        op.check()?;
        tx.execute("INSERT INTO skills(name,description,body,generated,files,revision,version)VALUES(?1,?2,?3,0,?4,1,?5)ON CONFLICT(name)DO UPDATE SET description=excluded.description,body=excluded.body,generated=0,files=excluded.files,revision=revision+1,version=excluded.version",params![value,description,body,serde_json::to_string(&files)?,self.generation])?;
        op.check()?;
        tx.commit()?;
        self.committed.set(true);
        Ok(())
    }
    pub fn publish_generated(
        &self,
        layout: &Layout,
        op: &Operation,
        values: &[Value],
    ) -> Result<()> {
        op.check()?;
        ensure!(
            directory_path(layout) == self.path,
            "generated publication changed operator root"
        );
        validate_generated(values)?;
        let directory = root(layout)?;
        let mut db = database(&directory)?;
        let tx = db.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        ensure!(
            manifests_db(&tx)? == self.previous,
            "managed library changed after target planning"
        );
        let stale = generated_versions(&tx)?;
        tx.execute("DELETE FROM skills WHERE generated=1", [])?;
        let authored = skills::discover_all(layout);
        for value in values {
            let skill_name = value["name"]
                .as_str()
                .context("generated skill name missing")?;
            if authored.iter().any(|skill| skill.name == skill_name) {
                continue;
            }
            let manual: bool = tx.query_row(
                "SELECT count(*) FROM skills WHERE name=?1 AND generated=0",
                [skill_name],
                |row| row.get::<_, i64>(0),
            )? > 0;
            if manual {
                continue;
            }
            let body = redact(
                value["content"]
                    .as_str()
                    .context("generated skill content missing")?,
            );
            let description = body
                .lines()
                .find(|line| !line.trim().is_empty())
                .unwrap_or(skill_name)
                .trim_start_matches('#')
                .trim()
                .chars()
                .take(1000)
                .collect::<String>();
            self.stage(&directory, op, skill_name, &description, &body, value)?;
            tx.execute("INSERT INTO skills(name,description,body,generated,files,revision,version)VALUES(?1,?2,?3,1,?4,1,?5)",params![skill_name,description,body,serde_json::to_string(&super::redact_value(value))?,self.generation])?;
        }
        op.check()?;
        tx.commit()?;
        self.committed.set(true);
        // Committed already: a failed cleanup of superseded trees must not report the publication as failed.
        if let Err(error) = prune_generations(&directory, &db, &stale) {
            eprintln!("warning: could not remove superseded memory skill generations: {error:#}");
        }
        Ok(())
    }
}
/// Pure target plan: no parent directory, manifest, journal or staging file is created before owner authorization.
pub fn prepare(layout: &Layout, values: &[Value]) -> Result<Publication> {
    layout.ensure_project_outside_storage()?;
    let path = directory_path(layout);
    let previous = manifests(layout)?;
    let generation = format!(
        "{:016x}{:016x}",
        rand::random::<u64>(),
        rand::random::<u64>()
    );
    let generations = path.join("generations");
    let generation_root = generations.join(&generation);
    let mut targets = vec![
        layout
            .user_dir
            .join("memory-backends")
            .join("managed-skills"),
        path.clone(),
        path.join("skills.sqlite3"),
        path.join("skills.sqlite3-journal"),
        generations,
        generation_root.clone(),
    ];
    let manual = values.iter().any(|value| value.get("action").is_some());
    let names: BTreeSet<&str> = values
        .iter()
        .filter_map(|value| value["name"].as_str())
        .collect();
    targets.extend(affected_targets(layout, manual.then_some(&names))?);
    for value in values {
        let skill_name = value["name"]
            .as_str()
            .context("managed skill name missing")?;
        name(skill_name)?;
        let base = generation_root.join(skill_name);
        targets.push(base.clone());
        targets.push(base.join("SKILL.md"));
        let files = if value["action"] == "update" {
            previous
                .get(skill_name)
                .map(|(_, files)| files)
                .unwrap_or(value)
        } else {
            value
        };
        for bucket in ["scripts", "templates", "examples"] {
            if let Some(files) = files[bucket].as_array() {
                targets.push(base.join(bucket));
                for file in files {
                    let relative = file["path"]
                        .as_str()
                        .context("managed asset path missing")?;
                    asset_path(relative)?;
                    let mut parent = base.join(bucket);
                    let mut parts = relative.split('/').peekable();
                    while let Some(part) = parts.next() {
                        parent.push(part);
                        targets.push(parent.clone());
                        if parts.peek().is_none() {
                            break;
                        }
                    }
                }
            }
        }
    }
    targets.sort();
    targets.dedup();
    Ok(Publication {
        path,
        generation,
        targets,
        previous,
        staging: RefCell::new(None),
        committed: Cell::new(false),
    })
}
pub fn validate_generated(values: &[Value]) -> Result<()> {
    let mut seen = BTreeSet::new();
    for value in values {
        let skill_name = value["name"]
            .as_str()
            .context("generated skill name missing")?;
        name(skill_name)?;
        ensure!(seen.insert(skill_name), "duplicate generated skill name");
        ensure!(
            value["content"]
                .as_str()
                .is_some_and(|body| !body.trim().is_empty() && body.len() <= 65536),
            "generated skill body missing, blank or too large"
        );
        for bucket in ["scripts", "templates", "examples"] {
            if let Some(value) = value.get(bucket).filter(|value| !value.is_null()) {
                let files = value
                    .as_array()
                    .context("generated asset collection must be an array")?;
                ensure!(files.len() <= 32, "generated asset count exceeds bound");
                let mut paths = BTreeSet::new();
                for file in files {
                    let relative = file["path"]
                        .as_str()
                        .context("generated asset path missing")?;
                    asset_path(relative)?;
                    ensure!(paths.insert(relative), "duplicate generated asset path");
                    ensure!(
                        file["content"]
                            .as_str()
                            .is_some_and(|text| text.len() <= 65536),
                        "generated asset missing or too large"
                    );
                }
            }
        }
    }
    Ok(())
}
/// `(name, version)` of every generated row, read before a replacement deletes them.
fn generated_versions(db: &rusqlite::Connection) -> Result<Vec<(String, String)>> {
    Ok(db
        .prepare("SELECT name,version FROM skills WHERE generated=1")?
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
        .collect::<rusqlite::Result<_>>()?)
}
fn is_not_found(error: &anyhow::Error) -> bool {
    error
        .downcast_ref::<std::io::Error>()
        .is_some_and(|error| error.kind() == std::io::ErrorKind::NotFound)
}
/// Removes superseded generation trees after commit; a tree still referenced by any row is kept.
fn prune_generations(
    directory: &PrivateDirectory,
    db: &rusqlite::Connection,
    stale: &[(String, String)],
) -> Result<()> {
    let generations = match directory.existing_child("generations") {
        Ok(generations) => generations,
        Err(error) if is_not_found(&error) => return Ok(()),
        Err(error) => return Err(error),
    };
    for (name, version) in stale {
        let live: i64 = db.query_row(
            "SELECT count(*) FROM skills WHERE version=?1",
            params![version],
            |row| row.get(0),
        )?;
        if live == 0 {
            generations.remove_tree(version)?;
            continue;
        }
        let named: i64 = db.query_row(
            "SELECT count(*) FROM skills WHERE name=?1 AND version=?2",
            params![name, version],
            |row| row.get(0),
        )?;
        if named == 0 {
            match generations.existing_child(version) {
                Ok(versioned) => versioned.remove_tree(name)?,
                Err(error) if is_not_found(&error) => {}
                Err(error) => return Err(error),
            }
        }
    }
    Ok(())
}
pub fn clear_generated(
    layout: &Layout,
    op: &Operation,
    runner: &mut dyn super::MemoryModelRunner,
) -> Result<()> {
    let Some(directory) = existing_root(layout)? else {
        return Ok(());
    };
    let Some(db) = directory.database_readonly("skills.sqlite3")? else {
        return Ok(());
    };
    drop(db);
    let previous = manifests(layout)?;
    let mut targets = affected_targets(layout, None)?;
    targets.extend([
        super::managed_root(layout),
        directory.path().to_owned(),
        directory.path().join("skills.sqlite3"),
        directory.path().join("skills.sqlite3-journal"),
    ]);
    targets.sort();
    targets.dedup();
    runner.authorize_managed_write(op, &targets)?;
    op.check()?;
    let mut db = database(&directory)?;
    let tx = db.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    ensure!(
        manifests_db(&tx)? == previous,
        "managed library changed before clear"
    );
    let stale = generated_versions(&tx)?;
    tx.execute("DELETE FROM skills WHERE generated=1", [])?;
    op.check()?;
    tx.commit()?;
    // Committed already: a failed cleanup of superseded trees must not report the clear as failed.
    if let Err(error) = prune_generations(&directory, &db, &stale) {
        eprintln!("warning: could not remove superseded memory skill generations: {error:#}");
    }
    Ok(())
}
/// Only committed manifest generations are discoverable; cancelled staging is deleted by its owner.
pub fn discover(layout: &Layout) -> Result<Vec<Skill>> {
    (|| -> Result<Vec<Skill>> {
        let Some(directory) = existing_root(layout)? else {
            return Ok(Vec::new());
        };
        let Some(db) = directory.database_readonly("skills.sqlite3")? else {
            return Ok(Vec::new());
        };
        let values: Vec<(String, String, String)> = db
            .prepare("SELECT name,description,version FROM skills ORDER BY name")?
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?
            .collect::<rusqlite::Result<_>>()?;
        Ok(values
            .into_iter()
            .map(|(name, description, version)| Skill {
                path: directory
                    .path()
                    .join("generations")
                    .join(version)
                    .join(&name)
                    .join("SKILL.md"),
                name,
                description,
                source: Scope::User,
                origin: "Dial",
            })
            .collect())
    })()
}
pub struct ManagedContent {
    pub text: String,
    pub files: Vec<(String, u64)>,
    pub truncated: bool,
}
pub fn read_skill(
    layout: &Layout,
    skill: &Skill,
    file: Option<&str>,
) -> Option<Result<ManagedContent>> {
    if !skill.path.starts_with(directory_path(layout)) {
        return None;
    }
    Some((|| {
        let directory = existing_root(layout)?.context("managed directory missing")?;
        let db = directory
            .database_readonly("skills.sqlite3")?
            .context("managed manifest missing")?;
        let row: Option<(String, String)> = db
            .query_row(
                "SELECT version,files FROM skills WHERE name=?1",
                [&skill.name],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        let (version, files) = row.context("managed skill no longer installed")?;
        ensure!(
            skill.path
                == directory
                    .path()
                    .join("generations")
                    .join(&version)
                    .join(&skill.name)
                    .join("SKILL.md"),
            "managed skill revision changed; rediscover before reading"
        );
        name(&skill.name)?;
        let body = directory
            .existing_child("generations")?
            .existing_child(&version)?
            .existing_child(&skill.name)?;
        let files: Value = serde_json::from_str(&files)?;
        let mut listed = Vec::new();
        for bucket in ["scripts", "templates", "examples"] {
            if let Some(values) = files[bucket].as_array() {
                for value in values {
                    let path = value["path"]
                        .as_str()
                        .context("managed asset path missing")?;
                    asset_path(path)?;
                    listed.push((
                        format!("{bucket}/{path}"),
                        value["content"]
                            .as_str()
                            .context("managed asset content missing")?
                            .len() as u64,
                    ));
                }
            }
        }
        let relative = file.unwrap_or("SKILL.md");
        if relative != "SKILL.md" {
            ensure!(
                listed.iter().any(|(path, _)| path == relative),
                "managed read requires a declared private scripts/templates/examples asset"
            );
        }
        asset_path(relative)?;
        let mut parent = body;
        let mut parts = relative.split('/').peekable();
        while let Some(part) = parts.next() {
            if parts.peek().is_some() {
                parent = parent.existing_child(part)?;
            } else {
                let (bytes, truncated) = parent.read_head(part, 65536)?;
                let text = if truncated {
                    String::from_utf8_lossy(&bytes).into_owned()
                } else {
                    String::from_utf8(bytes).context("managed asset is not UTF-8")?
                };
                return Ok(ManagedContent {
                    text,
                    files: listed,
                    truncated,
                });
            }
        }
        anyhow::bail!("managed file missing")
    })())
}
