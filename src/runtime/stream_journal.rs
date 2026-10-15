//! Append-only streaming supplement to the session journal. Final snapshots stay in the main
//! branch journal; incremental frames preserve observations between those boundaries.
use super::Node;
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    fs::File,
    io::{BufRead, BufReader, Write},
    path::{Path, PathBuf},
    sync::{Arc, LazyLock, Mutex, Weak},
};

#[derive(Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum Frame {
    Snapshot {
        #[serde(default)]
        schema_version: u32,
        node: Node,
    },
    Append {
        #[serde(default)]
        schema_version: u32,
        id: String,
        revision: u64,
        text: String,
    },
}
#[derive(Debug)]
struct Writer {
    file: File,
    path: PathBuf,
    error: Option<String>,
}
static WRITERS: LazyLock<Mutex<BTreeMap<PathBuf, Weak<Mutex<Writer>>>>> =
    LazyLock::new(Default::default);
const RECLAIM_AT: u64 = 4 * 1024 * 1024;
#[derive(Clone, Debug)]
pub(crate) struct Journal(Arc<Mutex<Writer>>);
pub(crate) fn path(session: &Path) -> std::path::PathBuf {
    session.with_extension("streams")
}
pub(crate) fn remove(session: &Path) -> std::io::Result<()> {
    let target = path(session);
    let canonical = target
        .parent()
        .and_then(|parent| parent.canonicalize().ok())
        .and_then(|parent| target.file_name().map(|name| parent.join(name)));
    if let Some(canonical) = canonical {
        let mut writers = WRITERS.lock().unwrap_or_else(|poison| poison.into_inner());
        if let Some(writer) = writers
            .remove(&canonical)
            .and_then(|writer| writer.upgrade())
        {
            writer
                .lock()
                .unwrap_or_else(|poison| poison.into_inner())
                .error =
                Some("stream journal removed by its owner; stale publication refused".into());
        }
    }
    match std::fs::remove_file(path(session)) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        result => result,
    }
}
pub(crate) fn copy(source: &Path, destination: &Path) -> anyhow::Result<()> {
    let nodes = recover(source)?;
    if nodes.is_empty() {
        return Ok(());
    }
    let journal = Journal::open(destination)?;
    for node in nodes {
        journal.snapshot(&node);
    }
    journal.check().map_err(anyhow::Error::msg)
}
pub(crate) fn relocate(source: &Path, destination: &Path) -> std::io::Result<()> {
    let source = path(source);
    let destination = path(destination);
    if destination.exists() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            "stream archive destination exists",
        ));
    }
    let source = source
        .parent()
        .ok_or_else(|| std::io::Error::other("stream source has no parent"))?
        .canonicalize()?
        .join(source.file_name().unwrap());
    let destination = destination
        .parent()
        .ok_or_else(|| std::io::Error::other("stream destination has no parent"))?
        .canonicalize()?
        .join(destination.file_name().unwrap());
    let mut writers = WRITERS.lock().unwrap_or_else(|poison| poison.into_inner());
    let live = writers.get(&source).and_then(Weak::upgrade);
    let mut writer = live
        .as_ref()
        .map(|writer| writer.lock().unwrap_or_else(|poison| poison.into_inner()));
    match std::fs::rename(&source, &destination) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
        Ok(()) => {}
    }
    if let Some(writer) = writer.as_mut() {
        writer.path = destination.clone();
    }
    writers.remove(&source);
    if let Some(live) = live.as_ref() {
        writers.insert(destination, Arc::downgrade(live));
    }
    Ok(())
}
impl Journal {
    #[cfg(test)]
    pub(crate) fn fail_writes_for_test(&self, read_only: File) {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).file = read_only;
    }
    pub fn open(session: &Path) -> std::io::Result<Self> {
        let path = path(session);
        let path = path
            .parent()
            .ok_or_else(|| std::io::Error::other("stream path has no parent"))?
            .canonicalize()?
            .join(
                path.file_name()
                    .ok_or_else(|| std::io::Error::other("stream path has no filename"))?,
            );
        let mut writers = WRITERS.lock().unwrap_or_else(|poison| poison.into_inner());
        writers.retain(|_, writer| writer.strong_count() > 0);
        if let Some(existing) = writers.get(&path).and_then(Weak::upgrade) {
            let writer = existing.lock().unwrap_or_else(|poison| poison.into_inner());
            if crate::platform::fs::identity(&writer.file)?
                != crate::platform::fs::path_identity(&path)?
            {
                return Err(std::io::Error::other(
                    "stream journal was replaced outside its writer",
                ));
            }
            drop(writer);
            return Ok(Self(existing));
        }
        let mut options = std::fs::OpenOptions::new();
        options.append(true).read(true).create(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.custom_flags(libc::O_NOFOLLOW);
        }
        let mut file = crate::platform::fs::open_owner_only(&options, &path)?;
        if !file.metadata()?.is_file()
            || !crate::platform::fs::owned_by_current_user(&file)?
            || crate::platform::fs::link_count(&file)? != 1
        {
            return Err(std::io::Error::other(
                "stream journal requires an owner-owned regular file without hard links",
            ));
        }
        // A killed write may leave a partial line. Separate it from all subsequent frames.
        if file.metadata()?.len() > 0 {
            recover(session).map_err(|error| {
                std::io::Error::new(std::io::ErrorKind::InvalidData, error.to_string())
            })?;
            file.write_all(b"\n")?;
        }
        let writer = Arc::new(Mutex::new(Writer {
            file,
            path: path.clone(),
            error: None,
        }));
        writers.insert(path, Arc::downgrade(&writer));
        Ok(Self(writer))
    }
    fn append(&self, frame: Frame) {
        let mut writer = self.0.lock().unwrap_or_else(|e| e.into_inner());
        if writer.error.is_some() {
            return;
        }
        let result = (|| {
            let mut bytes = serde_json::to_vec(&frame)?;
            bytes.push(b'\n');
            writer.file.write_all(&bytes)
        })();
        if let Err(error) = result {
            writer.error = Some(format!("stream journal failed: {error}"));
        }
    }
    pub fn snapshot(&self, node: &Node) {
        self.append(Frame::Snapshot {
            schema_version: 1,
            node: node.clone(),
        });
    }
    pub fn text(&self, id: &str, revision: u64, text: &str) {
        self.append(Frame::Append {
            schema_version: 1,
            id: id.into(),
            revision,
            text: text.into(),
        });
    }
    pub fn check(&self) -> Result<(), String> {
        self.0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .error
            .clone()
            .map_or(Ok(()), Err)
    }
    pub fn needs_reclamation(&self) -> bool {
        self.0
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .file
            .metadata()
            .is_ok_and(|metadata| metadata.len() >= RECLAIM_AT)
    }
    /// Caller must sync the main session before supplying its acknowledged runtime revisions.
    /// Unacknowledged observations from every branch survive, including an active partial reply.
    pub fn checkpoint(&self, acknowledged: &BTreeMap<String, u64>) -> anyhow::Result<()> {
        let mut writer = self.0.lock().unwrap_or_else(|poison| poison.into_inner());
        anyhow::ensure!(
            writer.error.is_none(),
            "stream journal has a latched write failure"
        );
        let result = (|| -> anyhow::Result<()> {
            let session = writer.path.with_extension("jsonl");
            let nodes = recover(&session)?;
            let mut bytes = Vec::new();
            for node in nodes {
                // Live bodies still append delta frames. Keep their complete baseline even
                // when the main log has acknowledged this revision.
                if node.result.phase != super::Phase::Running
                    && acknowledged
                        .get(&node.id)
                        .is_some_and(|revision| *revision >= node.result.revision)
                {
                    continue;
                }
                serde_json::to_writer(
                    &mut bytes,
                    &Frame::Snapshot {
                        schema_version: 1,
                        node,
                    },
                )?;
                bytes.push(b'\n');
            }
            crate::storage::settings_store::write_private(&writer.path, &bytes)?;
            let file = crate::platform::fs::open_owner_only(
                std::fs::OpenOptions::new().append(true),
                &writer.path,
            )?;
            writer.file = file;
            Ok(())
        })();
        if let Err(error) = &result {
            writer.error = Some(format!("stream journal checkpoint failed: {error:#}"));
        }
        result
    }
}

