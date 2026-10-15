//! Explicit human feedback bound to immutable artifact content and source lines.
use crate::{
    storage::{paths::Layout, settings_store},
    tools::{artifact::ArtifactStore, resources},
};
use anyhow::{Result, ensure};
use clap::Args;
use sha2::{Digest, Sha256};

#[derive(Debug, Args)]
pub struct AnnotateArgs {
    /// Content-addressed artifact URI from this project's session output.
    pub artifact: String,
    /// Human feedback. Stored as data; never executed or substituted for operator policy.
    #[arg(long, required_unless_present = "list", conflicts_with = "list")]
    pub note: Option<String>,
    /// First source line, inclusive (1-based).
    #[arg(long, requires = "note")]
    pub start: Option<u64>,
    /// Last source line, inclusive; defaults to --start.
    #[arg(long, requires = "start")]
    pub end: Option<u64>,
    /// List previously recorded human feedback for this exact artifact.
    #[arg(long)]
    pub list: bool,
    /// First annotation to list (1-based); source row selectors remain --start/--end.
    #[arg(long, requires = "list")]
    pub page_start: Option<usize>,
    /// Annotation count, capped at 500 and 64 KiB per page.
    #[arg(long, requires = "list")]
    pub count: Option<usize>,
}
use crate::storage::annotations::Annotation;
pub fn run(layout: &Layout, args: &AnnotateArgs) -> Result<()> {
    let id = resources::artifact_id(&args.artifact).map_err(anyhow::Error::msg)?;
    let root = layout.state_dir.join("annotations").join(&id);
    if args.list {
        let notes = crate::storage::annotations::read(
            &layout.state_dir.join("annotations"),
            &id,
            args.page_start.unwrap_or(1),
            args.count.unwrap_or(100),
        )?;
        println!("{}", serde_json::to_string_pretty(&notes)?);
        return Ok(());
    }
    let note = args.note.as_deref().unwrap_or("");
    ensure!(
        !note.trim().is_empty() && note.len() <= 16 * 1024,
        "feedback must be nonempty and at most 16 KiB"
    );
    let store = ArtifactStore::new(&layout.state_dir.join("artifacts"))?;
    // Validate the content hash as well as availability before accepting feedback.
    let file = layout.state_dir.join("artifacts").join(&id);
    ensure!(
        !std::fs::symlink_metadata(&file)?.file_type().is_symlink(),
        "artifact cannot be a symlink"
    );
    let hash = crate::runtime::fingerprint::sha256_file(&file)?;
    ensure!(
        hash == id,
        "artifact contents no longer match their identity"
    );
    let end = args.end.or(args.start);
    if let Some(start) = args.start {
        ensure!(
            start > 0 && end.is_some_and(|end| end >= start && end - start < 500),
            "annotation range must contain 1..500 source lines"
        );
        // Uses the artifact owner's bounded line reader; a missing source row is rejected.
        let rows = store.retrieve(
            &args.artifact,
            start as usize,
            (end.unwrap() - start + 1) as usize,
        )?;
        ensure!(
            rows.lines()
                .any(|row| row.starts_with(&format!("{start}:")))
                && rows
                    .lines()
                    .any(|row| row.starts_with(&format!("{}:", end.unwrap()))),
            "annotation range is outside available artifact lines"
        );
    }
    let annotation = Annotation {
        schema_version: 1,
        artifact: format!("artifact://{id}"),
        sha256: id,
        start: args.start,
        end,
        note: note.into(),
        created_at_unix_ms: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_millis(),
        origin: "human_cli".into(),
    };
    let bytes = serde_json::to_vec(&annotation)?;
    let key = format!("{:x}", Sha256::digest(&bytes));
    settings_store::private_dir(&root)?;
    // Content-addressed records avoid overwriting simultaneous annotations. GC scans their
    // artifact references through the existing state-file traversal, preserving target blobs.
    settings_store::write_private(&root.join(format!("{key}.json")), &bytes)?;
    println!("{}", serde_json::to_string_pretty(&annotation)?);
    Ok(())
}
