//! Durable fact rows for the semantic memory backend.
//!
//! `facts` and `fact_history` are the source of truth. The FTS5 index and the embedding cache are
//! derived: `rebuild_index` regenerates the first from rows, and vectors are keyed by content hash so
//! a changed fact never reuses a stale vector. Forgotten facts keep only an audit tombstone.
use anyhow::{Context, Result, ensure};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::{
    cmp::Ordering,
    collections::{BTreeSet, BinaryHeap, HashMap},
    time::Duration,
};

pub const MAX_CONTENT_BYTES: usize = 8192;
pub const MAX_SOURCE_BYTES: usize = 1024;
/// Threshold up to which `nearest` runs a full exact scan; above this count, a 64-bit SimHash
/// prefilter shortlists candidates across all live facts before exact cosine reranking.
pub const MAX_VECTOR_ROWS: usize = 20_000;
pub const RRF_K: f64 = 60.0;

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Fact {
    pub id: i64,
    pub content: String,
    pub source: String,
    pub revision: i64,
    pub created: i64,
    pub updated: i64,
}

#[derive(Debug, Serialize)]
pub struct Correction {
    pub revision: i64,
    pub action: String,
    pub content: Option<String>,
    pub source: String,
    pub at: i64,
    pub reason: String,
}

pub enum Saved {
    Created(Fact),
    Existing(Fact),
}

pub struct Store {
    conn: Connection,
}

pub fn content_hash(content: &str) -> String {
    format!("{:x}", Sha256::digest(content.as_bytes()))
}

/// Lowercased Unicode word tokens of at least two characters, deduplicated and bounded.
pub fn query_tokens(query: &str) -> Vec<String> {
    query
        .split(|c: char| !c.is_alphanumeric() && c != '_')
        .filter(|s| s.chars().count() >= 2)
        .map(str::to_lowercase)
        .collect::<BTreeSet<_>>()
        .into_iter()
        .take(64)
        .collect()
}

fn fts_expression(query: &str) -> Option<String> {
    let tokens = query_tokens(query);
    (!tokens.is_empty()).then(|| {
        tokens
            .iter()
            .map(|token| format!("\"{}\"", token.replace('"', "\"\"")))
            .collect::<Vec<_>>()
            .join(" OR ")
    })
}

/// Reciprocal-rank fusion: each list contributes `1 / (K + rank)`; ties break toward the newer id.
pub fn fuse(lists: &[Vec<i64>]) -> Vec<i64> {
    let mut scores: HashMap<i64, f64> = HashMap::new();
    for list in lists {
        for (rank, id) in list.iter().enumerate() {
            *scores.entry(*id).or_default() += 1.0 / (RRF_K + rank as f64 + 1.0);
        }
    }
    let mut ranked: Vec<(i64, f64)> = scores.into_iter().collect();
    ranked.sort_by(|a, b| b.1.total_cmp(&a.1).then(b.0.cmp(&a.0)));
    ranked.into_iter().map(|(id, _)| id).collect()
}

pub fn cosine(a: &[f32], b: &[f32]) -> f32 {
    let (mut dot, mut norm_a, mut norm_b) = (0.0f32, 0.0f32, 0.0f32);
    for (x, y) in a.iter().zip(b) {
        dot += x * y;
        norm_a += x * x;
        norm_b += y * y;
    }
    let denominator = norm_a.sqrt() * norm_b.sqrt();
    if denominator == 0.0 || !denominator.is_finite() {
        0.0
    } else {
        dot / denominator
    }
}

