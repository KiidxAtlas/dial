use super::*;
use image::{ImageFormat, Rgb, RgbImage};

/// A deterministic embedding: images map to their mean colour, text to a named colour.
fn colour_vector(input: &Input<'_>) -> Vec<f32> {
    let raw = match input {
        Input::Text(text) if text.contains("red") => vec![1.0, 0.0, 0.0],
        Input::Text(text) if text.contains("green") => vec![0.0, 1.0, 0.0],
        Input::Text(_) => vec![0.0, 0.0, 1.0],
        Input::Image(bytes) => {
            let image = image::load_from_memory(bytes).unwrap().to_rgb8();
            let n = f32::from(u16::try_from(image.pixels().len()).unwrap());
            (0..3)
                .map(|c| image.pixels().map(|p| f32::from(p[c])).sum::<f32>() / n)
                .collect()
        }
    };
    let norm = raw.iter().map(|v| v * v).sum::<f32>().sqrt().max(1e-6);
    raw.into_iter().map(|v| v / norm).collect()
}

fn png(path: &Path, colour: [u8; 3]) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    RgbImage::from_pixel(8, 8, Rgb(colour))
        .save_with_format(path, ImageFormat::Png)
        .unwrap();
}

struct Rig {
    _dir: tempfile::TempDir,
    ws: Workspace,
    config: Config,
    index: Index,
}

fn rig() -> Rig {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("project").canonicalize_or_create();
    png(&root.join("assets/red.png"), [250, 10, 10]);
    png(&root.join("assets/green.png"), [10, 240, 20]);
    png(&root.join("assets/nested/reddish.png"), [200, 60, 40]);
    std::fs::write(root.join("assets/notes.txt"), "not an image").unwrap();
    std::fs::write(root.join("assets/broken.png"), "not a png").unwrap();
    png(&root.join("node_modules/pkg/red.png"), [255, 0, 0]);
    let mut config = Config {
        state_dir: dir.path().join("state"),
        ..Config::default()
    };
    config.model_roles.insert(
        embedding::ROLE.into(),
        crate::llm::model_config::ModelSelection::new("mlx", "test/embedder"),
    );
    let index = Index::open(&config.state_dir.join("image_embeddings.sqlite3")).unwrap();
    Rig {
        ws: Workspace::new(&root).unwrap(),
        _dir: dir,
        config,
        index,
    }
}

trait CanonicalOrCreate {
    fn canonicalize_or_create(self) -> PathBuf;
}

impl CanonicalOrCreate for PathBuf {
    fn canonicalize_or_create(self) -> PathBuf {
        std::fs::create_dir_all(&self).unwrap();
        self.canonicalize().unwrap()
    }
}

fn search(rig: &Rig, args: Value) -> Result<String, String> {
    run(
        &rig.ws,
        &rig.config,
        &rig.index,
        &args,
        &Embedder::Fake(colour_vector),
    )
}

fn ranked_paths(out: &str) -> Vec<&str> {
    out.lines()
        .skip(1)
        .filter(|line| !line.starts_with('['))
        .filter_map(|line| line.split_once("  ").map(|(_, path)| path))
        .collect()
}

#[test]
fn text_mode_ranks_workspace_images_and_skips_dependencies_and_non_images() {
    let rig = rig();
    let out = search(&rig, json!({"mode": "text", "query": "a red square"})).unwrap();
    assert_eq!(
        ranked_paths(&out),
        [
            "assets/red.png",
            "assets/nested/reddish.png",
            "assets/green.png"
        ],
        "{out}"
    );
    assert!(out.contains("4 images, 3 newly embedded"), "{out}");
    assert!(out.contains("1 files skipped"), "{out}");
    // A second search reuses the cached vectors.
    let again = search(&rig, json!({"mode": "text", "query": "green", "limit": 1})).unwrap();
    assert!(again.contains("0 newly embedded"), "{again}");
    assert_eq!(ranked_paths(&again), ["assets/green.png"]);
}

#[test]
fn similar_mode_excludes_the_reference_and_respects_the_scope() {
    let rig = rig();
    let out = search(
        &rig,
        json!({"mode": "similar", "image": "assets/red.png", "path": "assets"}),
    )
    .unwrap();
    assert_eq!(
        ranked_paths(&out),
        ["assets/nested/reddish.png", "assets/green.png"],
        "{out}"
    );
    let nested = search(
        &rig,
        json!({"mode": "similar", "image": "assets/green.png", "path": "assets/nested"}),
    )
    .unwrap();
    assert_eq!(ranked_paths(&nested), ["assets/nested/reddish.png"]);
}

#[test]
fn score_mode_scores_each_image_against_the_prompt_in_order() {
    let rig = rig();
    let out = search(
        &rig,
        json!({"mode": "score", "query": "red", "images": ["assets/green.png", "assets/red.png"]}),
    )
    .unwrap();
    let scores: Vec<f32> = out
        .lines()
        .skip(1)
        .map(|line| line.split_once("  ").unwrap().0.parse().unwrap())
        .collect();
    assert_eq!(scores.len(), 2);
    assert!(scores[1] > 0.95 && scores[0] < 0.2, "{out}");
}

