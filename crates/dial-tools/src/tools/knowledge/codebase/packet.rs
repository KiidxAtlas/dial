//! Explicit, bounded navigation packet for one task: owner files with their indexed outline rows,
//! digest-verified related mentions, and recorded check status. Built only from the existing index and a
//! fresh freshness snapshot; it never saves anything, grants edit or verification authority, and
//! never guesses check commands.
use super::architecture::Snapshot;
use super::*;

/// Owner files a packet names; more would dilute the locator.
const PACKET_OWNERS: usize = 3;
/// Declaration/import rows shown per fresh owner, each `line: text`.
const PACKET_SYMBOLS: usize = 8;
/// Related rows (identifier or import mentions of a fresh owner) shown across all fresh owners.
const PACKET_RELATED: usize = 4;
const PACKET_RULE: &str = "Navigation packet: index facts for this task. This is a ranked, budget-limited sample; omitted rows are not evidence of absence. Each owner's freshness was checked against current source bytes now; symbols are shown only for fresh owners. Related rows are lexical mentions of a fresh owner, not verified consumers or a resolved dependency graph. Check status is recorded metadata, not verification; read current lines before editing.\n";
/// Stated whenever the last refresh stopped at a limit, so a missing owner reads as unknown, not absent.
const PACKET_COVERAGE: &str = "Index coverage: partial. The last refresh stopped at a time, size or file limit before reading every workspace file; owners in unread files are not indexed yet. Refresh again to read more.\n";

impl Codebase {
    /// Explicit navigation for a task. Reuses the ambient ranking's path/outline scoring, the
    /// freshness snapshot, and the digest-verified related-source scan; never writes to the cache.
    pub fn navigation_packet(&self, query: &str, max_bytes: usize) -> Result<String> {
        let terms = navigation::tokens(query);
        ensure!(!terms.is_empty(), "navigation needs a task query");
        let max_bytes = if max_bytes == 0 {
            usize::MAX
        } else {
            max_bytes
        };
        let snapshot = self.freshness_snapshot()?;
        let rows = self
            .conn
            .lock()
            .prepare("SELECT path,digest,outline FROM codebase_files ORDER BY path")?
            .query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let common = common_path_tokens(rows.iter().map(|(path, _, _)| path.as_str()));
        let mut ranked = rows
            .into_iter()
            .filter(|(path, _, outline)| screen(path).is_ok() && screen(outline).is_ok())
            .filter_map(|(path, digest, outline)| {
                let evidence = navigation::tokens(&format!("{path} {outline}"))
                    .intersection(&terms)
                    .count();
                let rank = evidence + path_relevance(query, &path, &terms, &common);
                (rank > 0).then_some((rank, path, digest, outline))
            })
            .collect::<Vec<_>>();
        ranked.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
        let owners = ranked
            .into_iter()
            .take(PACKET_OWNERS)
            .map(|(_, path, digest, outline)| {
                let status = owner_freshness(self, &snapshot, &path, &digest);
                (path, digest, outline, status)
            })
            .collect::<Vec<_>>();
        let check_status = self
            .behavior_report(&snapshot)?
            .get("status")
            .cloned()
            .unwrap_or(json!("missing"));
        let checks = format!(
            "Checks {}\n",
            json!({"status":check_status,"command":"unknown: no check command is recorded for this packet"})
        );
        let partial = self
            .conn
            .lock()
            .query_row(
                "SELECT value FROM codebase_meta WHERE key='partial'",
                [],
                |r| r.get::<_, String>(0),
            )
            .optional()?
            .is_none_or(|value| value != "false");
        let coverage = if partial { PACKET_COVERAGE } else { "" };
        // The rule, coverage and check rows are reserved before any owner or related row, so a
        // non-empty packet always carries its check status, and a budget that cannot hold them yields nothing.
        if PACKET_RULE.len() + coverage.len() + checks.len() > max_bytes {
            return Ok(String::new());
        }
        let mut out = PACKET_RULE.to_owned();
        out.push_str(coverage);
        if owners.is_empty() {
            let note = if partial {
                "No indexed owner matched this task in the files read so far; coverage is partial, so an owner may be unread. Use codebase_memory recall or read the workspace directly.\n"
            } else {
                "No indexed owner matched this task; use codebase_memory recall or read the workspace directly.\n"
            };
            if out.len() + note.len() + checks.len() <= max_bytes {
                out.push_str(note);
            }
        }
        for (path, digest, outline, status) in &owners {
            let symbols = if *status == "fresh" {
                let matched = matching_outline(outline, &terms, PACKET_SYMBOLS);
                if matched.is_empty() {
                    outline
                        .lines()
                        .take(PACKET_SYMBOLS)
                        .collect::<Vec<_>>()
                        .join("\n")
                } else {
                    matched
                }
            } else {
                // A stale or missing owner's indexed outline may no longer describe the file.
                String::new()
            };
            let row = format!(
                "Owner {}\n",
                json!({"path":path,"digest":digest,"freshness":status,"symbols":symbols})
            );
            if out.len() + row.len() + checks.len() <= max_bytes {
                out.push_str(&row);
            }
        }
        // Related rows come only from current sources whose digest still equals the index.
        let fresh_owners = owners
            .iter()
            .filter(|(_, _, _, status)| *status == "fresh")
            .map(|(path, _, _, _)| path.clone())
            .collect::<Vec<_>>();
        for (reason, path, digest) in
            self.related_sources(&fresh_owners, PACKET_RELATED, &|| false)?
        {
            let row = format!(
                "Related {}\n",
                json!({"path":path,"digest":digest,"reason":reason,"freshness":"fresh"})
            );
            if out.len() + row.len() + checks.len() <= max_bytes {
                out.push_str(&row);
            }
        }
        out.push_str(&checks);
        Ok(out)
    }
}

/// Freshness of one owner against the observed snapshot, with the same vocabulary as the
/// architecture report: a partial snapshot cannot vouch for an unseen file.
fn owner_freshness(
    cache: &Codebase,
    snapshot: &Snapshot,
    path: &str,
    indexed: &str,
) -> &'static str {
    match snapshot.digest(path) {
        Some(current) if current == indexed => "fresh",
        Some(_) => "changed",
        None if cache.source_missing(path) => "missing",
        None if snapshot.complete => "unadmitted",
        None => "unchecked",
    }
}
