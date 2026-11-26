use super::{Store, plan_in, remove_planned};
use crate::llm::catalog::ModelEntry;
use crate::llm::model_config::ModelSelection;
use std::fs;
use std::path::{Path, PathBuf};

fn entry(label: &str, model: &Path) -> ModelEntry {
    ModelEntry {
        selection: ModelSelection {
            model: model.to_string_lossy().into_owned(),
            ..ModelSelection::default()
        },
        label: label.into(),
        source: "test store".into(),
        available: true,
        metadata: Default::default(),
    }
}

fn write(path: &Path, bytes: usize) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, vec![0u8; bytes]).unwrap();
}

/// A Hugging Face cache repository whose snapshot links to a blob, and an LM Studio store with
/// one MLX folder and one GGUF folder.
struct Stores {
    _dir: tempfile::TempDir,
    root: PathBuf,
    snapshot: PathBuf,
    gguf: PathBuf,
    stores: Vec<Store>,
}

fn stores() -> Stores {
    let dir = tempfile::tempdir().unwrap();
    let root = fs::canonicalize(dir.path()).unwrap();
    let repo = root.join("hub/models--org--chat");
    write(&repo.join("blobs/abc"), 100);
    let snapshot = repo.join("snapshots/rev");
    fs::create_dir_all(&snapshot).unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink(repo.join("blobs/abc"), snapshot.join("model.safetensors")).unwrap();
    #[cfg(not(unix))]
    write(&snapshot.join("model.safetensors"), 100);
    write(&root.join("studio/pub/mlx-model/model.safetensors"), 30);
    write(&root.join("studio/pub/gguf-model/q4.gguf"), 20);
    write(&root.join("studio/pub/gguf-model/q8.gguf"), 40);
    let stores = vec![
        Store {
            root: root.join("hub"),
            depth: 1,
        },
        Store {
            root: root.join("studio"),
            depth: 2,
        },
    ];
    Stores {
        gguf: root.join("studio/pub/gguf-model/q4.gguf"),
        _dir: dir,
        root,
        snapshot,
        stores,
    }
}

#[test]
fn a_cache_snapshot_removes_its_whole_repository_with_shared_blobs() {
    let s = stores();
    let entries = [entry("org/chat", &s.snapshot)];
    let plan = plan_in("org/chat", &entries, &s.stores).unwrap();
    assert_eq!(plan.folder, s.root.join("hub/models--org--chat"));
    // The blob counts once; the snapshot link adds nothing.
    let expected = if cfg!(unix) { 100 } else { 200 };
    assert_eq!(plan.bytes, expected);
    let by_path = plan_in(&s.snapshot.to_string_lossy(), &entries, &s.stores).unwrap();
    assert_eq!(by_path, plan);
    remove_planned(&plan, &[]).unwrap();
    assert!(!plan.folder.exists());
    assert!(s.root.join("hub").is_dir(), "the cache itself stays");
}

#[test]
fn an_lm_studio_gguf_file_removes_its_model_folder_only() {
    let s = stores();
    let entries = [
        entry("pub/gguf-model", &s.gguf),
        entry("pub/mlx-model", &s.root.join("studio/pub/mlx-model")),
    ];
    let plan = plan_in("pub/gguf-model", &entries, &s.stores).unwrap();
    assert_eq!(plan.folder, s.root.join("studio/pub/gguf-model"));
    assert_eq!(plan.bytes, 60);
    remove_planned(&plan, &[]).unwrap();
    assert!(!plan.folder.exists());
    assert!(
        s.root
            .join("studio/pub/mlx-model/model.safetensors")
            .is_file()
    );
}

#[test]
fn a_selected_model_is_refused_and_kept() {
    let s = stores();
    let entries = [entry("org/chat", &s.snapshot)];
    let plan = plan_in("org/chat", &entries, &s.stores).unwrap();
    let error = remove_planned(&plan, &[s.snapshot.to_string_lossy().into_owned()])
        .unwrap_err()
        .to_string();
    assert!(error.contains("selected in your configuration"), "{error}");
    assert!(plan.folder.is_dir());
    // Another model being selected does not block it.
    remove_planned(&plan, &["/elsewhere/model".into()]).unwrap();
}

#[test]
fn gguf_folder_removal_protects_a_sibling_selected_by_label() {
    let s = stores();
    let entries = [entry("pub/gguf-model · q4.gguf", &s.gguf)];
    let plan = plan_in(&entries[0].label, &entries, &s.stores).unwrap();
    for selected in ["pub/gguf-model · q8.gguf", "pub/gguf-model"] {
        assert!(super::ensure_not_in_use(&plan, &[selected.into()]).is_err());
    }
    assert!(super::ensure_not_in_use(&plan, &["pub/gguf-model-other · q8.gguf".into()]).is_ok());
    let config = crate::config::types::Config::default();
    let mut selection = ModelSelection::new("mlx", "pub/gguf-model");
    assert!(
        super::models_in_use_with_current(&config, Some(&selection)).contains(&selection.model)
    );
    selection.base_url = Some("http://127.0.0.1:1234/v1".into());
    assert!(
        !super::models_in_use_with_current(&config, Some(&selection)).contains(&selection.model)
    );
}

#[test]
fn unknown_models_and_paths_outside_the_stores_are_refused() {
    let s = stores();
    let outside = s.root.join("elsewhere/model");
    write(&outside.join("model.safetensors"), 5);
    let entries = [entry("org/chat", &s.snapshot), entry("x/outside", &outside)];
    let unknown = plan_in("org/other", &entries, &s.stores).unwrap_err();
    assert!(unknown.to_string().contains("not a downloaded local model"));
    assert!(
        plan_in("  ", &entries, &s.stores)
            .unwrap_err()
            .to_string()
            .contains("name the local model")
    );
    let error = format!(
        "{:#}",
        plan_in("x/outside", &entries, &s.stores).unwrap_err()
    );
    assert!(error.contains("outside the Hugging Face cache"), "{error}");
    assert!(outside.is_dir());
    // A store root itself is never a model folder.
    let root_entry = [entry("hub", &s.root.join("hub"))];
    assert!(plan_in("hub", &root_entry, &s.stores).is_err());
}

#[cfg(unix)]
#[test]
fn a_linked_model_folder_is_not_followed() {
    let s = stores();
    let target = s.root.join("elsewhere/real");
    write(&target.join("model.safetensors"), 5);
    let link = s.root.join("studio/pub/linked");
    std::os::unix::fs::symlink(&target, &link).unwrap();
    let entries = [entry("pub/linked", &link)];
    assert!(plan_in("pub/linked", &entries, &s.stores).is_err());
    // A plan made before the folder was replaced by a link is re-checked before deleting.
    let plan = super::LocalRemoval {
        label: "pub/linked".into(),
        model: link.to_string_lossy().into_owned(),
        folder: link.clone(),
        bytes: 0,
        source: String::new(),
    };
    assert!(remove_planned(&plan, &[]).is_err());
    assert!(target.join("model.safetensors").is_file());
}
