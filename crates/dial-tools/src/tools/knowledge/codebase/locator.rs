//! Source-local behavior locators. Never re-emit a saved claim based on a partial tree scan:
//! the only things asserted here are the identity and location of two current declarations.
use super::*;

/// A label must name exactly one identifier, not a command or an expression.
fn identifier<'a>(summary: &'a str, label: &str) -> Option<&'a str> {
    let mut fields = summary
        .lines()
        .filter_map(|line| line.trim().strip_prefix(label));
    let name = fields.next()?.trim().strip_prefix('`')?.strip_suffix('`')?;
    (fields.next().is_none()
        && !name.is_empty()
        && name.len() <= 200
        && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_'))
    .then_some(name)
}

impl Codebase {
    pub(super) fn behavior_locator(&self, query: &str, budget: usize) -> Result<Option<String>> {
        let terms = ambient_terms(query);
        if terms.is_empty() || budget == 0 {
            return Ok(None);
        }
        let conn = self.conn.lock();
        let mut notes = conn.prepare("SELECT topic,content,sources FROM codebase_notes n
            WHERE NOT EXISTS (SELECT 1 FROM codebase_meta WHERE key='finding_contradiction:' || n.topic)
            ORDER BY topic")?;
        let mut candidates = Vec::new();
        for row in notes.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
            ))
        })? {
            let (topic, summary, sources) = row?;
            let (Some(owner), Some(check)) = (
                identifier(&summary, "Owner:"),
                identifier(&summary, "Check:"),
            ) else {
                continue;
            };
            if owner == check || screen(&topic).is_err() || screen(&summary).is_err() {
                continue;
            }
            let score = ambient_terms(&topic).intersection(&terms).count();
            let named = [owner, check].contains(&query.trim());
            if !named && (score < 2 || !ambient_overlap(query, score)) {
                continue;
            }
            candidates.push((
                !named,
                std::cmp::Reverse(score),
                topic,
                owner.to_owned(),
                check.to_owned(),
                sources,
            ));
        }
        if candidates.is_empty() {
            return Ok(None);
        }
        candidates.sort();
        let ws = Workspace::new(&self.root)?;
        let mut bytes = 8 * MAX_FILE as usize;
        let started = Instant::now();
        'notes: for (_, _, topic, owner, check, raw) in candidates.into_iter().take(4) {
            let sources: Vec<Source> = serde_json::from_str(&raw)?;
            let mut matches = Vec::new();
            for s in sources {
                if bytes == 0 || started.elapsed() >= SCAN_TIME {
                    return Ok(None);
                }
                if screen(&s.path).is_err() {
                    continue 'notes;
                }
                let Some((text, digest)) = self.observed_source(&ws, &s.path, &mut bytes) else {
                    continue 'notes;
                };
                if digest != s.digest {
                    continue 'notes;
                }
                for (name, line) in crate::tools::edit::syntax::declaration_sites(
                    Path::new(&s.path),
                    &text,
                    &[&owner, &check],
                ) {
                    matches.push((name, s.path.clone(), line));
                }
            }
            matches.sort();
            if matches.len() != 2 || matches[0].0 == matches[1].0 {
                continue;
            }
            let site = |name: &str| {
                matches
                    .iter()
                    .find(|(n, _, _)| n == name)
                    .map(|(_, path, line)| json!({"symbol": name, "path": path, "line": line}))
            };
            let row = format!(
                "Behavior locator (agent suggestion; current declarations in cited sources only, not proof of ownership, coverage or a passing check): {}\n",
                json!({"topic":topic,"owner":site(&owner),"check":site(&check)})
            );
            if row.len() <= budget {
                return Ok(Some(row));
            }
        }
        Ok(None)
    }
}
