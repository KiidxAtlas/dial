//! Full-text search across sessions: the derived FTS5 index, its incremental sync and query parsing.

use super::*;

/// Tool outputs larger than this are not searchable (their names and the model's replies are).
const MAX_INDEXED_TOOL_BODY: usize = 2048;
const MAX_INDEXED_TEXT: usize = 16 * 1024;
#[derive(Debug, Clone)]
pub struct SearchHit {
    pub session_id: String,
    /// Immutable journal entry identity. Empty for legacy records without an id.
    pub entry_id: String,
    /// Bounded full matching message for read-only inspection, never a rewind target.
    pub preview: String,
    pub role: String,
    pub snippet: String,
    pub when: String,
}

fn query_terms(query: &str) -> Vec<String> {
    query
        .split(|c: char| !c.is_alphanumeric())
        .filter(|t| !t.is_empty())
        .take(12)
        .map(|t| t.chars().take(64).collect())
        .collect()
}

fn open_index(layout: &Layout) -> Result<Connection> {
    crate::platform::atomic::private_dir(&layout.state_dir)?;
    let db = layout.state_dir.join("sessions.sqlite3");
    let _file =
        platform_fs::open_owner_only(OpenOptions::new().create(true).read(true).write(true), &db)?;
    ensure!(
        platform_fs::owned_by_current_user(&_file)? && platform_fs::link_count(&_file)? == 1,
        "session search index requires an owner-owned file without hard links"
    );
    platform_fs::set_owner_only_file(&_file)?;
    let conn = Connection::open(&db).with_context(|| format!("opening {}", db.display()))?;
    conn.busy_timeout(Duration::from_secs(10))?;
    crate::storage::sqlite::migrate(&conn, &[
        "CREATE TABLE IF NOT EXISTS indexed(session_id TEXT PRIMARY KEY, bytes INTEGER NOT NULL);
         CREATE VIRTUAL TABLE IF NOT EXISTS msgs USING fts5(text, session_id UNINDEXED, role UNINDEXED, ts UNINDEXED, tokenize='porter unicode61');
         ALTER TABLE indexed ADD COLUMN identity TEXT NOT NULL DEFAULT '';",
        "ALTER TABLE indexed ADD COLUMN prefix_digest TEXT NOT NULL DEFAULT '';",
        "DROP TABLE msgs;
         DELETE FROM indexed;
         CREATE VIRTUAL TABLE msgs USING fts5(text, session_id UNINDEXED, role UNINDEXED, ts UNINDEXED, entry_id UNINDEXED, tokenize='porter unicode61');",
    ])
    .context("creating the session search index (SQLite without FTS5?)")?;
    Ok(conn)
}

/// What of a message is searchable: user/assistant text plus the names of tools the assistant called,
/// and short tool results.
fn index_text(message: &Value) -> Option<(&str, String)> {
    let role = role_of(message);
    let mut text = content_text(message);
    match role {
        "user" => {}
        "assistant" => {
            for call in message
                .get("tool_calls")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                if let Some(name) = call.pointer("/function/name").and_then(Value::as_str) {
                    text.push_str(&format!("\n[tool: {name}]"));
                }
            }
        }
        "tool" if text.len() <= MAX_INDEXED_TOOL_BODY => {}
        _ => return None,
    }
    let text = text.trim();
    (!text.is_empty()).then(|| (role, text.chars().take(MAX_INDEXED_TEXT).collect()))
}

fn sync_index(conn: &mut Connection, dir: &Path) -> Result<()> {
    let files = session_files(dir)?;
    let known: HashMap<String, (u64, String)> = {
        let mut stmt = conn.prepare("SELECT session_id, bytes, identity FROM indexed")?;
        stmt.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                (r.get::<_, i64>(1)? as u64, r.get::<_, String>(2)?),
            ))
        })?
        .collect::<rusqlite::Result<_>>()?
    };
    let mut present = HashSet::new();
    for path in files {
        let Some(sid) = path
            .file_name()
            .and_then(|n| n.to_str())
            .and_then(file_id)
            .map(str::to_string)
        else {
            continue;
        };
        present.insert(sid.clone());
        let Ok(_) = fs::metadata(&path) else {
            continue;
        };
        // Metadata alone cannot distinguish an in-place rewrite, even at the same length.
        // Validate the indexed prefix on every search; parsing remains incremental for appends.
        index_session(conn, &sid, &path)?;
    }
    for gone in known.keys().filter(|k| !present.contains(*k)) {
        let tx = conn.transaction()?;
        tx.execute("DELETE FROM msgs WHERE session_id = ?1", [gone])?;
        tx.execute("DELETE FROM indexed WHERE session_id = ?1", [gone])?;
        tx.commit()?;
    }
    Ok(())
}

