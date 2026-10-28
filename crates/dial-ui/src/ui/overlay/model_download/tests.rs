use super::*;
use crate::stats::DecodeSpeed;
use crate::stats::local_models::{Improvement, Memory};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

fn row(label: &str) -> ModelRow {
    ModelRow {
        label: label.into(),
        path: format!("/models/{label}"),
        source: "test".into(),
        format: Some("MLX"),
        speed: DecodeSpeed::default(),
        memory: Memory {
            weights_bytes: None,
            resident_bytes: None,
            context_tokens: None,
            fit: Fit::Unknown,
        },
        improvement: Improvement {
            store: "no_store",
            store_dir: None,
            version: None,
            kind: None,
            adapter: None,
            last_gate: None,
            verified_turns: None,
            lora_min_examples: 24,
            new_turns: None,
            min_new_turns: 5,
            cooldown_secs: None,
            waiting: Some(vec!["no recorded turns for this model yet".into()]),
        },
    }
}

fn dashboard(models: Vec<ModelRow>) -> Dashboard {
    Dashboard {
        generated_ms: 0,
        memory_bytes: None,
        memory_source: "unknown",
        stats: "missing",
        models,
    }
}

fn text_of(lines: &[Line<'static>]) -> String {
    lines
        .iter()
        .map(|line| {
            line.spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn selection_pane(models: Vec<ModelRow>) -> ModelDownloads {
    let mut group = ModelDownloads::fixed(dashboard(models));
    group.focus = Focus::Results;
    group
}

#[test]
fn arrows_move_the_selection_within_the_list_and_escape_backs_out() {
    let keys = Keymap::defaults();
    let mut group = selection_pane(vec![row("a"), row("b")]);
    group.key(&key(KeyCode::Down), &keys);
    group.key(&key(KeyCode::Down), &keys);
    assert_eq!(group.cursor, 2, "suggestions precede installed models");
    assert!(matches!(group.current(), Some(Pick::Hit(2))));
    let last = group.picks().len() - 1;
    for _ in 0..=last {
        group.key(&key(KeyCode::Down), &keys);
    }
    assert_eq!(group.cursor, last, "the cursor stops at the last result");
    for _ in 1..last {
        group.key(&key(KeyCode::Up), &keys);
    }
    assert_eq!(group.cursor, 1);
    assert!(matches!(group.current(), Some(Pick::Hit(1))));
    group.key(&key(KeyCode::Up), &keys);
    assert_eq!(group.cursor, 0);
    assert_eq!(group.key(&key(KeyCode::Esc), &keys), Step::Back);
    group.paste("qwen");
    assert_eq!(group.key(&key(KeyCode::Esc), &keys), Step::Handled);
    assert!(group.query.is_empty(), "Esc clears a query first");
    assert_eq!(group.focus, Focus::Results);
    assert_eq!(group.key(&key(KeyCode::Esc), &keys), Step::Back);
}

#[test]
fn enter_on_an_installed_model_asks_for_it_as_the_main_model() {
    let keys = Keymap::defaults();
    let mut group = selection_pane(vec![row("a"), row("b")]);
    group.cursor = group
        .picks()
        .iter()
        .position(|pick| matches!(pick, Pick::Installed(1)))
        .unwrap();
    assert_eq!(
        group.key(&key(KeyCode::Enter), &keys),
        Step::UseModel("/models/b".into())
    );
    let mut empty = ModelDownloads::fixed(dashboard(Vec::new()));
    empty.focus = Focus::Results;
    assert_eq!(empty.key(&key(KeyCode::Enter), &keys), Step::Handled);
}

#[test]
fn typing_edits_the_query_and_never_starts_a_search_or_download() {
    let keys = Keymap::defaults();
    let mut group = ModelDownloads::fixed(dashboard(vec![row("a")]));
    assert!(group.curated && !group.hits.is_empty());
    for c in "qwen".chars() {
        group.key(&KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE), &keys);
    }
    group.key(&key(KeyCode::Backspace), &keys);
    assert_eq!(group.query_text(), "qwe");
    assert!(group.idle_now(), "typing never starts a search or download");
}

#[test]
fn an_empty_query_shows_the_curated_list_for_the_chosen_format_without_network() {
    let keys = Keymap::defaults();
    let mut group = ModelDownloads::fixed(dashboard(Vec::new()));
    group.key(&key(KeyCode::Up), &keys);
    group.key(&key(KeyCode::Enter), &keys);
    assert!(group.search.is_none());
    assert!(group.curated);
    group.key(&key(KeyCode::Tab), &keys);
    assert_eq!(group.shelf, Shelf::Hub(Family::Gguf));
    assert!(group.hits.iter().all(|hit| hit.family == Family::Gguf));
}

#[test]
fn opening_selects_a_visible_row_without_searching() {
    let keys = Keymap::defaults();
    let group = ModelDownloads::idle();
    assert_eq!(group.focus, Focus::Results);
    assert!(matches!(group.current(), Some(Pick::Hit(0))));
    assert!(group.idle_now());
    let shown = text_of(&group.body(100, 8, &Theme::dark()));
    assert!(shown.contains(&group.hits[0].repo), "{shown}");

    let mut installed = ModelDownloads::fixed(dashboard(vec![row("ready")]));
    assert!(matches!(installed.current(), Some(Pick::Hit(_))));
    installed.cursor = installed
        .picks()
        .iter()
        .position(|pick| matches!(pick, Pick::Installed(0)))
        .unwrap();
    assert_eq!(
        installed.key(&key(KeyCode::Enter), &keys),
        Step::UseModel("/models/ready".into())
    );
}

#[test]
fn short_panes_keep_the_first_model_visible_without_a_query() {
    let group = ModelDownloads::fixed(dashboard(Vec::new()));
    for height in 1..=5 {
        let lines = group.body(100, height, &Theme::dark());
        let shown = text_of(&lines);
        assert!(lines.len() <= height);
        assert!(
            shown.contains(&group.hits[0].repo),
            "height {height}: {shown}"
        );
    }
}

#[test]
fn clearing_query_restores_recommendations_and_ignores_old_responses() {
    let keys = Keymap::defaults();
    let mut group = ModelDownloads::fixed(dashboard(Vec::new()));
    group.paste("x");
    let (tx, rx) = mpsc::channel();
    group.search = Some(rx);
    group.hits.clear();
    group.curated = false;
    tx.send(Ok(Vec::new())).unwrap();
    group.key(&key(KeyCode::Backspace), &keys);
    group.poll();
    assert_eq!(group.query_text(), "");
    assert!(group.curated);
    assert_eq!(group.hits, download::recommended(Family::Mlx));
    assert!(group.idle_now());
}

#[test]
fn completed_search_focuses_results_so_enter_selects_instead_of_searching_again() {
    let keys = Keymap::defaults();
    let mut group = ModelDownloads::fixed(dashboard(Vec::new()));
    group.paste("qwen");
    let (tx, rx) = mpsc::channel();
    group.search = Some(rx);
    tx.send(Ok(download::recommended(Family::Mlx))).unwrap();
    group.poll();
    assert_eq!(group.focus, Focus::Results);
    assert!(matches!(group.current(), Some(Pick::Hit(0))));
    group.key(&key(KeyCode::Enter), &keys);
    assert!(group.message.contains("Downloads are unavailable"));
    assert!(group.idle_now(), "Enter must not start another search");
}

#[test]
fn enter_on_an_embedding_row_asks_for_its_setup() {
    let keys = Keymap::defaults();
    let mut group = ModelDownloads::fixed(dashboard(Vec::new()));
    group.shelf = Shelf::Embedding;
    group.focus = Focus::Results;
    let first = embeddings::CATALOG[0].name;
    assert_eq!(
        group.key(&key(KeyCode::Enter), &keys),
        Step::SetupEmbedding(first.into())
    );
    assert_eq!(embeddings::setup_id(first), format!("embeddings:{first}"));
}

fn embedder(provider: &str, model: &str, available: bool) -> ModelEntry {
    ModelEntry {
        selection: ModelSelection::new(provider, model),
        label: model.into(),
        source: provider.into(),
        available,
        metadata: crate::llm::catalog::ModelMetadata {
            kind: crate::llm::catalog::ModelKind::Embedding,
            ..Default::default()
        },
    }
}

#[test]
fn discovered_embedding_models_lead_the_shelf_and_enter_assigns_them() {
    let keys = Keymap::defaults();
    let mut group = ModelDownloads::fixed(dashboard(Vec::new()));
    group.shelf = Shelf::Embedding;
    group.focus = Focus::Results;
    let mut chat = embedder("ollama", "llama3:latest", true);
    chat.metadata.kind = crate::llm::catalog::ModelKind::Chat;
    group.set_embedding_models(&[
        embedder("ollama", "embeddinggemma:latest", true),
        chat,
        embedder("lm-studio", "text-embedding-qwen3", true),
        embedder("mlx", "/models/bge", false),
    ]);
    let shown = text_of(&group.body(120, 30, &Theme::dark()));
    assert!(shown.contains("ollama/embeddinggemma:latest"), "{shown}");
    assert!(shown.contains("lm-studio/text-embedding-qwen3"), "{shown}");
    assert!(shown.contains("not served"), "{shown}");
    assert!(!shown.contains("llama3"), "chat models stay off the shelf");
    assert!(
        !group
            .catalog_rows()
            .iter()
            .any(|m| m.name == "embeddinggemma"),
        "a curated model already served by Ollama is listed once"
    );
    assert_eq!(
        group.key(&key(KeyCode::Enter), &keys),
        Step::UseEmbedding(ModelSelection::new("ollama", "embeddinggemma:latest"))
    );
    group.cursor = 2;
    assert_eq!(group.key(&key(KeyCode::Enter), &keys), Step::Handled);
    assert!(group.message.contains("cannot be assigned yet"));
    group.cursor = 3;
    assert!(matches!(group.current(), Some(Pick::Embedding(_))));
}

#[test]
fn escape_cancels_a_running_download_before_it_closes_the_panel() {
    let keys = Keymap::defaults();
    let mut group = ModelDownloads::fixed(dashboard(vec![row("a")]));
    let (cancel, _done) = group.fake_download("org/model");
    assert_eq!(group.key(&key(KeyCode::Esc), &keys), Step::Handled);
    assert!(cancel.load(Ordering::SeqCst), "Esc sets the cancel flag");
    assert!(group.message.contains("Cancelling"), "{}", group.message);
    assert!(
        group.task.is_some(),
        "the worker reports the outcome itself"
    );
    group.task = None;
    assert_eq!(group.key(&key(KeyCode::Esc), &keys), Step::Back);
}

#[test]
fn a_finished_download_reports_its_result_and_keeps_the_group_usable() {
    let mut group = ModelDownloads::fixed(dashboard(vec![row("a")]));
    let (_cancel, done) = group.fake_download("org/model");
    done.send(Err("download cancelled".to_owned())).unwrap();
    group.poll();
    assert!(group.task.is_none());
    assert!(group.message.contains("failed"), "{}", group.message);
    assert!(
        group.message.contains("download cancelled"),
        "{}",
        group.message
    );
    assert!(!group.take_rescan());
}

#[test]
fn a_finished_download_asks_for_a_rescan_once() {
    let mut group = ModelDownloads::fixed(dashboard(vec![row("a")]));
    let (_cancel, done) = group.fake_download("org/model");
    done.send(Ok("/no/such/path".to_owned())).unwrap();
    group.poll();
    assert!(group.take_rescan());
    assert!(!group.take_rescan());
}

#[test]
fn same_model_accepts_a_file_inside_the_listed_directory() {
    let temp = tempfile::tempdir().unwrap();
    let dir = temp.path().join("snapshot");
    std::fs::create_dir(&dir).unwrap();
    let file = dir.join("m-Q4.gguf");
    std::fs::write(&file, b"x").unwrap();
    let other = temp.path().join("other");
    std::fs::create_dir(&other).unwrap();
    assert!(same_model(&dir.to_string_lossy(), &file.to_string_lossy()));
    assert!(same_model(&dir.to_string_lossy(), &dir.to_string_lossy()));
    assert!(!same_model(
        &other.to_string_lossy(),
        &file.to_string_lossy()
    ));
}

#[test]
fn unknown_sources_render_as_unknown_not_as_numbers() {
    let mut group = selection_pane(vec![row("mlx-model")]);
    group.cursor = group
        .picks()
        .iter()
        .position(|pick| matches!(pick, Pick::Installed(0)))
        .unwrap();
    let theme = Theme::dark();
    let shown = text_of(&group.body(120, 30, &theme));
    assert!(shown.contains("stats missing"), "{shown}");
    let line = shown
        .lines()
        .find(|line| line.contains("mlx-model"))
        .unwrap_or_default();
    assert!(
        line.contains("[MLX]") && line.contains("tok/s unknown") && line.contains("fit unknown"),
        "{shown}"
    );
    let (_, about) = group.detail(&theme);
    let about = text_of(&[Line::from(about)]);
    assert!(about.contains("unknown (stats unavailable)"), "{about}");
    assert!(
        about.contains("no recorded turns for this model yet"),
        "{about}"
    );
}

#[test]
fn an_empty_or_tiny_dashboard_draws_without_panicking() {
    let theme = Theme::dark();
    let empty = ModelDownloads::fixed(dashboard(Vec::new()));
    assert!(text_of(&empty.body(80, 24, &theme)).contains("None downloaded in MLX yet"));
    let tiny = ModelDownloads::fixed(dashboard(vec![row("a"), row("b"), row("c")]));
    assert!(!tiny.body(40, 4, &theme).is_empty());
}

#[test]
fn the_list_keeps_a_row_on_short_panels() {
    let theme = Theme::dark();
    let group = selection_pane(vec![row("alpha"), row("beta")]);
    for height in [4, 8, 12, 13] {
        let lines = group.body(80, height, &theme);
        assert!(
            text_of(&lines).contains(&group.hits[0].repo),
            "height {height}"
        );
        assert!(lines.len() <= height, "height {height}");
    }
}

#[test]
fn the_fit_verdict_survives_narrow_rows() {
    let theme = Theme::dark();
    let mut long = row("a-very-long-model-name-that-would-crowd-out-the-verdict");
    long.memory.fit = Fit::WontFit;
    let group = selection_pane(vec![long]);
    for width in [48, 60, 80] {
        let lines = group.body(width, 20, &theme);
        let shown = text_of(&lines);
        let line = shown
            .lines()
            .find(|line| line.contains("[MLX]"))
            .unwrap_or_else(|| panic!("installed row missing at width {width}: {shown}"));
        assert!(line.contains("will not fit"), "{width}: {line}");
        assert!(text::width(line) <= width, "{width}: {line}");
    }
}

#[test]
fn find_view_shows_results_with_counts_and_a_list_files_footer() {
    let theme = Theme::dark();
    let keys = Keymap::defaults();
    let mut group = ModelDownloads::fixed(dashboard(Vec::new()));
    group.focus = Focus::Results;
    group.curated = false;
    group.hits = vec![Hit {
        repo: "mlx-community/Qwen3-4B-4bit".into(),
        family: Family::Mlx,
        downloads: Some(12_345),
        likes: Some(7),
        created_at: Some("2025-01-01T00:00:00.000Z".into()),
        size_bytes: Some(3 << 30),
        parameters: None,
    }];
    let shown = text_of(&group.body(100, 20, &theme));
    assert!(shown.contains("Qwen3-4B-4bit"), "{shown}");
    assert!(shown.contains("4B"), "{shown}");
    assert!(shown.contains("12.3k"), "{shown}");
    assert!(shown.contains("3.0 GiB"), "{shown}");
    assert!(shown.contains("published"), "{shown}");
    assert!(
        group
            .footer(&keys)
            .iter()
            .any(|hint| hint.contains("list files"))
    );
}

#[test]
fn quantization_picker_lists_each_choice_with_its_size() {
    let theme = Theme::dark();
    let mut group = ModelDownloads::fixed(dashboard(Vec::new()));
    group.picker = Some(Picker {
        repo: "org/gguf".into(),
        quants: vec![
            Quant {
                label: "m-Q4_K_M".into(),
                files: vec!["m-Q4_K_M.gguf".into()],
                bytes: 2 << 30,
            },
            Quant {
                label: "m-Q8_0".into(),
                files: vec!["m-Q8_0.gguf".into(), "mmproj.gguf".into()],
                bytes: 512 << 20,
            },
        ],
        cursor: 0,
    });
    let shown = text_of(&group.body(90, 20, &theme));
    assert!(shown.contains("m-Q4_K_M"), "{shown}");
    assert!(shown.contains("2.0 GiB"), "{shown}");
    assert!(shown.contains("512 MiB"), "{shown}");
    assert!(shown.contains("2 files"), "{shown}");
}

#[test]
fn window_and_size_helpers_keep_the_cursor_visible_and_read_plainly() {
    assert_eq!(window_start(0, 3), 0);
    assert_eq!(window_start(2, 3), 0);
    assert_eq!(window_start(5, 3), 3);
    assert_eq!(human_bytes(12), "12 B");
    assert_eq!(human_bytes(8 << 10), "8 KiB");
    assert_eq!(human_bytes(312 << 20), "312 MiB");
    assert_eq!(human_bytes(3 << 30), "3.0 GiB");
    assert_eq!(compact(None), "n/a");
    assert_eq!(compact(Some(812)), "812");
    assert_eq!(compact(Some(34_500)), "34.5k");
    assert_eq!(compact(Some(1_200_000)), "1.2M");
}

#[test]
fn shelves_cycle_through_mlx_gguf_memory_embeddings_and_curated_both_ways() {
    assert_eq!(Shelf::Hub(Family::Mlx).next(), Shelf::Hub(Family::Gguf));
    assert_eq!(Shelf::Hub(Family::Gguf).next(), Shelf::Memory);
    assert_eq!(Shelf::Memory.next(), Shelf::Images);
    assert_eq!(Shelf::Images.next(), Shelf::EmbeddingHub);
    assert_eq!(Shelf::EmbeddingHub.next(), Shelf::Embedding);
    assert_eq!(Shelf::Embedding.next(), Shelf::Hub(Family::Mlx));
    assert_eq!(Shelf::Hub(Family::Mlx).prev(), Shelf::Embedding);
    assert_eq!(Shelf::Embedding.prev(), Shelf::EmbeddingHub);
    assert_eq!(Shelf::EmbeddingHub.prev(), Shelf::Images);
    assert_eq!(Shelf::Images.prev(), Shelf::Memory);
    assert_eq!(Shelf::Memory.prev(), Shelf::Hub(Family::Gguf));
}

#[test]
fn arrows_cycle_shelves_and_preserve_navigation_focus() {
    let mut group = ModelDownloads::fixed(dashboard(Vec::new()));
    let keys = Keymap::defaults();
    for shelf in [
        Shelf::Hub(Family::Gguf),
        Shelf::Memory,
        Shelf::Images,
        Shelf::EmbeddingHub,
        Shelf::Embedding,
        Shelf::Hub(Family::Mlx),
    ] {
        assert_eq!(group.key(&key(KeyCode::Right), &keys), Step::Handled);
        assert_eq!(group.shelf, shelf);
        assert_eq!(group.focus, Focus::Results);
    }
    group.key(&key(KeyCode::Left), &keys);
    assert_eq!(group.shelf, Shelf::Embedding);
    group.key(&key(KeyCode::BackTab), &keys);
    assert_eq!(group.shelf, Shelf::EmbeddingHub);
}

#[test]
fn memory_shelf_offers_small_curated_models_and_every_installed_format_without_network() {
    let keys = Keymap::defaults();
    let mut gguf = row("small-gguf");
    gguf.format = Some("GGUF");
    let mut group = ModelDownloads::fixed(dashboard(vec![row("small-mlx"), gguf]));
    group.show_memory();
    assert_eq!(group.shelf, Shelf::Memory);
    assert!(
        group.search.is_none(),
        "the Memory shelf never browses on its own"
    );
    assert!(group.curated);
    assert_eq!(
        group.hits,
        download::recommended_memory(download::default_family())
    );
    assert_eq!(
        group.installed_rows(),
        vec![0, 1],
        "installed rows of any format"
    );
    group.focus = Focus::Results;
    group.cursor = group
        .picks()
        .iter()
        .position(|pick| matches!(pick, Pick::Installed(1)))
        .unwrap();
    assert_eq!(
        group.key(&key(KeyCode::Enter), &keys),
        Step::UseMemory("/models/small-gguf".into()),
        "Enter on an installed model assigns the memory role, not main"
    );
}

#[test]
fn suggestions_are_visible_before_many_installed_models_and_exclude_duplicates() {
    let recommendations = download::recommended(Family::Mlx);
    let mut installed: Vec<_> = (0..20).map(|i| row(&format!("installed-{i}"))).collect();
    installed.push(row(&recommendations[0].repo));
    let group = ModelDownloads::fixed(dashboard(installed));
    assert_eq!(group.available_hits(), vec![1, 2]);
    assert!(matches!(group.current(), Some(Pick::Hit(1))));
    let shown = text_of(&group.body(100, 8, &Theme::dark()));
    assert!(shown.contains(&recommendations[1].repo), "{shown}");
    assert!(!shown.contains("installed-0"), "{shown}");
    assert!(!shown.contains(&recommendations[0].repo), "{shown}");
}

#[test]
fn browse_failure_preserves_offline_choices_and_surfaces_error() {
    let mut group = ModelDownloads::fixed(dashboard(Vec::new()));
    let (tx, rx) = mpsc::channel();
    group.search = Some(rx);
    group.browse_attempted = true;
    tx.send(Err("network offline".into())).unwrap();
    group.poll();
    assert_eq!(group.hits, download::recommended(Family::Mlx));
    assert!(group.status.contains("network offline"));
    assert!(
        group.browse_attempted,
        "failed requests must not retry on every draw"
    );
    assert!(group.task.is_none());
    group.reload();
    assert!(!group.browse_attempted, "explicit refresh permits a retry");
}

#[test]
fn background_browse_keeps_selected_model_identity() {
    let mut group = ModelDownloads::fixed(dashboard(vec![row("installed")]));
    let chosen = group.hits[0].repo.clone();
    let (tx, rx) = mpsc::channel();
    group.search = Some(rx);
    let mut hit = group.hits[1].clone();
    hit.repo = "org/new-model".into();
    tx.send(Ok(vec![hit])).unwrap();
    group.poll();
    let Some(Pick::Hit(index)) = group.current() else {
        panic!("selected hit lost")
    };
    assert_eq!(group.hits[index].repo, chosen);
    assert!(group.hits.iter().any(|hit| hit.repo == "org/new-model"));
    assert!(group.task.is_none());
}

#[test]
fn metadata_updates_only_its_model_without_losing_counts_or_selection() {
    let mut group = ModelDownloads::fixed(dashboard(Vec::new()));
    group.hits[0].downloads = Some(123);
    let mut details = group.hits[0].clone();
    details.downloads = None;
    details.size_bytes = Some(2 << 30);
    details.created_at = Some("2025-01-02T00:00:00.000Z".into());
    let (tx, rx) = mpsc::channel();
    group.metadata = Some((details.repo.clone(), rx));
    group.cursor = 1;
    tx.send(Ok(details)).unwrap();
    group.poll();
    assert_eq!(group.cursor, 1);
    assert_eq!(group.hits[0].size_bytes, Some(2 << 30));
    assert_eq!(group.hits[0].downloads, Some(123));
    assert_eq!(group.hits[1].size_bytes, None);
    assert!(group.task.is_none());

    let (tx, rx) = mpsc::channel();
    group.metadata = Some((group.hits[0].repo.clone(), rx));
    tx.send(Ok(group.hits[0].clone())).unwrap();
    group.paste("other");
    assert!(
        group.metadata.is_none(),
        "edits invalidate stale metadata requests"
    );
}

#[test]
fn publication_age_uses_valid_utc_dates_and_never_guesses_missing_values() {
    let now = crate::platform::time::parse_utc("2025-02-01T00:00:00").unwrap();
    assert_eq!(
        published_age(Some("2025-01-31T00:00:00.123Z"), now),
        "1d ago"
    );
    assert_eq!(published_age(Some("2025-02-01T00:00:00Z"), now), "today");
    for date in [
        None,
        Some("invalid"),
        Some("2025-02-30T00:00:00Z"),
        Some("2025-01-01T00:00:00.badZ"),
        Some("2025-01-01T00:00:00+01:00"),
        Some("2026-01-01T00:00:00Z"),
        Some("日本語"),
    ] {
        assert_eq!(published_age(date, now), "unknown", "{date:?}");
    }
    let hit = &download::recommended(Family::Mlx)[0];
    let shown = hit_metadata(hit, now);
    assert!(shown.contains("repo unknown"));
    assert!(shown.contains("published unknown"));
}

#[test]
fn image_shelf_uses_metadata_and_never_assigns_downloaded_images_to_chat() {
    let mut group = selection_pane(vec![row("chat"), row("image")]);
    let mut image = ModelEntry {
        selection: ModelSelection::new("mlx", "/models/image"),
        label: "image".into(),
        source: "downloaded".into(),
        available: true,
        metadata: Default::default(),
    };
    image.metadata.kind = crate::llm::catalog::ModelKind::Image;
    image.selection.base_url = Some("http://127.0.0.1:11234/v1".into());
    let image_selection = image.selection.clone();
    group.set_embedding_models(&[image]);
    assert_eq!(group.installed_rows(), [0]);
    group.shelf = Shelf::Memory;
    assert_eq!(group.installed_rows(), [0]);
    group.shelf = Shelf::Images;
    group.hits.clear();
    group.curated = false;
    assert_eq!(group.installed_rows(), [1]);
    assert_eq!(group.confirm(), Step::UseImage(image_selection));
    let shown = text_of(&group.body(140, 20, &Theme::dark()));
    assert!(shown.contains("image"), "{shown}");
    assert!(!shown.contains("/models/chat"), "{shown}");
    assert!(group.task.is_none(), "browsing/assigning never downloads");
}

#[test]
fn image_download_stays_image_after_switching_shelves_before_completion() {
    let mut group = selection_pane(vec![row("image")]);
    group.image_repos.insert("org/image".into());
    group.shelf = Shelf::Memory;
    group.finish_download("org/image", Family::Mlx, "/models/image");
    assert_eq!(group.shelf, Shelf::Images);
    assert_eq!(group.confirm(), Step::Handled);
    assert!(group.message.contains("MLX-Serve"));
    group.shelf = Shelf::Hub(Family::Mlx);
    assert!(group.installed_rows().is_empty());
    group.shelf = Shelf::Memory;
    assert!(group.installed_rows().is_empty());
}