pub(crate) fn recover(session: &Path) -> anyhow::Result<Vec<Node>> {
    let path = path(session);
    let file = match crate::platform::fs::open_nofollow_nonblocking(
        std::fs::OpenOptions::new().read(true),
        &path,
    ) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error.into()),
    };
    anyhow::ensure!(
        file.metadata()?.is_file()
            && crate::platform::fs::owned_by_current_user(&file)?
            && crate::platform::fs::link_count(&file)? == 1,
        "invalid stream journal"
    );
    let mut nodes = BTreeMap::<String, Node>::new();
    let mut reader = BufReader::new(file);
    let mut bytes = Vec::new();
    loop {
        bytes.clear();
        // Bound corrupt rows without allocating an unbounded line.
        let n = std::io::Read::take(&mut reader, 32 * 1024 * 1024).read_until(b'\n', &mut bytes)?;
        if n == 0 {
            break;
        }
        if bytes.last() != Some(&b'\n') {
            break;
        }
        #[derive(Deserialize)]
        struct Version {
            #[serde(default)]
            schema_version: u32,
        }
        if let Ok(version) = serde_json::from_slice::<Version>(&bytes) {
            anyhow::ensure!(
                version.schema_version <= 1,
                "unsupported stream journal version {}; source was preserved",
                version.schema_version
            );
        }
        let Ok(frame) = serde_json::from_slice::<Frame>(&bytes) else {
            continue;
        };
        match frame {
            Frame::Snapshot { node, .. } => {
                nodes.insert(node.id.clone(), node);
            }
            Frame::Append {
                id, revision, text, ..
            } => {
                if let Some(node) = nodes.get_mut(&id)
                    && revision == node.result.revision.saturating_add(1)
                {
                    node.result.text.push_str(&text);
                    node.result.revision = revision;
                }
            }
        }
    }
    Ok(nodes.into_values().collect())
}

