//! Bounded dependency expansion for authenticated evidence manifests, never source locators.
use super::*;
use std::collections::{HashSet, VecDeque};

const MAX_MEMBERS: usize = 4096;
const MAX_DEPTH: usize = 64;
const MAX_MANIFEST_BYTES: u64 = 1024 * 1024;
const MAX_SCAN_BYTES: u64 = 64 * 1024 * 1024;
// retain_evidence serializes StoredEvidence in declaration order with serde_json::to_vec.
// Discovery recognizes that canonical v1 encoding only, not arbitrary/reformatted JSON artifacts.
const MANIFEST_PREFIX: &[u8] = b"{\"schema_version\":1,\"evidence\":";

impl ArtifactStore {
    /// Add representation, snapshot, parent and clock-basis dependencies of retained manifests.
    /// Missing bodies remain in the set: pack/relocation fail when copying them, while GC reports
    /// them as missing. Other read failures and traversal exhaustion fail closed. Ordinary blobs
    /// are not searched for references; only a hash-verified, validated manifest contributes edges.
    /// Recognition covers the canonical v1 encoding written by retain_evidence. Ordinary root
    /// blobs get only a fixed-size prefix probe and do not consume evidence traversal budgets.
    pub fn expand_evidence_references(&self, references: &mut HashSet<String>) -> Result<()> {
        let mut evidence_members = HashSet::new();
        let mut roots: Vec<_> = references.iter().cloned().collect();
        roots.sort();
        let mut pending: VecDeque<_> = roots.into_iter().map(|uri| (uri, 0)).collect();
        let mut scanned_bytes = 0u64;
        while let Some((uri, depth)) = pending.pop_front() {
            let path = self.blob_path(&uri)?;
            let metadata = match fs::symlink_metadata(&path) {
                Ok(metadata) => metadata,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => return Err(error.into()),
            };
            ensure!(
                metadata.is_file(),
                "referenced artifact {uri} is not a regular file"
            );
            // Legacy outputs get only a fixed-size recognition probe. Their count and full body
            // size must not consume the new evidence graph budget or change pack/GC semantics.
            let mut file = self.open(&uri)?;
            let mut bytes = Vec::new();
            (&mut file)
                .take(MANIFEST_PREFIX.len() as u64)
                .read_to_end(&mut bytes)?;
            if bytes != MANIFEST_PREFIX {
                continue;
            }
            ensure!(
                metadata.len() <= MAX_MANIFEST_BYTES,
                "evidence manifest exceeds reference read limit"
            );
            evidence_members.insert(uri.clone());
            ensure!(
                evidence_members.len() <= MAX_MEMBERS,
                "evidence reference member limit exceeded"
            );
            file.take(MAX_MANIFEST_BYTES + 1 - bytes.len() as u64)
                .read_to_end(&mut bytes)?;
            ensure!(
                bytes.len() as u64 <= MAX_MANIFEST_BYTES,
                "evidence reference probe grew beyond limit"
            );
            // Charge actual probe bytes, not metadata (a file may grow between stat and read).
            // Reserve the same amount for evidence_record's authenticated reread; any mutation
            // of that immutable blob makes authentication fail and ends the traversal.
            scanned_bytes = scanned_bytes.saturating_add((bytes.len() as u64).saturating_mul(2));
            ensure!(
                scanned_bytes <= MAX_SCAN_BYTES,
                "evidence reference byte limit exceeded"
            );
            // A recognized but invalid manifest is an error, never a silently skipped graph.
            let record = self
                .evidence_record(&uri)
                .with_context(|| format!("expanding evidence manifest {uri}"))?;
            let context = record.evidence.context;
            let mut children = vec![record.representation_uri];
            children.extend(context.parents);
            if let Some(snapshot) = context.snapshot {
                children.extend(snapshot.artifact);
            }
            for alignment in context.alignments {
                children.extend(alignment.basis);
            }
            for child in children {
                evidence_members.insert(child.clone());
                ensure!(
                    evidence_members.len() <= MAX_MEMBERS,
                    "evidence reference member limit exceeded"
                );
                if references.contains(&child) {
                    continue;
                }
                ensure!(depth < MAX_DEPTH, "evidence reference depth limit exceeded");
                references.insert(child.clone());
                pending.push_back((child, depth + 1));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests;
