//! Path-scoped 1-line invariants and failure-triggered 1-line dead-end memory.
//!
//! Both record types live in the project's `codebase.sqlite3` database, are screened before
//! persistence and before emission, and are tied to workspace source digests so stale guidance
//! expires automatically instead of steering edits with outdated assumptions.
use super::*;

/// Default byte cap (~150-175 tokens) for one lazy guardrail attachment on a tool result.
pub const DEFAULT_ATTACH_BYTES: usize = 700;
const MAX_INVARIANTS: i64 = 256;
const MAX_INVARIANTS_PER_PATH: i64 = 8;
const MAX_DEAD_ENDS: i64 = 256;
const MAX_DEAD_ENDS_PER_PATH: i64 = 8;
const MAX_RULE_CHARS: usize = 240;
const MAX_SIGNATURE_CHARS: usize = 160;
const MAX_ATTACHED_INVARIANTS: usize = 3;
const MAX_ATTACHED_DEAD_ENDS: usize = 2;

#[derive(Debug, Clone)]
struct InvariantRow {
    id: String,
    path: String,
    rule: String,
    digest: String,
    created_at: i64,
}

#[derive(Debug, Clone)]
struct DeadEndRow {
    id: String,
    path: String,
    signature: String,
    summary: String,
    digest: String,
    created_at: i64,
}

pub(super) fn init_tables(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS codebase_invariants(
            id TEXT PRIMARY KEY,
            path TEXT NOT NULL,
            rule TEXT NOT NULL,
            digest TEXT NOT NULL,
            session_id TEXT NOT NULL,
            created_at INTEGER NOT NULL
        );
        CREATE INDEX IF NOT EXISTS idx_codebase_invariants_path ON codebase_invariants(path);
        CREATE TABLE IF NOT EXISTS codebase_dead_ends(
            id TEXT PRIMARY KEY,
            path TEXT NOT NULL,
            signature TEXT NOT NULL,
            summary TEXT NOT NULL,
            digest TEXT NOT NULL,
            session_id TEXT NOT NULL,
            created_at INTEGER NOT NULL
        );
        CREATE INDEX IF NOT EXISTS idx_codebase_dead_ends_path ON codebase_dead_ends(path);",
    )?;
    Ok(())
}

fn normalize_one_line(raw: &str, max_chars: usize, label: &str) -> Result<String> {
    ensure!(
        !raw.chars()
            .any(|c| c.is_control() && !matches!(c, '\n' | '\r' | '\t')),
        "{label} cannot contain control characters"
    );
    let cleaned = raw.split_whitespace().collect::<Vec<_>>().join(" ");
    ensure!(
        !cleaned.is_empty() && cleaned.chars().count() <= max_chars,
        "{label} must be 1..{max_chars} characters on one line"
    );
    screen(&cleaned).map_err(anyhow::Error::msg)?;
    Ok(cleaned)
}

fn normalize_optional_signature(raw: &str) -> Result<String> {
    if raw.trim().is_empty() {
        return Ok(String::new());
    }
    normalize_one_line(raw, MAX_SIGNATURE_CHARS, "signature")
}

fn short_id(prefix: &str, material: &str) -> String {
    let hex = format!("{:x}", Sha256::digest(material.as_bytes()));
    format!("{prefix}-{}", &hex[..12])
}

fn now_secs() -> i64 {
    i64::try_from(crate::runtime::clock::unix_secs()).unwrap_or(i64::MAX)
}

/// Distinct failing test names in common runner formats (Rust/libtest, pytest, Go, Jest/Vitest).
fn extract_failing_tests(text: &str) -> BTreeSet<String> {
    let mut names = BTreeSet::new();
    for line in text.lines() {
        let line = line.trim();
        let name = if let Some(rest) = line.strip_prefix("test ")
            && let Some(name) = rest.strip_suffix(" ... FAILED")
        {
            Some(name)
        } else if let Some(rest) = line.strip_prefix("FAILED ") {
            rest.split(" - ").next()
        } else if let Some(rest) = line.strip_prefix("--- FAIL: ") {
            rest.split_whitespace().next()
        } else {
            line.strip_prefix("✕ ").or_else(|| line.strip_prefix("× "))
        };
        if let Some(name) = name.map(str::trim).filter(|name| !name.is_empty()) {
            names.insert(name.to_owned());
        }
    }
    names
}