#[cfg(test)]
mod reclamation_tests {
    use super::*;
    #[test]
    fn acknowledged_live_baselines_survive_replacement_and_old_handle_appends() {
        let root = tempfile::tempdir().unwrap();
        let session = root.path().join("session.jsonl");
        let journal = Journal::open(&session).unwrap();
        let alias = Journal::open(&session).unwrap();
        let tree = crate::runtime::Tree::default();
        tree.journal(journal.clone());
        let body = tree.start("assistant", "live", None);
        body.append("first");
        let node = body.snapshot();
        let acknowledged = BTreeMap::from([(node.id.clone(), node.result.revision)]);
        alias.checkpoint(&acknowledged).unwrap();
        body.append(" second");
        journal.check().unwrap();
        let recovered = recover(&session).unwrap();
        assert_eq!(recovered.len(), 1);
        assert_eq!(recovered[0].result.text, "first second");
        assert_eq!(recovered[0].result.revision, body.revision());
    }
    #[test]
    fn only_durably_acknowledged_settled_nodes_are_reclaimed() {
        let root = tempfile::tempdir().unwrap();
        let session = root.path().join("session.jsonl");
        let journal = Journal::open(&session).unwrap();
        let tree = crate::runtime::Tree::default();
        tree.journal(journal.clone());
        let settled = tree.start("tool", "settled", None);
        settled.settle("done".into(), None, crate::runtime::Phase::Completed);
        let abandoned = tree.start("tool", "abandoned branch", None);
        abandoned.settle("preserve".into(), None, crate::runtime::Phase::Interrupted);
        journal.checkpoint(&BTreeMap::new()).unwrap();
        assert_eq!(recover(&session).unwrap().len(), 2);
        let node = settled.snapshot();
        journal
            .checkpoint(&BTreeMap::from([(node.id, node.result.revision)]))
            .unwrap();
        let recovered = recover(&session).unwrap();
        assert_eq!(recovered.len(), 1);
        assert_eq!(recovered[0].result.text, "preserve");
    }

    #[test]
    fn relocation_updates_all_publishers_and_owner_removal_refuses_stale_writes() {
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("source.jsonl");
        let destination = root.path().join("archive.jsonl");
        let journal = Journal::open(&source).unwrap();
        let tree = crate::runtime::Tree::default();
        tree.journal(journal.clone());
        let body = tree.start("assistant", "live", None);
        body.append("before");
        relocate(&source, &destination).unwrap();
        body.append(" after");
        journal.checkpoint(&BTreeMap::new()).unwrap();
        assert!(!path(&source).exists());
        assert_eq!(
            recover(&destination).unwrap()[0].result.text,
            "before after"
        );
        remove(&destination).unwrap();
        body.append(" stale");
        assert!(journal.check().is_err());
        assert!(!path(&destination).exists());
    }

    #[test]
    fn future_stream_versions_are_preserved_and_refused_before_append() {
        let root = tempfile::tempdir().unwrap();
        let session = root.path().join("future.jsonl");
        let bytes = b"{\"schema_version\":2,\"kind\":\"future\"}\n";
        std::fs::write(path(&session), bytes).unwrap();
        assert!(recover(&session).is_err());
        assert!(Journal::open(&session).is_err());
        assert_eq!(std::fs::read(path(&session)).unwrap(), bytes);
    }
}