/// Deterministic 64-bit random-hyperplane SimHash for cosine-distance candidate prefiltering.
/// Equal vectors have Hamming distance 0, orthogonal vectors ~32, opposite vectors 64.
pub fn simhash(vector: &[f32]) -> i64 {
    let mut acc = [0.0f64; 64];
    for (dim, &value) in vector.iter().enumerate() {
        let v = f64::from(value);
        if !v.is_finite() || v == 0.0 {
            continue;
        }
        let mut h = (dim as u64).wrapping_add(0x9e37_79b9_7f4a_7c15);
        h = (h ^ (h >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        h = (h ^ (h >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        let bits = h ^ (h >> 31);
        for (bit, slot) in acc.iter_mut().enumerate() {
            if (bits >> bit) & 1 != 0 {
                *slot += v;
            } else {
                *slot -= v;
            }
        }
    }
    let mut sig = 0u64;
    for (bit, &sum) in acc.iter().enumerate() {
        if sum >= 0.0 {
            sig |= 1u64 << bit;
        }
    }
    sig as i64
}

/// Heap ordering puts the worst retained match first: lower cosine, then older id.
/// `total_cmp` preserves the previous full-sort behavior, including non-finite scores.
#[derive(Debug)]
struct VectorMatch {
    id: i64,
    score: f32,
}

impl PartialEq for VectorMatch {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}

impl Eq for VectorMatch {}

impl PartialOrd for VectorMatch {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for VectorMatch {
    fn cmp(&self, other: &Self) -> Ordering {
        other
            .score
            .total_cmp(&self.score)
            .then(other.id.cmp(&self.id))
    }
}

fn keep_nearest(heap: &mut BinaryHeap<VectorMatch>, candidate: VectorMatch, limit: usize) {
    if heap.len() < limit {
        heap.push(candidate);
    } else if let Some(mut worst) = heap.peek_mut()
        && candidate < *worst
    {
        *worst = candidate;
    }
}

/// Rough token cost of one rendered fact: four bytes per token plus fixed framing.
pub fn estimate_tokens(content: &str) -> usize {
    content.len().div_ceil(4) + 8
}

/// Keeps facts in rank order while they fit `limit` and `token_budget`. Stops at the first overflow so
/// a lower-ranked short fact never jumps ahead of a higher-ranked one. Returns whether anything was cut.
pub fn fit_budget(facts: Vec<Fact>, limit: usize, token_budget: usize) -> (Vec<Fact>, bool) {
    let mut selected = Vec::new();
    let mut used = 0usize;
    for fact in facts {
        if selected.len() == limit {
            return (selected, true);
        }
        let cost = estimate_tokens(&fact.content);
        if used + cost > token_budget {
            return (selected, true);
        }
        used += cost;
        selected.push(fact);
    }
    (selected, false)
}

fn encode(vector: &[f32]) -> Vec<u8> {
    vector
        .iter()
        .flat_map(|value| value.to_le_bytes())
        .collect()
}

fn decode(bytes: &[u8], dimensions: usize) -> Result<Vec<f32>> {
    ensure!(
        dimensions.checked_mul(4) == Some(bytes.len()),
        "corrupt embedding row: stored bytes do not match its dimensions"
    );
    Ok(bytes
        .as_chunks::<4>()
        .0
        .iter()
        .map(|chunk| f32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]))
        .collect())
}

fn fact_by_id(conn: &Connection, id: i64) -> Result<Option<Fact>> {
    conn.query_row(
        "SELECT id,content,source,revision,created,updated FROM facts WHERE id=?1 AND forgotten=0",
        [id],
        |row| {
            Ok(Fact {
                id: row.get(0)?,
                content: row.get(1)?,
                source: row.get(2)?,
                revision: row.get(3)?,
                created: row.get(4)?,
                updated: row.get(5)?,
            })
        },
    )
    .optional()
    .context("read memory fact")
}

/// Drops a vector, its SimHash signature, and its LSH band entries once no live fact carries that content.
fn purge_vectors(conn: &Connection, hash: &str) -> Result<()> {
    conn.execute(
        "DELETE FROM embeddings WHERE content_hash=?1 AND NOT EXISTS(SELECT 1 FROM facts WHERE content_hash=?1 AND forgotten=0)",
        [hash],
    )?;
    conn.execute(
        "DELETE FROM embedding_sigs WHERE content_hash=?1 AND NOT EXISTS(SELECT 1 FROM facts WHERE content_hash=?1 AND forgotten=0)",
        [hash],
    )?;
    conn.execute(
        "DELETE FROM embedding_lsh WHERE content_hash=?1 AND NOT EXISTS(SELECT 1 FROM facts WHERE content_hash=?1 AND forgotten=0)",
        [hash],
    )?;
    Ok(())
}

/// Writes an audit tombstone and erases the fact's text, source and every prior revision's text.
fn tombstone(conn: &Connection, id: i64, revision: i64, at: i64) -> Result<()> {
    let old: Option<String> = conn
        .query_row(
            "SELECT content FROM facts WHERE id=?1 AND forgotten=0",
            [id],
            |row| row.get(0),
        )
        .optional()?;
    let old = old.context("memory id not found or already forgotten")?;
    conn.execute(
        "UPDATE facts SET content='',content_hash='',source='',revision=?1,updated=?2,forgotten=1 WHERE id=?3",
        params![revision + 1, at, id],
    )?;
    conn.execute("DELETE FROM facts_fts WHERE rowid=?1", [id])?;
    conn.execute(
        "UPDATE fact_history SET content=NULL,source='',reason='redacted by forget' WHERE fact_id=?1",
        [id],
    )?;
    conn.execute(
        "INSERT INTO fact_history(fact_id,revision,action,content,source,at,reason) VALUES(?1,?2,'forget',NULL,'',?3,'explicit forget')",
        params![id, revision + 1, at],
    )?;
    purge_vectors(conn, &content_hash(&old))?;
    Ok(())
}

/// Erase historical WAL frames after redaction. Uses bounded exponential backoff for transient
/// concurrent readers; a persistent busy reader is still reported rather than mistaken for erasure.
fn checkpoint_redaction(conn: &Connection) -> Result<()> {
    let mut delay_ms: u64 = 5;
    let deadline = std::time::Instant::now() + Duration::from_millis(1500);
    let (busy, log_frames, checkpointed) = loop {
        let (busy, log_frames, checkpointed): (i64, i64, i64) =
            conn.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?))
            })?;
        if busy == 0 && (log_frames <= 0 || checkpointed == log_frames) {
            return Ok(());
        }
        let now = std::time::Instant::now();
        if now >= deadline {
            break (busy, log_frames, checkpointed);
        }
        let wait = Duration::from_millis(delay_ms).min(deadline - now);
        std::thread::sleep(wait);
        delay_ms = (delay_ms * 2).min(100);
    };
    ensure!(
        busy == 0 && (log_frames <= 0 || checkpointed == log_frames),
        "memory was forgotten but a busy reader prevented WAL erasure (busy={busy}, checkpointed={checkpointed}/{log_frames}); erasure is incomplete until readers close and a TRUNCATE checkpoint succeeds"
    );
    Ok(())
}