fn path_fresh(
    root: &Path,
    path: &str,
    saved_digest: &str,
    baseline_digests: &BTreeMap<String, String>,
    observed: &mut BTreeMap<String, Option<String>>,
) -> bool {
    let current = observed
        .entry(path.to_owned())
        .or_insert_with(|| source(root, path).map(|(_, digest)| digest));
    let Some(current_digest) = current.as_deref() else {
        return false;
    };
    current_digest == saved_digest
        || baseline_digests
            .get(path)
            .is_some_and(|baseline| baseline == saved_digest)
}

impl Codebase {
    /// Save a path-scoped 1-line invariant tied to the file's current content digest.
    pub fn remember_invariant(&self, path: &str, rule: &str, session: &str) -> Result<Value> {
        let path = self.source_path(path).with_context(|| {
            format!("supporting source `{path}` is not a path inside the workspace")
        })?;
        let rule = normalize_one_line(rule, MAX_RULE_CHARS, "invariant summary")?;
        let (_, digest) = self.current_source(&path)?;
        let id = short_id("inv", &format!("{path}\0{}", rule.to_ascii_lowercase()));
        let at = now_secs();
        let mut conn = self.conn.lock();
        let tx = conn.transaction()?;
        tx.execute(
            "INSERT INTO codebase_invariants(id,path,rule,digest,session_id,created_at)
             VALUES(?1,?2,?3,?4,?5,?6)
             ON CONFLICT(id) DO UPDATE SET
               rule=excluded.rule,
               digest=excluded.digest,
               session_id=excluded.session_id,
               created_at=excluded.created_at",
            params![id, path, rule, digest, session, at],
        )?;
        tx.execute(
            "DELETE FROM codebase_invariants WHERE id IN (
                SELECT id FROM codebase_invariants WHERE path=?1
                ORDER BY created_at DESC, id ASC LIMIT -1 OFFSET ?2
            )",
            params![path, MAX_INVARIANTS_PER_PATH],
        )?;
        tx.execute(
            "DELETE FROM codebase_invariants WHERE id IN (
                SELECT id FROM codebase_invariants
                ORDER BY created_at DESC, id ASC LIMIT -1 OFFSET ?1
            )",
            params![MAX_INVARIANTS],
        )?;
        tx.commit()?;
        Ok(json!({
            "saved": id,
            "kind": "invariant",
            "path": path,
            "summary": rule,
            "digest": digest,
        }))
    }

    /// Delete one saved invariant by its `guardrail_id`.
    pub fn forget_invariant(&self, id: &str) -> Result<Value> {
        let id = id.trim();
        ensure!(!id.is_empty(), "guardrail_id is required");
        let removed = self
            .conn
            .lock()
            .execute("DELETE FROM codebase_invariants WHERE id=?1", [id])?;
        ensure!(removed == 1, "invariant `{id}` not found");
        Ok(json!({"forgotten": id, "kind": "invariant"}))
    }

    /// Save a failure-triggered 1-line dead end tied to a workspace file's current digest
    /// and an optional failing test/error signature.
    pub fn remember_dead_end(
        &self,
        path: &str,
        signature: &str,
        summary: &str,
        session: &str,
    ) -> Result<Value> {
        let path = self.source_path(path).with_context(|| {
            format!("supporting source `{path}` is not a path inside the workspace")
        })?;
        let signature = normalize_optional_signature(signature)?;
        let summary = normalize_one_line(summary, MAX_RULE_CHARS, "dead-end summary")?;
        let (_, digest) = self.current_source(&path)?;
        let id = short_id(
            "dead",
            &format!(
                "{path}\0{}\0{}",
                signature.to_ascii_lowercase(),
                summary.to_ascii_lowercase()
            ),
        );
        let at = now_secs();
        let mut conn = self.conn.lock();
        let tx = conn.transaction()?;
        tx.execute(
            "INSERT INTO codebase_dead_ends(id,path,signature,summary,digest,session_id,created_at)
             VALUES(?1,?2,?3,?4,?5,?6,?7)
             ON CONFLICT(id) DO UPDATE SET
               signature=excluded.signature,
               summary=excluded.summary,
               digest=excluded.digest,
               session_id=excluded.session_id,
               created_at=excluded.created_at",
            params![id, path, signature, summary, digest, session, at],
        )?;
        tx.execute(
            "DELETE FROM codebase_dead_ends WHERE id IN (
                SELECT id FROM codebase_dead_ends WHERE path=?1
                ORDER BY created_at DESC, id ASC LIMIT -1 OFFSET ?2
            )",
            params![path, MAX_DEAD_ENDS_PER_PATH],
        )?;
        tx.execute(
            "DELETE FROM codebase_dead_ends WHERE id IN (
                SELECT id FROM codebase_dead_ends
                ORDER BY created_at DESC, id ASC LIMIT -1 OFFSET ?1
            )",
            params![MAX_DEAD_ENDS],
        )?;
        tx.commit()?;
        Ok(json!({
            "saved": id,
            "kind": "dead_end",
            "path": path,
            "signature": signature,
            "summary": summary,
            "digest": digest,
        }))
    }

    /// Delete one saved dead end by its `guardrail_id`.
    pub fn forget_dead_end(&self, id: &str) -> Result<Value> {
        let id = id.trim();
        ensure!(!id.is_empty(), "guardrail_id is required");
        let removed = self
            .conn
            .lock()
            .execute("DELETE FROM codebase_dead_ends WHERE id=?1", [id])?;
        ensure!(removed == 1, "dead end `{id}` not found");
        Ok(json!({"forgotten": id, "kind": "dead_end"}))
    }

    /// Inspect saved invariants and dead ends, reporting live freshness status for each entry.
    pub fn list_guardrails(&self, paths: Option<&[String]>, query: Option<&str>) -> Result<Value> {
        let filter_paths: BTreeSet<String> = paths
            .into_iter()
            .flatten()
            .filter_map(|p| self.source_path(p))
            .collect();
        let terms = query.map(navigation::tokens).unwrap_or_default();
        let (invariants, dead_ends) = {
            let conn = self.conn.lock();
            let inv_rows = conn
                .prepare(
                    "SELECT id,path,rule,digest,created_at FROM codebase_invariants
                     ORDER BY created_at DESC, id ASC",
                )?
                .query_map([], |r| {
                    Ok(InvariantRow {
                        id: r.get(0)?,
                        path: r.get(1)?,
                        rule: r.get(2)?,
                        digest: r.get(3)?,
                        created_at: r.get(4)?,
                    })
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            let dead_rows = conn
                .prepare(
                    "SELECT id,path,signature,summary,digest,created_at FROM codebase_dead_ends
                     ORDER BY created_at DESC, id ASC",
                )?
                .query_map([], |r| {
                    Ok(DeadEndRow {
                        id: r.get(0)?,
                        path: r.get(1)?,
                        signature: r.get(2)?,
                        summary: r.get(3)?,
                        digest: r.get(4)?,
                        created_at: r.get(5)?,
                    })
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            (inv_rows, dead_rows)
        };

        let empty_baselines = BTreeMap::new();
        let mut observed = BTreeMap::new();
        let matches_query =
            |text: &str| terms.is_empty() || !navigation::tokens(text).is_disjoint(&terms);

        let mut inv_out = Vec::new();
        for row in invariants {
            if screen(&row.path).is_err() || screen(&row.rule).is_err() {
                continue;
            }
            if !filter_paths.is_empty() && !filter_paths.contains(&row.path) {
                continue;
            }
            if !matches_query(&format!("{} {}", row.path, row.rule)) {
                continue;
            }
            let fresh = path_fresh(
                &self.root,
                &row.path,
                &row.digest,
                &empty_baselines,
                &mut observed,
            );
            inv_out.push(json!({
                "guardrail_id": row.id,
                "path": row.path,
                "summary": row.rule,
                "status": if fresh { "fresh" } else { "stale" },
                "created_at": row.created_at,
            }));
        }

        let mut dead_out = Vec::new();
        for row in dead_ends {
            if screen(&row.path).is_err()
                || screen(&row.signature).is_err()
                || screen(&row.summary).is_err()
            {
                continue;
            }
            if !filter_paths.is_empty() && !filter_paths.contains(&row.path) {
                continue;
            }
            if !matches_query(&format!("{} {} {}", row.path, row.signature, row.summary)) {
                continue;
            }
            let fresh = path_fresh(
                &self.root,
                &row.path,
                &row.digest,
                &empty_baselines,
                &mut observed,
            );
            dead_out.push(json!({
                "guardrail_id": row.id,
                "path": row.path,
                "signature": row.signature,
                "summary": row.summary,
                "status": if fresh { "fresh" } else { "stale" },
                "created_at": row.created_at,
            }));
        }

        Ok(json!({
            "invariants": inv_out,
            "dead_ends": dead_out,
        }))
    }

    /// Return a compact advisory footer of fresh invariants for `paths` not yet shown in the
    /// current history generation. Does zero filesystem I/O when no invariants exist for `paths`.
    pub fn invariants_for_paths(
        &self,
        paths: &[String],
        baseline_digests: &BTreeMap<String, String>,
        shown: &mut BTreeSet<String>,
        max_bytes: usize,
    ) -> Result<Option<String>> {
        let mut normalized = Vec::new();
        for path in paths {
            if let Some(rel) = self.source_path(path)
                && !normalized.contains(&rel)
            {
                normalized.push(rel);
            }
        }
        if normalized.is_empty() {
            return Ok(None);
        }
        let rows = {
            let conn = self.conn.lock();
            let mut stmt = conn.prepare(
                "SELECT id,path,rule,digest,created_at FROM codebase_invariants
                 WHERE path=?1 ORDER BY created_at DESC, id ASC",
            )?;
            let mut collected = Vec::new();
            for path in &normalized {
                for row in stmt.query_map([path], |r| {
                    Ok(InvariantRow {
                        id: r.get(0)?,
                        path: r.get(1)?,
                        rule: r.get(2)?,
                        digest: r.get(3)?,
                        created_at: r.get(4)?,
                    })
                })? {
                    collected.push(row?);
                }
            }
            collected
        };
        if rows.is_empty() {
            return Ok(None);
        }

        let budget = if max_bytes == 0 {
            DEFAULT_ATTACH_BYTES
        } else {
            max_bytes
        };
        let header = "[Invariants (advisory, source-verified)]\n";
        let mut observed = BTreeMap::new();
        let mut out = String::from(header);
        let mut attached = Vec::new();

        for row in rows {
            if attached.len() >= MAX_ATTACHED_INVARIANTS {
                break;
            }
            if shown.contains(&row.id) || screen(&row.path).is_err() || screen(&row.rule).is_err() {
                continue;
            }
            if !path_fresh(
                &self.root,
                &row.path,
                &row.digest,
                baseline_digests,
                &mut observed,
            ) {
                continue;
            }
            let line = format!("- INVARIANT [{}] ({}): {}\n", row.path, row.id, row.rule);
            if out.len() + line.len() > budget && !attached.is_empty() {
                break;
            }
            if out.len() + line.len() > budget {
                continue;
            }
            out.push_str(&line);
            attached.push(row.id);
        }
        if attached.is_empty() {
            return Ok(None);
        }
        shown.extend(attached);
        Ok(Some(out.trim_end().to_owned()))
    }

    /// Return a compact advisory footer of fresh dead ends matching a failed edit/verify/bash run.
    /// Never called on passing runs; does zero filesystem I/O when no dead ends are recorded.
    pub fn dead_ends_for_failure(
        &self,
        paths: &[String],
        failure_text: &str,
        baseline_digests: &BTreeMap<String, String>,
        shown: &mut BTreeSet<String>,
        max_bytes: usize,
    ) -> Result<Option<String>> {
        let touched: BTreeSet<String> = paths.iter().filter_map(|p| self.source_path(p)).collect();
        if touched.is_empty() && failure_text.trim().is_empty() {
            return Ok(None);
        }
        let rows = {
            let conn = self.conn.lock();
            conn.prepare(
                "SELECT id,path,signature,summary,digest,created_at FROM codebase_dead_ends
                 ORDER BY created_at DESC, id ASC",
            )?
            .query_map([], |r| {
                Ok(DeadEndRow {
                    id: r.get(0)?,
                    path: r.get(1)?,
                    signature: r.get(2)?,
                    summary: r.get(3)?,
                    digest: r.get(4)?,
                    created_at: r.get(5)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?
        };
        if rows.is_empty() {
            return Ok(None);
        }

        let failure_lower = failure_text.to_ascii_lowercase();
        let failure_tokens = navigation::tokens(failure_text);
        let failed_tests = extract_failing_tests(failure_text);
        let mut matched: Vec<(usize, DeadEndRow)> = Vec::new();
        let mut observed = BTreeMap::new();

        for row in rows {
            if shown.contains(&row.id)
                || screen(&row.path).is_err()
                || screen(&row.signature).is_err()
                || screen(&row.summary).is_err()
            {
                continue;
            }
            let path_touched = touched.contains(&row.path);
            let path_mentioned = !failure_lower.is_empty()
                && (failure_lower.contains(&row.path.to_ascii_lowercase())
                    || Path::new(&row.path)
                        .file_name()
                        .and_then(|name| name.to_str())
                        .is_some_and(|name| {
                            name.len() >= 4
                                && names_file(&failure_lower, &name.to_ascii_lowercase())
                        }));

            let score = if row.signature.is_empty() {
                if path_touched { 10 } else { 0 }
            } else {
                let sig_lower = row.signature.to_ascii_lowercase();
                let exact_sig = failure_lower.contains(&sig_lower)
                    || failed_tests.iter().any(|test| {
                        let test_lower = test.to_ascii_lowercase();
                        test_lower == sig_lower || test_lower.ends_with(&format!("::{sig_lower}"))
                    });
                let sig_tokens = navigation::tokens(&row.signature);
                let overlap = sig_tokens.intersection(&failure_tokens).count();
                let token_match =
                    !sig_tokens.is_empty() && overlap >= 2 && overlap * 2 >= sig_tokens.len();

                if path_touched && exact_sig {
                    100
                } else if path_mentioned && exact_sig {
                    80
                } else if path_touched && token_match {
                    60 + overlap.min(15)
                } else if exact_sig && row.signature.len() >= 6 {
                    40
                } else {
                    0
                }
            };
            if score == 0 {
                continue;
            }
            if !path_fresh(
                &self.root,
                &row.path,
                &row.digest,
                baseline_digests,
                &mut observed,
            ) {
                continue;
            }
            matched.push((score, row));
        }

        if matched.is_empty() {
            return Ok(None);
        }
        matched.sort_by(|a, b| {
            b.0.cmp(&a.0)
                .then_with(|| b.1.created_at.cmp(&a.1.created_at))
                .then_with(|| a.1.id.cmp(&b.1.id))
        });

        let budget = if max_bytes == 0 {
            DEFAULT_ATTACH_BYTES
        } else {
            max_bytes
        };
        let header = "[Dead ends (advisory, source-verified — avoid repeating)]\n";
        let mut out = String::from(header);
        let mut attached = Vec::new();
        for (_, row) in matched {
            if attached.len() >= MAX_ATTACHED_DEAD_ENDS {
                break;
            }
            let line = if row.signature.is_empty() {
                format!("- DEAD_END [{}] ({}): {}\n", row.path, row.id, row.summary)
            } else {
                format!(
                    "- DEAD_END [{}] ({}) sig=\"{}\": {}\n",
                    row.path, row.id, row.signature, row.summary
                )
            };
            if out.len() + line.len() > budget && !attached.is_empty() {
                break;
            }
            if out.len() + line.len() > budget {
                continue;
            }
            out.push_str(&line);
            attached.push(row.id);
        }
        if attached.is_empty() {
            return Ok(None);
        }
        shown.extend(attached);
        Ok(Some(out.trim_end().to_owned()))
    }
}

#[cfg(test)]
mod tests;