#[test]
fn invalid_requests_and_paths_outside_the_workspace_are_refused() {
    let rig = rig();
    for (args, needle) in [
        (json!({}), "mode is required"),
        (json!({"mode": "draw"}), "unknown mode"),
        (json!({"mode": "text"}), "requires query"),
        (json!({"mode": "text", "query": "x", "limit": 0}), "limit"),
        (json!({"mode": "similar"}), "requires image"),
        (
            json!({"mode": "similar", "image": "assets/notes.txt"}),
            "not a readable image",
        ),
        (json!({"mode": "score", "query": "x"}), "requires images"),
        (json!({"mode": "score", "query": "x", "images": []}), "1 to"),
        (
            json!({"mode": "text", "query": "x", "path": "/etc"}),
            "outside the workspace",
        ),
        (
            json!({"mode": "text", "query": "x", "path": "../"}),
            "escapes",
        ),
        (
            json!({"mode": "text", "query": "x", "path": "missing"}),
            "no such file",
        ),
    ] {
        let error = search(&rig, args.clone()).unwrap_err();
        assert!(error.contains(needle), "{args}: {error}");
    }
}

#[test]
fn the_tool_is_offered_only_while_the_role_is_assigned() {
    let rig = rig();
    assert_eq!(tools(&rig.config).len(), 1);
    assert!(tools(&Config::default()).is_empty());
    let unassigned = Rig {
        config: Config {
            state_dir: rig.config.state_dir.clone(),
            ..Config::default()
        },
        ..rig
    };
    assert!(
        search(&unassigned, json!({"mode": "text", "query": "red"}))
            .unwrap_err()
            .contains("/setup image-embeddings")
    );
}

#[test]
fn cached_vectors_are_keyed_by_content_and_model() {
    let rig = rig();
    rig.index.put("abc", "m1", &[0.5, -0.25]).unwrap();
    assert_eq!(rig.index.get("abc", "m1").unwrap(), Some(vec![0.5, -0.25]));
    assert_eq!(rig.index.get("abc", "m2").unwrap(), None);
    assert_eq!(rig.index.get("abd", "m1").unwrap(), None);
}

#[test]
fn image_gen_similarity_line_is_absent_until_the_role_is_assigned() {
    let rig = rig();
    let image = rig.ws.root().join("assets/red.png");
    assert_eq!(
        prompt_similarity_line(&Config::default(), "a red square", &image),
        None
    );
    assert_eq!(prompt_similarity_line(&rig.config, "  ", &image), None);
    // Assigned but unusable (no model server in tests): reported on the line, never an error.
    let line = prompt_similarity_line(&rig.config, "a red square", &rig.ws.root().join("gone.png"))
        .unwrap();
    assert!(line.starts_with("Prompt similarity unavailable"), "{line}");
}

#[test]
#[ignore = "loads a downloaded image embedding model; set DIAL_TEST_IMAGE_EMBEDDING_MODEL and DIAL_TEST_IMAGE_DIR (holding red_circle.png and other images)"]
fn real_model_finds_the_described_image() {
    let env = |key: &str| std::env::var(key).unwrap_or_else(|_| panic!("{key} is required"));
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("project").canonicalize_or_create();
    for entry in std::fs::read_dir(env("DIAL_TEST_IMAGE_DIR"))
        .unwrap()
        .flatten()
    {
        if is_image(&entry.path()) {
            std::fs::copy(entry.path(), root.join(entry.file_name())).unwrap();
        }
    }
    let mut config = Config {
        state_dir: dir.path().join("state"),
        user_dir: dir.path().join("user"),
        python: PathBuf::from(env("HOME")).join(".dial/python/.venv/bin/python"),
        ..Config::default()
    };
    config.model_roles.insert(
        embedding::ROLE.into(),
        crate::llm::model_config::ModelSelection::new(
            "mlx",
            &env("DIAL_TEST_IMAGE_EMBEDDING_MODEL"),
        ),
    );
    let ws = Workspace::new(&root).unwrap();
    let index = Index::open(&config.state_dir.join("image_embeddings.sqlite3")).unwrap();
    let embedder = Embedder::Model(Arc::new(AtomicBool::new(false)));
    let text = run(
        &ws,
        &config,
        &index,
        &json!({"mode": "text", "query": "a red circle"}),
        &embedder,
    )
    .unwrap();
    let score = run(
        &ws,
        &config,
        &index,
        &json!({"mode": "score", "query": "a red circle", "images": ["red_circle.png"]}),
        &embedder,
    )
    .unwrap();
    embedding::release();
    println!("TEXT:\n{text}SCORE:\n{score}");
    assert_eq!(
        ranked_paths(&text).first(),
        Some(&"red_circle.png"),
        "{text}"
    );
    let first_vector_len = index
        .0
        .query_row("SELECT length(vector) FROM vectors LIMIT 1", [], |row| {
            row.get::<_, i64>(0)
        })
        .unwrap();
    assert!(first_vector_len >= 4 * 256, "{first_vector_len}");
}