impl Store {
    pub fn new(conn: Connection) -> Result<Self> {
        conn.busy_timeout(Duration::from_secs(10))?;
        crate::storage::sqlite::check_version(&conn, 1)?;
        conn.execute_batch("PRAGMA foreign_keys=ON; PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL; PRAGMA secure_delete=ON;")?;
        // FTS5 otherwise retains deleted terms in older index segments.
        crate::storage::sqlite::migrate(&conn, &[
            "
            CREATE TABLE IF NOT EXISTS facts(
                id INTEGER PRIMARY KEY, content TEXT NOT NULL, content_hash TEXT NOT NULL,
                source TEXT NOT NULL, revision INTEGER NOT NULL, created INTEGER NOT NULL,
                updated INTEGER NOT NULL, forgotten INTEGER NOT NULL DEFAULT 0);
            CREATE INDEX IF NOT EXISTS facts_hash ON facts(content_hash);
            CREATE TABLE IF NOT EXISTS fact_history(
                fact_id INTEGER NOT NULL REFERENCES facts(id), revision INTEGER NOT NULL,
                action TEXT NOT NULL, content TEXT, source TEXT NOT NULL, at INTEGER NOT NULL,
                reason TEXT NOT NULL, PRIMARY KEY(fact_id, revision));
            CREATE TABLE IF NOT EXISTS embeddings(
                content_hash TEXT NOT NULL, model TEXT NOT NULL, dimensions INTEGER NOT NULL,
                vector BLOB NOT NULL, PRIMARY KEY(content_hash, model));
            CREATE TABLE IF NOT EXISTS pending_turns(
                id INTEGER PRIMARY KEY, turn_key TEXT NOT NULL UNIQUE, session TEXT NOT NULL,
                prompt TEXT NOT NULL, answer TEXT NOT NULL, created INTEGER NOT NULL);
            CREATE TABLE IF NOT EXISTS observed_turns(turn_key TEXT PRIMARY KEY, created INTEGER NOT NULL);
            CREATE TABLE IF NOT EXISTS meta(key TEXT PRIMARY KEY, value TEXT NOT NULL);
            CREATE VIRTUAL TABLE IF NOT EXISTS facts_fts USING fts5(content, tokenize='unicode61');",
        ])?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS embedding_sigs(
                content_hash TEXT NOT NULL, model TEXT NOT NULL, sig INTEGER NOT NULL,
                PRIMARY KEY(content_hash, model));
             CREATE INDEX IF NOT EXISTS embedding_sigs_model_sig ON embedding_sigs(model, sig);
             CREATE TABLE IF NOT EXISTS embedding_lsh(
                model TEXT NOT NULL, band INTEGER NOT NULL, bucket INTEGER NOT NULL,
                content_hash TEXT NOT NULL,
                PRIMARY KEY(model, band, bucket, content_hash)) WITHOUT ROWID;
             CREATE INDEX IF NOT EXISTS embedding_lsh_hash ON embedding_lsh(content_hash);",
        )?;
        conn.execute(
            "INSERT INTO facts_fts(facts_fts, rank) VALUES('secure-delete', 1)",
            [],
        )?;
        Ok(Self { conn })
    }

    pub fn retain(&mut self, content: &str, source: &str, at: i64) -> Result<Saved> {
        ensure!(
            content.len() <= MAX_CONTENT_BYTES,
            "content exceeds {MAX_CONTENT_BYTES} bytes"
        );
        ensure!(
            source.len() <= MAX_SOURCE_BYTES,
            "source exceeds {MAX_SOURCE_BYTES} bytes"
        );
        let hash = content_hash(content);
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let existing: Option<i64> = tx
            .query_row(
                "SELECT id FROM facts WHERE content_hash=?1 AND forgotten=0 ORDER BY id LIMIT 1",
                [&hash],
                |row| row.get(0),
            )
            .optional()?;
        if let Some(id) = existing {
            let fact = fact_by_id(&tx, id)?.context("duplicate fact vanished")?;
            tx.commit()?;
            return Ok(Saved::Existing(fact));
        }
        tx.execute(
            "INSERT INTO facts(content,content_hash,source,revision,created,updated) VALUES(?1,?2,?3,1,?4,?4)",
            params![content, hash, source, at],
        )?;
        let id = tx.last_insert_rowid();
        tx.execute(
            "INSERT INTO fact_history(fact_id,revision,action,content,source,at,reason) VALUES(?1,1,'retain',?2,?3,?4,'initial retention')",
            params![id, content, source, at],
        )?;
        tx.execute(
            "INSERT INTO facts_fts(rowid,content) VALUES(?1,?2)",
            params![id, content],
        )?;
        let fact = fact_by_id(&tx, id)?.context("new fact vanished")?;
        tx.commit()?;
        Ok(Saved::Created(fact))
    }

    /// Corrections need the revision the caller last saw, so a stale session cannot overwrite a newer fact.
    pub fn update(
        &mut self,
        id: i64,
        expected_revision: i64,
        content: &str,
        source: &str,
        reason: &str,
        at: i64,
    ) -> Result<Fact> {
        ensure!(
            content.len() <= MAX_CONTENT_BYTES,
            "content exceeds {MAX_CONTENT_BYTES} bytes"
        );
        ensure!(
            source.len() <= MAX_SOURCE_BYTES,
            "source exceeds {MAX_SOURCE_BYTES} bytes"
        );
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let current = fact_by_id(&tx, id)?
            .context("memory id not found or forgotten; recall before correcting")?;
        ensure!(
            current.revision == expected_revision,
            "memory revision changed (now {}); recall before correcting",
            current.revision
        );
        let hash = content_hash(content);
        let duplicate: Option<i64> = tx
            .query_row(
                "SELECT id FROM facts WHERE content_hash=?1 AND forgotten=0 AND id!=?2 LIMIT 1",
                params![hash, id],
                |row| row.get(0),
            )
            .optional()?;
        if let Some(other) = duplicate {
            anyhow::bail!("another active fact already has this content (id {other})");
        }
        let next = expected_revision + 1;
        tx.execute(
            "UPDATE facts SET content=?1,content_hash=?2,source=?3,revision=?4,updated=?5 WHERE id=?6",
            params![content, hash, source, next, at, id],
        )?;
        tx.execute(
            "INSERT INTO fact_history(fact_id,revision,action,content,source,at,reason) VALUES(?1,?2,'update',?3,?4,?5,?6)",
            params![id, next, content, source, at, reason],
        )?;
        tx.execute("DELETE FROM facts_fts WHERE rowid=?1", [id])?;
        tx.execute(
            "INSERT INTO facts_fts(rowid,content) VALUES(?1,?2)",
            params![id, content],
        )?;
        purge_vectors(&tx, &content_hash(&current.content))?;
        let fact = fact_by_id(&tx, id)?.context("updated fact vanished")?;
        tx.commit()?;
        Ok(fact)
    }

    pub fn forget(&mut self, id: i64, expected_revision: i64, at: i64) -> Result<()> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let current = fact_by_id(&tx, id)?
            .context("memory id not found or already forgotten; recall before forgetting")?;
        ensure!(
            current.revision == expected_revision,
            "memory revision changed (now {}); recall before forgetting",
            current.revision
        );
        tombstone(&tx, id, expected_revision, at)?;
        tx.commit()?;
        checkpoint_redaction(&self.conn)?;
        Ok(())
    }

    /// Forgets every live fact and drops queued turns, whose text must not be extracted into the
    /// bank that was just cleared; returns how many facts were erased.
    pub fn clear_all(&mut self, at: i64) -> Result<usize> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let live: Vec<(i64, i64)> = {
            let mut statement =
                tx.prepare("SELECT id,revision FROM facts WHERE forgotten=0 ORDER BY id")?;
            statement
                .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
                .collect::<rusqlite::Result<_>>()?
        };
        for (id, revision) in &live {
            tombstone(&tx, *id, *revision, at)?;
        }
        tx.execute("DELETE FROM pending_turns", [])?;
        tx.commit()?;
        checkpoint_redaction(&self.conn)?;
        Ok(live.len())
    }

    pub fn history(&self, id: i64) -> Result<Vec<Correction>> {
        let exists: bool = self.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM facts WHERE id=?1)",
            [id],
            |row| row.get(0),
        )?;
        ensure!(exists, "memory id not found");
        let mut statement = self.conn.prepare(
            "SELECT revision,action,content,source,at,reason FROM fact_history WHERE fact_id=?1 ORDER BY revision DESC",
        )?;
        let rows = statement.query_map([id], |row| {
            Ok(Correction {
                revision: row.get(0)?,
                action: row.get(1)?,
                content: row.get(2)?,
                source: row.get(3)?,
                at: row.get(4)?,
                reason: row.get(5)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn fact(&self, id: i64) -> Result<Option<Fact>> {
        fact_by_id(&self.conn, id)
    }

    /// Live facts, most recently updated first, bounded by `limit`.
    pub fn list(&self, limit: usize) -> Result<Vec<Fact>> {
        let mut statement = self.conn.prepare(
            "SELECT id,content,source,revision,created,updated FROM facts
             WHERE forgotten=0 ORDER BY updated DESC, id DESC LIMIT ?1",
        )?;
        let rows = statement.query_map([limit as i64], |row| {
            Ok(Fact {
                id: row.get(0)?,
                content: row.get(1)?,
                source: row.get(2)?,
                revision: row.get(3)?,
                created: row.get(4)?,
                updated: row.get(5)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Completed turns queued for auto-retain extraction.
    pub fn pending_turn_count(&self) -> Result<usize> {
        let count: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM pending_turns", [], |row| row.get(0))?;
        Ok(usize::try_from(count).unwrap_or(0))
    }

    /// Ids of live facts matching any query token, best BM25 first.
    pub fn lexical(&self, query: &str, limit: usize) -> Result<Vec<i64>> {
        let Some(expression) = fts_expression(query) else {
            return Ok(Vec::new());
        };
        let mut statement = self.conn.prepare(
            "SELECT facts_fts.rowid FROM facts_fts JOIN facts f ON f.id=facts_fts.rowid
             WHERE facts_fts MATCH ?1 AND f.forgotten=0
             ORDER BY bm25(facts_fts), f.updated DESC, f.id DESC LIMIT ?2",
        )?;
        let rows = statement.query_map(params![expression, limit as i64], |row| row.get(0))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn recent(&self, limit: usize) -> Result<Vec<i64>> {
        let mut statement = self.conn.prepare(
            "SELECT id FROM facts WHERE forgotten=0 ORDER BY updated DESC, id DESC LIMIT ?1",
        )?;
        let rows = statement.query_map([limit as i64], |row| row.get(0))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Ids of live facts updated at or after `since`, newest first.
    pub fn recent_since(&self, since: i64, limit: usize) -> Result<Vec<i64>> {
        let mut statement = self.conn.prepare(
            "SELECT id FROM facts WHERE forgotten=0 AND updated>=?1 ORDER BY updated DESC, id DESC LIMIT ?2",
        )?;
        let rows = statement.query_map(params![since, limit as i64], |row| row.get(0))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// A value that changes whenever the stored facts may have changed: `data_version` moves when
    /// another connection commits, `total_changes` when this one writes. Recall caches key on it.
    pub fn write_token(&self) -> Result<(i64, u64)> {
        let version: i64 = self
            .conn
            .query_row("PRAGMA data_version", [], |row| row.get(0))?;
        Ok((version, self.conn.total_changes()))
    }

    /// Top-k nearest live facts for `model`.
    /// Up to `MAX_VECTOR_ROWS` facts, runs a streaming exact scan (O(limit) retained scores).
    /// Beyond `MAX_VECTOR_ROWS`, scans compact 64-bit SimHash signatures across ALL live facts
    /// (never silently dropping older facts) and exact-cosine reranks the top candidates.
    pub fn nearest(&self, model: &str, query: &[f32], limit: usize) -> Result<Vec<i64>> {
        self.nearest_with_threshold(model, query, limit, MAX_VECTOR_ROWS)
    }

    fn nearest_with_threshold(
        &self,
        model: &str,
        query: &[f32],
        limit: usize,
        exact_threshold: usize,
    ) -> Result<Vec<i64>> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        let exceeds_exact = self
            .conn
            .query_row(
                "SELECT 1 FROM facts f
                 JOIN embeddings e ON e.content_hash=f.content_hash AND e.model=?1
                 WHERE f.forgotten=0 LIMIT 1 OFFSET ?2",
                params![model, exact_threshold as i64],
                |_| Ok(()),
            )
            .optional()?
            .is_some();
        if !exceeds_exact {
            return self.nearest_exact(model, query, limit, exact_threshold);
        }
        // Check whether SimHash signatures exist for this model; if none exist (legacy raw rows),
        // fall back cleanly to the bounded newest-row exact scan.
        let has_sigs = self
            .conn
            .query_row(
                "SELECT 1 FROM embedding_sigs WHERE model=?1 LIMIT 1",
                [model],
                |_| Ok(()),
            )
            .optional()?
            .is_some();
        if !has_sigs {
            return self.nearest_exact(model, query, limit, exact_threshold);
        }
        self.nearest_simhash(model, query, limit, exact_threshold)
    }

    fn nearest_exact(
        &self,
        model: &str,
        query: &[f32],
        limit: usize,
        row_cap: usize,
    ) -> Result<Vec<i64>> {
        let mut statement = self.conn.prepare(
            "SELECT f.id, e.vector, e.dimensions FROM facts f
             JOIN embeddings e ON e.content_hash=f.content_hash AND e.model=?1
             WHERE f.forgotten=0 ORDER BY f.id DESC LIMIT ?2",
        )?;
        let mut rows = statement.query(params![model, row_cap as i64])?;
        let mut heap = BinaryHeap::with_capacity(limit.min(row_cap));
        while let Some(row) = rows.next()? {
            let id = row.get(0)?;
            let bytes = row.get_ref(1)?;
            let dimensions = row.get::<_, i64>(2)?;
            let vector = decode(bytes.as_blob()?, dimensions as usize)?;
            if vector.len() == query.len() {
                keep_nearest(
                    &mut heap,
                    VectorMatch {
                        id,
                        score: cosine(query, &vector),
                    },
                    limit,
                );
            }
        }
        Ok(heap
            .into_sorted_vec()
            .into_iter()
            .map(|item| item.id)
            .collect())
    }

    /// Partition a 64-bit SimHash signature into four disjoint 16-bit LSH bands.
    /// By the pigeonhole principle, any signature within Hamming distance <= 3 shares
    /// at least one exact 16-bit band, enabling O(log N) B-tree candidate retrieval.
    fn simhash_bands(sig: i64) -> [i64; 4] {
        let bits = sig as u64;
        [
            (bits & 0xFFFF) as i64,
            ((bits >> 16) & 0xFFFF) as i64,
            ((bits >> 32) & 0xFFFF) as i64,
            ((bits >> 48) & 0xFFFF) as i64,
        ]
    }

    /// Indexed LSH probe across the 4 16-bit SimHash bands. Returns deduplicated `(fact_id, sig)`
    /// pairs whose signatures share at least one band with `query_sig`.
    fn probe_lsh_candidates(
        &self,
        model: &str,
        query_sig: i64,
        probe_cap: usize,
    ) -> Result<Vec<(i64, i64)>> {
        let [b0, b1, b2, b3] = Self::simhash_bands(query_sig);
        let mut stmt = self.conn.prepare(
            "WITH hits(content_hash) AS (
                 SELECT content_hash FROM embedding_lsh WHERE model=?1 AND band=0 AND bucket=?2
                 UNION
                 SELECT content_hash FROM embedding_lsh WHERE model=?1 AND band=1 AND bucket=?3
                 UNION
                 SELECT content_hash FROM embedding_lsh WHERE model=?1 AND band=2 AND bucket=?4
                 UNION
                 SELECT content_hash FROM embedding_lsh WHERE model=?1 AND band=3 AND bucket=?5
             )
             SELECT f.id, s.sig
             FROM hits h
             JOIN facts f ON f.content_hash=h.content_hash AND f.forgotten=0
             JOIN embedding_sigs s ON s.content_hash=h.content_hash AND s.model=?1
             LIMIT ?6",
        )?;
        let rows = stmt.query_map(params![model, b0, b1, b2, b3, probe_cap as i64], |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?))
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Retrieves SimHash candidates for `model` using O(log N) 4-band LSH index probes first,
    /// falling back to a full `embedding_sigs` scan only when the indexed bands yield too few
    /// candidates (e.g. legacy databases without LSH rows or queries with no close neighbors).
    fn nearest_simhash(
        &self,
        model: &str,
        query: &[f32],
        limit: usize,
        exact_threshold: usize,
    ) -> Result<Vec<i64>> {
        use std::cmp::Reverse;
        let query_sig = simhash(query);
        let candidate_cap = limit
            .saturating_mul(32)
            .max(2048.min(exact_threshold))
            .min(exact_threshold)
            .max(limit);
        // Only trust the 4×16-bit LSH fast path when it was not truncated by the unordered SQL
        // `LIMIT` and yielded enough genuinely close signatures (Hamming <= 12, cos ≳ 0.83).
        // Distant hot-bucket collisions fall back to scanning compact 64-bit `embedding_sigs`.
        const LSH_MAX_TRUSTED_HAMMING: u32 = 12;
        // Max-heap by worst candidate: larger Hamming distance first, then older id (Reverse(id)).
        let mut shortlist: BinaryHeap<(u32, Reverse<i64>)> =
            BinaryHeap::with_capacity(candidate_cap);
        let lsh_probe_cap = candidate_cap.saturating_mul(4).max(256);
        let lsh_hits = self.probe_lsh_candidates(model, query_sig, lsh_probe_cap)?;
        let min_lsh_hits = limit.max(16).min(candidate_cap);
        let lsh_truncated = lsh_hits.len() >= lsh_probe_cap;
        let close_lsh_hits = if lsh_truncated {
            0
        } else {
            lsh_hits
                .iter()
                .filter(|&&(_, sig)| (query_sig ^ sig).count_ones() <= LSH_MAX_TRUSTED_HAMMING)
                .count()
        };
        if !lsh_truncated && close_lsh_hits >= min_lsh_hits {
            for (id, sig) in lsh_hits {
                let dist = (query_sig ^ sig).count_ones();
                let candidate = (dist, Reverse(id));
                if shortlist.len() < candidate_cap {
                    shortlist.push(candidate);
                } else if let Some(mut worst) = shortlist.peek_mut()
                    && candidate < *worst
                {
                    *worst = candidate;
                }
            }
        } else {
            let mut scan = self.conn.prepare(
                "SELECT f.id, s.sig FROM facts f
                 JOIN embeddings e ON e.content_hash=f.content_hash AND e.model=?1
                 LEFT JOIN embedding_sigs s ON s.content_hash=f.content_hash AND s.model=?1
                 WHERE f.forgotten=0",
            )?;
            let mut rows = scan.query([model])?;
            while let Some(row) = rows.next()? {
                let id: i64 = row.get(0)?;
                let sig: Option<i64> = row.get(1)?;
                let dist = sig.map_or(32, |s| (query_sig ^ s).count_ones());
                let candidate = (dist, Reverse(id));
                if shortlist.len() < candidate_cap {
                    shortlist.push(candidate);
                } else if let Some(mut worst) = shortlist.peek_mut()
                    && candidate < *worst
                {
                    *worst = candidate;
                }
            }
        }

        let ids: Vec<i64> = shortlist.into_iter().map(|(_, Reverse(id))| id).collect();
        let mut heap = BinaryHeap::with_capacity(limit.min(ids.len()));
        let mut fetch = self.conn.prepare(
            "SELECT e.vector, e.dimensions FROM facts f
             JOIN embeddings e ON e.content_hash=f.content_hash AND e.model=?1
             WHERE f.id=?2 AND f.forgotten=0",
        )?;
        for id in ids {
            if let Some((bytes, dimensions)) = fetch
                .query_row(params![model, id], |row| {
                    Ok((row.get::<_, Vec<u8>>(0)?, row.get::<_, i64>(1)?))
                })
                .optional()?
            {
                let vector = decode(&bytes, dimensions as usize)?;
                if vector.len() == query.len() {
                    keep_nearest(
                        &mut heap,
                        VectorMatch {
                            id,
                            score: cosine(query, &vector),
                        },
                        limit,
                    );
                }
            }
        }
        Ok(heap
            .into_sorted_vec()
            .into_iter()
            .map(|item| item.id)
            .collect())
    }

    /// Live facts with no cached vector for `model`, as (id, content).
    pub fn missing_vectors(&self, model: &str, limit: usize) -> Result<Vec<(i64, String)>> {
        let mut statement = self.conn.prepare(
            "SELECT f.id, f.content FROM facts f WHERE f.forgotten=0 AND NOT EXISTS(
                SELECT 1 FROM embeddings e WHERE e.content_hash=f.content_hash AND e.model=?1)
             ORDER BY f.id LIMIT ?2",
        )?;
        let rows = statement.query_map(params![model, limit as i64], |row| {
            Ok((row.get(0)?, row.get(1)?))
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Stores vectors keyed by the content they were computed from.
    pub fn store_vectors(&mut self, model: &str, items: &[(String, Vec<f32>)]) -> Result<()> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        for (content, vector) in items {
            ensure!(!vector.is_empty(), "embedding vector is empty");
            let hash = content_hash(content);
            let sig = simhash(vector);
            tx.execute(
                "INSERT OR REPLACE INTO embeddings(content_hash,model,dimensions,vector) VALUES(?1,?2,?3,?4)",
                params![&hash, model, vector.len() as i64, encode(vector)],
            )?;
            tx.execute(
                "INSERT OR REPLACE INTO embedding_sigs(content_hash,model,sig) VALUES(?1,?2,?3)",
                params![&hash, model, sig],
            )?;
            tx.execute(
                "DELETE FROM embedding_lsh WHERE model=?1 AND content_hash=?2",
                params![model, &hash],
            )?;
            for (band, bucket) in Self::simhash_bands(sig).into_iter().enumerate() {
                tx.execute(
                    "INSERT OR IGNORE INTO embedding_lsh(model,band,bucket,content_hash) VALUES(?1,?2,?3,?4)",
                    params![model, band as i64, bucket, &hash],
                )?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// Regenerates the lexical index from live rows; returns how many facts are indexed.
    pub fn rebuild_index(&mut self) -> Result<usize> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.execute("DELETE FROM facts_fts", [])?;
        let indexed = tx.execute(
            "INSERT INTO facts_fts(rowid,content) SELECT id,content FROM facts WHERE forgotten=0",
            [],
        )?;
        tx.commit()?;
        Ok(indexed)
    }

    /// Queues one completed turn for automatic retain unless an identical turn was ever queued,
    /// so re-observing a conversation never extracts the same exchange twice. Returns whether
    /// the turn was new.
    pub fn enqueue_turn(
        &mut self,
        session: &str,
        prompt: &str,
        answer: &str,
        at: i64,
    ) -> Result<bool> {
        let key = content_hash(&format!("{prompt}\n{answer}"));
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let new = tx.execute(
            "INSERT OR IGNORE INTO observed_turns(turn_key,created)VALUES(?1,?2)",
            params![key, at],
        )? == 1;
        if new {
            tx.execute(
                "INSERT OR IGNORE INTO pending_turns(turn_key,session,prompt,answer,created)VALUES(?1,?2,?3,?4,?5)",
                params![key, session, prompt, answer, at],
            )?;
        }
        tx.commit()?;
        Ok(new)
    }

    /// Oldest queued turns as (id, prompt, answer), bounded by `limit`.
    pub fn next_turns(&self, limit: usize) -> Result<Vec<(i64, String, String)>> {
        let mut statement = self
            .conn
            .prepare("SELECT id,prompt,answer FROM pending_turns ORDER BY id LIMIT ?1")?;
        let rows = statement.query_map([limit as i64], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?))
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn finish_turn(&mut self, id: i64) -> Result<()> {
        self.conn
            .execute("DELETE FROM pending_turns WHERE id=?1", [id])?;
        Ok(())
    }

    pub fn meta(&self, key: &str) -> Result<Option<String>> {
        Ok(self
            .conn
            .query_row("SELECT value FROM meta WHERE key=?1", [key], |row| {
                row.get(0)
            })
            .optional()?)
    }

    pub fn set_meta(&mut self, key: &str, value: &str) -> Result<()> {
        self.conn.execute(
            "INSERT INTO meta(key,value)VALUES(?1,?2)ON CONFLICT(key)DO UPDATE SET value=excluded.value",
            params![key, value],
        )?;
        Ok(())
    }

    /// (live facts, forgotten tombstones, live facts with a vector for `model`).
    pub fn counts(&self, model: Option<&str>) -> Result<(usize, usize, usize)> {
        let (live, forgotten): (i64, i64) = self.conn.query_row(
            "SELECT SUM(forgotten=0), SUM(forgotten=1) FROM facts",
            [],
            |row| {
                Ok((
                    row.get::<_, Option<i64>>(0)?.unwrap_or(0),
                    row.get::<_, Option<i64>>(1)?.unwrap_or(0),
                ))
            },
        )?;
        let embedded: i64 = match model {
            Some(model) => self.conn.query_row(
                "SELECT COUNT(*) FROM facts f JOIN embeddings e ON e.content_hash=f.content_hash AND e.model=?1 WHERE f.forgotten=0",
                [model],
                |row| row.get(0),
            )?,
            None => 0,
        };
        Ok((live as usize, forgotten as usize, embedded as usize))
    }
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod vector_tests;