/// Indexes complete lines of `path` from the stored offset up to `len` (a half-written tail line is
/// left for the next sync). Serialised per database by an immediate transaction, so two processes
/// searching at once never index a line twice.
fn index_session(conn: &mut Connection, sid: &str, path: &Path) -> Result<()> {
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let mut file = platform_fs::open_nofollow_nonblocking(OpenOptions::new().read(true), path)?;
    let len = file.metadata()?.len();
    let identity = platform_fs::identity(&file)?;
    let identity = format!("{}:{}", identity.device, identity.inode);
    let previous: Option<(i64, String, String)> = tx
        .query_row(
            "SELECT bytes, identity, prefix_digest FROM indexed WHERE session_id = ?1",
            [sid],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()?;
    let mut offset = previous.as_ref().map_or(0, |(bytes, _, _)| *bytes as u64);
    let mut hash = Sha256::new();
    let mut unchanged = false;
    if let Some((_, old_identity, digest)) = &previous
        && *old_identity == identity
        && offset <= len
    {
        let mut prefix = (&mut file).take(offset);
        let mut buffer = [0; 64 * 1024];
        let mut read = 0;
        loop {
            let n = prefix.read(&mut buffer)?;
            if n == 0 {
                break;
            }
            hash.update(&buffer[..n]);
            read += n as u64;
        }
        unchanged = read == offset && *digest == format!("{:x}", hash.clone().finalize());
    }
    if unchanged && offset == len {
        return Ok(());
    }
    if !unchanged {
        tx.execute("DELETE FROM msgs WHERE session_id = ?1", [sid])?;
        offset = 0;
        hash = Sha256::new();
    }
    let mut digest = format!("{:x}", hash.clone().finalize());
    if len > offset {
        file.seek(SeekFrom::Start(offset))?;
        let mut reader = IndexReader {
            inner: BufReader::new(file.take(len - offset)),
            hash,
        };
        let mut buf = Vec::new();
        while let Some(line) = read_bounded_line(&mut reader, &mut buf)? {
            if !line.terminated {
                break;
            }
            offset += line.consumed as u64;
            digest = format!("{:x}", reader.hash.clone().finalize());
            if line.oversized {
                continue;
            }
            let Ok(v) = serde_json::from_slice::<Value>(&buf) else {
                continue;
            };
            if v.get("type").and_then(Value::as_str) != Some("message")
                || v.get("copied").and_then(Value::as_bool) == Some(true)
            {
                continue;
            }
            let Some((role, text)) = v.get("message").and_then(index_text) else {
                continue;
            };
            let ts = v.get("ts").and_then(Value::as_str).unwrap_or("");
            tx.execute(
                "INSERT INTO msgs(text, session_id, role, ts, entry_id) VALUES(?1, ?2, ?3, ?4, ?5)",
                params![
                    text,
                    sid,
                    role,
                    ts,
                    v.get("id").and_then(Value::as_str).unwrap_or("")
                ],
            )?;
        }
    }
    tx.execute(
        "INSERT OR REPLACE INTO indexed(session_id, bytes, identity, prefix_digest) VALUES(?1, ?2, ?3, ?4)",
        params![sid, offset as i64, identity, digest],
    )?;
    tx.commit()?;
    Ok(())
}

/// Hash exactly the bytes consumed by the bounded line reader, including skipped oversized rows.
/// Hashing below `BufReader` would also hash read-ahead bytes and accidentally admit a torn tail.
struct IndexReader<R> {
    inner: BufReader<R>,
    hash: Sha256,
}

impl<R: Read> Read for IndexReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = self.inner.read(buf)?;
        self.hash.update(&buf[..n]);
        Ok(n)
    }
}

impl<R: Read> BufRead for IndexReader<R> {
    fn fill_buf(&mut self) -> io::Result<&[u8]> {
        self.inner.fill_buf()
    }

    fn consume(&mut self, amount: usize) {
        self.hash.update(&self.inner.buffer()[..amount]);
        self.inner.consume(amount);
    }
}

// ---------------------------------------------------------------- session_search tool

impl Session {
    /// Full-text search over every session of the project, best match first.
    pub fn search(layout: &Layout, query: &str, limit: usize) -> Result<Vec<SearchHit>> {
        let dir = layout.sessions_dir();
        let terms = query_terms(query);
        if terms.is_empty() || !dir.is_dir() {
            return Ok(Vec::new());
        }
        let limit = limit.clamp(1, 100);
        let mut conn = open_index(layout)?;
        sync_index(&mut conn, &dir)?;
        // Every term is a quoted FTS string, so no query syntax from the caller reaches the parser.
        // Prefer sessions containing all terms; fall back to any term.
        for joiner in [" ", " OR "] {
            let q = terms
                .iter()
                .map(|t| format!("\"{t}\""))
                .collect::<Vec<_>>()
                .join(joiner);
            let mut stmt = conn.prepare(
                "SELECT session_id, role, snippet(msgs, 0, '', '', '…', 24), ts, entry_id, text FROM msgs WHERE msgs MATCH ?1 ORDER BY rank LIMIT ?2",
            )?;
            let hits = stmt
                .query_map(params![q, limit as i64], |r| {
                    Ok(SearchHit {
                        session_id: r.get(0)?,
                        role: r.get(1)?,
                        snippet: r.get(2)?,
                        when: r.get(3)?,
                        entry_id: r.get(4)?,
                        preview: r.get(5)?,
                    })
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            if !hits.is_empty() || terms.len() == 1 {
                return Ok(hits);
            }
        }
        Ok(Vec::new())
    }
}
